#![cfg(feature = "noq_endpoint_setup")]

use std::{io, time::Duration};

use irpc::noq::{configure_client, make_server_endpoint};
use irpc::{
    Client, RpcMessage, WithChannels,
    channel::{
        SendError,
        mpsc::{self, Receiver, RecvError},
        oneshot,
    },
    noq::{configure_server, make_client_endpoint},
    rpc::{ErrorCode, Handler, MAX_MESSAGE_SIZE, RpcRecvError, RpcSendError},
    rpc_requests,
    util::{AsyncWriteVarintExt, WriteVarintExt},
};
use n0_error::e;
use n0_future::future::poll_once;
use n0_tracing_test::traced_test;
use noq::Endpoint;
use serde::{Deserialize, Serialize};
use testresult::TestResult;
use tokio::time::timeout;

/// The marker after a message in its frame, for a message that the receiver delivers.
const COMPLETE: u8 = 0;

mod common;
use common::*;

/// Checks that all clones of a `Sender` will get the closed signal as soon as
/// a send fails because the receiver ended the channel.
#[tokio::test]
#[traced_test]
async fn mpsc_sender_clone_closed_error() -> TestResult<()> {
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
        // this fails eventually, since the server stops the stream and closes the connection
        while !matches!(
            send3.send(vec![1, 2, 3]).await,
            Err(SendError::Rpc {
                source: RpcSendError::Remote { .. } | RpcSendError::ConnectionClosed { .. },
                ..
            })
        ) {}
    });
    // send until we get an error because the remote side stopped the stream
    while send1.send(vec![1, 2, 3]).await.is_ok() {}
    // the server stops the stream with code 1 and then closes the connection, so a
    // later send gets one of these, not an io error
    match send1.send(vec![4, 5, 6]).await {
        Err(SendError::Rpc {
            source: RpcSendError::Remote { code, .. },
            ..
        }) => assert_eq!(code, ErrorCode::RequestAborted),
        Err(SendError::Rpc {
            source: RpcSendError::ConnectionClosed { close, .. },
            ..
        }) => assert_eq!(close.code(), None),
        e => panic!("Expected Remote or ConnectionClosed, got {e:?}"),
    };
    // check that closed signal was received by the second sender
    second_client.await?;
    // check that the third sender will get the right kind of io error eventually
    third_client.await?;
    // server should finish without errors
    server.await??;
    Ok(())
}

/// Checks that sends cancelled at any point leave whole messages, and the sender and its clones usable.
#[tokio::test]
#[traced_test]
async fn mpsc_cancelled_sends_keep_clones_usable() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(collect_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send1 = mpsc::Sender::<Vec<u8>>::from(send);
    let send2 = send1.clone();
    let mut sent = Vec::new();
    let mut cancelled = 0;
    for id in 0..20u32 {
        // Each message starts with its id, so that the receiver can tell them apart.
        let mut msg = vec![1u8; 1024 * 1024];
        msg[..4].copy_from_slice(&id.to_le_bytes());
        // A tiny timeout cancels the send before, while, or after it writes.
        match timeout(Duration::from_micros(1), send1.send(msg.clone())).await {
            Ok(res) => {
                res?;
                sent.push(msg);
            }
            Err(_) => cancelled += 1,
        }
    }
    assert!(cancelled > 0, "no send was cancelled");
    send2.send(vec![2]).await?;
    sent.push(vec![2]);
    drop((send1, send2));
    // Exactly the sends that completed arrive, in order.
    assert_eq!(server.await??, sent);
    Ok(())
}

/// Checks that a send that is cancelled in the middle of the message is not delivered, and the channel keeps working.
#[tokio::test]
#[traced_test]
async fn mpsc_partly_cancelled_send_is_not_delivered() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(collect_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send = mpsc::Sender::<Vec<u8>>::from(send);
    send.send(vec![7]).await?;
    // The message is larger than the flow control window, so one poll cannot write all of it.
    let mut fut = Box::pin(send.send(vec![9u8; 8 * 1024 * 1024]));
    assert!(poll_once(&mut fut).await.is_none(), "the write is not done");
    drop(fut);
    send.send(vec![8]).await?;
    drop(send);
    assert_eq!(server.await??, vec![vec![7], vec![8]]);
    Ok(())
}

/// Checks that a `try_send` that is cancelled in the middle of the message is not delivered, and the channel keeps working.
#[tokio::test]
#[traced_test]
async fn mpsc_partly_cancelled_try_send_is_not_delivered() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(collect_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send = mpsc::Sender::<Vec<u8>>::from(send);
    send.send(vec![7]).await?;
    // The message is larger than the flow control window, so `try_send` writes
    // the first part and then waits for the rest.
    let mut fut = Box::pin(send.try_send(vec![9u8; 8 * 1024 * 1024]));
    assert!(poll_once(&mut fut).await.is_none(), "the write is not done");
    drop(fut);
    send.send(vec![8]).await?;
    drop(send);
    assert_eq!(server.await??, vec![vec![7], vec![8]]);
    Ok(())
}

/// Checks that sends cancelled while they write the rest of an earlier cancelled message deliver nothing.
#[tokio::test]
#[traced_test]
async fn mpsc_cancelled_while_finishing_an_aborted_message() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(collect_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send = mpsc::Sender::<Vec<u8>>::from(send);
    send.send(vec![7]).await?;
    let mut fut = Box::pin(send.send(vec![9u8; 8 * 1024 * 1024]));
    assert!(poll_once(&mut fut).await.is_none(), "the write is not done");
    drop(fut);
    // Each of these sends first writes the rest of the aborted message, and is
    // cancelled while it does.
    for id in 0..3u8 {
        let mut fut = Box::pin(send.send(vec![id; 8 * 1024 * 1024]));
        assert!(poll_once(&mut fut).await.is_none(), "the write is not done");
        drop(fut);
    }
    send.send(vec![8]).await?;
    drop(send);
    assert_eq!(server.await??, vec![vec![7], vec![8]]);
    Ok(())
}

/// A message that the sender encodes, but that the receiver rejects if it starts with `0xff`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Vec<u8>", into = "Vec<u8>")]
struct Checked(Vec<u8>);

impl TryFrom<Vec<u8>> for Checked {
    type Error = &'static str;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        match value.first() {
            Some(0xff) => Err("rejected"),
            _ => Ok(Self(value)),
        }
    }
}

impl From<Checked> for Vec<u8> {
    fn from(value: Checked) -> Self {
        value.0
    }
}

/// Checks that a cancelled message is dropped even if the receiver would reject it.
#[tokio::test]
#[traced_test]
async fn mpsc_cancelled_send_that_does_not_decode_is_dropped() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(collect::<Checked>(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send = mpsc::Sender::<Checked>::from(send);
    send.send(Checked(vec![7])).await?;
    let mut fut = Box::pin(send.send(Checked(vec![0xff; 8 * 1024 * 1024])));
    assert!(poll_once(&mut fut).await.is_none(), "the write is not done");
    drop(fut);
    send.send(Checked(vec![8])).await?;
    drop(send);
    assert_eq!(server.await??, vec![Checked(vec![7]), Checked(vec![8])]);
    Ok(())
}

/// Checks that a partly cancelled send is not delivered if the sender is dropped right after.
///
/// The stream then ends in the middle of the message, which the receiver gets
/// as the end of the stream after the last complete message.
#[tokio::test]
#[traced_test]
async fn mpsc_partly_cancelled_send_is_dropped_with_sender() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(collect_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send = mpsc::Sender::<Vec<u8>>::from(send);
    send.send(vec![7]).await?;
    let mut fut = Box::pin(send.send(vec![9u8; 8 * 1024 * 1024]));
    assert!(poll_once(&mut fut).await.is_none(), "the write is not done");
    drop(fut);
    drop(send);
    assert_eq!(server.await??, vec![vec![7]]);
    Ok(())
}

/// Checks that a send that is cancelled before it wrote anything is not sent, as for a local channel.
#[tokio::test]
#[traced_test]
async fn mpsc_unstarted_cancelled_send_is_not_sent() -> TestResult<()> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
    // A small window, so that the stream is full after a few messages.
    let (mut config, cert) = configure_server()?;
    std::sync::Arc::get_mut(&mut config.transport)
        .unwrap()
        .stream_receive_window(64u32.into());
    let server = noq::Endpoint::server(config, addr)?;
    let server_addr = server.local_addr()?;
    let client = make_client_endpoint(addr, &[cert.as_slice()])?;
    let (start_reading, reading) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await?;
        let (_, recv) = conn.accept_bi().await?;
        reading.await?;
        let mut recv = Receiver::<()>::from(recv);
        let mut received = 0;
        while recv.recv().await?.is_some() {
            received += 1;
        }
        TestResult::Ok(received)
    });
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send = mpsc::Sender::<()>::from(send);
    // The frame of `()` is two bytes, size and end marker, so the 64 byte window holds
    // a whole number of frames, and the send that does not fit writes nothing.
    let mut sent = 0;
    loop {
        let mut fut = Box::pin(send.send(()));
        if poll_once(&mut fut).await.is_none() {
            break;
        }
        sent += 1;
    }
    start_reading.send(()).unwrap();
    send.send(()).await?;
    drop(send);
    assert_eq!(server.await??, sent + 1);
    Ok(())
}

/// Checks that a frame with more than the end marker after the message is `DecodeFailed`.
#[tokio::test]
#[traced_test]
async fn mpsc_frame_with_extra_bytes() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(vec_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    send.write_all(&[4, 1, 7, 0, 0]).await?;
    send.finish()?;
    let Err(cause) = server.await? else {
        panic!("the frame has an extra byte");
    };
    assert!(
        matches!(
            cause,
            RecvError::Rpc {
                source: RpcRecvError::DecodeFailed { .. },
                ..
            }
        ),
        "{cause:?}"
    );
    Ok(())
}

/// Checks that a frame whose end marker is not known is `DecodeFailed`.
#[tokio::test]
#[traced_test]
async fn mpsc_bad_message_end() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(vec_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    send.write_all(&[3, 1, 7, 5]).await?;
    send.finish()?;
    let Err(cause) = server.await? else {
        panic!("the end byte is bad");
    };
    assert!(
        matches!(
            cause,
            RecvError::Rpc {
                source: RpcRecvError::DecodeFailed { .. },
                ..
            }
        ),
        "{cause:?}"
    );
    Ok(())
}

/// Checks that a sender gets `ReceiverClosed` if the remote receiver is dropped, as for a local one.
#[tokio::test]
#[traced_test]
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
#[traced_test]
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
#[traced_test]
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
#[traced_test]
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

/// Receives `Vec<u8>` messages on the first stream of the first connection, until the end.
async fn collect_receiver(server: Endpoint) -> Result<Vec<Vec<u8>>, RecvError> {
    collect::<Vec<u8>>(server).await
}

/// Receives messages on the first stream of the first connection, until the end.
async fn collect<T: RpcMessage>(server: Endpoint) -> Result<Vec<T>, RecvError> {
    let conn = server
        .accept()
        .await
        .unwrap()
        .await
        .map_err(|err| RecvError::from(e!(RpcRecvError::ConnectionFailed, err.into())))?;
    let (_, recv) = conn
        .accept_bi()
        .await
        .map_err(|err| RecvError::from(e!(RpcRecvError::ConnectionFailed, err.into())))?;
    let mut recv = Receiver::<T>::from(recv);
    let mut msgs = Vec::new();
    while let Some(msg) = recv.recv().await? {
        msgs.push(msg);
    }
    Ok(msgs)
}

async fn vec_receiver(server: Endpoint) -> Result<(), RecvError> {
    let conn = server
        .accept()
        .await
        .unwrap()
        .await
        .map_err(|err| RecvError::from(e!(RpcRecvError::ConnectionFailed, err.into())))?;
    let (_, recv) = conn
        .accept_bi()
        .await
        .map_err(|err| RecvError::from(e!(RpcRecvError::ConnectionFailed, err.into())))?;
    let mut recv = Receiver::<Vec<u8>>::from(recv);
    while recv.recv().await?.is_some() {}
    Err(RecvError::from(e!(
        RpcRecvError::ConnectionFailed,
        io::ErrorKind::UnexpectedEof.into()
    )))
}

/// Checks that a stream that ends in the middle of a message ends after the last complete message.
#[tokio::test]
#[traced_test]
async fn mpsc_truncated_message() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(collect_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    // The message `[7]`, then a size prefix of 100 bytes with only 3 following.
    send.write_all(&[2, 1, 7, 100, 1, 2, 3]).await?;
    send.finish()?;
    assert_eq!(server.await??, vec![vec![7]]);
    Ok(())
}

/// Checks that a stream that ends in the middle of the size prefix ends after the last complete message.
#[tokio::test]
#[traced_test]
async fn mpsc_truncated_size() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(collect_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    // The message `[7]`, then a size prefix whose high bit says that more bytes follow.
    send.write_all(&[2, 1, 7, 0x80]).await?;
    send.finish()?;
    assert_eq!(server.await??, vec![vec![7]]);
    Ok(())
}

/// Checks that a stream that is reset in the middle of a message is an error, not the end of the stream.
#[tokio::test]
#[traced_test]
async fn mpsc_reset_inside_message_is_an_error() -> TestResult<()> {
    // The message `[7]`, then part of a body, or part of a size prefix.
    for partial in [&[2, 1, 7, 100, 1, 2, 3][..], &[2, 1, 7, 0x80]] {
        let (server, client, server_addr) = create_connected_endpoints()?;
        let server = tokio::spawn(collect_receiver(server));
        let conn = client.connect(server_addr, "localhost")?.await?;
        let (mut send, _) = conn.open_bi().await?;
        send.write_all(partial).await?;
        send.reset(noq::VarInt::from_u64(
            ErrorCode::MessageEncodeFailed.to_code(),
        )?)?;
        let Err(cause) = server.await? else {
            panic!("the stream was reset");
        };
        assert_eq!(cause.remote_code(), Some(ErrorCode::MessageEncodeFailed));
    }
    Ok(())
}

/// Checks that a connection that is closed in the middle of a message is an error, not the end of the stream.
#[tokio::test]
#[traced_test]
async fn mpsc_connection_closed_inside_message_is_an_error() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let (first_tx, first_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await?;
        let (_, recv) = conn.accept_bi().await?;
        let mut recv = Receiver::<Vec<u8>>::from(recv);
        first_tx.send(recv.recv().await?).ok();
        TestResult::Ok(recv.recv().await)
    });
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    // The message `[7]`, then part of a body.
    send.write_all(&[2, 1, 7, 100, 1, 2, 3]).await?;
    // Close only after the receiver has the stream.
    assert_eq!(first_rx.await?, Some(vec![7]));
    conn.close(
        noq::VarInt::from_u64(ErrorCode::RequestAborted.to_code())?,
        b"",
    );
    let Err(cause) = server.await?? else {
        panic!("the connection was closed");
    };
    assert_eq!(cause.remote_code(), Some(ErrorCode::RequestAborted));
    Ok(())
}

/// Checks that a size prefix that does not fit in a u64 is `MaxSizeExceeded`, not an io error.
#[tokio::test]
#[traced_test]
async fn mpsc_too_long_size_prefix() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(vec_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    send.write_all(&[0xff; 11]).await?;
    let Err(cause) = server.await? else {
        panic!("the size prefix is too long");
    };
    assert!(
        matches!(
            cause,
            RecvError::Rpc {
                source: RpcRecvError::MaxSizeExceeded { .. },
                ..
            }
        ),
        "{cause:?}"
    );
    Ok(())
}

/// Checks that a reset with code 0 is a closed channel, not an io error.
#[tokio::test]
#[traced_test]
async fn mpsc_reset_with_code_0() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(vec_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    send.reset(0u32.into())?;
    let Err(cause) = server.await? else {
        panic!("the sender reset the stream");
    };
    assert!(
        matches!(
            cause,
            RecvError::Rpc {
                source: RpcRecvError::Remote { code: None, .. },
                ..
            }
        ),
        "{cause:?}"
    );
    Ok(())
}

/// Checks that the max message size is enforced on the sender side and that errors are propagated to the receiver side.
#[tokio::test]
#[traced_test]
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
    assert!(matches!(
        cause,
        SendError::Rpc {
            source: RpcSendError::MaxSizeExceeded { .. },
            ..
        }
    ));
    // later sends fail because the sender is closed, also for a message that is small enough
    assert!(matches!(
        send.send(vec![0u8; 1]).await,
        Err(SendError::Rpc {
            source: RpcSendError::Closed { .. },
            ..
        })
    ));
    let Err(cause) = server.await? else {
        panic!("server should have failed due to max message size");
    };
    assert_eq!(cause.remote_code(), Some(ErrorCode::MessageMaxSizeExceeded));
    Ok(())
}

/// Checks that `try_send` resets the stream for a message that is too large, as `send` does.
#[tokio::test]
#[traced_test]
async fn mpsc_max_message_size_try_send() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(vec_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send = mpsc::Sender::<Vec<u8>>::from(send);
    let Err(cause) = send.try_send(vec![0u8; 1024 * 1024 * 32]).await else {
        panic!("client should have failed due to max message size");
    };
    assert!(matches!(
        cause,
        SendError::Rpc {
            source: RpcSendError::MaxSizeExceeded { .. },
            ..
        }
    ));
    let Err(cause) = server.await? else {
        panic!("server should have failed due to max message size");
    };
    assert_eq!(cause.remote_code(), Some(ErrorCode::MessageMaxSizeExceeded));
    Ok(())
}

/// Checks that the largest mpsc message is one byte smaller than [`MAX_MESSAGE_SIZE`], for the end marker.
#[tokio::test]
#[traced_test]
async fn mpsc_max_message_size_boundary() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await?;
        let (_, recv) = conn.accept_bi().await?;
        let mut recv = Receiver::<Vec<u8>>::from(recv);
        let msg = recv.recv().await?;
        TestResult::Ok(msg.map(|msg| msg.len()))
    });
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send = mpsc::Sender::<Vec<u8>>::from(send);
    // A vec of this length encodes with a 4 byte length, and the end marker takes 1 byte.
    let largest = MAX_MESSAGE_SIZE as usize - 5;
    send.send(vec![0u8; largest]).await?;
    // Wait for the receiver first: the reset for a message that is too large
    // can drop the earlier message before the receiver reads it.
    assert_eq!(server.await??, Some(largest));
    let Err(cause) = send.send(vec![0u8; largest + 1]).await else {
        panic!("one byte more is too large");
    };
    assert!(
        matches!(
            cause,
            SendError::Rpc {
                source: RpcSendError::MaxSizeExceeded { .. },
                ..
            }
        ),
        "{cause:?}"
    );
    Ok(())
}

/// Checks that the max message size is enforced on receiver side.
#[tokio::test]
#[traced_test]
async fn mpsc_max_message_size_recv() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(vec_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    // this one should work!
    send.write_length_prefixed((vec![0u8; 1024 * 1024], COMPLETE))
        .await?;
    // this one should fail on receive!
    send.write_length_prefixed((vec![0u8; 1024 * 1024 * 32], COMPLETE))
        .await
        .ok();
    let Err(cause) = server.await? else {
        panic!("server should have failed due to max message size");
    };
    assert!(matches!(
        cause,
        mpsc::RecvError::Rpc {
            source: RpcRecvError::MaxSizeExceeded { .. },
            ..
        }
    ));
    Ok(())
}

async fn noser_receiver(server: Endpoint) -> Result<(), mpsc::RecvError> {
    let conn = server
        .accept()
        .await
        .unwrap()
        .await
        .map_err(|err| mpsc::RecvError::from(e!(RpcRecvError::ConnectionFailed, err.into())))?;
    let (_, recv) = conn
        .accept_bi()
        .await
        .map_err(|err| mpsc::RecvError::from(e!(RpcRecvError::ConnectionFailed, err.into())))?;
    let mut recv = mpsc::Receiver::<NoSer>::from(recv);
    while recv.recv().await?.is_some() {}
    Err(mpsc::RecvError::from(e!(
        RpcRecvError::ConnectionFailed,
        io::ErrorKind::UnexpectedEof.into()
    )))
}

/// Checks that a serialization error is caught and propagated to the receiver.
#[tokio::test]
#[traced_test]
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
    assert!(
        matches!(
            cause,
            SendError::Rpc {
                source: RpcSendError::EncodeFailed { .. },
                ..
            }
        ),
        "{cause:?}"
    );
    let Err(cause) = server.await? else {
        panic!("server should have failed due to serialization error");
    };
    assert_eq!(cause.remote_code(), Some(ErrorCode::MessageEncodeFailed));
    Ok(())
}

#[tokio::test]
#[traced_test]
async fn mpsc_serialize_error_recv() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(noser_receiver(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    // this one should work!
    send.write_length_prefixed((0u64, COMPLETE)).await?;
    // this one should fail on receive!
    send.write_length_prefixed((1u64, COMPLETE)).await.ok();
    let Err(cause) = server.await? else {
        panic!("server should have failed due to serialization error");
    };
    assert!(
        matches!(
            cause,
            mpsc::RecvError::Rpc {
                source: RpcRecvError::DecodeFailed { .. },
                ..
            }
        ),
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
#[traced_test]
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
#[traced_test]
async fn mpsc_decode_error_stops_stream() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(noser_receiver_kept(server));
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    // an odd number does not decode as `NoSer`
    send.write_length_prefixed((1u64, COMPLETE)).await?;
    let code = timeout(Duration::from_secs(5), send.stopped()).await??;
    let code = code.and_then(|code| ErrorCode::from_code(code.into_inner()));
    assert_eq!(code, Some(ErrorCode::MessageDecodeFailed));
    conn.close(0u32.into(), b"");
    server.await??;
    Ok(())
}

/// Checks that a receive cancelled in the middle of a message keeps the part it read, so that the
/// next receive returns the whole message.
#[tokio::test]
#[traced_test]
async fn mpsc_receive_resumes_a_message_read_in_part() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let message = vec![7u8; 1000];
    let mut frame = Vec::new();
    WriteVarintExt::write_length_prefixed(&mut frame, (&message, COMPLETE))?;
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
#[traced_test]
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
#[traced_test]
async fn mpsc_receive_mixed_sizes() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let messages = mixed_size_messages();
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

/// Messages of sizes around the inline frame buffer, and a few much larger ones.
fn mixed_size_messages() -> Vec<Vec<u8>> {
    [
        10, 120, 3, 124, 125, 126, 127, 5000, 0, 200, 300, 1, 100_000, 7,
    ]
    .into_iter()
    .enumerate()
    .map(|(i, len)| vec![i as u8; len])
    .collect()
}

/// Checks that a sender sends messages of mixed sizes intact, with both `send` and `try_send`,
/// while its buffer grows for the large ones and shrinks back after.
#[tokio::test]
#[traced_test]
async fn mpsc_send_mixed_sizes() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let messages = mixed_size_messages();
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
    let (send, _) = conn.open_bi().await?;
    let send = mpsc::Sender::<Vec<u8>>::from(send);
    for (i, message) in messages.iter().enumerate() {
        if i % 2 == 0 || !send.try_send(message.clone()).await? {
            send.send(message.clone()).await?;
        }
    }
    drop(send);
    assert_eq!(server.await??, messages);
    Ok(())
}

/// Checks that later sends after the connection was lost get the io error again, not `Closed`.
#[tokio::test]
#[traced_test]
async fn mpsc_send_after_connection_lost() -> TestResult<()> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
    // The server runs on its own runtime, so that shutting the runtime down
    // silences the server without a close, and the client times out.
    let server_rt = tokio::runtime::Runtime::new()?;
    let (server, cert) = {
        let _guard = server_rt.enter();
        make_server_endpoint(addr)?
    };
    let server_addr = server.local_addr()?;
    let accepted = server_rt.spawn(async move {
        let conn = server.accept().await.unwrap().await.unwrap();
        let streams = conn.accept_bi().await.unwrap();
        (server, conn, streams)
    });
    let client = noq::Endpoint::client(addr)?;
    let mut config = configure_client(&[cert.as_slice()])?;
    let mut transport = noq::TransportConfig::default();
    transport.max_idle_timeout(Some(Duration::from_millis(200).try_into()?));
    config.transport_config(std::sync::Arc::new(transport));
    let conn = client
        .connect_with(config, server_addr, "localhost")?
        .await?;
    let (mut send, _) = conn.open_bi().await?;
    // The server accepts the stream once it sees data.
    send.write_all(&[2, 1, 7]).await?;
    let send = mpsc::Sender::<Vec<u8>>::from(send);
    let accepted = accepted.await?;
    server_rt.shutdown_background();
    let lost = conn.closed().await;
    assert_eq!(lost, noq::ConnectionError::TimedOut);
    drop(accepted);
    for _ in 0..2 {
        let Err(cause) = send.send(vec![8]).await else {
            panic!("the connection was lost");
        };
        assert!(
            matches!(
                cause,
                SendError::Rpc {
                    source: RpcSendError::ConnectionFailed { .. },
                    ..
                }
            ),
            "{cause:?}"
        );
    }
    Ok(())
}
