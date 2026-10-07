#![cfg(all(feature = "noq_endpoint_setup", feature = "derive"))]
//! Checks what a server does with a bad request.

use irpc::{
    Client, WithChannels,
    noq::listen,
    rpc::{ERROR_CODE_DECODE_FAILED, Handler, ReadRequestError, read_request},
};
use n0_future::{future::poll_once, task::AbortOnDropHandle};
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

    client
        .rpc(client::Shout("a".into()))
        .await
        .expect_err("server does not know the request");
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

    client
        .rpc(client::Shout("a".into()))
        .await
        .expect_err("server does not know the request");
    assert_eq!(client.rpc(client::Echo("b".into())).await?, "b");
    Ok(())
}

#[tokio::test]
async fn dropped_rpc_keeps_connection() -> TestResult<()> {
    let (server, client_endpoint, server_addr) = create_connected_endpoints()?;
    let _server = AbortOnDropHandle::new(tokio::spawn(listen(server, echo_handler())));
    let conn = client_endpoint.connect(server_addr, "localhost")?.await?;
    let client = Client::<client::EchoProtocol>::boxed(conn);

    // The request is larger than the flow control window, so one poll cannot
    // write all of it. The drop then finishes the stream before the request ends.
    let mut rpc = Box::pin(client.rpc(client::Echo("a".repeat(8 * 1024 * 1024))));
    assert!(poll_once(&mut rpc).await.is_none(), "the write is not done");
    drop(rpc);
    assert_eq!(client.rpc(client::Echo("b".into())).await?, "b");
    Ok(())
}

#[tokio::test]
async fn reset_request_keeps_connection() -> TestResult<()> {
    let (server, client_endpoint, server_addr) = create_connected_endpoints()?;
    let _server = AbortOnDropHandle::new(tokio::spawn(listen(server, echo_handler())));
    let conn = client_endpoint.connect(server_addr, "localhost")?.await?;
    let client = Client::<client::EchoProtocol>::boxed(conn.clone());

    let (mut send, mut recv) = conn.open_bi().await?;
    // The size prefix says 100 bytes, but only 3 follow.
    send.write_all(&[100, 1, 2, 3]).await?;
    send.reset(1000u32.into())?;
    // The server resets its side too, so this is not an empty response.
    assert_eq!(
        recv.read(&mut [0]).await,
        Err(noq::ReadError::Reset(0u32.into()))
    );
    assert_eq!(client.rpc(client::Echo("b".into())).await?, "b");
    Ok(())
}

#[tokio::test]
async fn read_request_returns_bad_request() -> TestResult<()> {
    let (server, client_endpoint, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(async move {
        let conn = server.accept().await.expect("endpoint is open").await?;
        let err = read_request::<server::EchoProtocol>(&conn)
            .await
            .expect_err("server does not know the request");
        assert!(
            matches!(err, ReadRequestError::InvalidRequest { .. }),
            "{err:?}"
        );
        // The connection is still open, so the server reads the next request.
        let Some(server::EchoMessage::Echo(msg)) =
            read_request::<server::EchoProtocol>(&conn).await?
        else {
            panic!("client sends a second request");
        };
        msg.tx.send(msg.inner.0).await?;
        conn.closed().await;
        TestResult::Ok(())
    });
    let conn = client_endpoint.connect(server_addr, "localhost")?.await?;
    let client = Client::<client::EchoProtocol>::boxed(conn.clone());

    client
        .rpc(client::Shout("a".into()))
        .await
        .expect_err("server does not know the request");
    assert_eq!(client.rpc(client::Echo("b".into())).await?, "b");
    conn.close(0u32.into(), b"");
    server.await??;
    Ok(())
}
