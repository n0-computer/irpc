use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
};

use anyhow::{Context, Result};
use irpc::{
    channel::{mpsc, oneshot},
    rpc::Handler,
    rpc_requests,
    util::{make_client_endpoint, make_server_endpoint},
    Client, LocalSender, Request, Service,
};
// Import the macro
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

#[derive(Debug, Serialize, Deserialize)]
struct List;

#[derive(Debug, Serialize, Deserialize)]
struct Set {
    key: String,
    value: String,
}

impl From<(String, String)> for Set {
    fn from((key, value): (String, String)) -> Self {
        Self { key, value }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SetMany;

// Use the macro to generate both the StorageProtocol and StorageMessage enums
// plus implement Channels for each type
#[rpc_requests(StorageService, message = StorageMessage)]
#[derive(Serialize, Deserialize)]
enum StorageProtocol {
    #[rpc(reply=oneshot::Sender<Option<String>>)]
    Get(Get),
    #[rpc(reply=oneshot::Sender<()>)]
    Set(Set),
    #[rpc(reply=oneshot::Sender<u64>, updates=mpsc::Receiver<(String, String)>)]
    SetMany(SetMany),
    #[rpc(reply=mpsc::Sender<String>)]
    List(List),
}

struct StorageActor {
    recv: tokio::sync::mpsc::Receiver<StorageMessage>,
    state: BTreeMap<String, String>,
}

impl StorageActor {
    pub fn spawn() -> StorageApi {
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
            StorageMessage::SetMany(set) => {
                info!("set-many {:?}", set);
                let Request {
                    mut updates, reply, ..
                } = set;
                let mut count = 0;
                while let Ok(Some((key, value))) = updates.recv().await {
                    self.state.insert(key, value);
                    count += 1;
                }
                reply.send(count).await.ok();
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
    pub fn connect(endpoint: quinn::Endpoint, addr: SocketAddr) -> Result<StorageApi> {
        Ok(StorageApi {
            inner: Client::quinn(endpoint, addr),
        })
    }

    pub fn listen(&self, endpoint: quinn::Endpoint) -> Result<AbortOnDropHandle<()>> {
        let local = self.inner.local().context("cannot listen on remote API")?;
        let handler: Handler<StorageProtocol> = Arc::new(move |msg, request, reply| {
            let local = local.clone();
            Box::pin(match msg {
                StorageProtocol::Get(msg) => local.send((msg, reply)),
                StorageProtocol::Set(msg) => local.send((msg, reply)),
                StorageProtocol::SetMany(msg) => local.send((msg, reply, request)),
                StorageProtocol::List(msg) => local.send((msg, reply)),
            })
        });
        let join_handle = task::spawn(irpc::rpc::listen(endpoint, handler));
        Ok(AbortOnDropHandle::new(join_handle))
    }

    pub async fn get(&self, key: String) -> irpc::Result<Option<String>> {
        self.inner.rpc(Get { key }).await
    }

    pub async fn list(&self) -> irpc::Result<mpsc::Receiver<String>> {
        self.inner.server_streaming(List, 16).await
    }

    pub async fn set(&self, key: String, value: String) -> irpc::Result<()> {
        self.inner.rpc(Set { key, value }).await
    }

    pub async fn set_many(
        &self,
    ) -> irpc::Result<(mpsc::Sender<(String, String)>, oneshot::Receiver<u64>)> {
        self.inner.client_streaming(SetMany, 4).await
    }
}

async fn client_demo(api: StorageApi) -> Result<()> {
    api.set("hello".to_string(), "world".to_string()).await?;
    let value = api.get("hello".to_string()).await?;
    println!("get: hello = {:?}", value);

    let (reply, request) = api.set_many().await?;
    for i in 0..3 {
        reply.send((format!("key{i}"), format!("value{i}"))).await?;
    }
    drop(reply);
    let count = request.await?;
    println!("set-many: {count} values set");

    let mut list = api.list().await?;
    while let Some(value) = list.recv().await? {
        println!("list value = {:?}", value);
    }
    Ok(())
}

async fn local() -> Result<()> {
    let api = StorageActor::spawn();
    client_demo(api).await?;
    Ok(())
}

async fn remote() -> Result<()> {
    let port = 10113;
    let addr: SocketAddr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into();

    let (server_handle, cert) = {
        let (endpoint, cert) = make_server_endpoint(addr)?;
        let api = StorageActor::spawn();
        let handle = api.listen(endpoint)?;
        (handle, cert)
    };

    let endpoint =
        make_client_endpoint(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into(), &[&cert])?;
    let api = StorageApi::connect(endpoint, addr)?;
    client_demo(api).await?;

    drop(server_handle);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    println!("Local use");
    local().await?;
    println!("Remote use");
    remote().await.unwrap();
    Ok(())
}
