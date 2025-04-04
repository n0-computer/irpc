use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
};

use anyhow::{Context, Result};
use irpc::{
    channel::{oneshot, spsc},
    rpc::Handler,
    util::{make_client_endpoint, make_server_endpoint},
    Client, LocalSender, Request, Service, WithChannels,
};
// Import the macro
use irpc_derive::rpc_requests;
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
    #[rpc(tx=oneshot::Sender<Option<String>>)]
    Get(Get),
    #[rpc(tx=oneshot::Sender<()>)]
    Set(Set),
    #[rpc(tx=oneshot::Sender<u64>, rx=spsc::Receiver<(String, String)>)]
    SetMany(SetMany),
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
            StorageMessage::SetMany(set) => {
                info!("set-many {:?}", set);
                let WithChannels { mut rx, tx, .. } = set;
                let mut count = 0;
                while let Ok(Some((key, value))) = rx.recv().await {
                    self.state.insert(key, value);
                    count += 1;
                }
                tx.send(count).await.ok();
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
        let handler: Handler<StorageProtocol> = Arc::new(move |msg, rx, tx| {
            let local = local.clone();
            Box::pin(match msg {
                StorageProtocol::Get(msg) => local.send((msg, tx)),
                StorageProtocol::Set(msg) => local.send((msg, tx)),
                StorageProtocol::SetMany(msg) => local.send((msg, tx, rx)),
                StorageProtocol::List(msg) => local.send((msg, tx)),
            })
        });
        let join_handle = task::spawn(irpc::rpc::listen(endpoint, handler));
        Ok(AbortOnDropHandle::new(join_handle))
    }

    pub async fn get(&self, key: String) -> anyhow::Result<oneshot::Receiver<Option<String>>> {
        let msg = Get { key };
        match self.inner.request().await? {
            Request::Local(request) => {
                let (tx, rx) = oneshot::channel();
                request.send((msg, tx)).await?;
                Ok(rx)
            }
            Request::Remote(request) => {
                let (_tx, rx) = request.write(msg).await?;
                Ok(rx.into())
            }
        }
    }

    pub async fn list(&self) -> anyhow::Result<spsc::Receiver<String>> {
        let msg = List;
        match self.inner.request().await? {
            Request::Local(request) => {
                let (tx, rx) = spsc::channel(10);
                request.send((msg, tx)).await?;
                Ok(rx)
            }
            Request::Remote(request) => {
                let (_tx, rx) = request.write(msg).await?;
                Ok(rx.into())
            }
        }
    }

    pub async fn set(&self, key: String, value: String) -> anyhow::Result<oneshot::Receiver<()>> {
        let msg = Set { key, value };
        match self.inner.request().await? {
            Request::Local(request) => {
                let (tx, rx) = oneshot::channel();
                request.send((msg, tx)).await?;
                Ok(rx)
            }
            Request::Remote(request) => {
                let (_tx, rx) = request.write(msg).await?;
                Ok(rx.into())
            }
        }
    }

    pub async fn set_many(
        &self,
    ) -> Result<(spsc::Sender<(String, String)>, oneshot::Receiver<u64>)> {
        let msg = SetMany;
        match self.inner.request().await? {
            Request::Local(request) => {
                let (req_tx, req_rx) = spsc::channel(16);
                let (res_tx, res_rx) = oneshot::channel();
                request.send((msg, res_tx, req_rx)).await?;
                Ok((req_tx, res_rx))
            }
            Request::Remote(request) => {
                let (tx, rx) = request.write(msg).await?;
                Ok((tx.into(), rx.into()))
            }
        }
    }
}

async fn client_demo(api: StorageApi) -> anyhow::Result<()> {
    api.set("hello".to_string(), "world".to_string())
        .await?
        .await?;
    let value = api.get("hello".to_string()).await?.await?;
    println!("hello = {:?}", value);

    let (mut tx, rx) = api.set_many().await?;
    for i in 0..3 {
        tx.send((format!("key{i}"), format!("value{i}"))).await?;
    }
    drop(tx);
    let count = rx.await?;
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
