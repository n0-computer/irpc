use std::{
    collections::BTreeMap,
    future::{Future, IntoFuture},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
};

use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use irpc::{
    channel::{mpsc, oneshot},
    rpc::RemoteService,
    rpc_requests,
    util::{
        make_client_endpoint, make_server_endpoint, IrpcReceiverFutExt, MpscSenderExt, StreamItem,
    },
    Client, WithChannels,
};
// Import the macro
use n0_future::{
    task::{self, AbortOnDropHandle},
    Stream, StreamExt,
};
use serde::{Deserialize, Serialize};
use tracing::info;

#[derive(Debug, Serialize, Deserialize)]
struct Error {
    message: String,
}

impl std::error::Error for Error {}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

#[derive(Debug, Serialize, Deserialize, StreamItem)]
enum GetItem {
    Item(String),
    Error(Error),
    Done,
}

#[derive(Debug, Serialize, Deserialize)]
struct Set {
    key: String,
    value: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Get {
    key: String,
}

// Use the macro to generate both the StorageProtocol and StorageMessage enums
// plus implement Channels for each type
#[rpc_requests(message = StorageMessage)]
#[derive(Serialize, Deserialize, Debug)]
enum StorageProtocol {
    #[rpc(tx=oneshot::Sender<()>)]
    Set(Set),
    #[rpc(tx=mpsc::Sender<GetItem>)]
    Get(Get),
}

struct StorageActor {
    recv: tokio::sync::mpsc::Receiver<StorageMessage>,
    state: BTreeMap<String, String>,
}

struct GetProgress {
    fut: BoxFuture<'static, irpc::Result<mpsc::Receiver<GetItem>>>,
}

impl GetProgress {
    pub fn new(
        fut: impl Future<Output = irpc::Result<mpsc::Receiver<GetItem>>> + Send + 'static,
    ) -> Self {
        Self { fut: Box::pin(fut) }
    }

    pub fn stream(self) -> impl Stream<Item = anyhow::Result<String>> {
        self.fut.into_stream()
    }
}

impl IntoFuture for GetProgress {
    type Output = anyhow::Result<String>;
    type IntoFuture = BoxFuture<'static, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.fut.try_collect())
    }
}

impl StorageActor {
    pub fn spawn() -> StorageApi {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let actor = Self {
            recv: rx,
            state: BTreeMap::new(),
        };
        n0_future::task::spawn(actor.run());
        StorageApi {
            inner: Client::local(tx),
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
                let WithChannels {
                    tx,
                    inner: Get { key },
                    ..
                } = get;
                let value = self.state.get(&key).cloned().unwrap_or_default();
                let parts = value.split_inclusive(" ");
                tx.forward_iter(parts.map(|x| Ok(x.to_string()))).await.ok();
            }
            StorageMessage::Set(set) => {
                info!("set {:?}", set);
                let WithChannels {
                    tx,
                    inner: Set { key, value },
                    ..
                } = set;
                self.state.insert(key, value);
                tx.send(()).await.ok();
            }
        }
    }
}

struct StorageApi {
    inner: Client<StorageProtocol>,
}

impl StorageApi {
    pub fn connect(endpoint: quinn::Endpoint, addr: SocketAddr) -> Result<StorageApi> {
        Ok(StorageApi {
            inner: Client::quinn(endpoint, addr),
        })
    }

    pub fn listen(&self, endpoint: quinn::Endpoint) -> Result<AbortOnDropHandle<()>> {
        let local = self
            .inner
            .as_local()
            .context("cannot listen on remote API")?;
        let join_handle = task::spawn(irpc::rpc::listen(
            endpoint,
            StorageProtocol::remote_handler(local),
        ));
        Ok(AbortOnDropHandle::new(join_handle))
    }

    pub fn get(&self, key: String) -> GetProgress {
        GetProgress::new(self.inner.server_streaming(Get { key }, 16))
    }

    pub async fn set(&self, key: String, value: String) -> irpc::Result<()> {
        self.inner.rpc(Set { key, value }).await
    }
}

async fn client_demo(api: StorageApi) -> Result<()> {
    api.set("hello".to_string(), "world".to_string()).await?;
    let value = api.get("hello".to_string()).await?;
    println!("get: hello = {value:?}");

    api.set("loremipsum".to_string(), "dolor sit amet".to_string())
        .await?;

    let mut parts = api.get("loremipsum".to_string()).stream();
    while let Some(part) = parts.next().await {
        match part {
            Ok(item) => println!("Received item: {item}"),
            Err(e) => println!("Error receiving item: {e}"),
        }
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
