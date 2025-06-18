//! This example demonstrates a few things:
//! * Using irpc with a cloneable server struct instead of with an actor loop
//! * Manually implementing the connection loop
//! * Authenticating peers

use anyhow::Result;
use iroh::{protocol::Router, Endpoint};

use self::storage::{StorageClient, StorageServer};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    println!("Remote use");
    remote().await?;
    Ok(())
}

async fn remote() -> Result<()> {
    let (server_router, server_addr) = {
        let endpoint = Endpoint::builder().discovery_n0().bind().await?;
        let server = StorageServer::new("secret".to_string());
        let router = Router::builder(endpoint.clone())
            .accept(StorageServer::ALPN, server.clone())
            .spawn();
        let addr = endpoint.node_addr().await?;
        (router, addr)
    };

    // correct authentication
    let client_endpoint = Endpoint::builder().bind().await?;
    let api = StorageClient::connect(client_endpoint, server_addr.clone());
    api.auth("secret").await?;
    api.set("hello".to_string(), "world".to_string()).await?;
    api.set("goodbye".to_string(), "world".to_string()).await?;
    let value = api.get("hello".to_string()).await?;
    println!("value = {:?}", value);
    let mut list = api.list().await?;
    while let Some(value) = list.recv().await? {
        println!("list value = {:?}", value);
    }

    // invalid authentication
    let client_endpoint = Endpoint::builder().bind().await?;
    let api = StorageClient::connect(client_endpoint, server_addr.clone());
    assert!(api.auth("bad").await.is_err());
    assert!(api.get("hello".to_string()).await.is_err());

    // no authentication
    let client_endpoint = Endpoint::builder().bind().await?;
    let api = StorageClient::connect(client_endpoint, server_addr);
    assert!(api.get("hello".to_string()).await.is_err());

    drop(server_router);
    Ok(())
}

mod storage {
    //! Implementation of our storage service.
    //!
    //! The only `pub` item is [`StorageApi`], everything else is private.

    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };

    use anyhow::Result;
    use iroh::{
        endpoint::{Connection, RecvStream, SendStream},
        protocol::ProtocolHandler,
        Endpoint,
    };
    use irpc::{
        channel::{oneshot, spsc},
        Client, Service, WithChannels,
    };
    // Import the macro
    use irpc_derive::rpc_requests;
    use irpc_iroh::{read_request, IrohRemoteConnection};
    use serde::{Deserialize, Serialize};
    use tracing::info;

    const ALPN: &[u8] = b"storage-api/0";

    /// A simple storage service, just to try it out
    #[derive(Debug, Clone, Copy)]
    struct StorageService;

    impl Service for StorageService {}

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
    #[rpc_requests(StorageService, message = StorageMessage)]
    #[derive(Serialize, Deserialize)]
    enum StorageProtocol {
        #[rpc(tx=oneshot::Sender<Result<(), String>>)]
        Auth(Auth),
        #[rpc(tx=oneshot::Sender<Option<String>>)]
        Get(Get),
        #[rpc(tx=oneshot::Sender<()>)]
        Set(Set),
        #[rpc(tx=oneshot::Sender<u64>, rx=spsc::Receiver<(String, String)>)]
        SetMany(SetMany),
        #[rpc(tx=spsc::Sender<String>)]
        List(List),
    }

    #[derive(Debug, Clone)]
    pub struct StorageServer {
        state: Arc<Mutex<BTreeMap<String, String>>>,
        auth_token: String,
    }

    impl ProtocolHandler for StorageServer {
        fn accept(&self, conn: Connection) -> n0_future::future::Boxed<Result<()>> {
            let this = self.clone();
            Box::pin(async move {
                let mut authed = false;
                while let Some((msg, rx, tx)) = read_request(&conn).await? {
                    let msg_with_channels = upcast_message(msg, rx, tx);
                    match msg_with_channels {
                        StorageMessage::Auth(msg) => {
                            let WithChannels { inner, tx, .. } = msg;
                            if authed {
                                conn.close(1u32.into(), b"invalid message");
                                break;
                            } else if inner.token != this.auth_token {
                                conn.close(1u32.into(), b"permission denied");
                                break;
                            } else {
                                authed = true;
                                tx.send(Ok(())).await.ok();
                            }
                        }
                        _ => {
                            if !authed {
                                conn.close(1u32.into(), b"permission denied");
                                break;
                            } else {
                                this.handle_authenticated(msg_with_channels).await;
                            }
                        }
                    }
                }
                conn.closed().await;
                Ok(())
            })
        }
    }

    fn upcast_message(msg: StorageProtocol, rx: RecvStream, tx: SendStream) -> StorageMessage {
        match msg {
            StorageProtocol::Auth(msg) => WithChannels::from((msg, tx, rx)).into(),
            StorageProtocol::Get(msg) => WithChannels::from((msg, tx, rx)).into(),
            StorageProtocol::Set(msg) => WithChannels::from((msg, tx, rx)).into(),
            StorageProtocol::SetMany(msg) => WithChannels::from((msg, tx, rx)).into(),
            StorageProtocol::List(msg) => WithChannels::from((msg, tx, rx)).into(),
        }
    }

    impl StorageServer {
        pub const ALPN: &[u8] = ALPN;

        pub fn new(auth_token: String) -> Self {
            Self {
                state: Default::default(),
                auth_token,
            }
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
                    let WithChannels { mut tx, .. } = list;
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

    pub struct StorageClient {
        inner: Client<StorageMessage, StorageProtocol, StorageService>,
    }

    impl StorageClient {
        pub const ALPN: &[u8] = ALPN;

        pub fn connect(endpoint: Endpoint, addr: impl Into<iroh::NodeAddr>) -> StorageClient {
            let conn = IrohRemoteConnection::new(endpoint, addr.into(), Self::ALPN.to_vec());
            StorageClient {
                inner: Client::boxed(conn),
            }
        }

        pub async fn auth(&self, token: &str) -> Result<(), anyhow::Error> {
            self.inner
                .rpc(Auth {
                    token: token.to_string(),
                })
                .await?
                .map_err(|err| anyhow::anyhow!(err))
        }

        pub async fn get(&self, key: String) -> Result<Option<String>, irpc::Error> {
            self.inner.rpc(Get { key }).await
        }

        pub async fn list(&self) -> Result<spsc::Receiver<String>, irpc::Error> {
            self.inner.server_streaming(List, 10).await
        }

        pub async fn set(&self, key: String, value: String) -> Result<(), irpc::Error> {
            let msg = Set { key, value };
            self.inner.rpc(msg).await
        }
    }
}
