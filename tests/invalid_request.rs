#![cfg(all(feature = "noq_endpoint_setup", feature = "derive"))]
//! Checks what a server does with a bad request.

use std::{pin::pin, time::Duration};

use irpc::{
    Client, WithChannels,
    noq::listen,
    rpc::{
        ErrorCode, Handler, MAX_MESSAGE_SIZE, ReadRequestError, RemoteCode, WriteError,
        read_request,
    },
};
use n0_future::{future::poll_once, task::AbortOnDropHandle};
use noq::ConnectionError;
use testresult::TestResult;

#[allow(dead_code, reason = "this test uses only some of the helpers")]
mod common;
use common::*;

/// The protocol of the server.
mod server {
    use irpc::{
        channel::{mpsc, oneshot},
        rpc_requests,
    };
    use serde::{Deserialize, Serialize};

    use super::NoSer;

    #[rpc_requests(message = EchoMessage)]
    #[derive(Debug, Serialize, Deserialize)]
    pub enum EchoProtocol {
        #[rpc(tx = oneshot::Sender<String>)]
        #[wrap(Echo)]
        Echo(String),
        #[rpc(tx = oneshot::Sender<()>, rx = mpsc::Receiver<NoSer>)]
        #[wrap(Upload)]
        Upload(String),
    }
}

/// A newer version of the protocol, with a request that the server does not know.
mod client {
    use irpc::{
        channel::{mpsc, oneshot},
        rpc_requests,
    };
    use serde::{Deserialize, Serialize};

    use super::NoSer;

    #[rpc_requests(message = EchoMessage)]
    #[derive(Debug, Serialize, Deserialize)]
    pub enum EchoProtocol {
        #[rpc(tx = oneshot::Sender<String>)]
        #[wrap(Echo)]
        Echo(String),
        #[rpc(tx = oneshot::Sender<()>, rx = mpsc::Receiver<NoSer>)]
        #[wrap(Upload)]
        Upload(String),
        #[rpc(tx = mpsc::Sender<()>, rx = mpsc::Receiver<NoSer>)]
        #[wrap(Exchange)]
        Exchange(String),
        #[rpc(tx = oneshot::Sender<String>)]
        #[wrap(Shout)]
        Shout(String),
        #[rpc(tx = oneshot::Sender<()>, rx = mpsc::Receiver<u32>)]
        #[wrap(ShoutStream)]
        ShoutStream(String),
    }
}

/// Returns a handler that answers `Echo`.
fn echo_handler() -> Handler<server::EchoProtocol> {
    Handler::sequential(|msg| async move {
        if let server::EchoMessage::Echo(WithChannels { inner, tx, .. }) = msg {
            tx.send(inner.0).await.ok();
        }
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
    // The close usually arrives before the reset of the stream.
    assert_eq!(
        err.remote_code(),
        Some(ErrorCode::RequestDecodeFailed.into())
    );
    let ConnectionError::ApplicationClosed(close) = conn.closed().await else {
        panic!("server closes the connection");
    };
    assert_eq!(close.error_code, ErrorCode::RequestDecodeFailed.into());
    Ok(())
}

#[tokio::test]
async fn too_large_request_closes_connection() -> TestResult<()> {
    let (server, client_endpoint, server_addr) = create_connected_endpoints()?;
    let _server = AbortOnDropHandle::new(tokio::spawn(listen(server, echo_handler())));
    let conn = client_endpoint.connect(server_addr, "localhost")?.await?;

    // An irpc client does not send a request this large, so write only its size prefix.
    let (mut send, _recv) = conn.open_bi().await?;
    send.write_all(&postcard::to_stdvec(&(MAX_MESSAGE_SIZE + 1))?)
        .await?;
    let ConnectionError::ApplicationClosed(close) = conn.closed().await else {
        panic!("server closes the connection");
    };
    assert_eq!(close.error_code, ErrorCode::RequestMaxSizeExceeded.into());
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
    assert_eq!(
        err.remote_code(),
        Some(ErrorCode::RequestDecodeFailed.into())
    );
    assert_eq!(client.rpc(client::Echo("b".into())).await?, "b");
    Ok(())
}

/// The sender of the updates of a bad request gets the code of the rejection, as the receiver of its response does.
#[tokio::test]
async fn bad_request_update_sender() -> TestResult<()> {
    let (server, client_endpoint, server_addr) = create_connected_endpoints()?;
    let handler = echo_handler().skip_bad_requests(true);
    let _server = AbortOnDropHandle::new(tokio::spawn(listen(server, handler)));
    let conn = client_endpoint.connect(server_addr, "localhost")?.await?;
    let client = Client::<client::EchoProtocol>::boxed(conn);

    let (tx, rx) = client
        .client_streaming(client::ShoutStream("a".into()), 1)
        .await?;
    let err = rx.await.expect_err("server does not know the request");
    assert_eq!(
        err.remote_code(),
        Some(ErrorCode::RequestDecodeFailed.into())
    );
    let err = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Err(err) = tx.send(1).await {
                break err;
            }
        }
    })
    .await?;
    assert_eq!(
        err.remote_code(),
        Some(ErrorCode::RequestDecodeFailed.into())
    );
    Ok(())
}

/// A client that cannot send a request resets its stream with the reason.
#[tokio::test]
async fn too_large_rpc_resets_with_reason() -> TestResult<()> {
    let (server, client_endpoint, server_addr) = create_connected_endpoints()?;
    let connecting = client_endpoint.connect(server_addr, "localhost")?;
    let accepting = async { server.accept().await.expect("endpoint is open").await };
    let (client_conn, server_conn) = tokio::try_join!(connecting, accepting)?;
    let client = Client::<client::EchoProtocol>::boxed(client_conn);

    let large = "a".repeat(MAX_MESSAGE_SIZE as usize + 1);
    let err = client
        .rpc(client::Echo(large))
        .await
        .expect_err("the request is too large");
    assert!(
        matches!(
            err,
            irpc::Error::Write {
                source: WriteError::MaxSizeExceeded { .. },
                ..
            }
        ),
        "{err:?}"
    );
    let (_send, mut recv) = server_conn.accept_bi().await?;
    assert_eq!(
        recv.read(&mut [0]).await,
        Err(noq::ReadError::Reset(
            ErrorCode::RequestMaxSizeExceeded.into()
        ))
    );
    Ok(())
}

#[tokio::test]
async fn dropped_rpc_keeps_connection() -> TestResult<()> {
    let (server, client_endpoint, server_addr) = create_connected_endpoints()?;
    let _server = AbortOnDropHandle::new(tokio::spawn(listen(server, echo_handler())));
    let conn = client_endpoint.connect(server_addr, "localhost")?.await?;
    let client = Client::<client::EchoProtocol>::boxed(conn);

    // The request is larger than the flow control window, so one poll cannot
    // write all of it. The drop then resets the stream with `ErrorCode::RequestAborted`.
    let mut rpc = Box::pin(client.rpc(client::Echo("a".repeat(8 * 1024 * 1024))));
    assert!(poll_once(&mut rpc).await.is_none(), "the write is not done");
    drop(rpc);
    assert_eq!(client.rpc(client::Echo("b".into())).await?, "b");
    Ok(())
}

#[tokio::test]
async fn truncated_request_closes_connection() -> TestResult<()> {
    let (server, client_endpoint, server_addr) = create_connected_endpoints()?;
    let _server = AbortOnDropHandle::new(tokio::spawn(listen(server, echo_handler())));
    let conn = client_endpoint.connect(server_addr, "localhost")?.await?;

    let (mut send, _recv) = conn.open_bi().await?;
    // The size prefix says 100 bytes, but only 3 follow.
    send.write_all(&[100, 1, 2, 3]).await?;
    send.finish()?;
    let ConnectionError::ApplicationClosed(close) = conn.closed().await else {
        panic!("server closes the connection");
    };
    assert_eq!(close.error_code, ErrorCode::RequestDecodeFailed.into());
    Ok(())
}

/// The update senders reset the streams of their requests before the server reads them.
#[tokio::test]
async fn reset_before_read_skips_request() -> TestResult<()> {
    let (server, client_endpoint, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(async move {
        let conn = server.accept().await.expect("endpoint is open").await?;
        // Read only after both resets arrived: noq then has dropped both requests.
        while conn.stats().frame_rx.reset_stream < 2 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let Some(server::EchoMessage::Echo(msg)) =
            read_request::<server::EchoProtocol>(&conn).await?
        else {
            panic!("server skips the upload and the exchange");
        };
        msg.tx.send(msg.inner.0).await?;
        conn.closed().await;
        TestResult::Ok(())
    });
    let conn = client_endpoint.connect(server_addr, "localhost")?.await?;
    let client = Client::<client::EchoProtocol>::boxed(conn.clone());

    let (upload_tx, upload_rx) = client
        .client_streaming(client::Upload("a".into()), 1)
        .await?;
    let (exchange_tx, mut exchange_rx) = client
        .bidi_streaming(client::Exchange("a".into()), 1, 1)
        .await?;
    // The updates do not encode, so the senders reset with `ErrorCode::MessageEncodeFailed`.
    upload_tx.send(NoSer(1)).await.expect_err("does not encode");
    exchange_tx
        .send(NoSer(1))
        .await
        .expect_err("does not encode");
    assert_eq!(client.rpc(client::Echo("b".into())).await?, "b");
    // The server reset its side of both streams, so the mpsc response fails instead of ending.
    let err = upload_rx.await.expect_err("server skips the upload");
    assert_eq!(err.remote_code(), Some(ErrorCode::RequestAborted.into()));
    let err = exchange_rx
        .recv()
        .await
        .expect_err("server skips the exchange");
    assert_eq!(err.remote_code(), Some(ErrorCode::RequestAborted.into()));
    conn.close(0u32.into(), b"");
    server.await??;
    Ok(())
}

#[tokio::test]
async fn read_request_returns_truncated_request() -> TestResult<()> {
    let (server, client_endpoint, server_addr) = create_connected_endpoints()?;
    let connecting = client_endpoint.connect(server_addr, "localhost")?;
    let accepting = async { server.accept().await.expect("endpoint is open").await };
    let (client_conn, server_conn) = tokio::try_join!(connecting, accepting)?;

    let (mut send, _recv) = client_conn.open_bi().await?;
    // The size prefix says 100 bytes, but only 3 follow.
    send.write_all(&[100, 1, 2, 3]).await?;
    send.finish()?;
    let err = read_request::<server::EchoProtocol>(&server_conn)
        .await
        .expect_err("the request is not complete");
    assert!(matches!(err, ReadRequestError::Malformed { .. }), "{err:?}");
    Ok(())
}

/// A close in the middle of a request ends `read_request` as a close between requests does.
#[tokio::test]
async fn close_during_request_returns_none() -> TestResult<()> {
    let (server, client_endpoint, server_addr) = create_connected_endpoints()?;
    let connecting = client_endpoint.connect(server_addr, "localhost")?;
    let accepting = async { server.accept().await.expect("endpoint is open").await };
    let (client_conn, server_conn) = tokio::try_join!(connecting, accepting)?;

    let (mut send, _recv) = client_conn.open_bi().await?;
    // The size prefix says 100 bytes, but only 3 follow.
    send.write_all(&[100, 1, 2, 3]).await?;
    let mut read = pin!(read_request::<server::EchoProtocol>(&server_conn));
    while server_conn.stats().frame_rx.stream == 0 {
        assert!(poll_once(&mut read).await.is_none());
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(
        poll_once(&mut read).await.is_none(),
        "server waits for the rest of the request"
    );
    client_conn.close(0u32.into(), b"");
    assert!(read.await?.is_none());
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
            matches!(err, ReadRequestError::DecodeFailed { .. }),
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

#[tokio::test]
async fn close_with_code_returns_connection_closed() -> TestResult<()> {
    let (server, client_endpoint, server_addr) = create_connected_endpoints()?;
    let connecting = client_endpoint.connect(server_addr, "localhost")?;
    let accepting = async { server.accept().await.expect("endpoint is open").await };
    let (client_conn, server_conn) = tokio::try_join!(connecting, accepting)?;

    client_conn.close(401u32.into(), b"bye");
    let err = read_request::<server::EchoProtocol>(&server_conn)
        .await
        .expect_err("the client closed the connection with a code");
    let ReadRequestError::ConnectionClosed { close, .. } = err else {
        panic!("expected ConnectionClosed, got {err:?}");
    };
    assert_eq!(close.code(), Some(RemoteCode::Other(401)));

    // A client that opens a request on the closed connection gets the same close.
    let client = Client::<client::EchoProtocol>::boxed(server_conn);
    let err = client
        .rpc(client::Echo("a".into()))
        .await
        .expect_err("the connection is closed");
    assert_eq!(err.remote_code(), Some(RemoteCode::Other(401)));
    Ok(())
}
