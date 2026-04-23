//! Benchmark comparing irpc transports: local, quinn, iroh, and UDS.
//!
//! Ported from examples/compute.rs to cover all four transports.

use std::{
    io::{self, Write},
    net::{Ipv4Addr, SocketAddrV4},
};

use anyhow::Result;
use futures_buffered::BufferedStreamExt;
use iroh::{protocol::Router, Endpoint};
use irpc::{
    channel::{mpsc, oneshot},
    rpc::{listen, RemoteService},
    rpc_requests,
    util::{make_client_endpoint, make_server_endpoint},
    Client, WithChannels,
};
use irpc_iroh::IrohProtocol;
use n0_future::{
    stream::StreamExt,
    task::{self, AbortOnDropHandle},
};
use serde::{Deserialize, Serialize};
use thousands::Separable;

// --- Protocol ---

#[rpc_requests(message = BenchMessage)]
#[derive(Serialize, Deserialize, Debug)]
enum BenchProtocol {
    #[rpc(tx = oneshot::Sender<u128>)]
    Sqr(u64),

    #[rpc(rx = mpsc::Receiver<u64>, tx = mpsc::Sender<u64>)]
    #[wrap(MultiplyRequest)]
    Multiply { initial: u64 },
}

// --- Actor ---

async fn actor(mut rx: tokio::sync::mpsc::Receiver<BenchMessage>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            BenchMessage::Sqr(msg) => {
                let WithChannels { inner, tx, .. } = msg;
                let result = (inner as u128) * (inner as u128);
                tx.send(result).await.ok();
            }
            BenchMessage::Multiply(msg) => {
                let WithChannels {
                    inner, tx, mut rx, ..
                } = msg;
                let MultiplyRequest { initial } = inner;
                // Spawn so the actor loop stays free for other messages
                tokio::task::spawn(async move {
                    while let Ok(Some(num)) = rx.recv().await {
                        if tx.send(initial * num).await.is_err() {
                            break;
                        }
                    }
                });
            }
        }
    }
}

fn spawn_actor() -> Client<BenchProtocol> {
    let (tx, rx) = tokio::sync::mpsc::channel(128);
    task::spawn(actor(rx));
    Client::local(tx)
}

// --- Transport Setup ---

fn setup_local() -> Client<BenchProtocol> {
    spawn_actor()
}

fn setup_quinn() -> Result<(Client<BenchProtocol>, AbortOnDropHandle<()>)> {
    let (server_ep, cert) =
        make_server_endpoint(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into())?;
    let port = server_ep.local_addr()?.port();
    let client_ep =
        make_client_endpoint(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into(), &[&cert])?;

    let local = spawn_actor();
    let handler = BenchProtocol::remote_handler(local.as_local().unwrap());
    let handle = AbortOnDropHandle::new(task::spawn(listen(server_ep, handler)));

    let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into();
    let client = Client::quinn(client_ep, addr);
    Ok((client, handle))
}

async fn setup_iroh() -> Result<(Client<BenchProtocol>, Router)> {
    const ALPN: &[u8] = b"irpc-bench/0";

    let server_endpoint = Endpoint::bind().await?;
    let local = spawn_actor();
    let handler = BenchProtocol::remote_handler(local.as_local().unwrap());
    let router = Router::builder(server_endpoint.clone())
        .accept(ALPN, IrohProtocol::new(handler))
        .spawn();

    server_endpoint.online().await;

    let client_endpoint = Endpoint::builder().bind().await?;
    let client = irpc_iroh::client(client_endpoint, server_endpoint.addr(), ALPN);
    Ok((client, router))
}

#[cfg(unix)]
async fn setup_uds() -> Result<(Client<BenchProtocol>, std::path::PathBuf)> {
    let path = std::env::temp_dir().join(format!("irpc-bench-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let local = spawn_actor();
    let endpoint = irpc_uds::server_endpoint(&path)?;
    let handler = BenchProtocol::remote_handler(local.as_local().unwrap());
    task::spawn(listen(endpoint, handler));

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = irpc_uds::client(&path).await?;
    Ok((client, path))
}

// --- Benchmark Runners ---

fn sum_of_squares(n: u64) -> u128 {
    (0..n).map(|x| (x * x) as u128).sum()
}

fn clear_line() -> io::Result<()> {
    io::stdout().write_all(b"\r\x1b[K")?;
    io::stdout().flush()
}

async fn bench_seq(name: &str, client: &Client<BenchProtocol>, n: u64) -> Result<()> {
    let mut sum = 0u128;
    let t0 = std::time::Instant::now();
    for i in 0..n {
        sum += client.rpc(i).await?;
        if i.is_multiple_of(10000) {
            print!(".");
            io::stdout().flush()?;
        }
    }
    let elapsed = t0.elapsed();
    let rps = ((n as f64) / elapsed.as_secs_f64()).round() as u64;
    assert_eq!(sum, sum_of_squares(n));
    clear_line()?;
    println!(
        "  {name:<8} {rps:>10} rps",
        rps = rps.separate_with_underscores()
    );
    Ok(())
}

async fn bench_par(name: &str, client: &Client<BenchProtocol>, n: u64, par: usize) -> Result<()> {
    let t0 = std::time::Instant::now();
    let client = client.clone();
    let reqs = n0_future::stream::iter((0..n).map(move |i| {
        let client = client.clone();
        async move { anyhow::Ok(client.rpc(i).await?) }
    }));
    let resp: Vec<_> = reqs.buffered_unordered(par).try_collect().await?;
    let sum: u128 = resp.into_iter().sum();
    let elapsed = t0.elapsed();
    let rps = ((n as f64) / elapsed.as_secs_f64()).round() as u64;
    assert_eq!(sum, sum_of_squares(n));
    clear_line()?;
    println!(
        "  {name:<8} {rps:>10} rps",
        rps = rps.separate_with_underscores()
    );
    Ok(())
}

async fn bench_bidi(name: &str, client: &Client<BenchProtocol>, n: u64) -> Result<()> {
    let t0 = std::time::Instant::now();
    let (send, mut recv) = client
        .bidi_streaming(MultiplyRequest { initial: 2 }, 128, 128)
        .await?;
    let handle = tokio::task::spawn(async move {
        for i in 0..n {
            send.send(i).await?;
        }
        Ok::<(), io::Error>(())
    });
    let mut sum = 0u64;
    let mut i = 0u64;
    while let Some(res) = recv.recv().await? {
        sum += res;
        if i.is_multiple_of(10000) {
            print!(".");
            io::stdout().flush()?;
        }
        i += 1;
    }
    let elapsed = t0.elapsed();
    let rps = ((n as f64) / elapsed.as_secs_f64()).round() as u64;
    assert_eq!(sum, (0..n).map(|x| x * 2).sum::<u64>());
    clear_line()?;
    println!(
        "  {name:<8} {rps:>10} rps",
        rps = rps.separate_with_underscores()
    );
    handle.await??;
    Ok(())
}

// --- Main ---

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("RUST_LOG").is_ok() {
        tracing_subscriber::fmt::init();
    }

    let n: u64 = std::env::var("N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);
    let par: usize = std::env::var("PAR")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(32);

    // Setup all transports
    let local = setup_local();
    let (quinn_client, _quinn_handle) = setup_quinn()?;
    let (iroh_client, _iroh_router) = setup_iroh().await?;
    #[cfg(unix)]
    let (uds_client, _uds_path) = setup_uds().await?;

    // --- Sequential RPCs ---
    println!("=== RPC seq (n={}) ===", n.separate_with_underscores());
    bench_seq("local", &local, n).await?;
    bench_seq("quinn", &quinn_client, n).await?;
    bench_seq("iroh", &iroh_client, n).await?;
    #[cfg(unix)]
    bench_seq("uds", &uds_client, n).await?;

    // --- Parallel RPCs ---
    println!(
        "\n=== RPC par (n={}, parallelism={par}) ===",
        n.separate_with_underscores()
    );
    bench_par("local", &local, n, par).await?;
    bench_par("quinn", &quinn_client, n, par).await?;
    bench_par("iroh", &iroh_client, n, par).await?;
    #[cfg(unix)]
    bench_par("uds", &uds_client, n, par).await?;

    // --- Bidirectional streaming ---
    println!("\n=== Bidi seq (n={}) ===", n.separate_with_underscores());
    bench_bidi("local", &local, n).await?;
    bench_bidi("quinn", &quinn_client, n).await?;
    bench_bidi("iroh", &iroh_client, n).await?;
    #[cfg(unix)]
    bench_bidi("uds", &uds_client, n).await?;

    Ok(())
}
