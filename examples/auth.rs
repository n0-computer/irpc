//! Connection authentication that survives reconnects.
//!
//! The server keeps authentication state for each connection. The client sends
//! an Auth request from its initializer before making application requests.

use std::time::Duration;

use anyhow::Result;
use iroh::{Endpoint, endpoint::presets, protocol::Router};

use self::storage::{StorageClient, StorageServer};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    println!("Remote use");
    remote().await?;
    Ok(())
}

async fn remote() -> Result<()> {
    let server = StorageServer::new("secret".to_string());
    let (server_router, server_addr) = {
        let endpoint = Endpoint::bind(presets::N0).await?;
        let router = Router::builder(endpoint.clone())
            .accept(StorageServer::ALPN, server.clone())
            .spawn();
        let addr = endpoint.addr();
        (router, addr)
    };

    // Authentication runs before the first request on every new connection.
    let client_endpoint = Endpoint::builder(presets::N0).bind().await?;
    let api = StorageClient::connect(
        client_endpoint.clone(),
        server_addr.clone(),
        "secret".into(),
    )
    .await?;
    api.set("hello".to_string(), "world".to_string()).await?;
    api.set("goodbye".to_string(), "world".to_string()).await?;
    let value = api.get("hello".to_string()).await?;
    println!("value = {value:?}");
    let mut list = api.list().await?;
    while let Some(value) = list.recv().await? {
        println!("list value = {value:?}");
    }

    assert_eq!(server.authentications(), 1);

    // Closing the connection discards its authentication state, just as a server
    // restart would. The same client must authenticate before its next Get.
    server.disconnect_clients();
    println!("Server closed the connection. Reusing the same client...");

    // The client may not have observed the old connection's close yet. Retry
    // this read, which is safe to repeat. Do not blindly retry writes: the
    // server might have applied them before the response was lost.
    let value = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match api.get("hello".into()).await {
                Ok(value) => break value,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await?;
    assert_eq!(value.as_deref(), Some("world"));
    assert_eq!(server.authentications(), 2);
    println!("The same client authenticated the replacement connection.");

    // Clones share the initialized connection and can make concurrent calls.
    let other = api.clone();
    let (hello, goodbye) = tokio::try_join!(api.get("hello".into()), other.get("goodbye".into()))?;
    assert_eq!(hello.as_deref(), Some("world"));
    assert_eq!(goodbye.as_deref(), Some("world"));
    assert_eq!(server.authentications(), 2);

    // Eager connection setup reports invalid credentials before any normal RPC.
    let invalid_endpoint = Endpoint::builder(presets::N0).bind().await?;
    assert!(
        StorageClient::connect(invalid_endpoint.clone(), server_addr, "bad".into())
            .await
            .is_err()
    );

    invalid_endpoint.close().await;
    client_endpoint.close().await;
    server_router.shutdown().await?;
    Ok(())
}

mod storage {
    //! A storage service with authentication state for each connection.

    use std::{
        collections::BTreeMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use anyhow::Result;
    use iroh::{
        Endpoint,
        endpoint::{Connection, WeakConnectionHandle},
        protocol::{AcceptError, ProtocolHandler},
    };
    use irpc::{
        Client, WithChannels,
        channel::{mpsc, oneshot},
        rpc::read_request,
        rpc_requests,
    };
    use serde::{Deserialize, Serialize};
    use tracing::info;

    const ALPN: &[u8] = b"storage-api/0";

    #[derive(Debug, Serialize, Deserialize)]
    struct Auth {
        token: String,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct Get {
        key: String,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct List;

    #[derive(Debug, Serialize, Deserialize)]
    struct Set {
        key: String,
        value: String,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct SetMany;

    // Use the macro to generate both the StorageProtocol and StorageMessage enums
    // plus implement Channels for each type
    #[rpc_requests(message = StorageMessage)]
    #[derive(Serialize, Deserialize, Debug)]
    enum StorageProtocol {
        #[rpc(tx=oneshot::Sender<Result<(), String>>)]
        Auth(Auth),
        #[rpc(tx=oneshot::Sender<Option<String>>)]
        Get(Get),
        #[rpc(tx=oneshot::Sender<()>)]
        Set(Set),
        #[rpc(tx=oneshot::Sender<u64>, rx=mpsc::Receiver<(String, String)>)]
        SetMany(SetMany),
        #[rpc(tx=mpsc::Sender<String>)]
        List(List),
    }

    #[derive(Debug, Clone)]
    pub struct StorageServer {
        state: Arc<Mutex<BTreeMap<String, String>>>,
        auth_token: String,
        connections: Arc<Mutex<Vec<WeakConnectionHandle>>>,
        authentications: Arc<AtomicUsize>,
    }

    impl ProtocolHandler for StorageServer {
        async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
            self.connections.lock().unwrap().push(conn.weak_handle());
            let mut authenticated = false;
            while let Some(msg) = read_request::<StorageProtocol>(&conn).await? {
                match msg {
                    StorageMessage::Auth(msg) => {
                        let WithChannels { inner, tx, .. } = msg;
                        if authenticated {
                            conn.close(400u32.into(), b"invalid message");
                            break;
                        } else if inner.token != self.auth_token {
                            conn.close(401u32.into(), b"permission denied");
                            break;
                        } else {
                            authenticated = true;
                            self.authentications.fetch_add(1, Ordering::Relaxed);
                            tx.send(Ok(())).await.ok();
                        }
                    }
                    msg => {
                        if !authenticated {
                            conn.close(401u32.into(), b"permission denied");
                            break;
                        } else {
                            self.handle_authenticated(msg).await;
                        }
                    }
                }
            }
            conn.closed().await;
            Ok(())
        }
    }

    impl StorageServer {
        pub const ALPN: &[u8] = ALPN;

        pub fn new(auth_token: String) -> Self {
            Self {
                state: Default::default(),
                auth_token,
                connections: Default::default(),
                authentications: Default::default(),
            }
        }

        /// Closes active connections so clients must authenticate again.
        pub fn disconnect_clients(&self) {
            for handle in self.connections.lock().unwrap().drain(..) {
                if let Some(conn) = handle.upgrade() {
                    conn.close(0u32.into(), b"reconnect demonstration");
                }
            }
        }

        /// Returns the number of successful authentications.
        pub fn authentications(&self) -> usize {
            self.authentications.load(Ordering::Relaxed)
        }

        async fn handle_authenticated(&self, msg: StorageMessage) {
            match msg {
                StorageMessage::Auth(_) => unreachable!("handled in ProtocolHandler::accept"),
                StorageMessage::Get(get) => {
                    info!("get {:?}", get);
                    let WithChannels { tx, inner, .. } = get;
                    let res = self.state.lock().unwrap().get(&inner.key).cloned();
                    tx.send(res).await.ok();
                }
                StorageMessage::Set(set) => {
                    info!("set {:?}", set);
                    let WithChannels { tx, inner, .. } = set;
                    self.state.lock().unwrap().insert(inner.key, inner.value);
                    tx.send(()).await.ok();
                }
                StorageMessage::SetMany(list) => {
                    let WithChannels { tx, mut rx, .. } = list;
                    let mut i = 0;
                    while let Ok(Some((key, value))) = rx.recv().await {
                        let mut state = self.state.lock().unwrap();
                        state.insert(key, value);
                        i += 1;
                    }
                    tx.send(i).await.ok();
                }
                StorageMessage::List(list) => {
                    info!("list {:?}", list);
                    let WithChannels { tx, .. } = list;
                    let values = {
                        let state = self.state.lock().unwrap();
                        // TODO: use async lock to not clone here.
                        let values: Vec<_> = state
                            .iter()
                            .map(|(key, value)| format!("{key}={value}"))
                            .collect();
                        values
                    };
                    for value in values {
                        if tx.send(value).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }

    #[derive(Clone)]
    pub struct StorageClient {
        inner: Client<StorageProtocol>,
    }

    impl StorageClient {
        pub const ALPN: &[u8] = ALPN;

        /// Connects and authenticates, then repeats authentication on reconnect.
        pub async fn connect(
            endpoint: Endpoint,
            addr: impl Into<iroh::EndpointAddr>,
            token: String,
        ) -> Result<StorageClient> {
            let inner = Client::iroh_builder(endpoint, addr, Self::ALPN)
                .on_connect(move |client| {
                    let token = token.clone();
                    async move {
                        client
                            .rpc(Auth { token })
                            .await
                            .map_err(std::io::Error::from)?
                            .map_err(std::io::Error::other)
                    }
                })
                .build();
            // Optional: authenticate now so bad credentials fail at construction.
            // Without this, the first request connects and authenticates lazily.
            inner.connect().await?;
            Ok(StorageClient { inner })
        }

        pub async fn get(&self, key: String) -> Result<Option<String>, irpc::Error> {
            self.inner.rpc(Get { key }).await
        }

        pub async fn list(&self) -> Result<mpsc::Receiver<String>, irpc::Error> {
            self.inner.server_streaming(List, 10).await
        }

        pub async fn set(&self, key: String, value: String) -> Result<(), irpc::Error> {
            let msg = Set { key, value };
            self.inner.rpc(msg).await
        }
    }
}
