//! Checks mpsc channels against irpc 0.17.0, the last release before the mpsc end marker.
#![cfg(feature = "noq_endpoint_setup")]

use irpc::channel::mpsc;
use n0_future::future::poll_once;
use testresult::TestResult;

#[allow(dead_code, reason = "this test uses only some of the helpers")]
mod common;
use common::create_connected_endpoints;

/// Checks that an old receiver reads the messages of a new sender, including a cancelled one.
///
/// The old receiver does not know the end marker, so it delivers the cancelled message whole.
#[tokio::test]
async fn mpsc_new_sender_old_receiver() -> TestResult<()> {
    let (server, client, server_addr) = create_connected_endpoints()?;
    let server = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await?;
        let (_, recv) = conn.accept_bi().await?;
        let mut recv = irpc_old::channel::mpsc::Receiver::<Vec<u8>>::from(recv);
        let mut msgs = Vec::new();
        while let Some(msg) = recv.recv().await? {
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
    let (send, _) = conn.open_bi().await?;
    let send = irpc_old::channel::mpsc::Sender::<Vec<u8>>::from(send);
    send.send(vec![7]).await?;
    send.send(vec![]).await?;
    send.send(vec![8; 1024 * 1024]).await?;
    drop(send);
    let msgs = server.await??;
    assert_eq!(msgs, vec![vec![7], vec![], vec![8; 1024 * 1024]]);
    Ok(())
}
