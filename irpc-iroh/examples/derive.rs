use anyhow::Result;
use iroh::{protocol::Router, Endpoint};

use self::storage::StorageApi;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    println!("Local use");
    local().await?;
    println!("Remote use");
    remote().await?;
    Ok(())
}

async fn local() -> Result<()> {
    let api = StorageApi::spawn();
    api.set("hello".to_string(), "world".to_string()).await?;
    let value = api.get("hello".to_string()).await?;
    let mut list = api.list().await?;
    while let Some(value) = list.recv().await? {
        println!("list value = {:?}", value);
    }
    println!("value = {:?}", value);
    Ok(())
}

async fn remote() -> Result<()> {
    let (server_router, server_addr) = {
        let endpoint = Endpoint::builder().discovery_n0().bind().await?;
        let api = StorageApi::spawn();
        let router = Router::builder(endpoint.clone())
            .accept(StorageApi::ALPN, api.expose()?)
            .spawn();
        let addr = endpoint.node_addr().await?;
        (router, addr)
    };

    let client_endpoint = Endpoint::builder().bind().await?;
    let api = StorageApi::connect(client_endpoint, server_addr)?;
    api.set("hello".to_string(), "world".to_string()).await?;
    api.set("goodbye".to_string(), "world".to_string()).await?;
    let value = api.get("hello".to_string()).await?;
    println!("value = {:?}", value);
    let mut list = api.list().await?;
    while let Some(value) = list.recv().await? {
        println!("list value = {:?}", value);
    }
    drop(server_router);
    Ok(())
}

mod storage {
    //! Implementation of our storage service.
    //!
    //! The only `pub` item is [`StorageApi`], everything else is private.

    use std::{collections::BTreeMap, sync::Arc};

    use anyhow::{Context, Result};
    use iroh::{protocol::ProtocolHandler, Endpoint};
    use irpc::{
        channel::{oneshot, spsc},
        rpc::Handler,
        rpc_requests, Client, LocalSender, Service, WithChannels,
    };
    // Import the macro
    use irpc_iroh::{IrohProtocol, IrohRemoteConnection};
    use serde::{Deserialize, Serialize};
    use tracing::info;
    /// A simple storage service, just to try it out
    #[derive(Debug, Clone, Copy)]
    struct StorageService;

    impl Service for StorageService {}

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

    // Use the macro to generate both the StorageProtocol and StorageMessage enums
    // plus implement Channels for each type
    #[rpc_requests(StorageService, message = StorageMessage)]
    #[derive(Serialize, Deserialize)]
    enum StorageProtocol {
        #[rpc(tx=oneshot::Sender<Option<String>>)]
        Get(Get),
        #[rpc(tx=oneshot::Sender<()>)]
        Set(Set),
        #[rpc(tx=spsc::Sender<String>)]
        List(List),
    }

    struct StorageActor {
        recv: tokio::sync::mpsc::Receiver<StorageMessage>,
        state: BTreeMap<String, String>,
    }

    impl StorageActor {
        pub fn spawn() -> StorageApi {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let actor = Self {
                recv: rx,
                state: BTreeMap::new(),
            };
            n0_future::task::spawn(actor.run());
            let local = LocalSender::<StorageMessage, StorageService>::from(tx);
            StorageApi {
                inner: local.into(),
            }
        }

        async fn run(mut self) {
            while let Some(msg) = self.recv.recv().await {
                self.handle(msg).await;
            }
        }

        async fn handle(&mut self, msg: StorageMessage) {
            match msg {
                StorageMessage::Get(get) => {
                    info!("get {:?}", get);
                    let WithChannels { tx, inner, .. } = get;
                    tx.send(self.state.get(&inner.key).cloned()).await.ok();
                }
                StorageMessage::Set(set) => {
                    info!("set {:?}", set);
                    let WithChannels { tx, inner, .. } = set;
                    self.state.insert(inner.key, inner.value);
                    tx.send(()).await.ok();
                }
                StorageMessage::List(list) => {
                    info!("list {:?}", list);
                    let WithChannels { mut tx, .. } = list;
                    for (key, value) in &self.state {
                        if tx.send(format!("{key}={value}")).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }

    pub struct StorageApi {
        inner: Client<StorageMessage, StorageProtocol, StorageService>,
    }

    impl StorageApi {
        pub const ALPN: &[u8] = b"irpc-iroh/derive-demo/0";

        pub fn spawn() -> Self {
            StorageActor::spawn()
        }

        pub fn connect(endpoint: Endpoint, addr: impl Into<iroh::NodeAddr>) -> Result<StorageApi> {
            let conn = IrohRemoteConnection::new(endpoint, addr.into(), Self::ALPN.to_vec());
            Ok(StorageApi {
                inner: Client::boxed(conn),
            })
        }

        pub fn expose(&self) -> Result<impl ProtocolHandler> {
            let local = self
                .inner
                .local()
                .context("can not listen on remote service")?;
            let handler: Handler<StorageProtocol> = Arc::new(move |msg, _rx, tx| {
                let local = local.clone();
                Box::pin(match msg {
                    StorageProtocol::Get(msg) => local.send((msg, tx)),
                    StorageProtocol::Set(msg) => local.send((msg, tx)),
                    StorageProtocol::List(msg) => local.send((msg, tx)),
                })
            });
            Ok(IrohProtocol::new(handler))
        }

        pub async fn get(&self, key: String) -> irpc::Result<Option<String>> {
            self.inner.rpc(Get { key }).await
        }

        pub async fn list(&self) -> irpc::Result<spsc::Receiver<String>> {
            self.inner.server_streaming(List, 10).await
        }

        pub async fn set(&self, key: String, value: String) -> irpc::Result<()> {
            let msg = Set { key, value };
            self.inner.rpc(msg).await
        }
    }
}
