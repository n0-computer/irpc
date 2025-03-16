use std::{
    collections::BTreeMap,
    marker::PhantomData,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
};

use anyhow::bail;
use n0_future::task::{self, AbortOnDropHandle};
use quic_rpc::{
    channel::{mpsc, oneshot},
    rpc::{listen, Handler},
    util::{make_client_endpoint, make_server_endpoint},
    LocalMpscChannel, Msg, Service, ServiceRequest, ServiceSender,
};
// Import the macro
use quic_rpc_derive::rpc_requests;
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
#[rpc_requests(StorageService, StorageMessage)]
#[derive(derive_more::From, Serialize, Deserialize)]
enum StorageProtocol {
    #[rpc(tx=oneshot::Sender<Option<String>>)]
    Get(Get),
    #[rpc(tx=oneshot::Sender<()>)]
    Set(Set),
    #[rpc(tx=mpsc::Sender<String>)]
    List(List),
}

struct StorageActor {
    recv: tokio::sync::mpsc::Receiver<StorageMessage>,
    state: BTreeMap<String, String>,
}

impl StorageActor {
    pub fn local() -> StorageApi {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let actor = Self {
            recv: rx,
            state: BTreeMap::new(),
        };
        n0_future::task::spawn(actor.run());
        let local = LocalMpscChannel::<StorageMessage, StorageService>::from(tx);
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
                let Msg { tx, inner, .. } = get;
                tx.send(self.state.get(&inner.key).cloned()).await.ok();
            }
            StorageMessage::Set(set) => {
                info!("set {:?}", set);
                let Msg { tx, inner, .. } = set;
                self.state.insert(inner.key, inner.value);
                tx.send(()).await.ok();
            }
            StorageMessage::List(list) => {
                info!("list {:?}", list);
                let Msg { mut tx, .. } = list;
                for (key, value) in &self.state {
                    if tx.send(format!("{key}={value}")).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
}

struct StorageApi {
    inner: ServiceSender<StorageMessage, StorageProtocol, StorageService>,
}

impl StorageApi {
    pub fn connect(endpoint: quinn::Endpoint, addr: SocketAddr) -> anyhow::Result<StorageApi> {
        Ok(StorageApi {
            inner: ServiceSender::Remote(endpoint, addr, PhantomData),
        })
    }

    pub fn listen(&self, endpoint: quinn::Endpoint) -> anyhow::Result<AbortOnDropHandle<()>> {
        match &self.inner {
            ServiceSender::Local(local, _) => {
                let local = LocalMpscChannel::from(local.clone());
                let handler: Handler<StorageProtocol> = Arc::new(move |msg, _, tx| {
                    let local = local.clone();
                    Box::pin(match msg {
                        StorageProtocol::Get(msg) => local.send((msg, tx)),
                        StorageProtocol::Set(msg) => local.send((msg, tx)),
                        StorageProtocol::List(msg) => local.send((msg, tx)),
                    })
                });
                Ok(AbortOnDropHandle::new(task::spawn(listen(
                    endpoint, handler,
                ))))
            }
            ServiceSender::Remote(_, _, _) => {
                bail!("cannot listen on a remote service");
            }
        }
    }

    pub async fn get(&self, key: String) -> anyhow::Result<oneshot::Receiver<Option<String>>> {
        let msg = Get { key };
        match self.inner.request().await? {
            ServiceRequest::Local(request, _) => {
                let (tx, rx) = oneshot::channel();
                request.send((msg, tx)).await?;
                Ok(rx)
            }
            ServiceRequest::Remote(request) => {
                let (rx, _tx) = request.write(msg).await?;
                Ok(rx.into())
            }
        }
    }

    pub async fn list(&self) -> anyhow::Result<mpsc::Receiver<String>> {
        let msg = List;
        match self.inner.request().await? {
            ServiceRequest::Local(request, _) => {
                let (tx, rx) = mpsc::channel(10);
                request.send((msg, tx)).await?;
                Ok(rx)
            }
            ServiceRequest::Remote(request) => {
                let (rx, _tx) = request.write(msg).await?;
                Ok(rx.into())
            }
        }
    }

    pub async fn set(&self, key: String, value: String) -> anyhow::Result<oneshot::Receiver<()>> {
        let msg = Set { key, value };
        match self.inner.request().await? {
            ServiceRequest::Local(request, _) => {
                let (tx, rx) = oneshot::channel();
                request.send((msg, tx)).await?;
                Ok(rx)
            }
            ServiceRequest::Remote(request) => {
                let (rx, _tx) = request.write(msg).await?;
                Ok(rx.into())
            }
        }
    }
}

async fn local() -> anyhow::Result<()> {
    let api = StorageActor::local();
    api.set("hello".to_string(), "world".to_string())
        .await?
        .await?;
    let value = api.get("hello".to_string()).await?.await?;
    let mut list = api.list().await?;
    while let Some(value) = list.recv().await? {
        println!("list value = {:?}", value);
    }
    println!("value = {:?}", value);
    Ok(())
}

async fn remote() -> anyhow::Result<()> {
    let port = 10113;
    let (server, cert) =
        make_server_endpoint(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())?;
    let client =
        make_client_endpoint(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into(), &[&cert])?;
    let store = StorageActor::local();
    let handle = store.listen(server)?;
    let api = StorageApi::connect(client, SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into())?;
    api.set("hello".to_string(), "world".to_string())
        .await?
        .await?;
    api.set("goodbye".to_string(), "world".to_string())
        .await?
        .await?;
    let value = api.get("hello".to_string()).await?.await?;
    println!("value = {:?}", value);
    let mut list = api.list().await?;
    while let Some(value) = list.recv().await? {
        println!("list value = {:?}", value);
    }
    drop(handle);
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();
    println!("Local use");
    local().await?;
    println!("Remote use");
    remote().await?;
    Ok(())
}
