use anyhow::Result;
use iroh::{protocol::Router, Endpoint};

use self::storage::StorageApi;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    println!("Local use");
    local().await?;
    println!("Remote use");
    remote().await?;
    Ok(())
}

async fn local() -> Result<()> {
    let api = StorageApi::spawn(None);
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
        let api = StorageApi::spawn(Some("secret-token".to_string()));
        let router = Router::builder(endpoint.clone())
            .accept(StorageApi::ALPN, api.expose()?)
            .spawn()
            .await?;
        let addr = endpoint.node_addr().await?;
        (router, addr)
    };

    let client_endpoint = Endpoint::builder().bind().await?;

    let api = StorageApi::connect(client_endpoint, server_addr.clone());
    api.auth("secret-token".to_string()).await?;
    api.set("hello".to_string(), "world".to_string()).await?;
    api.set("goodbye".to_string(), "world".to_string()).await?;
    let value = api.get("hello".to_string()).await?;
    println!("value = {:?}", value);
    let mut list = api.list().await?;
    while let Some(value) = list.recv().await? {
        println!("list value = {:?}", value);
    }

    println!("ROUND 2");

    let client_endpoint = Endpoint::builder().bind().await?;
    println!("ROUND 2 connect");
    let api = StorageApi::connect(client_endpoint, server_addr);
    println!("ROUND 2 auth req");
    let res = api.auth("bad-token".to_string()).await;
    println!("auth response: {res:?}");
    assert!(res.is_err());
    println!("ROUND 2 set req");
    let res = api.set("foo".to_string(), "bar".to_string()).await;
    println!("request response: {res:?}");
    let res = api.get("foo".to_string()).await;
    println!("request response: {res:?}");
    assert!(res.is_err());

    drop(server_router);
    Ok(())
}

mod storage {
    //! Implementation of our storage service.
    //!
    //! The only `pub` item is [`StorageApi`], everything else is private.

    use std::{
        collections::BTreeMap,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    };

    use anyhow::{Context, Result};
    use iroh::{protocol::ProtocolHandler, Endpoint};
    use irpc::{
        channel::{oneshot, spsc},
        Client, LocalSender, Service, WithChannels,
    };
    // Import the macro
    use irpc_derive::rpc_requests;
    use irpc_iroh::{Handler, IrohProtocol, IrohRemoteConnection};
    use serde::{Deserialize, Serialize};
    use tracing::info;
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

    // Use the macro to generate both the StorageProtocol and StorageMessage enums
    // plus implement Channels for each type
    #[rpc_requests(StorageService, message = StorageMessage)]
    #[derive(Debug, Serialize, Deserialize)]
    enum StorageProtocol {
        #[rpc(tx=oneshot::Sender<Result<(), String>>)]
        Auth(Auth),
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
        client_token: Option<String>,
    }

    impl StorageActor {
        pub fn spawn(client_token: Option<String>) -> StorageApi {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let actor = Self {
                recv: rx,
                state: BTreeMap::new(),
                client_token,
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
                StorageMessage::Auth(msg) => {
                    let WithChannels { tx, inner, .. } = msg;
                    if self.client_token.as_ref() == Some(&inner.token) {
                        tx.send(Ok(())).await.ok();
                    } else {
                        tx.send(Err("unauthorized".to_string())).await.ok();
                    }
                }
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

        pub fn spawn(client_token: Option<String>) -> Self {
            StorageActor::spawn(client_token)
        }

        pub fn connect(endpoint: Endpoint, addr: impl Into<iroh::NodeAddr>) -> Self {
            let conn = IrohRemoteConnection::new(endpoint, addr.into(), Self::ALPN.to_vec());
            StorageApi {
                inner: Client::boxed(conn),
            }
        }

        pub async fn auth(&self, token: String) -> Result<()> {
            self.inner
                .rpc(Auth { token })
                .await?
                .map_err(|err| anyhow::anyhow!("authentication failed: {err}"))?;
            Ok(())
        }

        pub fn expose(&self) -> Result<impl ProtocolHandler> {
            let local = self
                .inner
                .local()
                .context("can not listen on remote service")?;
            let create_handler = move || {
                let local = local.clone();
                let is_authed = Arc::new(AtomicBool::new(false));
                let handler: Handler<StorageProtocol> = Arc::new(move |conn, msg, _rx, tx| {
                    let local = local.clone();
                    println!(
                        "SERVER req {msg:?} is_authed {}",
                        is_authed.load(Ordering::SeqCst)
                    );
                    Box::pin({
                        if !matches!(msg, StorageProtocol::Auth(_))
                            && !is_authed.load(Ordering::SeqCst)
                        {
                            println!("SERVER unauth close!");
                            conn.close(401u32.into(), b"unauthorized");
                        }
                        match msg {
                            StorageProtocol::Auth(msg) => {
                                let (local_tx, local_rx) = irpc::channel::oneshot::channel();
                                let is_authed = is_authed.clone();
                                return Box::pin(async move {
                                    local.send((msg, local_tx)).await?;
                                    let res = match local_rx.await {
                                        Err(err) => Err(err.to_string()),
                                        Ok(Err(err)) => Err(err),
                                        Ok(Ok(())) => Ok(()),
                                    };
                                    if !res.is_ok() {
                                        println!("SERVER unauth close!");
                                        conn.close(401u32.into(), b"unauthorized");
                                    } else {
                                        println!("SERVER auth ok!");
                                        is_authed.store(true, Ordering::SeqCst);
                                        let tx = irpc::channel::oneshot::Sender::from(tx);
                                        tx.send(res).await.ok();
                                    }
                                    Ok(())
                                });
                            }
                            StorageProtocol::Get(msg) => local.send((msg, tx)),
                            StorageProtocol::Set(msg) => local.send((msg, tx)),
                            StorageProtocol::List(msg) => local.send((msg, tx)),
                        }
                    })
                });
                handler
            };
            Ok(IrohProtocol::new(create_handler))
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
