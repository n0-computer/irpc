use std::{
    env,
    str::FromStr,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use clap::Parser;
use iroh::{protocol::Router, Endpoint, EndpointAddr, EndpointId, SecretKey};
use ping::EchoApi;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = cli::Args::parse();
    match args {
        cli::Args::Listen { no_0rtt } => {
            let (server_router, server_addr) = {
                let secret_key = get_or_generate_secret_key()?;
                let endpoint = Endpoint::builder().secret_key(secret_key).bind().await?;
                endpoint.online().await;
                let addr = endpoint.addr();
                let api = EchoApi::spawn();
                let router = Router::builder(endpoint.clone());
                let router = if !no_0rtt {
                    router.accept(EchoApi::ALPN, api.expose_0rtt()?)
                } else {
                    router.accept(EchoApi::ALPN, api.expose()?)
                };
                let router = router.spawn();
                (router, addr)
            };
            println!("EndpointId: {}", server_addr.id);
            println!("Accepting 0rtt connections: {}", !no_0rtt);
            let ticket = server_addr.id.to_string();
            println!("Connect using:\n\ncargo run --example 0rtt connect {ticket}\n");
            println!("Control-C to stop");
            tokio::signal::ctrl_c()
                .await
                .expect("failed to listen for ctrl_c");
            server_router.shutdown().await?;
        }
        cli::Args::Connect {
            ticket,
            n,
            delay_ms,
            no_0rtt,
            wait_for_ticket,
        } => {
            if !no_0rtt && !wait_for_ticket {
                eprintln!("0-RTT is enabled but wait_for_ticket is not set. After 2 requests with 0rtt the 0rtt resumption tickets will be consumed and a connection will be done without 0rtt.");
            }
            let n = n
                .iter()
                .filter_map(|x| u64::try_from(*x).ok())
                .next()
                .unwrap_or(u64::MAX);
            let delay = std::time::Duration::from_millis(delay_ms);
            let endpoint = Endpoint::builder().bind().await?;
            let addr: EndpointAddr = ticket.into();
            for i in 0..n {
                if let Err(e) = ping_one(no_0rtt, &endpoint, &addr, i, wait_for_ticket).await {
                    eprintln!("Error pinging {}: {e}", addr.id);
                }
                tokio::time::sleep(delay).await;
            }
        }
    }
    Ok(())
}

async fn ping_one_0rtt(
    api: EchoApi,
    endpoint: &Endpoint,
    endpoint_id: EndpointId,
    wait_for_ticket: bool,
    i: u64,
    t0: Instant,
) -> Result<()> {
    let msg = i.to_be_bytes();
    let data = api.echo_0rtt(msg.to_vec()).await?;
    let latency = endpoint.latency(endpoint_id);
    if wait_for_ticket {
        tokio::spawn(async move {
            let latency = latency.unwrap_or(Duration::from_millis(500));
            tokio::time::sleep(latency * 2).await;
            drop(api);
        });
    } else {
        drop(api);
    }
    let elapsed = t0.elapsed();
    assert!(data == msg);
    println!(
        "latency: {}",
        latency
            .map(|x| format!("{}ms", x.as_micros() as f64 / 1000.0))
            .unwrap_or("unknown".into())
    );
    println!("ping:    {}ms\n", elapsed.as_micros() as f64 / 1000.0);
    Ok(())
}

async fn ping_one_no_0rtt(
    api: EchoApi,
    endpoint: &Endpoint,
    endpoint_id: EndpointId,
    i: u64,
    t0: Instant,
) -> Result<()> {
    let msg = i.to_be_bytes();
    let data = api.echo(msg.to_vec()).await?;
    let latency = endpoint.latency(endpoint_id);
    drop(api);
    let elapsed = t0.elapsed();
    assert!(data == msg);
    println!(
        "latency: {}",
        latency
            .map(|x| format!("{}ms", x.as_micros() as f64 / 1000.0))
            .unwrap_or("unknown".into())
    );
    println!("ping:    {}ms\n", elapsed.as_micros() as f64 / 1000.0);
    Ok(())
}

async fn ping_one(
    no_0rtt: bool,
    endpoint: &Endpoint,
    addr: &EndpointAddr,
    i: u64,
    wait_for_ticket: bool,
) -> Result<()> {
    let endpoint_id = addr.id;
    let t0 = Instant::now();
    if !no_0rtt {
        let api = EchoApi::connect_0rtt(endpoint.clone(), addr.clone()).await?;
        ping_one_0rtt(api, endpoint, endpoint_id, wait_for_ticket, i, t0).await?;
    } else {
        let api = EchoApi::connect(endpoint.clone(), addr.clone()).await?;
        ping_one_no_0rtt(api, endpoint, endpoint_id, i, t0).await?;
    }
    Ok(())
}

/// Gets a secret key from the IROH_SECRET environment variable or generates a new random one.
/// If the environment variable is set, it must be a valid string representation of a secret key.
pub fn get_or_generate_secret_key() -> Result<SecretKey> {
    if let Ok(secret) = env::var("IROH_SECRET") {
        // Parse the secret key from string
        SecretKey::from_str(&secret).context("Invalid secret key format")
    } else {
        // Generate a new random key
        let secret_key = SecretKey::generate(&mut rand::rng());
        println!(
            "Generated new secret key: {}",
            hex::encode(secret_key.to_bytes())
        );
        println!("To reuse this key, set the IROH_SECRET environment variable to this value");
        Ok(secret_key)
    }
}

mod cli {
    use clap::Parser;
    use iroh::EndpointId;

    #[derive(Debug, Parser)]
    pub enum Args {
        Listen {
            #[clap(long)]
            no_0rtt: bool,
        },
        Connect {
            ticket: EndpointId,
            #[clap(short)]
            n: Option<usize>,
            #[clap(long)]
            no_0rtt: bool,
            #[clap(long, default_value = "1000")]
            delay_ms: u64,
            #[clap(long, default_value = "false")]
            wait_for_ticket: bool,
        },
    }
}

mod ping {
    use anyhow::{Context, Result};
    use futures_util::FutureExt;
    use iroh::{endpoint::ZeroRttStatus, Endpoint};
    use irpc::{channel::oneshot, rpc::RemoteService, rpc_requests, Client, WithChannels};
    use irpc_iroh::{
        Iroh0RttProtocol, IrohProtocol, IrohRemoteConnection, IrohZrttRemoteConnection,
    };
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
            addr: impl Into<iroh::EndpointAddr>,
        ) -> Result<EchoApi> {
            let conn = endpoint
                .connect(addr, Self::ALPN)
                .await
                .context("failed to connect to remote service")?;
            let fut: future::Boxed<bool> = Box::pin(async { true });
            Ok(EchoApi {
                inner: Client::boxed(IrohRemoteConnection::new(conn)),
                zero_rtt_accepted: fut.shared(),
            })
        }

        pub async fn connect_0rtt(
            endpoint: Endpoint,
            addr: impl Into<iroh::EndpointAddr>,
        ) -> Result<EchoApi> {
            let connecting = endpoint
                .connect_with_opts(addr, Self::ALPN, Default::default())
                .await
                .context("failed to connect to remote service")?;
            match connecting.into_0rtt() {
                Ok(conn) => {
                    info!("0-RTT possible from our side");
                    let fut: future::Boxed<bool> = Box::pin({
                        let conn = conn.clone();
                        async move {
                            match conn.handshake_completed().await {
                                Err(_) => false,
                                Ok(ZeroRttStatus::Accepted(_)) => true,
                                Ok(ZeroRttStatus::Rejected(_)) => false,
                            }
                        }
                    });
                    Ok(EchoApi {
                        inner: Client::boxed(IrohZrttRemoteConnection::new(conn)),
                        zero_rtt_accepted: fut.shared(),
                    })
                }
                Err(connecting) => {
                    info!("0-RTT not possible from our side");
                    let fut: future::Boxed<bool> = Box::pin(async { true });
                    let conn = connecting.await?;
                    Ok(EchoApi {
                        inner: Client::boxed(IrohRemoteConnection::new(conn)),
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
}
