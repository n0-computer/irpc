//! This example demonstrates using irpc-iroh with a cloneable state struct
//! on the server side instead of with an actor loop.

use anyhow::Result;
use iroh::{endpoint::presets, protocol::Router, Endpoint};

use self::storage::{StorageClient, StorageServer};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Start the server.
    let (server_router, server_addr) = {
        let endpoint = Endpoint::bind(presets::N0).await?;
        let storage = StorageServer::default();
        let router = Router::builder(endpoint)
            .accept(storage::ALPN, storage)
            .spawn();
        let addr = router.endpoint().addr();
        (router, addr)
    };

    // Connect by passing an endpoint, which allows automatic reconnection.
    let client_endpoint = Endpoint::bind(presets::N0).await?;
    let api = StorageClient::connect(client_endpoint, server_addr.clone());
    api.set("hello", "world").await?;
    api.set("goodbye", "see you soon").await?;
    let value = api.get("hello").await?;
    println!("hello = {value:?}");
    let mut list = api.list().await?;
    while let Some(value) = list.recv().await? {
        println!("list: {value:?}");
    }

    // Or create a client from a connection directly.
    let client2 = Endpoint::bind(presets::N0).await?;
    let conn = client2.connect(server_addr, storage::ALPN).await?;
    let api = StorageClient::from_connection(conn);
    let value = api.get("goodbye").await?;
    println!("goodbye = {value:?}");

    drop(server_router);
    Ok(())
}

mod storage {
    //! Implementation of our storage service.

    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex, MutexGuard},
    };

    use anyhow::Result;
    use iroh::{
        endpoint::Connection,
        protocol::{AcceptError, ProtocolHandler},
        Endpoint,
    };
    use irpc::{
        channel::{mpsc, oneshot},
        rpc_requests, Client, WithChannels,
    };
    // Import the macro
    use irpc_iroh::{read_request, IrohLazyRemoteConnection, IrohRemoteConnection};
    use serde::{Deserialize, Serialize};
    use tracing::info;

    pub const ALPN: &[u8] = b"irpc/example-storage/0";

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
        #[rpc(tx=oneshot::Sender<Option<String>>)]
        Get(Get),
        #[rpc(tx=oneshot::Sender<()>)]
        Set(Set),
        #[rpc(tx=oneshot::Sender<u64>, rx=mpsc::Receiver<(String, String)>)]
        SetMany(SetMany),
        #[rpc(tx=mpsc::Sender<String>)]
        List(List),
    }

    #[derive(Debug, Clone, Default)]
    pub struct StorageServer {
        state: Arc<Mutex<BTreeMap<String, String>>>,
    }

    impl ProtocolHandler for StorageServer {
        async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
            while let Some(msg) = read_request::<StorageProtocol>(&conn).await? {
                self.handle_message(msg).await;
            }
            conn.closed().await;
            Ok(())
        }
    }

    impl StorageServer {
        async fn handle_message(&self, msg: StorageMessage) {
            info!("handle message {:?}", msg);
            match msg {
                StorageMessage::Get(msg) => {
                    let WithChannels { tx, inner, .. } = msg;
                    let value = self.state().get(&inner.key).cloned();
                    tx.send(value).await.ok();
                }
                StorageMessage::Set(msg) => {
                    let WithChannels { tx, inner, .. } = msg;
                    self.state().insert(inner.key, inner.value);
                    tx.send(()).await.ok();
                }
                StorageMessage::SetMany(msg) => {
                    let WithChannels { tx, mut rx, .. } = msg;
                    let mut i = 0;
                    while let Ok(Some((key, value))) = rx.recv().await {
                        self.state().insert(key, value);
                        i += 1;
                    }
                    tx.send(i).await.ok();
                }
                StorageMessage::List(msg) => {
                    let WithChannels { tx, .. } = msg;
                    let values = {
                        let state = self.state();
                        // We clone the values so that we don't keep the lock open for the lifetime of the request.
                        // If we wouldn't want to clone here because there can be many entries,
                        // we have to redesign the storage to support a notion of snapshots, or use an async lock
                        // but that would mean that no other requests can be processed while the stream here is sent out.
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

        fn state(&self) -> MutexGuard<'_, BTreeMap<String, String>> {
            self.state.lock().expect("poisoned")
        }
    }

    pub struct StorageClient {
        inner: Client<StorageProtocol>,
    }

    impl StorageClient {
        /// Connect via an [`Endpoint`].
        ///
        /// This will create a client that automatically reconnects if the connection closes.
        pub fn connect(endpoint: Endpoint, addr: impl Into<iroh::EndpointAddr>) -> StorageClient {
            let conn = IrohLazyRemoteConnection::new(endpoint, addr.into(), ALPN.to_vec());
            StorageClient {
                inner: Client::boxed(conn),
            }
        }

        /// Create a client from a [`Connection`].
        ///
        /// This creates a client from a single [`Connection`]. If the connection closes, the client will
        /// not reconnect and all calls will return errors.
        pub fn from_connection(conn: Connection) -> StorageClient {
            StorageClient {
                inner: Client::boxed(IrohRemoteConnection::new(conn)),
            }
        }

        pub async fn get(&self, key: impl ToString) -> Result<Option<String>, irpc::Error> {
            self.inner
                .rpc(Get {
                    key: key.to_string(),
                })
                .await
        }

        pub async fn list(&self) -> Result<mpsc::Receiver<String>, irpc::Error> {
            self.inner.server_streaming(List, 10).await
        }

        pub async fn set(
            &self,
            key: impl ToString,
            value: impl ToString,
        ) -> Result<(), irpc::Error> {
            let msg = Set {
                key: key.to_string(),
                value: value.to_string(),
            };
            self.inner.rpc(msg).await
        }
    }
}
