use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
};

use anyhow::bail;
use irpc::{
    channel::{mpsc, none::NoReceiver, oneshot},
    rpc::{listen, Handler},
    util::{make_client_endpoint, make_server_endpoint},
    Channels, Client, LocalSender, Request, RequestSender, Service,
};
use n0_future::task::{self, AbortOnDropHandle};
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

impl Channels<StorageService> for Get {
    type Updates = NoReceiver;
    type Reply = oneshot::Sender<Option<String>>;
}

#[derive(Debug, Serialize, Deserialize)]
struct List;

impl Channels<StorageService> for List {
    type Updates = NoReceiver;
    type Reply = mpsc::Sender<String>;
}

#[derive(Debug, Serialize, Deserialize)]
struct Set {
    key: String,
    value: String,
}

impl Channels<StorageService> for Set {
    type Updates = NoReceiver;
    type Reply = oneshot::Sender<()>;
}

#[derive(derive_more::From, Serialize, Deserialize)]
enum StorageProtocol {
    Get(Get),
    Set(Set),
    List(List),
}

#[derive(derive_more::From)]
enum StorageMessage {
    Get(Request<Get, StorageService>),
    Set(Request<Set, StorageService>),
    List(Request<List, StorageService>),
}

struct StorageActor {
    recv: tokio::sync::mpsc::Receiver<StorageMessage>,
    state: BTreeMap<String, String>,
}

impl StorageActor {
    pub fn local() -> StorageApi {
        let (reply, request) = tokio::sync::mpsc::channel(1);
        let actor = Self {
            recv: request,
            state: BTreeMap::new(),
        };
        n0_future::task::spawn(actor.run());
        let local = LocalSender::<StorageMessage, StorageService>::from(reply);
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
                let Request { reply, message, .. } = get;
                reply.send(self.state.get(&message.key).cloned()).await.ok();
            }
            StorageMessage::Set(set) => {
                info!("set {:?}", set);
                let Request { reply, message, .. } = set;
                self.state.insert(message.key, message.value);
                reply.send(()).await.ok();
            }
            StorageMessage::List(list) => {
                info!("list {:?}", list);
                let Request { reply, .. } = list;
                for (key, value) in &self.state {
                    if reply.send(format!("{key}={value}")).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
}
struct StorageApi {
    inner: Client<StorageMessage, StorageProtocol, StorageService>,
}

impl StorageApi {
    pub fn connect(endpoint: quinn::Endpoint, addr: SocketAddr) -> anyhow::Result<StorageApi> {
        Ok(StorageApi {
            inner: Client::quinn(endpoint, addr),
        })
    }

    pub fn listen(&self, endpoint: quinn::Endpoint) -> anyhow::Result<AbortOnDropHandle<()>> {
        let Some(local) = self.inner.local() else {
            bail!("cannot listen on a remote service");
        };
        let handler: Handler<StorageProtocol> = Arc::new(move |msg, _request, reply| {
            let local = local.clone();
            Box::pin(match msg {
                StorageProtocol::Get(msg) => local.send((msg, reply)),
                StorageProtocol::Set(msg) => local.send((msg, reply)),
                StorageProtocol::List(msg) => local.send((msg, reply)),
            })
        });
        Ok(AbortOnDropHandle::new(task::spawn(listen(
            endpoint, handler,
        ))))
    }

    pub async fn get(&self, key: String) -> anyhow::Result<oneshot::Receiver<Option<String>>> {
        let msg = Get { key };
        match self.inner.request().await? {
            RequestSender::Local(sender) => {
                let (reply, request) = oneshot::channel();
                sender.send((msg, reply)).await?;
                Ok(request)
            }
            RequestSender::Remote(sender) => {
                let (_reply, request) = sender.write(msg).await?;
                Ok(request.into())
            }
        }
    }

    pub async fn list(&self) -> anyhow::Result<mpsc::Receiver<String>> {
        let msg = List;
        match self.inner.request().await? {
            RequestSender::Local(sender) => {
                let (reply, request) = mpsc::channel(10);
                sender.send((msg, reply)).await?;
                Ok(request)
            }
            RequestSender::Remote(sender) => {
                let (_reply, request) = sender.write(msg).await?;
                Ok(request.into())
            }
        }
    }

    pub async fn set(&self, key: String, value: String) -> anyhow::Result<oneshot::Receiver<()>> {
        let msg = Set { key, value };
        match self.inner.request().await? {
            RequestSender::Local(sender) => {
                let (reply, request) = oneshot::channel();
                sender.send((msg, reply)).await?;
                Ok(request)
            }
            RequestSender::Remote(sender) => {
                let (_reply, request) = sender.write(msg).await?;
                Ok(request.into())
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
