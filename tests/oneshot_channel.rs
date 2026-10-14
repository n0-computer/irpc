#![cfg(feature = "noq_endpoint_setup")]

use std::io;

use irpc::{
    channel::{
        SendError,
        oneshot::{self, RecvError},
    },
    rpc::ErrorCode,
    util::AsyncWriteVarintExt,
};
use n0_error::e;
use n0_future::future::poll_once;
use noq::Endpoint;
use testresult::TestResult;

mod common;
use common::*;

async fn vec_receiver(server: Endpoint) -> Result<(), RecvError> {
    let conn = server
        .accept()
        .await
        .unwrap()
        .await
        .map_err(|err| e!(RecvError::Io, err.into()))?;
    let (_, recv) = conn
        .accept_bi()
        .await
        .map_err(|err| e!(RecvError::Io, err.into()))?;
    let recv = oneshot::Receiver::<Vec<u8>>::from(recv);
    recv.await?;
    Err(e!(RecvError::Io, io::ErrorKind::UnexpectedEof.into()))
}

/// Checks that a receiver gets `SenderClosed` if the remote sender is dropped without sending, as for a local one.
#[tokio::test]
async fn oneshot_sender_dropped() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await?;
        let (_, recv) = conn.accept_bi().await?;
        let res = oneshot::Receiver::<Vec<u8>>::from(recv).await;
        TestResult::Ok(res)
    });
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    drop(oneshot::Sender::<Vec<u8>>::from(send));
    let res = server.await??;
    assert!(
        matches!(res, Err(RecvError::SenderClosed { .. })),
        "expected SenderClosed, got {res:?}"
    );
    Ok(())
}

/// Checks that the max message size is enforced on the sender side and that errors are propagated to the receiver side.
#[tokio::test]
async fn oneshot_max_message_size_send() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(vec_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send = oneshot::Sender::<Vec<u8>>::from(send);
    // this one should fail!
    let Err(cause) = send.send(vec![0u8; 1024 * 1024 * 32]).await else {
        panic!("client should have failed due to max message size");
    };
    assert!(matches!(cause, SendError::MaxSizeExceeded { .. }));
    let Err(cause) = server.await? else {
        panic!("server should have failed due to max message size");
    };
    assert_eq!(
        cause.remote_code(),
        Some(ErrorCode::MessageMaxSizeExceeded.into())
    );
    Ok(())
}

/// Checks that a send that is cancelled in the middle of the message is `SenderClosed` for the receiver.
#[tokio::test]
async fn oneshot_cancelled_send_is_sender_closed() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(vec_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send = oneshot::Sender::<Vec<u8>>::from(send);
    // The message is larger than the flow control window, so one poll cannot write all of it.
    let mut fut = Box::pin(send.send(vec![0u8; 8 * 1024 * 1024]));
    assert!(poll_once(&mut fut).await.is_none(), "the write is not done");
    drop(fut);
    let Err(cause) = server.await? else {
        panic!("the send was cancelled");
    };
    assert!(matches!(cause, RecvError::SenderClosed { .. }), "{cause:?}");
    Ok(())
}

/// Checks that a stream that ends in the middle of a message is `SenderClosed`, not an io error.
#[tokio::test]
async fn oneshot_truncated_message() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(vec_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    // The size prefix says 100 bytes, but only 3 follow.
    send.write_all(&[100, 1, 2, 3]).await?;
    send.finish()?;
    let Err(cause) = server.await? else {
        panic!("the stream ended in the middle of a message");
    };
    assert!(matches!(cause, RecvError::SenderClosed { .. }), "{cause:?}");
    Ok(())
}

/// Checks that a stream that ends in the middle of the size prefix is `SenderClosed`.
#[tokio::test]
async fn oneshot_truncated_size() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(vec_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    // The high bit says that more bytes of the size prefix follow.
    send.write_all(&[0x80]).await?;
    send.finish()?;
    let Err(cause) = server.await? else {
        panic!("the stream ended in the middle of the size prefix");
    };
    assert!(matches!(cause, RecvError::SenderClosed { .. }), "{cause:?}");
    Ok(())
}

/// Checks that a stream that is reset in the middle of the message is an error, not `SenderClosed`.
#[tokio::test]
async fn oneshot_reset_inside_message_is_an_error() -> TestResult<()> {
    // Part of a body, or part of a size prefix.
    for partial in [&[100, 1, 2, 3][..], &[0x80]] {
        let (server, client, server_addr) = create_connected_endpoints()?;
        let server = tokio::spawn(vec_receiver(server));
        let conn = client.connect(server_addr, "localhost")?.await?;
        let (mut send, _) = conn.open_bi().await?;
        send.write_all(partial).await?;
        send.reset(ErrorCode::MessageEncodeFailed.into())?;
        let Err(cause) = server.await? else {
            panic!("the stream was reset");
        };
        assert_eq!(
            cause.remote_code(),
            Some(ErrorCode::MessageEncodeFailed.into())
        );
    }
    Ok(())
}

/// Checks that a connection that is closed in the middle of the message is an error, not `SenderClosed`.
#[tokio::test]
async fn oneshot_connection_closed_inside_message_is_an_error() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await?;
        let (_, recv) = conn.accept_bi().await?;
        accepted_tx.send(()).ok();
        TestResult::Ok(oneshot::Receiver::<Vec<u8>>::from(recv).await)
    });
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    // Part of a body.
    send.write_all(&[100, 1, 2, 3]).await?;
    // Close only after the receiver has the stream.
    accepted_rx.await?;
    conn.close(ErrorCode::RequestAborted.into(), b"");
    let Err(cause) = server.await?? else {
        panic!("the connection was closed");
    };
    assert_eq!(cause.remote_code(), Some(ErrorCode::RequestAborted.into()));
    Ok(())
}

/// Checks that the max message size is enforced on receiver side.
#[tokio::test]
async fn oneshot_max_message_size_recv() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(vec_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    // this one should fail on receive!
    send.write_length_prefixed(vec![0u8; 1024 * 1024 * 32])
        .await
        .ok();
    let Err(cause) = server.await? else {
        panic!("server should have failed due to max message size");
    };
    assert!(matches!(cause, RecvError::MaxSizeExceeded { .. }));
    Ok(())
}

async fn noser_receiver(server: Endpoint) -> Result<(), RecvError> {
    let conn = server
        .accept()
        .await
        .unwrap()
        .await
        .map_err(|err| e!(RecvError::Io, err.into()))?;
    let (_, recv) = conn
        .accept_bi()
        .await
        .map_err(|err| e!(RecvError::Io, err.into()))?;
    let recv = oneshot::Receiver::<NoSer>::from(recv);
    recv.await?;
    Err(e!(RecvError::Io, io::ErrorKind::UnexpectedEof.into()))
}

/// Checks that a message that does not encode gives `MessageEncodeFailed` on both sides, as an error code for the receiver.
#[tokio::test]
async fn oneshot_serialize_error_send() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(noser_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send = oneshot::Sender::<NoSer>::from(send);
    // this one should fail!
    let Err(cause) = send.send(NoSer(1)).await else {
        panic!("client should have failed due to serialization error");
    };
    assert!(matches!(cause, SendError::EncodeFailed { .. }), "{cause:?}");
    let Err(cause) = server.await? else {
        panic!("server should have failed due to serialization error");
    };
    assert_eq!(
        cause.remote_code(),
        Some(ErrorCode::MessageEncodeFailed.into())
    );
    Ok(())
}

#[tokio::test]
async fn oneshot_serialize_error_recv() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(noser_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    // this one should fail on receive!
    send.write_length_prefixed(1u64).await?;
    send.finish()?;
    let Err(cause) = server.await? else {
        panic!("server should have failed due to serialization error");
    };
    assert!(matches!(cause, RecvError::DecodeFailed { .. }), "{cause:?}");
    Ok(())
}

/// Checks that a local sender is not rpc, also after a transform.
#[tokio::test]
async fn oneshot_local_is_not_rpc() -> TestResult<()> {
    let (send, recv) = oneshot::channel::<u64>();
    assert!(!send.is_rpc());
    let send = send.with_map(|x: u32| x as u64).with_filter(|x| *x > 0);
    assert!(!send.is_rpc());
    send.send(7).await?;
    assert_eq!(recv.await?, 7);
    Ok(())
}

/// Checks that a remote sender is rpc, also after a transform.
#[tokio::test]
async fn oneshot_remote_is_rpc() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await?;
        let (_, recv) = conn.accept_bi().await?;
        let res = oneshot::Receiver::<u64>::from(recv).await?;
        TestResult::Ok(res)
    });
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send = oneshot::Sender::<u64>::from(send);
    assert!(send.is_rpc());
    let send = send.with_map(|x: u32| x as u64);
    assert!(send.is_rpc());
    send.send(7).await?;
    assert_eq!(server.await??, 7);
    Ok(())
}
