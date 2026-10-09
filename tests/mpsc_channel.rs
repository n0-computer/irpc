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
    util::AsyncWriteVarintExt,
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
    assert!(matches!(cause, SendError::MaxMessageSizeExceeded { .. }));
    // later sends get the same error, also for a message that is small enough
    assert!(matches!(
        send.send(vec![0u8; 1]).await,
        Err(SendError::MaxMessageSizeExceeded { .. })
    ));
    let Err(cause) = server.await? else {
        panic!("server should have failed due to max message size");
    };
    assert!(matches!(
        cause,
        mpsc::RecvError::MaxMessageSizeExceeded { .. }
    ));
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
    assert!(matches!(cause, SendError::MaxMessageSizeExceeded { .. }));
    let Err(cause) = server.await? else {
        panic!("server should have failed due to max message size");
    };
    assert!(matches!(
        cause,
        mpsc::RecvError::MaxMessageSizeExceeded { .. }
    ));
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
    assert!(matches!(
        cause,
        mpsc::RecvError::MaxMessageSizeExceeded { .. }
    ));
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
    assert!(matches!(cause, mpsc::RecvError::EncodeFailed { .. }));
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

/// Checks that a sender gets `DecodeFailed` if the remote receiver cannot decode a message.
#[tokio::test]
async fn mpsc_decode_error_remote_decode_failed() -> TestResult<()> {
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
    assert!(matches!(err, SendError::DecodeFailed { .. }), "{err:?}");
    // later sends, also on a clone that did not see the stop
    assert!(matches!(
        send.send(2).await,
        Err(SendError::DecodeFailed { .. })
    ));
    assert!(matches!(
        clone.try_send(2).await,
        Err(SendError::DecodeFailed { .. })
    ));
    conn.close(0u32.into(), b"");
    server.await??;
    Ok(())
}

/// Checks that a receiver stops the stream with code 3 if a message does not decode.
#[tokio::test]
async fn mpsc_decode_error_stops_stream() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(noser_receiver_kept(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    // an odd number does not decode as `NoSer`
    send.write_length_prefixed(1u64).await?;
    let code = timeout(Duration::from_secs(5), send.stopped()).await??;
    assert_eq!(code, Some(ErrorCode::DecodeFailed.into()));
    conn.close(0u32.into(), b"");
    server.await??;
    Ok(())
}
