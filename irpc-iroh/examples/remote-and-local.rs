//! Demonstrates how to talk to an actor loop both from the same process and from remotes.
//!
//! The [`StorageApi`] struct is only defined once and can be used both locally and as a remote client.

use anyhow::Result;
use iroh::{endpoint::presets, protocol::Router, Endpoint};

use self::storage::StorageApi;

#[tokio::main]
async fn main() -> Result<()> {
    run().await
}

#[tokio::test]
async fn test() -> Result<()> {
    run().await
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt::init();
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
        println!("list value = {value:?}");
    }
    println!("value = {value:?}");
    Ok(())
}

async fn remote() -> Result<()> {
    let endpoint = Endpoint::bind(presets::N0).await?;
    let api = StorageApi::spawn();
    let router = Router::builder(endpoint.clone())
        .accept(StorageApi::ALPN, api.protocol_handler()?)
        .spawn();

    endpoint.online().await;

    let client_endpoint = Endpoint::bind(presets::N0).await?;
    let api = StorageApi::connect(client_endpoint, endpoint.addr())?;
    api.set("hello".to_string(), "world".to_string()).await?;
    api.set("goodbye".to_string(), "world".to_string()).await?;
    let value = api.get("hello".to_string()).await?;
    println!("value = {value:?}");
    let mut list = api.list().await?;
    while let Some(value) = list.recv().await? {
        println!("list value = {value:?}");
    }

    router.shutdown().await?;

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
        channel::{mpsc, oneshot},
        rpc::RemoteService,
        rpc_requests, Client, WithChannels,
    };
    // Import the macro
    use irpc_iroh::{IrohLazyRemoteConnection, IrohProtocol};
    use n0_future::task::AbortOnDropHandle;
    use serde::{Deserialize, Serialize};
    use tracing::info;

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
    #[rpc_requests(message = StorageMessage)]
    #[derive(Serialize, Deserialize, Debug)]
    enum StorageProtocol {
        #[rpc(tx=oneshot::Sender<Option<String>>)]
        Get(Get),
        #[rpc(tx=oneshot::Sender<()>)]
        Set(Set),
        #[rpc(tx=mpsc::Sender<String>)]
        List(List),
    }

    #[derive(Default)]
    struct StorageActor {
        state: BTreeMap<String, String>,
    }

    impl StorageActor {
        async fn run(mut self, mut rx: tokio::sync::mpsc::Receiver<StorageMessage>) {
            while let Some(msg) = rx.recv().await {
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
                    let WithChannels { tx, .. } = list;
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
        client: Client<StorageProtocol>,
        _actor_task: Option<Arc<AbortOnDropHandle<()>>>,
    }

    impl StorageApi {
        pub const ALPN: &[u8] = b"irpc-iroh/derive-demo/0";

        pub fn spawn() -> Self {
            let (tx, rx) = tokio::sync::mpsc::channel(2);
            let actor = StorageActor::default();
            let actor_task = n0_future::task::spawn(actor.run(rx));
            StorageApi {
                client: Client::local(tx),
                _actor_task: Some(Arc::new(AbortOnDropHandle::new(actor_task))),
            }
        }

        pub fn connect(
            endpoint: Endpoint,
            addr: impl Into<iroh::EndpointAddr>,
        ) -> Result<StorageApi> {
            let conn = IrohLazyRemoteConnection::new(endpoint, addr.into(), Self::ALPN.to_vec());
            Ok(StorageApi {
                client: Client::boxed(conn),
                _actor_task: None,
            })
        }

        pub fn protocol_handler(&self) -> Result<impl ProtocolHandler> {
            let local = self
                .client
                .as_local()
                .context("can not listen on remote service")?;
            Ok(IrohProtocol::new(StorageProtocol::remote_handler(local)))
        }

        pub async fn get(&self, key: String) -> irpc::Result<Option<String>> {
            self.client.rpc(Get { key }).await
        }

        pub async fn list(&self) -> irpc::Result<mpsc::Receiver<String>> {
            self.client.server_streaming(List, 10).await
        }

        pub async fn set(&self, key: String, value: String) -> irpc::Result<()> {
            let msg = Set { key, value };
            self.client.rpc(msg).await
        }
    }
}
