#![cfg(all(feature = "noq_endpoint_setup", feature = "derive"))]
//! Checks that a client returns the connection under it.

use irpc::{
    Client, WithChannels,
    noq::listen,
    rpc::{ConnectionHandle, Handler},
};
use n0_future::task::AbortOnDropHandle;
use n0_tracing_test::traced_test;
use testresult::TestResult;

#[allow(dead_code, reason = "this test uses only some of the helpers")]
mod common;
use common::*;

mod proto {
    use irpc::{channel::oneshot, rpc_requests};
    use serde::{Deserialize, Serialize};

    #[rpc_requests(message = EchoMessage)]
    #[derive(Debug, Serialize, Deserialize)]
    pub enum EchoProtocol {
        #[rpc(tx = oneshot::Sender<String>)]
        #[wrap(Echo)]
        Echo(String),
    }
}
use proto::{Echo, EchoMessage, EchoProtocol};

/// Returns a handler that answers `Echo`.
fn echo_handler() -> Handler<EchoProtocol> {
    Handler::sequential(|msg| async move {
        let EchoMessage::Echo(WithChannels { inner, tx, .. }) = msg;
        tx.send(inner.0).await.ok();
        Ok(())
    })
}

/// Returns the noq connection of `client`, or `None` if it has none.
fn noq_connection(client: &Client<EchoProtocol>) -> Option<noq::Connection> {
    match client.connection()? {
        ConnectionHandle::Noq(conn) => Some(conn),
        other => panic!("expected a noq connection, got {other:?}"),
    }
}

#[tokio::test]
#[traced_test]
async fn local_client_has_no_connection() {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let client = Client::<EchoProtocol>::local(tx);
    assert!(client.connection().is_none());
}

#[tokio::test]
#[traced_test]
async fn client_returns_its_connection() -> TestResult<()> {
    let (server, client_endpoint, server_addr) = create_connected_endpoints()?;
    let _server = AbortOnDropHandle::new(tokio::spawn(listen(server, echo_handler())));
    let conn = client_endpoint.connect(server_addr, "localhost")?.await?;
    let client = Client::<EchoProtocol>::boxed(conn.clone());

    let got = noq_connection(&client).expect("the connection is established");
    assert_eq!(got.stable_id(), conn.stable_id());
    Ok(())
}

#[tokio::test]
#[traced_test]
async fn lazy_client_returns_the_current_connection() -> TestResult<()> {
    let (server, client_endpoint, server_addr) = create_connected_endpoints()?;
    let _server = AbortOnDropHandle::new(tokio::spawn(listen(server, echo_handler())));
    let client = Client::<EchoProtocol>::noq(client_endpoint, server_addr);

    // The client connects on the first request.
    assert!(client.connection().is_none());
    assert_eq!(client.rpc(Echo("a".into())).await?, "a");
    let first = noq_connection(&client).expect("the first request connected");

    // After the connection closes, the next request connects again.
    first.close(0u32.into(), b"");
    assert_eq!(client.rpc(Echo("b".into())).await?, "b");
    let second = noq_connection(&client).expect("the second request connected");
    assert_ne!(second.stable_id(), first.stable_id());
    Ok(())
}
