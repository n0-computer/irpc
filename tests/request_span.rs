#![cfg(all(feature = "noq_endpoint_setup", feature = "derive"))]
//! Checks that a handler runs in the request span, also without span propagation.

use std::{
    io,
    net::{Ipv4Addr, SocketAddrV4},
    sync::{Arc, Mutex},
};

use irpc::{
    Client, WithChannels,
    channel::oneshot,
    noq::{listen, make_client_endpoint, make_server_endpoint},
    rpc::{Handler, read_request},
    rpc_requests,
};
use n0_future::task::AbortOnDropHandle;
use serde::{Deserialize, Serialize};
use testresult::TestResult;
use tracing::{Span, info};

#[rpc_requests(message = EchoMessage)]
#[derive(Debug, Serialize, Deserialize)]
enum EchoProtocol {
    #[rpc(tx = oneshot::Sender<(Option<String>, bool)>)]
    #[wrap(Echo)]
    Echo,
}

#[tokio::test]
async fn handler_runs_in_request_span() -> TestResult<()> {
    // The test runtime has one thread, so the server task sees this subscriber too.
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry());

    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into();
    let (server, cert) = make_server_endpoint(addr)?;
    let client = make_client_endpoint(addr, &[cert.as_slice()])?;
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server.local_addr()?.port()).into();

    // The handler reports the name of the current span, and whether the message has this span.
    let handler = Handler::<EchoProtocol>::sequential(|msg| async move {
        let current = Span::current();
        let same = msg.span().id() == current.id();
        let EchoMessage::Echo(WithChannels { tx, .. }) = msg;
        tx.send((current.metadata().map(|m| m.name().to_string()), same))
            .await
            .ok();
        Ok(())
    });
    let _server = AbortOnDropHandle::new(tokio::spawn(listen(server, handler)));
    let client = Client::<EchoProtocol>::noq(client, server_addr);

    let (name, same) = client.rpc(Echo).await?;
    assert_eq!(name.as_deref(), Some("Echo"));
    assert!(same, "the message has the request span");
    Ok(())
}

/// A log writer that keeps everything in memory.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("poisoned").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The logs of a request have the number of its connection and of the request.
#[tokio::test]
async fn logs_have_connection_and_request_ids() -> TestResult<()> {
    let logs = Logs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    // The test runtime has one thread, so the server task sees this subscriber too.
    let _guard = tracing::subscriber::set_default(subscriber);

    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into();
    let (server, cert) = make_server_endpoint(addr)?;
    let client = make_client_endpoint(addr, &[cert.as_slice()])?;
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server.local_addr()?.port()).into();

    let handler = Handler::<EchoProtocol>::sequential(|msg| async move {
        info!("handled");
        let EchoMessage::Echo(WithChannels { tx, .. }) = msg;
        tx.send((None, true)).await.ok();
        Ok(())
    });
    let _server = AbortOnDropHandle::new(tokio::spawn(listen(server, handler)));
    let client = Client::<EchoProtocol>::noq(client, server_addr);
    client.rpc(Echo).await?;
    client.rpc(Echo).await?;

    let logs = String::from_utf8(logs.0.lock().expect("poisoned").clone())?;
    assert!(
        logs.contains("rpc{conn=0}: irpc::rpc::noq_impl: connection accepted remote=127.0.0.1:"),
        "{logs}"
    );
    assert!(
        logs.contains("rpc{conn=0}:Echo{req=0}: request_span: handled"),
        "{logs}"
    );
    assert!(
        logs.contains("rpc{conn=0}:Echo{req=1}: request_span: handled"),
        "{logs}"
    );
    Ok(())
}

/// [`read_request`] gives the request span the number of the request too.
#[tokio::test]
async fn read_request_numbers_the_requests() -> TestResult<()> {
    let logs = Logs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .without_time()
        .finish();
    // The test runtime has one thread, so the server task sees this subscriber too.
    let _guard = tracing::subscriber::set_default(subscriber);

    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into();
    let (server, cert) = make_server_endpoint(addr)?;
    let client = make_client_endpoint(addr, &[cert.as_slice()])?;
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server.local_addr()?.port()).into();

    let _server = AbortOnDropHandle::new(tokio::spawn(async move {
        let connection = server
            .accept()
            .await
            .expect("accept")
            .await
            .expect("connect");
        while let Ok(Some(msg)) = read_request::<EchoProtocol>(&connection).await {
            msg.span().in_scope(|| info!("handled"));
            let EchoMessage::Echo(WithChannels { tx, .. }) = msg;
            tx.send((None, true)).await.ok();
        }
    }));
    let client = Client::<EchoProtocol>::noq(client, server_addr);
    client.rpc(Echo).await?;
    client.rpc(Echo).await?;

    let logs = String::from_utf8(logs.0.lock().expect("poisoned").clone())?;
    assert!(
        logs.contains("Echo{req=0}: request_span: handled"),
        "{logs}"
    );
    assert!(
        logs.contains("Echo{req=1}: request_span: handled"),
        "{logs}"
    );
    Ok(())
}
