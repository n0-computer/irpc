//! Example demonstrating per-connection and per-request rate limiting.
//!
//! Uses [`irpc::rpc::ConnectionFilter`] for per-IP connection filtering and
//! [`irpc::rpc::map_filter`] for per-request filtering with the `governor` crate.
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::num::NonZeroU32;
use anyhow::{Context, Result};
use governor::{DefaultDirectRateLimiter, DefaultKeyedRateLimiter, Quota, RateLimiter};
use irpc::{
    channel::oneshot,
    rpc::{ConnectionFilter, ListenerBuilder, RemoteService, RequestFilter},
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

#[derive(Debug, Serialize, Deserialize)]
struct Info;

#[rpc_requests(message = AppMessage)]
#[derive(Serialize, Deserialize, Debug)]
enum AppProtocol {
    #[rpc(tx = oneshot::Sender<Vec<u8>>)]
    Ping(Ping),
    #[rpc(tx = oneshot::Sender<String>)]
    Info(Info),
}

struct AppActor {
    recv: tokio::sync::mpsc::Receiver<AppMessage>,
}

impl AppActor {
    pub fn spawn() -> AppApi {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        n0_future::task::spawn(Self { recv: rx }.run());
        AppApi {
            inner: Client::local(tx),
        }
    }

    async fn run(mut self) {
        while let Some(msg) = self.recv.recv().await {
            match msg {
                AppMessage::Ping(ping) => {
                    let WithChannels { tx, inner, .. } = ping;
                    tx.send(inner.payload).await.ok();
                }
                AppMessage::Info(info) => {
                    let WithChannels { tx, .. } = info;
                    tx.send("irpc rate-limit example".to_string()).await.ok();
                }
            }
        }
    }
}

/// Per-connection rate limiter using governor, keyed by remote address.
struct GovernorConnectionFilter {
    limiter: DefaultKeyedRateLimiter<SocketAddr>,
}

impl GovernorConnectionFilter {
    fn new(per_second: u32) -> Self {
        Self {
            limiter: RateLimiter::keyed(
                Quota::per_second(NonZeroU32::new(per_second).expect("per_second must be > 0")),
            ),
        }
    }
}

impl ConnectionFilter for GovernorConnectionFilter {
    fn accept(&self, addr: &SocketAddr) -> bool {
        self.limiter.check_key(addr).is_ok()
    }
}

/// Per-request rate limiter: rate-limits Ping requests, always allows Info.
struct PingRateLimiter {
    limiter: DefaultDirectRateLimiter,
}

impl PingRateLimiter {
    fn new(per_second: u32) -> Self {
        Self {
            limiter: RateLimiter::direct(
                Quota::per_second(NonZeroU32::new(per_second).expect("per_second must be > 0")),
            ),
        }
    }
}

impl RequestFilter<AppProtocol> for PingRateLimiter {
    fn accept(&self, req: &AppProtocol) -> bool {
        match req {
            AppProtocol::Ping(_) => self.limiter.check().is_ok(),
            _ => true,
        }
    }
}

struct AppApi {
    inner: Client<AppProtocol>,
}

impl AppApi {
    pub fn connect(endpoint: quinn::Endpoint, addr: SocketAddr) -> Result<AppApi> {
        Ok(AppApi {
            inner: Client::quinn(endpoint, addr),
        })
    }

    pub fn listen(&self, endpoint: quinn::Endpoint) -> Result<AbortOnDropHandle<()>> {
        let local = self
            .inner
            .as_local()
            .context("cannot listen on remote API")?;
        let handler = AppProtocol::remote_handler(local);
        let listener = ListenerBuilder::new(endpoint, handler)
            .request_filter(PingRateLimiter::new(2))
            .connection_filter(GovernorConnectionFilter::new(10));
        Ok(AbortOnDropHandle::new(task::spawn(listener.listen())))
    }

    pub async fn ping(&self, payload: Vec<u8>) -> irpc::Result<Vec<u8>> {
        self.inner.rpc(Ping { payload }).await
    }

    pub async fn info(&self) -> irpc::Result<String> {
        self.inner.rpc(Info).await
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let port = 10114;
    let addr: SocketAddr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into();

    let (server_handle, cert) = {
        let (endpoint, cert) = make_server_endpoint(addr)?;
        let api = AppActor::spawn();
        let handle = api.listen(endpoint)?;
        (handle, cert)
    };

    let endpoint =
        make_client_endpoint(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into(), &[&cert])?;

    // Fire bursts of Ping with interspersed Info requests.
    // Ping is rate-limited to 2/sec, Info always gets through.
    for i in 0..10 {
        let api = AppApi::connect(endpoint.clone(), addr)?;
        match api.ping(b"hello".to_vec()).await {
            Ok(response) => println!("{i}: ping = {}", String::from_utf8_lossy(&response)),
            Err(e) => println!("{i}: ping rejected: {e}"),
        }
        let api = AppApi::connect(endpoint.clone(), addr)?;
        match api.info().await {
            Ok(response) => println!("{i}: info = {response}"),
            Err(e) => println!("{i}: info rejected: {e}"),
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    drop(server_handle);
    Ok(())
}
