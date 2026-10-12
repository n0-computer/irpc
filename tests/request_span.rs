#![cfg(all(feature = "noq_endpoint_setup", feature = "derive"))]
//! Checks the request spans and the logs of requests, on the client and on the server.

use irpc::{
    Client, WithChannels,
    channel::{mpsc, oneshot},
    noq::listen,
    rpc::{Handler, read_request},
    rpc_requests,
};
use n0_future::task::AbortOnDropHandle;
use n0_tracing_test::traced_test;
use serde::{Deserialize, Serialize};
use testresult::TestResult;
use tracing::{Instrument, Span, info, info_span};

#[allow(dead_code, reason = "this test uses only some of the helpers")]
mod common;
use common::create_connected_endpoints;

#[rpc_requests(message = EchoMessage)]
#[derive(Debug, Serialize, Deserialize)]
enum EchoProtocol {
    #[rpc(tx = oneshot::Sender<()>)]
    #[wrap(Echo)]
    Echo,
    /// Streams the numbers below `n`.
    #[rpc(tx = mpsc::Sender<u64>)]
    #[wrap(Count)]
    Count(u64),
    /// Adds up the numbers that the client streams.
    #[rpc(rx = mpsc::Receiver<u64>, tx = oneshot::Sender<u64>)]
    #[wrap(Sum)]
    Sum,
}

/// Handles each request, and logs `handled` in the handler.
fn handler() -> Handler<EchoProtocol> {
    Handler::sequential(|msg: EchoMessage| async move {
        // The handler runs in the request span, which is also the span of the message.
        assert_eq!(msg.span().id(), Span::current().id());
        info!("handled");
        match msg {
            EchoMessage::Echo(WithChannels { tx, .. }) => {
                tx.send(()).await.ok();
            }
            EchoMessage::Count(WithChannels { inner, tx, .. }) => {
                for i in 0..inner.0 {
                    tx.send(i).await.ok();
                }
            }
            EchoMessage::Sum(WithChannels { mut rx, tx, .. }) => {
                let mut sum = 0;
                while let Ok(Some(i)) = rx.recv().await {
                    sum += i;
                }
                tx.send(sum).await.ok();
            }
        }
        Ok(())
    })
}

/// Asserts that the logs of this test contain each of `lines`.
macro_rules! assert_logs {
    ($($line:expr),* $(,)?) => {
        $(assert!(logs_contain(&$line), "missing log: {}", $line);)*
    };
}

/// The logs of requests and streams have the connection and the request, on both sides.
#[tokio::test]
#[traced_test]
async fn logs_have_connection_and_request() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = listen(server, handler()).instrument(info_span!("server"));
    let _server = AbortOnDropHandle::new(tokio::spawn(server));
    let client = Client::<EchoProtocol>::noq(client, server_addr);
    async {
        client.rpc(Echo).await?;
        let mut rx = client.server_streaming(Count(3), 8).await?;
        while rx.recv().await?.is_some() {}
        let (tx, rx) = client.client_streaming(Sum, 8).await?;
        for i in 0..3 {
            tx.send(i).await?;
        }
        drop(tx);
        assert_eq!(rx.await?, 3);
        TestResult::Ok(())
    }
    .instrument(info_span!("client"))
    .await?;

    // The client logs in a span with the same name and request id as the server.
    assert_logs!(
        "server:rpc{conn=0}: irpc::rpc::noq_impl: connection accepted remote=127.0.0.1:",
        "client:Echo{req=0}: irpc::rpc: request sent",
        "server:rpc{conn=0}:Echo{req=0}: irpc::rpc: request received",
        "server:rpc{conn=0}:Echo{req=0}: request_span: handled",
        "server:rpc{conn=0}:Echo{req=0}: irpc::rpc: response sent",
        "client:Echo{req=0}: irpc::rpc: response received",
    );
    // The server streams the responses of `Count`.
    assert_logs!(
        "server:rpc{conn=0}:Count{req=1}: irpc::rpc: first message sent",
        "server:rpc{conn=0}:Count{req=1}: irpc::rpc: stream finished sent=3",
        "client:Count{req=1}: irpc::rpc: first message received",
        "client:Count{req=1}: irpc::rpc: stream finished received=3",
    );
    // The client streams the updates of `Sum`.
    assert_logs!(
        "client:Sum{req=2}: irpc::rpc: first message sent",
        "client:Sum{req=2}: irpc::rpc: stream finished sent=3",
        "server:rpc{conn=0}:Sum{req=2}: irpc::rpc: first message received",
        "server:rpc{conn=0}:Sum{req=2}: irpc::rpc: stream finished received=3",
        "server:rpc{conn=0}:Sum{req=2}: irpc::rpc: response sent",
        "client:Sum{req=2}: irpc::rpc: response received",
    );
    Ok(())
}

/// [`read_request`] gives the request span the number of the request too.
#[tokio::test]
#[traced_test]
async fn read_request_numbers_the_requests() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = async move {
        let connection = server
            .accept()
            .await
            .expect("accept")
            .await
            .expect("connect");
        while let Ok(Some(msg)) = read_request::<EchoProtocol>(&connection).await {
            msg.span().in_scope(|| info!("handled"));
            let EchoMessage::Echo(WithChannels { tx, .. }) = msg else {
                unreachable!()
            };
            tx.send(()).await.ok();
        }
    };
    let _server = AbortOnDropHandle::new(tokio::spawn(server.instrument(info_span!("server"))));
    let client = Client::<EchoProtocol>::noq(client, server_addr);
    client.rpc(Echo).await?;
    client.rpc(Echo).await?;

    assert_logs!(
        "server:Echo{req=0}: request_span: handled",
        "server:Echo{req=1}: request_span: handled",
    );
    Ok(())
}
