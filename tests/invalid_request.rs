#![cfg(all(feature = "noq_endpoint_setup", feature = "derive"))]
//! Checks what a server does with a bad request, and the error classification.

use irpc::{
    Client, WithChannels,
    noq::listen,
    rpc::{ERROR_CODE_DECODE_FAILED, Handler, read_request},
};
use n0_future::task::AbortOnDropHandle;
use noq::ConnectionError;
use testresult::TestResult;

#[allow(dead_code, reason = "this test uses only some of the helpers")]
mod common;
use common::*;

/// The protocol of the server. It knows only `Echo`.
mod server {
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

/// A newer version of the protocol, with a request that the server does not know.
mod client {
    use irpc::{channel::oneshot, rpc_requests};
    use serde::{Deserialize, Serialize};

    #[rpc_requests(message = EchoMessage)]
    #[derive(Debug, Serialize, Deserialize)]
    pub enum EchoProtocol {
        #[rpc(tx = oneshot::Sender<String>)]
        #[wrap(Echo)]
        Echo(String),
        #[rpc(tx = oneshot::Sender<String>)]
        #[wrap(Shout)]
        Shout(String),
    }
}

/// Returns a handler that answers `Echo`.
fn echo_handler() -> Handler<server::EchoProtocol> {
    Handler::sequential(|msg| async move {
        let server::EchoMessage::Echo(WithChannels { inner, tx, .. }) = msg;
        tx.send(inner.0).await.ok();
        Ok(())
    })
}

#[tokio::test]
async fn bad_request_closes_connection() -> TestResult<()> {
    let (server, client_endpoint, server_addr) = create_connected_endpoints()?;
    let _server = AbortOnDropHandle::new(tokio::spawn(listen(server, echo_handler())));
    let conn = client_endpoint.connect(server_addr, "localhost")?.await?;
    let client = Client::<client::EchoProtocol>::boxed(conn.clone());

    let err = client
        .rpc(client::Shout("a".into()))
        .await
        .expect_err("server does not know the request");
    assert!(err.is_invalid_request(), "{err:?}");
    let ConnectionError::ApplicationClosed(close) = conn.closed().await else {
        panic!("server closes the connection");
    };
    assert_eq!(close.error_code, ERROR_CODE_DECODE_FAILED.into());
    Ok(())
}

#[tokio::test]
async fn skip_bad_requests_keeps_connection() -> TestResult<()> {
    let (server, client_endpoint, server_addr) = create_connected_endpoints()?;
    let handler = echo_handler().skip_bad_requests(true);
    let _server = AbortOnDropHandle::new(tokio::spawn(listen(server, handler)));
    let conn = client_endpoint.connect(server_addr, "localhost")?.await?;
    // A client on this one connection, so the second request shows that it still works.
    let client = Client::<client::EchoProtocol>::boxed(conn);

    let err = client
        .rpc(client::Shout("a".into()))
        .await
        .expect_err("server does not know the request");
    assert!(err.is_invalid_request(), "{err:?}");
    assert!(!err.is_connection_lost(), "{err:?}");
    assert_eq!(client.rpc(client::Echo("b".into())).await?, "b");
    Ok(())
}

#[tokio::test]
async fn closed_connection_is_connection_lost() -> TestResult<()> {
    let (server, client_endpoint, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(async move {
        let conn = server.accept().await.expect("endpoint is open").await?;
        read_request::<server::EchoProtocol>(&conn).await?;
        conn.close(1000u32.into(), b"going away");
        TestResult::Ok(())
    });
    let client = Client::<server::EchoProtocol>::noq(client_endpoint, server_addr);
    let err = client
        .rpc(server::Echo("a".into()))
        .await
        .expect_err("server closes the connection");
    assert!(err.is_connection_lost(), "{err:?}");
    assert!(!err.is_invalid_request(), "{err:?}");
    server.await??;
    Ok(())
}
