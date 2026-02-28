//! Benchmark comparing irpc transports: local, quinn, iroh, and UDS.

use std::{
    io::{self, Write},
    net::{Ipv4Addr, SocketAddrV4},
};

use anyhow::Result;
use futures_buffered::BufferedStreamExt;
use irpc::{
    channel::oneshot,
    rpc::{listen, RemoteService},
    rpc_requests,
    util::{make_client_endpoint, make_server_endpoint},
    Client, WithChannels,
};
use iroh::{protocol::Router, Endpoint};
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

    #[rpc(tx = oneshot::Sender<Vec<u8>>)]
    Echo(Vec<u8>),
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
            BenchMessage::Echo(msg) => {
                let WithChannels { inner, tx, .. } = msg;
                tx.send(inner).await.ok();
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

async fn bench_seq_small(name: &str, client: &Client<BenchProtocol>, n: u64) -> Result<()> {
    let mut sum = 0u128;
    let t0 = std::time::Instant::now();
    for i in 0..n {
        sum += client.rpc(i).await?;
        if i % 1000 == 0 {
            print!(".");
            io::stdout().flush()?;
        }
    }
    let elapsed = t0.elapsed();
    let rps = ((n as f64) / elapsed.as_secs_f64()).round() as u64;
    assert_eq!(sum, sum_of_squares(n));
    clear_line()?;
    println!("  {name:<8} {rps:>10} rps", rps = rps.separate_with_underscores());
    Ok(())
}

async fn bench_par_small(
    name: &str,
    client: &Client<BenchProtocol>,
    n: u64,
    par: usize,
) -> Result<()> {
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
    println!("  {name:<8} {rps:>10} rps", rps = rps.separate_with_underscores());
    Ok(())
}

async fn bench_large_seq(
    name: &str,
    client: &Client<BenchProtocol>,
    n: u64,
    payload_size: usize,
) -> Result<()> {
    let payload = vec![42u8; payload_size];
    let t0 = std::time::Instant::now();
    for _ in 0..n {
        let resp = client.rpc(payload.clone()).await?;
        assert_eq!(resp.len(), payload_size);
    }
    let elapsed = t0.elapsed();
    let rps = ((n as f64) / elapsed.as_secs_f64()).round() as u64;
    let throughput = (n as f64 * payload_size as f64) / elapsed.as_secs_f64();
    let throughput_gb = throughput / (1024.0 * 1024.0 * 1024.0);
    clear_line()?;
    println!(
        "  {name:<8} {rps:>10} rps  ({throughput_gb:.1} GB/s)",
        rps = rps.separate_with_underscores(),
    );
    Ok(())
}

// --- Main ---

#[tokio::main]
async fn main() -> Result<()> {
    // Only enable tracing if RUST_LOG is set
    if std::env::var("RUST_LOG").is_ok() {
        tracing_subscriber::fmt::init();
    }

    let n_small: u64 = std::env::var("N_SMALL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);
    let n_large: u64 = std::env::var("N_LARGE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let par: usize = std::env::var("PAR")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(32);
    let payload_mb: usize = std::env::var("PAYLOAD_MB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let payload_size = payload_mb * 1024 * 1024;

    // Setup all transports
    let local = setup_local();
    let (quinn_client, _quinn_handle) = setup_quinn()?;
    let (iroh_client, _iroh_router) = setup_iroh().await?;
    let (uds_client, uds_path) = setup_uds().await?;

    // --- Sequential small RPCs ---
    println!("=== Sequential small RPCs (n={}) ===", n_small.separate_with_underscores());
    bench_seq_small("local", &local, n_small).await?;
    bench_seq_small("quinn", &quinn_client, n_small).await?;
    bench_seq_small("iroh", &iroh_client, n_small).await?;
    bench_seq_small("uds", &uds_client, n_small).await?;

    // --- Concurrent small RPCs ---
    println!(
        "\n=== Concurrent small RPCs (n={}, parallelism={par}) ===",
        n_small.separate_with_underscores()
    );
    bench_par_small("local", &local, n_small, par).await?;
    bench_par_small("quinn", &quinn_client, n_small, par).await?;
    bench_par_small("iroh", &iroh_client, n_small, par).await?;
    bench_par_small("uds", &uds_client, n_small, par).await?;

    // --- Large sequential RPCs ---
    println!(
        "\n=== Large sequential RPCs (n={}, {}MB payload) ===",
        n_large.separate_with_underscores(),
        payload_mb,
    );
    bench_large_seq("local", &local, n_large, payload_size).await?;
    bench_large_seq("quinn", &quinn_client, n_large, payload_size).await?;
    bench_large_seq("iroh", &iroh_client, n_large, payload_size).await?;
    bench_large_seq("uds", &uds_client, n_large, payload_size).await?;

    // uds_path cleanup happens automatically via UdsSocket::drop
    drop(uds_path);

    Ok(())
}
