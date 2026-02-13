//! Example demonstrating how to use the [`irpc::rpc::ConnectionFilter`] trait
//! with the `governor` crate for per-IP rate limiting on a real RPC endpoint.
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::num::NonZeroU32;

use anyhow::{Context, Result};
use governor::{DefaultKeyedRateLimiter, Quota, RateLimiter};
use irpc::{
    channel::oneshot,
    rpc::{ConnectionFilter, Listener, RemoteService},
    rpc_requests,
    util::{make_client_endpoint, make_server_endpoint},
    Client, WithChannels,
};
use n0_future::task::{self, AbortOnDropHandle};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct Ping {
    payload: Vec<u8>,
}

#[rpc_requests(message = PingMessage)]
#[derive(Serialize, Deserialize, Debug)]
enum PingProtocol {
    #[rpc(tx = oneshot::Sender<Vec<u8>>)]
    Ping(Ping),
}

struct PingActor {
    recv: tokio::sync::mpsc::Receiver<PingMessage>,
}

impl PingActor {
    pub fn spawn() -> PingApi {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        n0_future::task::spawn(Self { recv: rx }.run());
        PingApi {
            inner: Client::local(tx),
        }
    }

    async fn run(mut self) {
        while let Some(PingMessage::Ping(ping)) = self.recv.recv().await {
            let WithChannels { tx, inner, .. } = ping;
            tx.send(inner.payload).await.ok();
        }
    }
}

/// A [`ConnectionFilter`] backed by a governor keyed rate limiter.
struct GovernorFilter {
    limiter: DefaultKeyedRateLimiter<SocketAddr>,
}

impl GovernorFilter {
    fn new(per_second: u32) -> Self {
        Self {
            limiter: RateLimiter::keyed(
                Quota::per_second(NonZeroU32::new(per_second).expect("per_second must be > 0")),
            ),
        }
    }
}

impl ConnectionFilter for GovernorFilter {
    fn accept(&self, addr: &SocketAddr) -> bool {
        self.limiter.check_key(addr).is_ok()
    }
}

struct PingApi {
    inner: Client<PingProtocol>,
}

impl PingApi {
    pub fn connect(endpoint: quinn::Endpoint, addr: SocketAddr) -> Result<PingApi> {
        Ok(PingApi {
            inner: Client::quinn(endpoint, addr),
        })
    }

    pub fn listen(&self, endpoint: quinn::Endpoint) -> Result<AbortOnDropHandle<()>> {
        let local = self
            .inner
            .as_local()
            .context("cannot listen on remote API")?;
        let handler = PingProtocol::remote_handler(local);
        // Rate limit: allow 2 new connections per second per IP
        let filter = GovernorFilter::new(2);
        let listener = Listener::new(endpoint, handler, filter);
        Ok(AbortOnDropHandle::new(task::spawn(listener.listen())))
    }

    pub async fn ping(&self, payload: Vec<u8>) -> irpc::Result<Vec<u8>> {
        self.inner.rpc(Ping { payload }).await
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let port = 10114;
    let addr: SocketAddr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into();

    let (server_handle, cert) = {
        let (endpoint, cert) = make_server_endpoint(addr)?;
        let api = PingActor::spawn();
        let handle = api.listen(endpoint)?;
        (handle, cert)
    };

    let endpoint =
        make_client_endpoint(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into(), &[&cert])?;

    // Fire 10 pings at 300ms intervals — with a limit of 2/sec (one token
    // every 500ms), roughly every other request gets through.
    for i in 0..10 {
        let api = PingApi::connect(endpoint.clone(), addr)?;
        match api.ping(b"hello".to_vec()).await {
            Ok(response) => {
                println!("{i}: {}", String::from_utf8_lossy(&response));
            }
            Err(e) => {
                println!("{i}: rejected: {e}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    drop(server_handle);
    Ok(())
}
