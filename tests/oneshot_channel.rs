use std::{
    io::{self, ErrorKind},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
};

use irpc::{
    channel::{oneshot, RecvError, SendError},
    util::{make_client_endpoint, make_server_endpoint, AsyncWriteVarintExt},
};
use quinn::Endpoint;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use testresult::TestResult;
use tokio::task::JoinHandle;

mod common;
use common::*;

#[derive(Debug)]
struct NoSer(u64);

#[derive(Debug, thiserror::Error)]
#[error("Cannot serialize odd number: {0}")]
struct OddNumberError(u64);

impl Serialize for NoSer {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if self.0 % 2 == 1 {
            Err(serde::ser::Error::custom(OddNumberError(self.0)))
        } else {
            serializer.serialize_u64(self.0)
        }
    }
}

impl<'de> Deserialize<'de> for NoSer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u64::deserialize(deserializer)?;
        if value % 2 != 0 {
            Err(serde::de::Error::custom(OddNumberError(value)))
        } else {
            Ok(NoSer(value))
        }
    }
}

/// Checks that the max message size is enforced on the sender side and that errors are propagated to the receiver side.
#[tokio::test]
async fn oneshot_max_message_size_send() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server: JoinHandle<Result<(), RecvError>> = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await.map_err(|e| RecvError::Io(e.into()))?;
        let (_, recv) = conn.accept_bi().await.map_err(|e| RecvError::Io(e.into()))?;
        let recv = oneshot::Receiver::<Vec<u8>>::from(recv);
        recv.await?;
        return Err(RecvError::Io(io::ErrorKind::UnexpectedEof.into()));
    });
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send = oneshot::Sender::<Vec<u8>>::from(send);
    // this one should fail!
    let Err(cause) = send.send(vec![0u8; 1024 * 1024 * 32]).await else {
        panic!("client should have failed due to max message size");
    };
    assert!(matches!(cause, SendError::MaxMessageSizeExceeded));
    let Err(cause) = server.await? else {
        panic!("server should have failed due to max message size");
    };
    assert!(matches!(cause, RecvError::Io(e) if e.kind() == ErrorKind::ConnectionReset));
    Ok(())
}

/// Checks that the max message size is enforced on receiver side.
#[tokio::test]
async fn oneshot_max_message_size_recv() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server: JoinHandle<Result<(), RecvError>> = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await.map_err(|e| RecvError::Io(e.into()))?;
        let (_, recv) = conn.accept_bi().await.map_err(|e| RecvError::Io(e.into()))?;
        let recv = oneshot::Receiver::<Vec<u8>>::from(recv);
        recv.await?;
        return Err(RecvError::Io(io::ErrorKind::UnexpectedEof.into()));
    });
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    // this one should fail on receive!
    send.write_length_prefixed(vec![0u8; 1024 * 1024 * 32]).await.ok();
    let Err(cause) = server.await? else {
        panic!("server should have failed due to max message size");
    };
    assert!(matches!(cause, RecvError::MaxMessageSizeExceeded));
    Ok(())
}

/// Checks that trying to send a message that cannot be serialized results in an error on the sender side and a connection reset on the receiver side.
#[tokio::test]
async fn oneshot_serialize_error_send() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server: JoinHandle<Result<(), RecvError>> = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await.map_err(|e| RecvError::Io(e.into()))?;
        let (_, recv) = conn.accept_bi().await.map_err(|e| RecvError::Io(e.into()))?;
        let recv = oneshot::Receiver::<NoSer>::from(recv);
        recv.await?;
        return Err(RecvError::Io(io::ErrorKind::UnexpectedEof.into()));
    });
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send = oneshot::Sender::<NoSer>::from(send);
    // this one should fail!
    let Err(cause) = send.send(NoSer(1)).await else {
        panic!("client should have failed due to serialization error");
    };
    assert!(matches!(cause, SendError::Io(e) if e.kind() == ErrorKind::InvalidData));
    let Err(cause) = server.await? else {
        panic!("server should have failed due to serialization error");
    };
    println!("Server error: {:?}", cause);
    assert!(matches!(cause, RecvError::Io(e) if e.kind() == ErrorKind::ConnectionReset));
    Ok(())
}

#[tokio::test]
async fn oneshot_serialize_error_recv() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server: JoinHandle<Result<(), RecvError>> = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await.map_err(|e| RecvError::Io(e.into()))?;
        let (_, recv) = conn.accept_bi().await.map_err(|e| RecvError::Io(e.into()))?;
        let recv = oneshot::Receiver::<NoSer>::from(recv);
        recv.await?;
        return Err(RecvError::Io(io::ErrorKind::UnexpectedEof.into()));
    });
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    // this one should fail on receive!
    send.write_length_prefixed(1u64).await?;
    send.finish()?;
    let Err(cause) = server.await? else {
        panic!("server should have failed due to serialization error");
    };
    println!("Server error: {:?}", cause);
    assert!(matches!(cause, RecvError::Io(e) if e.kind() == ErrorKind::InvalidData));
    Ok(())
}