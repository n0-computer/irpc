#![cfg(all(feature = "noq_endpoint_setup", feature = "derive", feature = "spans"))]

use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use irpc::{
    Client, Request, WithChannels,
    channel::{none::NoSender, oneshot},
    noq::{listen, make_client_endpoint, make_server_endpoint},
    rpc::{CloseConnection, Handler, RemoteService},
    rpc_requests,
};
use n0_future::task::AbortOnDropHandle;
use noq::{ConnectionError, Endpoint};
use serde::{Deserialize, Serialize};
use testresult::TestResult;

fn create_connected_endpoints() -> TestResult<(Endpoint, Endpoint, SocketAddr)> {
    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into();
    let (server, cert) = make_server_endpoint(addr)?;
    let client = make_client_endpoint(addr, &[cert.as_slice()])?;
    let port = server.local_addr()?.port();
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into();
    Ok((server, client, server_addr))
}

#[rpc_requests(message = WaitMessage)]
#[derive(Debug, Serialize, Deserialize)]
enum WaitProtocol {
    /// Waits for a short time, then replies.
    #[rpc(tx = oneshot::Sender<()>)]
    #[wrap(Wait)]
    Wait,
    /// Waits for a short time, then sets the `marked` flag. Sends no reply.
    #[rpc(tx = NoSender)]
    #[wrap(Mark)]
    Mark,
}

/// Counts the requests that run at the same time.
#[derive(Debug, Default)]
struct Gauge {
    running: AtomicUsize,
    max: AtomicUsize,
    marked: AtomicBool,
}

impl Gauge {
    async fn handle(&self, msg: WaitMessage) {
        let tx = match msg {
            WaitMessage::Wait(WithChannels { tx, .. }) => tx,
            WaitMessage::Mark(_) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                self.marked.store(true, Ordering::SeqCst);
                return;
            }
        };
        let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(running, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.running.fetch_sub(1, Ordering::SeqCst);
        tx.send(()).await.ok();
    }
}

/// Sends `requests` requests at once and returns how many of them ran at the same time.
///
/// All requests use one connection.
async fn max_running(
    requests: usize,
    handler: impl FnOnce(Arc<Gauge>) -> Handler<WaitProtocol>,
) -> TestResult<usize> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let gauge = Arc::new(Gauge::default());
    let _server = AbortOnDropHandle::new(tokio::spawn(listen(server, handler(gauge.clone()))));
    let client = Client::<WaitProtocol>::noq(client, server_addr);
    // Open the connection first, so that all requests share it.
    client.rpc(Wait).await?;
    gauge.max.store(0, Ordering::SeqCst);
    let calls = (0..requests).map(|_| client.rpc(Wait));
    for res in futures_util::future::join_all(calls).await {
        res?;
    }
    Ok(gauge.max.load(Ordering::SeqCst))
}

fn gauge_handler(max_concurrent: usize, gauge: Arc<Gauge>) -> Handler<WaitProtocol> {
    Handler::concurrent(max_concurrent, move |msg| {
        let gauge = gauge.clone();
        async move {
            gauge.handle(msg).await;
            Ok(())
        }
    })
}

#[tokio::test]
async fn concurrent_handler_runs_requests_up_to_the_limit() -> TestResult<()> {
    let max = max_running(8, |gauge| gauge_handler(3, gauge)).await?;
    assert_eq!(max, 3);
    let max = max_running(8, |gauge| gauge_handler(64, gauge)).await?;
    assert_eq!(max, 8);
    Ok(())
}

#[tokio::test]
async fn concurrent_handler_completes_requests_after_the_connection_closes() -> TestResult<()> {
    let (server, client_endpoint, server_addr) = create_connected_endpoints()?;
    let gauge = Arc::new(Gauge::default());
    let handler = gauge_handler(4, gauge.clone());
    let _server = AbortOnDropHandle::new(tokio::spawn(listen(server, handler)));
    let client = Client::<WaitProtocol>::noq(client_endpoint.clone(), server_addr);
    client.notify(Mark).await?;
    // The response shows that the server read the `Mark` request before it.
    client.rpc(Wait).await?;
    client_endpoint.close(0u32.into(), b"");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(gauge.marked.load(Ordering::SeqCst));
    Ok(())
}

#[tokio::test]
async fn sequential_handler_runs_one_request_at_a_time() -> TestResult<()> {
    let max = max_running(4, |gauge| {
        Handler::sequential(move |msg| {
            let gauge = gauge.clone();
            async move {
                gauge.handle(msg).await;
                Ok(())
            }
        })
    })
    .await?;
    assert_eq!(max, 1);
    Ok(())
}

#[tokio::test]
async fn concurrent_handler_propagates_a_panic() -> TestResult<()> {
    #[rpc_requests(message = PanicMessage)]
    #[derive(Debug, Serialize, Deserialize)]
    enum PanicProtocol {
        /// Panics if the argument is `true`, else replies.
        #[rpc(tx = oneshot::Sender<()>)]
        #[wrap(MaybePanic)]
        MaybePanic(bool),
    }

    let (server, client, server_addr) = create_connected_endpoints()?;
    let handler = Handler::concurrent(4, |msg: PanicMessage| async move {
        let PanicMessage::MaybePanic(WithChannels { inner, tx, .. }) = msg;
        assert!(!inner.0, "requested panic");
        tx.send(()).await.ok();
        Ok(())
    });
    let server = tokio::spawn(listen::<PanicProtocol>(server, handler));
    let client = Client::<PanicProtocol>::noq(client, server_addr);
    client.rpc(MaybePanic(false)).await?;
    assert!(client.rpc(MaybePanic(true)).await.is_err());
    // The panic reaches the server loop of the connection, and `listen`
    // panics too. The client does not have to close the connection first.
    let res = tokio::time::timeout(Duration::from_secs(5), server).await?;
    assert!(res.is_err_and(|err| err.is_panic()));
    Ok(())
}

#[tokio::test]
async fn raw_handler_gets_the_protocol_enum_and_the_streams() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let handler = Handler::raw(|request: WaitProtocol, rx, tx| async move {
        // The request arrives as the protocol enum, before irpc creates the channels.
        assert!(matches!(request, WaitProtocol::Wait(_)));
        match request.with_remote_channels(rx, tx) {
            WaitMessage::Wait(WithChannels { tx, .. }) => {
                tx.send(()).await.ok();
            }
            WaitMessage::Mark(_) => {}
        }
        Ok(())
    });
    let _server = AbortOnDropHandle::new(tokio::spawn(listen(server, handler)));
    let client = Client::<WaitProtocol>::noq(client, server_addr);
    client.rpc(Wait).await?;
    client.rpc(Wait).await?;
    Ok(())
}

#[rpc_requests(message = GuardMessage)]
#[derive(Debug, Serialize, Deserialize)]
enum GuardProtocol {
    /// Replies, or closes the connection with code 400 if the argument is `true`.
    #[rpc(tx = oneshot::Sender<()>)]
    #[wrap(Check)]
    Check(bool),
}

/// Handles a [`GuardProtocol`] request.
async fn guard(msg: GuardMessage) -> Result<(), CloseConnection> {
    let GuardMessage::Check(WithChannels { inner, tx, .. }) = msg;
    if inner.0 {
        return Err(CloseConnection::new(400, "violation"));
    }
    tx.send(()).await.ok();
    Ok(())
}

/// Checks that a request that violates the protocol closes the connection with code 400.
async fn assert_handler_closes_connection(handler: Handler<GuardProtocol>) -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let _server = AbortOnDropHandle::new(tokio::spawn(listen(server, handler)));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let client = Client::<GuardProtocol>::boxed(conn.clone());
    client.rpc(Check(false)).await?;
    assert!(client.rpc(Check(true)).await.is_err());
    let ConnectionError::ApplicationClosed(close) = conn.closed().await else {
        panic!("the server closes the connection");
    };
    assert_eq!(close.error_code, 400u32.into());
    assert_eq!(&close.reason[..], b"violation");
    Ok(())
}

#[tokio::test]
async fn sequential_handler_closes_the_connection() -> TestResult<()> {
    assert_handler_closes_connection(Handler::sequential(guard)).await
}

#[tokio::test]
async fn concurrent_handler_closes_the_connection() -> TestResult<()> {
    assert_handler_closes_connection(Handler::concurrent(4, guard)).await
}

/// Holds a request of `WaitProtocol` or of `GuardProtocol`.
///
/// A crate like irpc-schema can generate such a protocol. The test writes
/// its requests with [`irpc::rpc::RemoteSender::write`].
#[derive(Debug, Serialize, Deserialize)]
enum AppProtocol {
    Wait(WaitProtocol),
    Guard(GuardProtocol),
}

impl irpc::Service for AppProtocol {
    // The server passes the inner requests to other handlers, so it needs no message enum.
    type Message = ();
}

#[tokio::test]
async fn raw_handler_calls_handlers_of_other_protocols() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let wait = gauge_handler(4, Arc::new(Gauge::default()));
    let guard = Handler::sequential(guard);
    let handler = Handler::<AppProtocol>::raw(move |request, rx, tx| {
        let (wait, guard) = (wait.clone(), guard.clone());
        async move {
            match request {
                AppProtocol::Wait(request) => wait.call(request, rx, tx).await,
                AppProtocol::Guard(request) => guard.call(request, rx, tx).await,
            }
        }
    });
    let _server = AbortOnDropHandle::new(tokio::spawn(listen(server, handler)));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let client = Client::<AppProtocol>::boxed(conn.clone());

    // Sends `request` and waits for the response of a `oneshot::Sender<()>`.
    let call = async |request: AppProtocol| -> irpc::Result<()> {
        let Request::Remote(sender) = client.request().await? else {
            panic!("the client is remote");
        };
        let (_send, recv) = sender.write(request).await?;
        let response: oneshot::Receiver<()> = recv.into();
        response.await?;
        Ok(())
    };
    call(AppProtocol::Wait(WaitProtocol::Wait(Wait))).await?;
    call(AppProtocol::Guard(GuardProtocol::Check(Check(false)))).await?;
    // A `CloseConnection` from the inner handler closes the connection too.
    assert!(
        call(AppProtocol::Guard(GuardProtocol::Check(Check(true))))
            .await
            .is_err()
    );
    let ConnectionError::ApplicationClosed(close) = conn.closed().await else {
        panic!("the server closes the connection");
    };
    assert_eq!(close.error_code, 400u32.into());
    Ok(())
}
