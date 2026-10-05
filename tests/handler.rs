#![cfg(all(feature = "noq_endpoint_setup", feature = "derive", feature = "spans"))]

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use irpc::{Client, WithChannels, channel::oneshot, noq::listen, rpc::Handler, rpc_requests};
use n0_future::task::AbortOnDropHandle;
use serde::{Deserialize, Serialize};
use testresult::TestResult;
use tokio::sync::watch;

#[allow(dead_code, reason = "this test uses only some of the helpers")]
mod common;
use common::*;

#[rpc_requests(message = BlockMessage)]
#[derive(Debug, Serialize, Deserialize)]
enum BlockProtocol {
    /// Waits until the test opens the gate, then replies.
    #[rpc(tx = oneshot::Sender<()>)]
    #[wrap(Block)]
    Block,
}

/// Blocks requests until the test opens it, and counts the requests that wait.
#[derive(Debug)]
struct Gate {
    waiting: watch::Sender<usize>,
    max_waiting: AtomicUsize,
    passed: AtomicUsize,
    open: watch::Sender<bool>,
}

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            waiting: watch::Sender::new(0),
            max_waiting: AtomicUsize::new(0),
            passed: AtomicUsize::new(0),
            open: watch::Sender::new(false),
        })
    }

    /// Returns a concurrent handler that blocks each request at this gate.
    fn handler(self: &Arc<Self>, max_concurrent: usize) -> Handler<BlockProtocol> {
        let gate = self.clone();
        Handler::concurrent(max_concurrent, move |msg| {
            let gate = gate.clone();
            async move {
                let BlockMessage::Block(WithChannels { tx, .. }) = msg;
                gate.waiting.send_modify(|n| {
                    *n += 1;
                    gate.max_waiting.fetch_max(*n, Ordering::SeqCst);
                });
                gate.open.subscribe().wait_for(|open| *open).await.ok();
                gate.waiting.send_modify(|n| *n -= 1);
                gate.passed.fetch_add(1, Ordering::SeqCst);
                tx.send(()).await.ok();
                Ok(())
            }
        })
    }

    /// Waits until `n` requests wait at the gate.
    async fn wait_for(&self, n: usize) -> TestResult<()> {
        let mut waiting = self.waiting.subscribe();
        tokio::time::timeout(Duration::from_secs(10), waiting.wait_for(|w| *w == n)).await??;
        Ok(())
    }
}

/// Sends `requests` requests at once to a handler with `max_concurrent`.
///
/// Returns how many of them ran at the same time. All requests use one connection.
async fn max_running(max_concurrent: usize, requests: usize) -> TestResult<usize> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let gate = Gate::new();
    let _server =
        AbortOnDropHandle::new(tokio::spawn(listen(server, gate.handler(max_concurrent))));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let client = Client::<BlockProtocol>::boxed(conn);
    let calls = (0..requests).map(|_| client.rpc(Block));
    let calls = tokio::spawn(futures_util::future::join_all(calls));
    gate.wait_for(max_concurrent.min(requests)).await?;
    gate.open.send_replace(true);
    for res in calls.await? {
        res?;
    }
    Ok(gate.max_waiting.load(Ordering::SeqCst))
}

#[tokio::test]
async fn concurrent_handler_runs_requests_up_to_the_limit() -> TestResult<()> {
    assert_eq!(max_running(3, 8).await?, 3);
    assert_eq!(max_running(64, 8).await?, 8);
    Ok(())
}

#[tokio::test]
async fn concurrent_handler_completes_requests_after_the_connection_closes() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let gate = Gate::new();
    let handler = gate.handler(4);
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let conn = server.accept().await.expect("incoming connection").await?;
        let closed = conn.clone();
        tokio::spawn(async move {
            closed.closed().await;
            closed_tx.send(()).ok();
        });
        handler.handle_connection(&conn).await
    });
    let conn = client.connect(server_addr, "localhost")?.await?;
    let client = Client::<BlockProtocol>::boxed(conn.clone());
    let _call = tokio::spawn(async move { client.rpc(Block).await });
    gate.wait_for(1).await?;
    conn.close(0u32.into(), b"");
    // Open the gate only after the server sees the close, so that the request
    // still runs when the connection ends.
    closed_rx.await?;
    gate.open.send_replace(true);
    server.await??;
    assert_eq!(gate.passed.load(Ordering::SeqCst), 1);
    Ok(())
}
