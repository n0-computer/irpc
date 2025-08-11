//! This example demonstrates a few things:
//! * Using irpc with a cloneable server struct instead of with an actor loop
//! * Manually implementing the connection loop
//! * Authenticating peers

use std::time::Duration;

use anyhow::Result;
use iroh::{protocol::Router, Endpoint, NodeAddr, SecretKey, Watcher};

use self::storage::{StorageClient, StorageServer};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    println!("Remote use");
    remote().await?;
    Ok(())
}

async fn remote() -> Result<()> {
    let server_secret_key = SecretKey::generate(&mut rand::rngs::OsRng);
    let server_addr = NodeAddr::new(server_secret_key.public());
    let start_server = async move || {
        let endpoint = Endpoint::builder()
            .secret_key(server_secret_key.clone())
            .discovery_n0()
            .bind()
            .await?;
        let server = StorageServer::new("secret".to_string());
        let router = Router::builder(endpoint.clone())
            .accept(StorageServer::ALPN, server.clone())
            .spawn();
        let _ = endpoint.home_relay().initialized().await;
        // wait a bit for publishing to complete..
        tokio::time::sleep(Duration::from_millis(500)).await;
        anyhow::Ok(router)
    };
    let mut server_router = (start_server)().await?;

    // correct authentication
    let client_endpoint = Endpoint::builder().discovery_n0().bind().await?;
    let api = StorageClient::connect(client_endpoint, server_addr.clone(), "secret");
    api.set("hello".to_string(), "world".to_string()).await?;
    api.set("goodbye".to_string(), "world".to_string()).await?;
    let value = api.get("hello".to_string()).await?;
    println!("value = {value:?}");
    let mut list = api.list().await?;
    while let Some(value) = list.recv().await? {
        println!("list value = {value:?}");
    }

    // restart server
    server_router.shutdown().await?;
    server_router = (start_server)().await?;

    // reconnections work: client will transparently reauthenticate
    println!("restarting server");
    let value = api.get("hello".to_string()).await?;
    println!("value = {value:?}");
    api.set("hello".to_string(), "world".to_string()).await?;
    let value = api.get("hello".to_string()).await?;
    println!("value = {value:?}");

    // invalid authentication
    let client_endpoint = Endpoint::builder().bind().await?;
    let api = StorageClient::connect(client_endpoint, server_addr.clone(), "bad");
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

    use anyhow::{anyhow, Result};
    use iroh::{
        endpoint::Connection,
        protocol::{AcceptError, ProtocolHandler},
        Endpoint,
    };
    use irpc::{
        channel::{mpsc, oneshot},
        rpc_requests, Client, Request, RequestError, WithChannels,
    };
    use irpc_iroh::{read_request, IrohRemoteConnection};
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
        // Connection will be closed if auth fails.
        #[rpc(tx=oneshot::Sender<()>)]
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
    }

    impl ProtocolHandler for StorageServer {
        async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
            // read first message: must be auth!
            let msg = read_request::<StorageProtocol>(&conn).await?;
            let auth_ok = if let Some(StorageMessage::Auth(msg)) = msg {
                let WithChannels { inner, tx, .. } = msg;
                if inner.token == self.auth_token {
                    tx.send(()).await.ok();
                    true
                } else {
                    false
                }
            } else {
                false
            };

            // if not authenticated: close connection immediately.
            if !auth_ok {
                conn.close(1u32.into(), b"permission denied");
                return Ok(());
            }

            // now the connection is authenticated and we can handle all subsequent requests.
            while let Some(msg) = read_request::<StorageProtocol>(&conn).await? {
                self.handle_request(msg).await;
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
            }
        }

        async fn handle_request(&self, msg: StorageMessage) {
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

    pub struct StorageClient {
        api_token: String,
        inner: Client<StorageProtocol>,
    }

    impl StorageClient {
        pub const ALPN: &[u8] = ALPN;

        pub fn connect(
            endpoint: Endpoint,
            addr: impl Into<iroh::NodeAddr>,
            api_token: &str,
        ) -> StorageClient {
            let conn = IrohRemoteConnection::new(endpoint, addr.into(), Self::ALPN.to_vec());
            StorageClient {
                api_token: api_token.to_string(),
                inner: Client::boxed(conn),
            }
        }

        async fn authenticated_request(&self) -> Result<Request<StorageProtocol>, irpc::Error> {
            let request = self.inner.request().await?;

            // if the connection is not new: no need to reauthenticate.
            if !request.is_new_connection() {
                return Ok(request);
            }

            // if this is a new connection: use this request to send an auth message.
            request
                .rpc(Auth {
                    token: self.api_token.clone(),
                })
                .await?;
            // and create a new request for the actual call.
            let request = self.inner.request().await?;
            // if this *again* created a new connection, we error out.
            if request.is_new_connection() {
                Err(RequestError::Other(anyhow!("Connection is reconnecting too often")).into())
            } else {
                Ok(request)
            }
        }

        pub async fn get(&self, key: String) -> Result<Option<String>, irpc::Error> {
            self.authenticated_request().await?.rpc(Get { key }).await
        }

        pub async fn list(&self) -> Result<mpsc::Receiver<String>, irpc::Error> {
            self.authenticated_request()
                .await?
                .server_streaming(List, 10)
                .await
        }

        pub async fn set(&self, key: String, value: String) -> Result<(), irpc::Error> {
            let msg = Set { key, value };
            self.authenticated_request().await?.rpc(msg).await
        }
    }
}
