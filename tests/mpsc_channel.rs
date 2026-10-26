#![cfg(feature = "noq_endpoint_setup")]

use std::{
    io::{self, ErrorKind},
    time::Duration,
};

use irpc::{
    Client, WithChannels,
    channel::{
        SendError,
        mpsc::{self, Receiver, RecvError},
        oneshot,
    },
    rpc::{ErrorCode, Handler},
    rpc_requests,
    util::{AsyncWriteVarintExt, WriteVarintExt},
};
use n0_error::e;
use noq::Endpoint;
use serde::{Deserialize, Serialize};
use testresult::TestResult;
use tokio::time::timeout;

mod common;
use common::*;

/// Checks that all clones of a `Sender` will get the closed signal as soon as
/// a send fails with an io error.
#[tokio::test]
async fn mpsc_sender_clone_closed_error() -> TestResult<()> {
    tracing_subscriber::fmt::try_init().ok();
    let (server, client, server_addr) = create_connected_endpoints()?;
    // accept a single bidi stream on a single connection, then immediately stop it
    let server = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await?;
        let (_, mut recv) = conn.accept_bi().await?;
        recv.stop(1u8.into())?;
        TestResult::Ok(())
    });
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send1 = mpsc::Sender::<Vec<u8>>::from(send);
    let send2 = send1.clone();
    let send3 = send1.clone();
    let second_client = tokio::spawn(async move {
        send2.closed().await;
    });
    let third_client = tokio::spawn(async move {
        // this should fail with an io error, since the stream was stopped
        loop {
            match send3.send(vec![1, 2, 3]).await {
                Err(SendError::Io { source, .. }) if source.kind() == ErrorKind::BrokenPipe => {
                    break;
                }
                _ => {}
            };
        }
    });
    // send until we get an error because the remote side stopped the stream
    while send1.send(vec![1, 2, 3]).await.is_ok() {}
    match send1.send(vec![4, 5, 6]).await {
        Err(SendError::Io { source, .. }) if source.kind() == ErrorKind::BrokenPipe => {}
        e => panic!("Expected SendError::Io with kind BrokenPipe, got {e:?}"),
    };
    // check that closed signal was received by the second sender
    second_client.await?;
    // check that the third sender will get the right kind of io error eventually
    third_client.await?;
    // server should finish without errors
    server.await??;
    Ok(())
}

/// Checks that all clones of a `Sender` will get the closed signal as soon as
/// a send future gets dropped before completing.
#[tokio::test]
async fn mpsc_sender_clone_drop_error() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    // accept a single bidi stream on a single connection, then read indefinitely
    // until we get an error or the stream is finished
    let server = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await?;
        let (_, mut recv) = conn.accept_bi().await?;
        let mut buf = vec![0u8; 1024];
        while let Ok(Some(_)) = recv.read(&mut buf).await {}
        TestResult::Ok(())
    });
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send1 = mpsc::Sender::<Vec<u8>>::from(send);
    let send2 = send1.clone();
    let send3 = send1.clone();
    let second_client = tokio::spawn(async move {
        send2.closed().await;
    });
    let third_client = tokio::spawn(async move {
        // this should fail with an io error, since the stream was stopped
        loop {
            match send3.send(vec![1, 2, 3]).await {
                Err(SendError::Io { source, .. }) if source.kind() == ErrorKind::BrokenPipe => {
                    break;
                }
                _ => {}
            };
        }
    });
    // send a lot of data with a tiny timeout, this will cause the send future to be dropped
    loop {
        let send_future = send1.send(vec![0u8; 1024 * 1024]);
        // not sure if there is a better way. I want to poll the future a few times so it has time to
        // start sending, but don't want to give it enough time to complete.
        // I don't think now_or_never would work, since it wouldn't have time to start sending
        if timeout(Duration::from_micros(1), send_future)
            .await
            .is_err()
        {
            break;
        }
    }
    server.await??;
    second_client.await?;
    third_client.await?;
    Ok(())
}

/// Checks that a sender gets `ReceiverClosed` if the remote receiver is dropped, as for a local one.
#[tokio::test]
async fn mpsc_receiver_dropped() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await?;
        let (_, recv) = conn.accept_bi().await?;
        drop(Receiver::<Vec<u8>>::from(recv));
        // keep the connection, so that the stop reaches the client
        conn.closed().await;
        TestResult::Ok(())
    });
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send = mpsc::Sender::<Vec<u8>>::from(send);
    let res = timeout(Duration::from_secs(5), async {
        loop {
            if let Err(err) = send.send(vec![1, 2, 3]).await {
                break err;
            }
        }
    })
    .await?;
    assert!(
        matches!(res, SendError::ReceiverClosed { .. }),
        "expected ReceiverClosed, got {res:?}"
    );
    conn.close(0u32.into(), b"");
    server.await??;
    Ok(())
}

/// Checks that a clone that waits for `closed` does not block sends on other clones.
#[tokio::test]
async fn mpsc_closed_does_not_block_send() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await?;
        let (_, recv) = conn.accept_bi().await?;
        let mut recv = Receiver::<Vec<u8>>::from(recv);
        assert_eq!(recv.recv().await?, Some(vec![1, 2, 3]));
        drop(recv);
        conn.closed().await;
        TestResult::Ok(())
    });
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send1 = mpsc::Sender::<Vec<u8>>::from(send);
    let send2 = send1.clone();
    let closed = tokio::spawn(async move { send1.closed().await });
    // let the first clone wait for the close
    tokio::time::sleep(Duration::from_millis(10)).await;
    timeout(Duration::from_secs(5), send2.send(vec![1, 2, 3])).await??;
    // the receiver drops after the message, which ends the wait of the first clone
    timeout(Duration::from_secs(5), closed).await??;
    conn.close(0u32.into(), b"");
    server.await??;
    Ok(())
}

/// Checks that after the remote receiver is dropped, every send on every clone
/// of a `Sender` gets `ReceiverClosed`, also on clones that did not see the stop.
#[tokio::test]
async fn mpsc_receiver_dropped_clones() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await?;
        let (_, recv) = conn.accept_bi().await?;
        drop(Receiver::<Vec<u8>>::from(recv));
        // keep the connection, so that the stop reaches the client
        conn.closed().await;
        TestResult::Ok(())
    });
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send1 = mpsc::Sender::<Vec<u8>>::from(send);
    let send2 = send1.clone();
    // send on the first clone until the stop arrives
    let err = timeout(Duration::from_secs(5), async {
        loop {
            if let Err(err) = send1.send(vec![1, 2, 3]).await {
                break err;
            }
        }
    })
    .await?;
    assert!(
        matches!(err, SendError::ReceiverClosed { .. }),
        "expected ReceiverClosed, got {err:?}"
    );
    // later sends on the same clone, and sends on a clone that never saw the stop
    assert!(matches!(
        send1.send(vec![1]).await,
        Err(SendError::ReceiverClosed { .. })
    ));
    assert!(matches!(
        send2.send(vec![1]).await,
        Err(SendError::ReceiverClosed { .. })
    ));
    assert!(matches!(
        send2.try_send(vec![1]).await,
        Err(SendError::ReceiverClosed { .. })
    ));
    conn.close(0u32.into(), b"");
    server.await??;
    Ok(())
}

/// Check that `ReceiverClosed` works well with the `Handler` as well.
#[tokio::test]
async fn mpsc_receiver_dropped_proto() -> TestResult<()> {
    #[rpc_requests(message = ProtoMessage)]
    #[derive(Debug, Serialize, Deserialize)]
    enum Proto {
        #[rpc(tx = oneshot::Sender<()>, rx = mpsc::Receiver<u32>)]
        #[wrap(ClientStream)]
        ClientStream(()),
    }
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await.unwrap();
        Handler::<Proto>::sequential(|msg| async move {
            match msg {
                ProtoMessage::ClientStream(msg) => {
                    let WithChannels { tx, mut rx, .. } = msg;
                    tx.send(()).await.unwrap();
                    let n = rx.recv().await.unwrap().unwrap();
                    assert_eq!(n, 1);
                    drop(rx);
                }
            }
            Ok(())
        })
        .handle_connection(&conn)
        .await
        .unwrap();
    });
    let client = Client::<Proto>::noq(client, server_addr);
    let (tx, rx) = client.client_streaming(ClientStream(()), 4).await.unwrap();
    assert!(matches!(rx.await.unwrap(), ()));

    // First send works.
    let res = tx.send(1).await;
    assert!(matches!(res, Ok(())));

    // As soon as the Stopped frame arrives, our sends fail.
    n0_future::time::timeout(Duration::from_secs(1), async {
        loop {
            let res = tx.send(2).await;
            if matches!(res, Err(SendError::ReceiverClosed { .. })) {
                break;
            }
        }
    })
    .await
    .unwrap();

    drop(client);
    server.await.unwrap();
    Ok(())
}

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
    let mut recv = Receiver::<Vec<u8>>::from(recv);
    while recv.recv().await?.is_some() {}
    Err(e!(RecvError::Io, io::ErrorKind::UnexpectedEof.into()))
}

/// Checks that the max message size is enforced on the sender side and that errors are propagated to the receiver side.
#[tokio::test]
async fn mpsc_max_message_size_send() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(vec_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send = mpsc::Sender::<Vec<u8>>::from(send);
    // this one should work!
    send.send(vec![0u8; 1024 * 1024]).await?;
    // this one should fail!
    let Err(cause) = send.send(vec![0u8; 1024 * 1024 * 32]).await else {
        panic!("client should have failed due to max message size");
    };
    assert!(matches!(cause, SendError::MaxSizeExceeded { .. }));
    // later sends get a broken pipe, also for a message that is small enough
    assert!(matches!(
        send.send(vec![0u8; 1]).await,
        Err(SendError::Io { source, .. }) if source.kind() == io::ErrorKind::BrokenPipe
    ));
    let Err(cause) = server.await? else {
        panic!("server should have failed due to max message size");
    };
    assert_eq!(cause.remote_code(), Some(ErrorCode::MessageMaxSizeExceeded));
    Ok(())
}

/// Checks that `try_send` resets the stream for a message that is too large, as `send` does.
#[tokio::test]
async fn mpsc_max_message_size_try_send() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(vec_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send = mpsc::Sender::<Vec<u8>>::from(send);
    let Err(cause) = send.try_send(vec![0u8; 1024 * 1024 * 32]).await else {
        panic!("client should have failed due to max message size");
    };
    assert!(matches!(cause, SendError::MaxSizeExceeded { .. }));
    let Err(cause) = server.await? else {
        panic!("server should have failed due to max message size");
    };
    assert_eq!(cause.remote_code(), Some(ErrorCode::MessageMaxSizeExceeded));
    Ok(())
}

/// Checks that the max message size is enforced on receiver side.
#[tokio::test]
async fn mpsc_max_message_size_recv() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(vec_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    // this one should work!
    send.write_length_prefixed(vec![0u8; 1024 * 1024]).await?;
    // this one should fail on receive!
    send.write_length_prefixed(vec![0u8; 1024 * 1024 * 32])
        .await
        .ok();
    let Err(cause) = server.await? else {
        panic!("server should have failed due to max message size");
    };
    assert!(matches!(cause, mpsc::RecvError::MaxSizeExceeded { .. }));
    Ok(())
}

async fn noser_receiver(server: Endpoint) -> Result<(), mpsc::RecvError> {
    let conn = server
        .accept()
        .await
        .unwrap()
        .await
        .map_err(|err| e!(mpsc::RecvError::Io, err.into()))?;
    let (_, recv) = conn
        .accept_bi()
        .await
        .map_err(|err| e!(mpsc::RecvError::Io, err.into()))?;
    let mut recv = mpsc::Receiver::<NoSer>::from(recv);
    while recv.recv().await?.is_some() {}
    Err(e!(mpsc::RecvError::Io, io::ErrorKind::UnexpectedEof.into()))
}

/// Checks that a serialization error is caught and propagated to the receiver.
#[tokio::test]
async fn mpsc_serialize_error_send() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(noser_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send = mpsc::Sender::<NoSer>::from(send);
    // this one should work!
    send.send(NoSer(0)).await?;
    // this one should fail!
    let Err(cause) = send.send(NoSer(1)).await else {
        panic!("client should have failed due to serialization error");
    };
    assert!(matches!(cause, SendError::EncodeFailed { .. }), "{cause:?}");
    let Err(cause) = server.await? else {
        panic!("server should have failed due to serialization error");
    };
    assert_eq!(cause.remote_code(), Some(ErrorCode::MessageEncodeFailed));
    Ok(())
}

#[tokio::test]
async fn mpsc_serialize_error_recv() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(noser_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    // this one should work!
    send.write_length_prefixed(0u64).await?;
    // this one should fail on receive!
    send.write_length_prefixed(1u64).await.ok();
    let Err(cause) = server.await? else {
        panic!("server should have failed due to serialization error");
    };
    assert!(
        matches!(cause, mpsc::RecvError::DecodeFailed { .. }),
        "{cause:?}"
    );
    Ok(())
}

/// Receives a message that does not decode, and keeps the connection until the client closes it.
async fn noser_receiver_kept(server: Endpoint) -> TestResult<()> {
    let conn = server.accept().await.unwrap().await?;
    let (_, recv) = conn.accept_bi().await?;
    let mut recv = Receiver::<NoSer>::from(recv);
    assert!(recv.recv().await.is_err());
    // keep the connection, so that the stop reaches the client
    conn.closed().await;
    Ok(())
}

/// Checks that a sender gets `MessageDecodeFailed` from the remote receiver if it cannot decode a message.
#[tokio::test]
async fn mpsc_decode_error_reaches_sender() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(noser_receiver_kept(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    // An odd number encodes as `u64`, but does not decode as `NoSer`.
    let send = mpsc::Sender::<u64>::from(send);
    let clone = send.clone();
    let err = timeout(Duration::from_secs(5), async {
        loop {
            if let Err(err) = send.send(1).await {
                break err;
            }
        }
    })
    .await?;
    let decode_failed = Some(ErrorCode::MessageDecodeFailed);
    assert_eq!(err.remote_code(), decode_failed);
    // later sends, also on a clone that did not see the stop
    assert_eq!(send.send(2).await.unwrap_err().remote_code(), decode_failed);
    assert_eq!(
        clone.try_send(2).await.unwrap_err().remote_code(),
        decode_failed
    );
    conn.close(0u32.into(), b"");
    server.await??;
    Ok(())
}

/// Checks that a receiver stops the stream with `MessageDecodeFailed` if a message does not decode.
#[tokio::test]
async fn mpsc_decode_error_stops_stream() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(noser_receiver_kept(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    // an odd number does not decode as `NoSer`
    send.write_length_prefixed(1u64).await?;
    let code = timeout(Duration::from_secs(5), send.stopped()).await??;
    assert_eq!(code, Some(ErrorCode::MessageDecodeFailed.into()));
    conn.close(0u32.into(), b"");
    server.await??;
    Ok(())
}

/// Checks that a receive cancelled in the middle of a message keeps the part it read, so that the
/// next receive returns the whole message.
#[tokio::test]
async fn mpsc_receive_resumes_a_message_read_in_part() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let message = vec![7u8; 1000];
    let mut frame = Vec::new();
    WriteVarintExt::write_length_prefixed(&mut frame, &message)?;
    let (first, rest) = frame.split_at(frame.len() / 2);
    let (half_read, rest_wanted) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await?;
        let (_, recv) = conn.accept_bi().await?;
        let mut recv = Receiver::<Vec<u8>>::from(recv);
        assert!(
            timeout(Duration::from_millis(200), recv.recv())
                .await
                .is_err()
        );
        half_read.send(()).unwrap();
        TestResult::Ok(recv.recv().await?)
    });
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    send.write_all(first).await?;
    rest_wanted.await?;
    send.write_all(rest).await?;
    send.finish()?;
    assert_eq!(server.await??, Some(message));
    Ok(())
}

/// Checks that a receiver reads as a stream of its messages, ending when the sender drops.
#[tokio::test]
async fn mpsc_receiver_is_a_stream() -> TestResult<()> {
    use n0_future::StreamExt;

    let (send, recv) = tokio::sync::mpsc::channel(4);
    let recv = Receiver::<u8>::from(recv);
    for value in 1..=3 {
        send.send(value).await?;
    }
    drop(send);
    let received: Vec<Result<u8, RecvError>> = recv.collect().await;
    let received: Vec<u8> = received.into_iter().map(Result::unwrap).collect();
    assert_eq!(received, [1, 2, 3]);
    Ok(())
}

/// Checks that a receiver splits messages of mixed sizes, written back to back, into the right
/// messages, while its buffer grows for the large ones and shrinks back after.
#[tokio::test]
async fn mpsc_receive_mixed_sizes() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let messages: Vec<Vec<u8>> = [
        10, 120, 3, 124, 125, 126, 127, 5000, 0, 200, 300, 1, 100_000, 7,
    ]
    .into_iter()
    .enumerate()
    .map(|(i, len)| vec![i as u8; len])
    .collect();
    let mut frames = Vec::new();
    for message in &messages {
        WriteVarintExt::write_length_prefixed(&mut frames, message)?;
    }
    let server = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await?;
        let (_, recv) = conn.accept_bi().await?;
        let mut recv = Receiver::<Vec<u8>>::from(recv);
        let mut received = Vec::new();
        while let Some(message) = recv.recv().await? {
            received.push(message);
        }
        TestResult::Ok(received)
    });
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    send.write_all(&frames).await?;
    send.finish()?;
    assert_eq!(server.await??, messages);
    Ok(())
}
