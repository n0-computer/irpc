use anyhow::Result;
use iroh::{protocol::Router, Endpoint, Watcher};
use ping::EchoApi;

#[tokio::main]
async fn main() -> Result<()> {
    // tracing_subscriber::fmt().init();
    println!("Local use");
    local().await?;
    println!("Remote use");
    remote().await?;
    Ok(())
}

async fn local() -> Result<()> {
    let api = EchoApi::spawn();
    let res = api.echo(b"hello".to_vec()).await?;
    println!("value = {}", String::from_utf8_lossy(&res));
    Ok(())
}

async fn remote() -> Result<()> {
    let (server_router, server_addr) = {
        let endpoint = Endpoint::builder().discovery_n0().bind().await?;
        let api = EchoApi::spawn();
        let router = Router::builder(endpoint.clone())
            .accept(EchoApi::ALPN, api.expose_0rtt()?)
            .spawn();
        let addr = endpoint.node_addr().initialized().await;
        (router, addr)
    };

    let client_endpoint = Endpoint::builder().bind().await?;
    for i in 0..10 {
        let api = EchoApi::connect(client_endpoint.clone(), server_addr.clone()).await?;
        let res = api.echo_0rtt(b"hello".to_vec()).await?;
        println!("value = {}", String::from_utf8_lossy(&res));
    }
    drop(server_router);
    Ok(())
}

mod ping {
    use anyhow::{Context, Result};
    use futures_util::FutureExt;
    use iroh::{
        endpoint::{Connection, RecvStream, SendStream, ZeroRttAccepted},
        Endpoint,
    };
    use irpc::{channel::oneshot, rpc::RemoteService, rpc_requests, Client, WithChannels};
    use irpc_iroh::{Iroh0RttProtocol, IrohProtocol, IrohRemoteConnection};
    use n0_future::future;
    use serde::{Deserialize, Serialize};
    use tracing::info;

    #[rpc_requests(message = EchoMessage)]
    #[derive(Serialize, Deserialize, Debug)]
    pub enum EchoProtocol {
        #[rpc(tx=oneshot::Sender<Vec<u8>>)]
        #[wrap(Echo)]
        Echo { data: Vec<u8> },
    }

    pub struct EchoApi {
        inner: Client<EchoProtocol>,
        zero_rtt_accepted: futures_util::future::Shared<future::Boxed<bool>>,
    }

    impl EchoApi {
        pub const ALPN: &[u8] = b"echo";

        pub async fn echo(&self, data: Vec<u8>) -> irpc::Result<Vec<u8>> {
            self.inner.rpc(Echo { data }).await
        }

        pub async fn echo_0rtt(&self, data: Vec<u8>) -> irpc::Result<Vec<u8>> {
            self.inner
                .rpc_0rtt(Echo { data }, self.zero_rtt_accepted.clone())
                .await
        }

        pub fn expose_0rtt(self) -> Result<Iroh0RttProtocol<EchoProtocol>> {
            let local = self
                .inner
                .as_local()
                .context("can not listen on remote service")?;
            Ok(Iroh0RttProtocol::new(EchoProtocol::remote_handler(local)))
        }

        pub fn expose(self) -> Result<IrohProtocol<EchoProtocol>> {
            let local = self
                .inner
                .as_local()
                .context("can not listen on remote service")?;
            Ok(IrohProtocol::new(EchoProtocol::remote_handler(local)))
        }

        pub async fn connect(
            endpoint: Endpoint,
            addr: impl Into<iroh::NodeAddr>,
        ) -> Result<EchoApi> {
            let connecting = endpoint
                .connect_with_opts(addr, Self::ALPN, Default::default())
                .await
                .context("failed to connect to remote service")?;
            match connecting.into_0rtt() {
                Ok((conn, zero_rtt_accepted)) => {
                    println!("0-RTT possible from our side");
                    let fut: future::Boxed<bool> = Box::pin(zero_rtt_accepted);
                    Ok(EchoApi {
                        inner: Client::boxed(IrohConnection(conn)),
                        zero_rtt_accepted: fut.shared(),
                    })
                }
                Err(connecting) => {
                    println!("0-RTT not possible from our side");
                    let fut: future::Boxed<bool> = Box::pin(async { true });
                    let conn = connecting.await?;
                    Ok(EchoApi {
                        inner: Client::boxed(IrohConnection(conn)),
                        zero_rtt_accepted: fut.shared(),
                    })
                }
            }
        }

        pub fn spawn() -> Self {
            EchoActor::spawn()
        }
    }

    struct EchoActor {
        recv: tokio::sync::mpsc::Receiver<EchoMessage>,
    }

    impl EchoActor {
        pub fn spawn() -> EchoApi {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let actor = Self { recv: rx };
            n0_future::task::spawn(actor.run());
            let fut: future::Boxed<bool> = Box::pin(async { true });
            EchoApi {
                inner: Client::local(tx),
                zero_rtt_accepted: fut.shared(),
            }
        }

        async fn run(mut self) {
            while let Some(msg) = self.recv.recv().await {
                self.handle(msg).await;
            }
        }

        async fn handle(&mut self, msg: EchoMessage) {
            match msg {
                EchoMessage::Echo(msg) => {
                    info!("{:?}", msg);
                    let WithChannels { tx, inner, .. } = msg;
                    tx.send(inner.data).await.ok();
                }
            }
        }
    }

    #[derive(Debug, Clone)]
    struct IrohConnection(Connection);

    impl irpc::rpc::RemoteConnection for IrohConnection {
        fn clone_boxed(&self) -> Box<dyn irpc::rpc::RemoteConnection> {
            Box::new(self.clone())
        }

        fn open_bi(
            &self,
        ) -> n0_future::future::Boxed<
            std::result::Result<(SendStream, RecvStream), irpc::RequestError>,
        > {
            let conn = self.0.clone();
            Box::pin(async move {
                let (send, recv) = conn.open_bi().await?;
                Ok((send, recv))
            })
        }
    }
}
