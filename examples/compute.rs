use std::{
    io::{self, Write},
    marker::PhantomData,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
};

use anyhow::bail;
use n0_future::task::{self, AbortOnDropHandle};
use quic_rpc::{
    channel::{mpsc, oneshot},
    rpc::{listen, Handler, RemoteRead},
    util::{make_client_endpoint, make_server_endpoint},
    LocalMpscChannel, Msg, Service, ServiceRequest, ServiceSender,
};
use quic_rpc_derive::rpc_requests;
use serde::{Deserialize, Serialize};
use tracing::trace;

// Define the ComputeService
#[derive(Debug, Clone, Copy)]
struct ComputeService;

impl Service for ComputeService {}

// Define ComputeRequest sub-messages
#[derive(Debug, Serialize, Deserialize)]
struct Sqr {
    num: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Sum;

#[derive(Debug, Serialize, Deserialize)]
struct Fibonacci {
    max: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Multiply {
    initial: u64,
}

// Define ComputeRequest enum
#[derive(Debug, Serialize, Deserialize)]
enum ComputeRequest {
    Sqr(Sqr),
    Sum(Sum),
    Fibonacci(Fibonacci),
    Multiply(Multiply),
}

// Define the protocol and message enums using the macro
#[rpc_requests(ComputeService, ComputeMessage)]
#[derive(derive_more::From, Serialize, Deserialize)]
enum ComputeProtocol {
    #[rpc(tx=oneshot::Sender<u128>)]
    Sqr(Sqr),
    #[rpc(rx=mpsc::Receiver<i64>, tx=oneshot::Sender<i64>)]
    Sum(Sum),
    #[rpc(tx=mpsc::Sender<u64>)]
    Fibonacci(Fibonacci),
    #[rpc(rx=mpsc::Receiver<u64>, tx=mpsc::Sender<u64>)]
    Multiply(Multiply),
}

// The actor that processes requests
struct ComputeActor {
    recv: tokio::sync::mpsc::Receiver<ComputeMessage>,
}

impl ComputeActor {
    pub fn local() -> ComputeApi {
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        let actor = Self { recv: rx };
        n0_future::task::spawn(actor.run());
        let local = LocalMpscChannel::<ComputeMessage, ComputeService>::from(tx);
        ComputeApi {
            inner: local.into(),
        }
    }

    async fn run(mut self) {
        while let Some(msg) = self.recv.recv().await {
            if let Err(cause) = self.handle(msg).await {
                eprintln!("Error: {}", cause);
            }
        }
    }

    async fn handle(&mut self, msg: ComputeMessage) -> io::Result<()> {
        match msg {
            ComputeMessage::Sqr(sqr) => {
                trace!("sqr {:?}", sqr);
                let Msg { tx, inner, .. } = sqr;
                let result = (inner.num as u128) * (inner.num as u128);
                tx.send(result).await?;
            }
            ComputeMessage::Sum(sum) => {
                trace!("sum {:?}", sum);
                let Msg { rx, tx, .. } = sum;
                let mut receiver = rx;
                let mut total = 0;
                while let Some(num) = receiver.recv().await? {
                    total += num;
                }
                tx.send(total).await?;
            }
            ComputeMessage::Fibonacci(fib) => {
                trace!("fibonacci {:?}", fib);
                let Msg { tx, inner, .. } = fib;
                let mut sender = tx;
                let mut a = 0u64;
                let mut b = 1u64;
                while a <= inner.max {
                    sender.send(a).await?;
                    let next = a + b;
                    a = b;
                    b = next;
                }
            }
            ComputeMessage::Multiply(mult) => {
                trace!("multiply {:?}", mult);
                let Msg { rx, tx, inner } = mult;
                let mut receiver = rx;
                let mut sender = tx;
                let multiplier = inner.initial;
                while let Some(num) = receiver.recv().await? {
                    sender.send(multiplier * num).await?;
                }
            }
        }
        Ok(())
    }
}
// The API for interacting with the ComputeService
#[derive(Clone)]
struct ComputeApi {
    inner: ServiceSender<ComputeMessage, ComputeProtocol, ComputeService>,
}

impl ComputeApi {
    pub fn connect(endpoint: quinn::Endpoint, addr: SocketAddr) -> anyhow::Result<ComputeApi> {
        Ok(ComputeApi {
            inner: ServiceSender::Remote(endpoint, addr, PhantomData),
        })
    }

    pub fn listen(&self, endpoint: quinn::Endpoint) -> anyhow::Result<AbortOnDropHandle<()>> {
        match &self.inner {
            ServiceSender::Local(local, _) => {
                let local = LocalMpscChannel::from(local.clone());
                let handler: Handler<ComputeProtocol> = Arc::new(move |msg, rx: RemoteRead, tx| {
                    let local = local.clone();
                    Box::pin(match msg {
                        ComputeProtocol::Sqr(msg) => local.send((msg, tx)),
                        ComputeProtocol::Sum(msg) => local.send((msg, tx, rx)),
                        ComputeProtocol::Fibonacci(msg) => local.send((msg, tx)),
                        ComputeProtocol::Multiply(msg) => local.send((msg, tx, rx)),
                    })
                });
                Ok(AbortOnDropHandle::new(task::spawn(listen(
                    endpoint, handler,
                ))))
            }
            ServiceSender::Remote(_, _, _) => {
                bail!("cannot listen on a remote service");
            }
        }
    }

    pub async fn sqr(&self, num: u64) -> anyhow::Result<oneshot::Receiver<u128>> {
        let msg = Sqr { num };
        match self.inner.request().await? {
            ServiceRequest::Local(request, _) => {
                let (tx, rx) = oneshot::channel();
                request.send((msg, tx)).await?;
                Ok(rx)
            }
            ServiceRequest::Remote(request) => {
                let (rx, _tx) = request.write(msg).await?;
                Ok(rx.into())
            }
        }
    }

    pub async fn sum(&self) -> anyhow::Result<(mpsc::Sender<i64>, oneshot::Receiver<i64>)> {
        let msg = Sum;
        match self.inner.request().await? {
            ServiceRequest::Local(request, _) => {
                let (num_tx, num_rx) = mpsc::channel(10);
                let (sum_tx, sum_rx) = oneshot::channel();
                request.send((msg, sum_tx, num_rx)).await?;
                Ok((num_tx, sum_rx))
            }
            ServiceRequest::Remote(request) => {
                let (rx, tx) = request.write(msg).await?;
                Ok((tx.into(), rx.into()))
            }
        }
    }

    pub async fn fibonacci(&self, max: u64) -> anyhow::Result<mpsc::Receiver<u64>> {
        let msg = Fibonacci { max };
        match self.inner.request().await? {
            ServiceRequest::Local(request, _) => {
                let (tx, rx) = mpsc::channel(10);
                request.send((msg, tx)).await?;
                Ok(rx)
            }
            ServiceRequest::Remote(request) => {
                let (rx, _tx) = request.write(msg).await?;
                Ok(rx.into())
            }
        }
    }

    pub async fn multiply(
        &self,
        initial: u64,
    ) -> anyhow::Result<(mpsc::Sender<u64>, mpsc::Receiver<u64>)> {
        let msg = Multiply { initial };
        match self.inner.request().await? {
            ServiceRequest::Local(request, _) => {
                let (in_tx, in_rx) = mpsc::channel(10);
                let (out_tx, out_rx) = mpsc::channel(10);
                request.send((msg, out_tx, in_rx)).await?;
                Ok((in_tx, out_rx))
            }
            ServiceRequest::Remote(request) => {
                let (rx, tx) = request.write(msg).await?;
                Ok((tx.into(), rx.into()))
            }
        }
    }
}

// Local usage example
async fn local() -> anyhow::Result<()> {
    let api = ComputeActor::local();

    // Test Sqr
    let rx = api.sqr(5).await?;
    println!("Local: 5^2 = {}", rx.await?);

    // Test Sum
    let (mut tx, rx) = api.sum().await?;
    tx.send(1).await?;
    tx.send(2).await?;
    tx.send(3).await?;
    drop(tx);
    println!("Local: sum of [1, 2, 3] = {}", rx.await?);

    // Test Fibonacci
    let mut rx = api.fibonacci(10).await?;
    print!("Local: Fibonacci up to 10 = ");
    while let Some(num) = rx.recv().await? {
        print!("{} ", num);
    }
    println!();

    // Test Multiply
    let (mut in_tx, mut out_rx) = api.multiply(3).await?;
    in_tx.send(2).await?;
    in_tx.send(4).await?;
    in_tx.send(6).await?;
    drop(in_tx);
    print!("Local: 3 * [2, 4, 6] = ");
    while let Some(num) = out_rx.recv().await? {
        print!("{} ", num);
    }
    println!();

    Ok(())
}

// Remote usage example
async fn remote() -> anyhow::Result<()> {
    let port = 10114;
    let (server, cert) =
        make_server_endpoint(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())?;
    let client =
        make_client_endpoint(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into(), &[&cert])?;
    let compute = ComputeActor::local();
    let handle = compute.listen(server)?;
    let api = ComputeApi::connect(client, SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into())?;

    // Test Sqr
    let rx = api.sqr(4).await?;
    println!("Remote: 4^2 = {}", rx.await?);

    // Test Sum
    let (mut tx, rx) = api.sum().await?;
    tx.send(4).await?;
    tx.send(5).await?;
    tx.send(6).await?;
    drop(tx);
    println!("Remote: sum of [4, 5, 6] = {}", rx.await?);

    // Test Fibonacci
    let mut rx = api.fibonacci(20).await?;
    print!("Remote: Fibonacci up to 20 = ");
    while let Some(num) = rx.recv().await? {
        print!("{} ", num);
    }
    println!();

    // Test Multiply
    let (mut in_tx, mut out_rx) = api.multiply(5).await?;
    in_tx.send(1).await?;
    in_tx.send(2).await?;
    in_tx.send(3).await?;
    drop(in_tx);
    print!("Remote: 5 * [1, 2, 3] = ");
    while let Some(num) = out_rx.recv().await? {
        print!("{} ", num);
    }
    println!();

    drop(handle);
    Ok(())
}

// Benchmark function using the new ComputeApi
async fn bench(api: ComputeApi, n: u64) -> anyhow::Result<()> {
    // Individual RPCs (sequential)
    {
        let mut sum = 0;
        let t0 = std::time::Instant::now();
        for i in 0..n {
            sum += api.sqr(i).await?.await?;
            if i % 10000 == 0 {
                print!(".");
                io::stdout().flush()?;
            }
        }
        let rps = ((n as f64) / t0.elapsed().as_secs_f64()).round() as u64;
        assert_eq!(sum, sum_of_squares(n));
        clear_line()?;
        println!("RPC seq {} rps", rps);
    }

    // Parallel RPCs
    {
        let t0 = std::time::Instant::now();
        let api = api.clone();
        let reqs = n0_future::stream::iter((0..n).map(move |i| {
            let api = api.clone();
            async move { anyhow::Ok(api.sqr(i).await?.await?) }
        }));
        use futures_buffered::BufferedStreamExt;
        use n0_future::stream::StreamExt;
        let resp: Vec<_> = reqs.buffered_unordered(32).try_collect().await?;
        let sum = resp.into_iter().sum::<u128>();
        let rps = ((n as f64) / t0.elapsed().as_secs_f64()).round() as u64;
        assert_eq!(sum, sum_of_squares(n));
        clear_line()?;
        println!("RPC par {} rps", rps);
    }

    // Sequential streaming (using Multiply instead of MultiplyUpdate)
    {
        let t0 = std::time::Instant::now();
        let (mut send, mut recv) = api.multiply(2).await?;
        let handle = tokio::task::spawn(async move {
            for i in 0..n {
                send.send(i).await?;
            }
            Ok::<(), io::Error>(())
        });
        let mut sum = 0;
        let mut i = 0;
        while let Some(res) = recv.recv().await? {
            sum += res;
            if i % 10000 == 0 {
                print!(".");
                io::stdout().flush()?;
            }
            i += 1;
        }
        let rps = ((n as f64) / t0.elapsed().as_secs_f64()).round() as u64;
        assert_eq!(sum, (0..n).map(|x| x * 2).sum());
        clear_line()?;
        println!("bidi seq {} rps", rps);
        handle.await??;
    }

    Ok(())
}

// Helper function to compute the sum of squares
fn sum_of_squares(n: u64) -> u128 {
    (0..n).map(|x| (x * x) as u128).sum()
}

// Helper function to clear the current line
fn clear_line() -> io::Result<()> {
    io::stdout().write_all(b"\r\x1b[K")?;
    io::stdout().flush()?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();
    println!("Local use");
    local().await?;
    println!("Remote use");
    remote().await?;

    let api = ComputeActor::local();
    bench(api, 1000000).await?;
    Ok(())
}
