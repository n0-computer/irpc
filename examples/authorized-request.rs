//! Per-request authorization over an iroh connection.
//!
//! The QUIC handshake says *who* is calling, and nothing about what they may
//! do. That travels separately: an issuer signs a [`Delegation`] naming one
//! endpoint and what it may reach, the caller presents it once, and every
//! later request cites the [`CapId`] the server answered with.
//!
//! What this adds over `auth.rs`, which authenticates a connection once and
//! then trusts it:
//!
//! * [`Authorized<T>`] wraps every request, so a variant cannot be added
//!   without someone deciding what authorizes it.
//! * Authority is named per request, not per connection, so a client can
//!   present a fresh delegation while other requests are in flight.
//! * A refusal is a value, not a closed connection: requests answer
//!   `Result<_, ServerError>`, and a stream puts it in as its first item.
//! * A delegation names its audience, checked against the connection's
//!   remote id, so a leaked one is useless from elsewhere.
//!
//! `authorized-requests-long.rs` is the same with expiry and more prose.

use anyhow::Result;
use iroh::{Endpoint, SecretKey, endpoint::presets, protocol::Router};

use self::kv::{Access, Cap, Delegation, KvClient, KvServer};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // What the server roots authority at, which here is a key of its own.
    let issuer = SecretKey::generate();

    let server_endpoint = Endpoint::bind(presets::N0).await?;
    let addr = server_endpoint.addr();
    let router = Router::builder(server_endpoint)
        .accept(KvServer::ALPN, KvServer::new(issuer.public()))
        .spawn();

    // Each key is an identity on the wire and the audience of one delegation.
    let writer_key = SecretKey::generate();
    let reader_key = SecretKey::generate();
    let thief_key = SecretKey::generate();

    // A delegation that grants everything.
    let writer = KvClient::connect(
        endpoint(&writer_key).await?,
        addr.clone(),
        Delegation::issue(
            &issuer,
            writer_key.public(),
            Cap::new("", Access::ReadWrite),
        ),
    )
    .await?;
    writer.put("public/hello", "world").await?;
    writer.put("private/secret", "shhh").await?;
    println!("writer: wrote two keys");

    // A delegation that grants reading one prefix and nothing else.
    let reader = KvClient::connect(
        endpoint(&reader_key).await?,
        addr.clone(),
        Delegation::issue(
            &issuer,
            reader_key.public(),
            Cap::new("public/", Access::Read),
        ),
    )
    .await?;
    for key in ["public/hello", "private/secret"] {
        println!("reader: get {key:14} -> {}", answer(reader.get(key).await));
    }
    let put = answer(reader.put("public/hello", "x").await);
    println!("reader: put public/hello   -> {put}");

    // Listing inside the prefix streams; listing outside it is refused.
    for prefix in ["public/", ""] {
        let mut listed = reader.list(prefix).await?;
        while let Some(item) = listed.recv().await? {
            println!("reader: list {prefix:10}    -> {item:?}");
        }
    }

    // Granted to someone else. Holding it is not being its audience.
    let stolen = Delegation::issue(
        &issuer,
        writer_key.public(),
        Cap::new("", Access::ReadWrite),
    );
    let err = KvClient::connect(endpoint(&thief_key).await?, addr, stolen)
        .await
        .unwrap_err();
    println!("thief:  authenticate       -> {err}");

    drop(router);
    Ok(())
}

/// One demo answer, printed whichever way it went.
fn answer(res: Result<impl std::fmt::Debug, kv::Error>) -> String {
    match res {
        Ok(value) => format!("{value:?}"),
        Err(err) => format!("{err}"),
    }
}

async fn endpoint(secret_key: &SecretKey) -> Result<Endpoint> {
    let builder = Endpoint::builder(presets::N0).secret_key(secret_key.clone());
    Ok(builder.bind().await?)
}

mod kv {
    //! A key-value store where every request states what authorizes it.

    use std::{
        collections::{BTreeMap, VecDeque},
        sync::{Arc, Mutex},
    };

    use iroh::{
        Endpoint, EndpointAddr, EndpointId, PublicKey, SecretKey, Signature,
        endpoint::Connection,
        protocol::{AcceptError, ProtocolHandler},
    };
    use irpc::{
        Client, WithChannels,
        channel::{mpsc, oneshot},
        iroh::{IrohLazyRemoteConnection, read_request},
        rpc_requests,
    };
    use n0_error::stack_error;
    use serde::{Deserialize, Serialize};

    /// The domain-separation tag, so a signature made here cannot be replayed
    /// as one made anywhere else that signs with the same key.
    const DST: &[u8] = b"irpc/authorized-kv/delegation/0";

    /// What a cap allows over the keys it covers.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Access {
        Read,
        Write,
        ReadWrite,
    }

    /// What a delegation grants: an access over a key prefix. A real one
    /// would carry more verbs, several ranges, expiry, and a delegation chain.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Cap {
        prefix: String,
        access: Access,
    }

    impl Cap {
        pub fn new(prefix: impl Into<String>, access: Access) -> Self {
            let prefix = prefix.into();
            Self { prefix, access }
        }

        /// Returns `true` if `self` grants the permissions described by `other`.
        fn permits(&self, other: &Cap) -> bool {
            other.prefix.starts_with(&self.prefix)
                && (self.access == other.access || self.access == Access::ReadWrite)
        }
    }

    /// A signed capability grant: who it was granted to, what it carries, and
    /// the issuer's signature over both. One hop, where rcan chains these so a
    /// receiver can re-delegate an attenuated capability it was given.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Delegation {
        /// Checked against the connection's remote id, so a leak is useless.
        audience: EndpointId,
        cap: Cap,
        signature: Signature,
    }

    impl Delegation {
        /// Signs a delegation of `cap` to `audience`, which only the issuer can do.
        pub fn issue(issuer: &SecretKey, audience: EndpointId, cap: Cap) -> Self {
            let signature = issuer.sign(&signed_bytes(audience, &cap));
            Self {
                audience,
                cap,
                signature,
            }
        }

        /// Checks that this is backed by `principal` and was granted to
        /// `invoker`, and answers with the capability it carries.
        fn check_invocation_from(
            &self,
            principal: &PublicKey,
            invoker: EndpointId,
        ) -> Result<&Cap, InvocationError> {
            let over = signed_bytes(self.audience, &self.cap);
            if principal.verify(&over, &self.signature).is_err() {
                return Err(InvocationError::BadSignature);
            }
            if self.audience != invoker {
                return Err(InvocationError::InvokerMismatch);
            }
            Ok(&self.cap)
        }
    }

    fn signed_bytes(audience: EndpointId, cap: &Cap) -> Vec<u8> {
        let body = postcard::to_stdvec(&(audience, cap)).expect("a delegation encodes");
        [DST, &body].concat()
    }

    /// Why an invocation was not authorized. Each is a fact the invoker can
    /// act on, and none is secret: they hold the delegation, and the principal
    /// is public.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub enum InvocationError {
        /// Does not check out under the principal this server pins.
        BadSignature,
        /// Granted to another endpoint than the one invoking it.
        InvokerMismatch,
        /// Verifies, but does not permit what was asked for.
        NotPermitted,
        /// Never issued on this connection, or dropped to make room.
        UnknownCap,
    }

    #[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[stack_error(derive)]
    pub enum ServerError {
        #[error("unauthorized: {_0:?}")]
        Unauthorized(#[error(from)] InvocationError),
        #[error("not found")]
        NotFound,
    }

    #[stack_error(derive, add_meta, from_sources)]
    pub enum Error {
        #[error(transparent)]
        Rpc { source: irpc::Error },
        #[error(transparent)]
        Server {
            #[error(std_err)]
            source: ServerError,
        },
    }

    /// Presents a delegation, binding it to this connection. The invoker is
    /// its remote endpoint, authenticated by the handshake, and the audience.
    #[derive(Debug, Serialize, Deserialize)]
    pub struct Authenticate(Delegation);

    /// Names a verified capability, on the connection that presented it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct CapId(u64);

    pub use self::authorized::{Authorized, RequiresCap};

    /// Holds [`Authorized`] and nothing else, so its `request` field is out of
    /// reach from here. The only way to a request is to authorize it.
    mod authorized {
        use serde::{Deserialize, Serialize};

        use super::{Cap, CapId, CapStore, InvocationError};

        /// What a request takes before it may run. Also the bound on
        /// [`Authorized`], so a request type cannot travel in one until
        /// someone has said what authorizes it.
        pub trait RequiresCap {
            /// The authority the caller must hold for this request.
            fn required_cap(&self) -> Cap;
        }

        /// A request together with the cap it is invoked under. Naming the cap
        /// per request rather than per connection is what lets one be replaced
        /// mid-flight: a delegation lasts minutes, a watch hours.
        #[derive(Debug, Serialize, Deserialize)]
        pub struct Authorized<T: RequiresCap> {
            cap: CapId,
            request: T,
        }

        impl<T: RequiresCap> Authorized<T> {
            pub fn new(cap: CapId, request: T) -> Self {
                Self { cap, request }
            }

            /// Answers with the request, if the cap it cites is still held and
            /// permits what the request takes. The only way in, so a handler
            /// cannot serve a request it has not authorized.
            pub fn authorize(self, caps: &CapStore) -> Result<T, InvocationError> {
                match caps.get(self.cap) {
                    None => Err(InvocationError::UnknownCap),
                    Some(held) if !held.permits(&self.request.required_cap()) => {
                        Err(InvocationError::NotPermitted)
                    }
                    Some(_) => Ok(self.request),
                }
            }
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct Get(String);
    impl RequiresCap for Get {
        fn required_cap(&self) -> Cap {
            Cap::new(&self.0, Access::Read)
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct Put(String, String);
    impl RequiresCap for Put {
        fn required_cap(&self) -> Cap {
            Cap::new(&self.0, Access::Write)
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct List(String);
    impl RequiresCap for List {
        fn required_cap(&self) -> Cap {
            Cap::new(&self.0, Access::Read)
        }
    }

    #[rpc_requests(message = KvMessage)]
    #[derive(Debug, Serialize, Deserialize)]
    enum KvProtocol {
        /// Presents a delegation, which the server verifies once and caches.
        #[rpc(tx = oneshot::Sender<Result<CapId, ServerError>>)]
        Authenticate(Authenticate),
        #[rpc(tx = oneshot::Sender<Result<String, ServerError>>)]
        Get(Authorized<Get>),
        #[rpc(tx = oneshot::Sender<Result<(), ServerError>>)]
        Put(Authorized<Put>),
        /// Lists the keys under a prefix.
        #[rpc(tx = mpsc::Sender<Result<String, ServerError>>)]
        List(Authorized<List>),
    }

    /// The caps a connection has had verified, oldest first. Bounded, since
    /// presenting one is what an unauthenticated peer can reach; past the
    /// bound the oldest is dropped, as [`InvocationError::UnknownCap`] to its holder.
    #[derive(Default)]
    pub struct CapStore {
        issued: u64,
        held: VecDeque<(CapId, Cap)>,
    }

    impl CapStore {
        const MAX: usize = 4;

        /// Interns a verified cap, answering with the id requests cite it by.
        fn insert(&mut self, cap: Cap) -> CapId {
            self.issued += 1;
            let id = CapId(self.issued);
            if self.held.len() == Self::MAX {
                self.held.pop_front();
            }
            self.held.push_back((id, cap));
            id
        }

        fn get(&self, id: CapId) -> Option<&Cap> {
            self.held.iter().find(|(h, _)| *h == id).map(|(_, cap)| cap)
        }
    }

    /// Authorizes a request, or answers the refusal on the channel it came
    /// with and leaves the handler. On a streaming request that refusal is the
    /// stream's first item, so a caller is not left guessing why it was empty.
    ///
    /// A macro rather than a function because it expands to a `return` in its
    /// caller, and because the channel differs in type per request.
    macro_rules! authorize {
        ($inner:expr, $caps:expr, $tx:expr) => {
            match $inner.authorize($caps) {
                Ok(request) => request,
                Err(refusal) => {
                    $tx.send(Err(refusal.into())).await.ok();
                    return;
                }
            }
        };
    }

    /// Serves [`KvProtocol`] for delegations rooted at one principal.
    #[derive(Debug, Clone)]
    pub struct KvServer {
        kv: Arc<Mutex<BTreeMap<String, String>>>,
        principal: PublicKey,
    }

    impl KvServer {
        pub const ALPN: &[u8] = b"irpc-examples/authorized-kv/0";
        pub fn new(principal: PublicKey) -> Self {
            let kv = Default::default();
            Self { kv, principal }
        }

        /// Serves one request, each arm authorizing its own on the way in.
        async fn handle(&self, msg: KvMessage, remote: EndpointId, caps: &mut CapStore) {
            match msg {
                // The one request that authorizes itself.
                KvMessage::Authenticate(msg) => {
                    let WithChannels { inner, tx, .. } = msg;
                    let res = inner
                        .0
                        .check_invocation_from(&self.principal, remote)
                        .map(|cap| caps.insert(cap.clone()))
                        .map_err(ServerError::from);
                    tx.send(res).await.ok();
                }
                KvMessage::Get(msg) => {
                    let WithChannels { inner, tx, .. } = msg;
                    let Get(key) = authorize!(inner, caps, tx);
                    let found = self.kv.lock().unwrap().get(&key).cloned();
                    tx.send(found.ok_or(ServerError::NotFound)).await.ok();
                }
                KvMessage::Put(msg) => {
                    let WithChannels { inner, tx, .. } = msg;
                    let Put(key, value) = authorize!(inner, caps, tx);
                    self.kv.lock().unwrap().insert(key, value);
                    tx.send(Ok(())).await.ok();
                }
                KvMessage::List(msg) => {
                    let WithChannels { inner, tx, .. } = msg;
                    let List(prefix) = authorize!(inner, caps, tx);
                    // Cloned rather than held, since the lock may not cross an await.
                    let kv = self.kv.lock().unwrap().clone();
                    for key in kv.keys().filter(|k| k.starts_with(&prefix)) {
                        if tx.send(Ok(key.clone())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }

    impl ProtocolHandler for KvServer {
        async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
            // The handshake authenticated it. A cap id is per connection, and
            // means nothing on any other.
            let remote = conn.remote_id();
            let mut caps = CapStore::default();
            while let Some(msg) = read_request::<KvProtocol>(&conn).await? {
                self.handle(msg, remote, &mut caps).await;
            }
            conn.closed().await;
            Ok(())
        }
    }

    /// Talks to a [`KvServer`] under one delegation.
    #[derive(Debug)]
    pub struct KvClient {
        inner: Client<KvProtocol>,
        cap: CapId,
    }

    impl KvClient {
        /// Connects and presents `delegation`, verified once. A fresh one
        /// later answers with another [`CapId`]; requests in flight cite the old.
        pub async fn connect(
            endpoint: Endpoint,
            addr: impl Into<EndpointAddr>,
            delegation: Delegation,
        ) -> Result<Self, Error> {
            let conn =
                IrohLazyRemoteConnection::new(endpoint, addr.into(), KvServer::ALPN.to_vec());
            let inner = Client::boxed(conn);
            let cap = inner.rpc(Authenticate(delegation)).await??;
            Ok(Self { inner, cap })
        }

        pub async fn get(&self, key: &str) -> Result<String, Error> {
            let req = Authorized::new(self.cap, Get(key.into()));
            Ok(self.inner.rpc(req).await??)
        }

        pub async fn put(&self, key: &str, value: &str) -> Result<(), Error> {
            let req = Authorized::new(self.cap, Put(key.into(), value.into()));
            Ok(self.inner.rpc(req).await??)
        }

        /// The stream's own items stay fallible: a refusal arrives as the first.
        pub async fn list(
            &self,
            prefix: &str,
        ) -> Result<mpsc::Receiver<Result<String, ServerError>>, Error> {
            let req = Authorized::new(self.cap, List(prefix.into()));
            Ok(self.inner.server_streaming(req, 16).await?)
        }
    }
}
