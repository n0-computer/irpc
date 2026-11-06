//! Checks mpsc channels against the framing of irpc 0.17.0, the last release before the mpsc end
//! marker.
#![cfg(feature = "noq_endpoint_setup")]

use irpc::channel::mpsc;
use n0_future::future::poll_once;
use n0_tracing_test::traced_test;
use testresult::TestResult;

#[allow(dead_code, reason = "this test uses only some of the helpers")]
mod common;
use common::create_connected_endpoints;

/// The mpsc framing of irpc 0.17.0, copied from its source with errors simplified.
///
/// Do not change this to match the current code, it stands for peers that are already deployed.
mod old {
    use std::io;

    use serde::{Serialize, de::DeserializeOwned};
    use tokio::io::{AsyncRead, AsyncReadExt};

    const MAX_MESSAGE_SIZE: u64 = 1024 * 1024 * 16;

    /// `irpc::util::read_varint_u64` in 0.17.0.
    async fn read_varint_u64<R>(reader: &mut R) -> io::Result<Option<u64>>
    where
        R: AsyncRead + Unpin,
    {
        let mut result: u64 = 0;
        let mut shift: u32 = 0;

        loop {
            if shift >= 64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Varint is too large for u64",
                ));
            }

            let res = reader.read_u8().await;
            if shift == 0
                && let Err(cause) = res
            {
                if cause.kind() == io::ErrorKind::UnexpectedEof {
                    return Ok(None);
                } else {
                    return Err(cause);
                }
            }

            let byte = res?;
            let value = (byte & 0x7F) as u64;
            result |= value << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }

        Ok(Some(result))
    }

    /// `irpc::util::write_varint_u64_sync` in 0.17.0.
    fn write_varint_u64_sync<W: std::io::Write>(writer: &mut W, value: u64) -> io::Result<usize> {
        if value == 0 {
            writer.write_all(&[0])?;
            return Ok(1);
        }

        let mut bytes_written = 0;
        let mut remaining = value;

        while remaining > 0 {
            let mut byte = (remaining & 0x7F) as u8;
            remaining >>= 7;
            if remaining > 0 {
                byte |= 0x80;
            }
            writer.write_all(&[byte])?;
            bytes_written += 1;
        }

        Ok(bytes_written)
    }

    /// `irpc::util::write_length_prefixed` in 0.17.0.
    fn write_length_prefixed<T: Serialize>(
        mut write: impl std::io::Write,
        value: T,
    ) -> io::Result<()> {
        let size = postcard::experimental::serialized_size(&value)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))? as u64;
        write_varint_u64_sync(&mut write, size)?;
        postcard::to_io(&value, &mut write)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(())
    }

    /// `NoqReceiver::recv` in 0.17.0.
    pub async fn recv<T: DeserializeOwned>(read: &mut noq::RecvStream) -> io::Result<Option<T>> {
        let Some(size) = read_varint_u64(read).await? else {
            return Ok(None);
        };
        if size > MAX_MESSAGE_SIZE {
            return Err(io::Error::other("max message size exceeded"));
        }
        let mut buf = vec![0; size as usize];
        read.read_exact(&mut buf)
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::UnexpectedEof, e))?;
        let msg: T = postcard::from_bytes(&buf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Some(msg))
    }

    /// `NoqSenderInner::send` in 0.17.0.
    pub async fn send<T: Serialize>(send: &mut noq::SendStream, value: T) -> io::Result<()> {
        let mut buffer = Vec::new();
        write_length_prefixed(&mut buffer, value)?;
        send.write_all(&buffer).await?;
        Ok(())
    }
}

/// Checks that an old receiver reads the messages of a new sender, including a cancelled one.
///
/// The old receiver does not know the end marker, so it delivers the cancelled message whole.
#[tokio::test]
#[traced_test]
async fn mpsc_new_sender_old_receiver() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await?;
        let (_, mut recv) = conn.accept_bi().await?;
        let mut msgs = Vec::new();
        while let Some(msg) = old::recv::<Vec<u8>>(&mut recv).await? {
            msgs.push(msg);
        }
        TestResult::Ok(msgs)
    });
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (send, _) = conn.open_bi().await?;
    let send = mpsc::Sender::<Vec<u8>>::from(send);
    send.send(vec![7]).await?;
    let mut fut = Box::pin(send.send(vec![9u8; 8 * 1024 * 1024]));
    assert!(poll_once(&mut fut).await.is_none(), "the write is not done");
    drop(fut);
    send.send(vec![8]).await?;
    drop(send);
    let msgs = server.await??;
    assert_eq!(msgs, vec![vec![7], vec![9u8; 8 * 1024 * 1024], vec![8]]);
    Ok(())
}

/// Checks that a new receiver reads the messages of an old sender.
#[tokio::test]
#[traced_test]
async fn mpsc_old_sender_new_receiver() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await?;
        let (_, recv) = conn.accept_bi().await?;
        let mut recv = mpsc::Receiver::<Vec<u8>>::from(recv);
        let mut msgs = Vec::new();
        while let Some(msg) = recv.recv().await? {
            msgs.push(msg);
        }
        TestResult::Ok(msgs)
    });
    let conn = client.connect(server_addr, "localhost")?.await?;
    let (mut send, _) = conn.open_bi().await?;
    old::send(&mut send, vec![7u8]).await?;
    old::send(&mut send, Vec::<u8>::new()).await?;
    old::send(&mut send, vec![8u8; 1024 * 1024]).await?;
    send.finish()?;
    let msgs = server.await??;
    assert_eq!(msgs, vec![vec![7], vec![], vec![8; 1024 * 1024]]);
    Ok(())
}
