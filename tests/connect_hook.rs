#![cfg(all(feature = "noq_endpoint_setup", feature = "derive"))]
//! Checks that `Client::on_connect` runs before the first request on each connection.

use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use irpc::{Client, WithChannels, channel::oneshot, rpc::read_request, rpc_requests};
use n0_future::task::AbortOnDropHandle;
use noq::Endpoint;
use serde::{Deserialize, Serialize};
use testresult::TestResult;

#[allow(dead_code, reason = "this test uses only some of the helpers")]
mod common;
use common::*;

#[rpc_requests(message = AuthMessage)]
#[derive(Debug, Serialize, Deserialize)]
enum AuthProtocol {
    /// Authenticates the connection. Returns whether the token is valid.
    #[rpc(tx = oneshot::Sender<bool>)]
    #[wrap(Auth)]
    Auth(String),
    /// Returns whether the connection is authenticated.
    #[rpc(tx = oneshot::Sender<bool>)]
    #[wrap(IsAuthed)]
    IsAuthed,
}

const TOKEN: &str = "secret";

/// Serves each connection with its own auth state, and sends each connection to `conns`.
async fn server(endpoint: Endpoint, conns: tokio::sync::mpsc::UnboundedSender<noq::Connection>) {
    while let Some(incoming) = endpoint.accept().await {
        let Ok(conn) = incoming.await else {
            continue;
        };
        conns.send(conn.clone()).ok();
        tokio::spawn(async move {
            let mut authed = false;
            while let Ok(Some(msg)) = read_request::<AuthProtocol>(&conn).await {
                match msg {
                    AuthMessage::Auth(WithChannels { inner, tx, .. }) => {
                        authed = inner.0 == TOKEN;
                        tx.send(authed).await.ok();
                    }
                    AuthMessage::IsAuthed(WithChannels { tx, .. }) => {
                        tx.send(authed).await.ok();
                    }
                }
            }
        });
    }
}

/// A client whose hook authenticates with the token in `token`, and counts its runs.
fn auth_client(
    endpoint: Endpoint,
    addr: std::net::SocketAddr,
    token: Arc<Mutex<String>>,
    runs: Arc<AtomicUsize>,
) -> Client<AuthProtocol> {
    Client::noq(endpoint, addr).on_connect(move |client| {
        let token = token.lock().expect("poisoned").clone();
        let runs = runs.clone();
        async move {
            runs.fetch_add(1, Ordering::SeqCst);
            let valid = client.rpc(Auth(token)).await.map_err(io::Error::from)?;
            match valid {
                true => Ok(()),
                false => Err(io::Error::other("invalid token")),
            }
        }
    })
}

#[tokio::test]
async fn hook_runs_on_each_new_connection() -> TestResult<()> {
    let (server_endpoint, client_endpoint, addr) = create_connected_endpoints()?;
    let (conns_tx, mut conns) = tokio::sync::mpsc::unbounded_channel();
    let _server = AbortOnDropHandle::new(tokio::spawn(server(server_endpoint, conns_tx)));
    let token = Arc::new(Mutex::new(TOKEN.to_string()));
    let runs = Arc::new(AtomicUsize::new(0));
    let client = auth_client(client_endpoint, addr, token, runs.clone());

    // The first request connects, and the hook authenticates before it.
    assert!(client.rpc(IsAuthed).await?);
    assert!(client.clone().rpc(IsAuthed).await?);
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    // The server closes the connection. The client connects again, and the
    // hook authenticates the new connection.
    let conn = conns.recv().await.expect("server is running");
    conn.close(0u32.into(), b"restart");
    let authed = retry(|| client.rpc(IsAuthed)).await?;
    assert!(authed);
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn failed_hook_fails_the_request_and_runs_again() -> TestResult<()> {
    let (server_endpoint, client_endpoint, addr) = create_connected_endpoints()?;
    let (conns_tx, _conns) = tokio::sync::mpsc::unbounded_channel();
    let _server = AbortOnDropHandle::new(tokio::spawn(server(server_endpoint, conns_tx)));
    let token = Arc::new(Mutex::new("wrong".to_string()));
    let runs = Arc::new(AtomicUsize::new(0));
    let client = auth_client(client_endpoint, addr, token.clone(), runs.clone());

    let err = client.rpc(IsAuthed).await.expect_err("the hook fails");
    assert!(matches!(err, irpc::Error::Request { .. }), "{err:?}");
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    // The client does not keep the failed connection, so the next request runs the hook again.
    *token.lock().expect("poisoned") = TOKEN.to_string();
    assert!(client.rpc(IsAuthed).await?);
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn connect_runs_the_hook_early() -> TestResult<()> {
    let (server_endpoint, client_endpoint, addr) = create_connected_endpoints()?;
    let (conns_tx, _conns) = tokio::sync::mpsc::unbounded_channel();
    let _server = AbortOnDropHandle::new(tokio::spawn(server(server_endpoint, conns_tx)));
    let token = Arc::new(Mutex::new(TOKEN.to_string()));
    let runs = Arc::new(AtomicUsize::new(0));
    let client = auth_client(client_endpoint, addr, token, runs.clone());

    client.connect().await?;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    client.connect().await?;
    assert!(client.rpc(IsAuthed).await?);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    Ok(())
}

/// Runs `f` until it succeeds, at most 20 times.
///
/// A request can fail while the client has not seen the close of the old connection yet.
async fn retry<T, Fut>(f: impl Fn() -> Fut) -> irpc::Result<T>
where
    Fut: Future<Output = irpc::Result<T>>,
{
    let mut attempts = 0;
    loop {
        match f().await {
            Ok(value) => return Ok(value),
            Err(err) if attempts >= 20 => return Err(err),
            Err(_) => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}
