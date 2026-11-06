#![cfg(all(feature = "noq_endpoint_setup", feature = "derive"))]
//! Connection initialization is a barrier before application streams are opened.

use std::{
    future::Future,
    io,
    pin::pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use irpc::{
    Client, RemoteClientBuilder, Request, RequestError, WithChannels,
    channel::oneshot,
    rpc::{IncomingRemoteConnection, RemoteConnection, read_request},
    rpc_requests,
};
use n0_future::future::{Boxed as BoxFuture, poll_once};
use serde::{Deserialize, Serialize};
use testresult::TestResult;
use tokio::{
    sync::{Semaphore, mpsc},
    task::JoinSet,
};

#[allow(dead_code, reason = "this test uses only some of the helpers")]
mod common;

#[rpc_requests(message = AuthMessage)]
#[derive(Debug, Serialize, Deserialize)]
enum AuthProtocol {
    #[rpc(tx = oneshot::Sender<bool>)]
    #[wrap(Auth)]
    Auth(String),
    #[rpc(tx = oneshot::Sender<usize>)]
    #[wrap(Ping)]
    Ping,
    #[rpc(tx = oneshot::Sender<()>)]
    #[wrap(Hold)]
    Hold,
}

const TOKEN: &str = "secret";

#[derive(Debug, PartialEq)]
enum Event {
    Auth,
    Ping,
    Hold,
}

enum ServerConnection {
    Noq(noq::Connection),
    #[cfg(feature = "iroh")]
    Iroh(iroh::endpoint::Connection),
}

impl ServerConnection {
    fn close(&self) {
        match self {
            Self::Noq(conn) => conn.close(0u32.into(), b"test disconnect"),
            #[cfg(feature = "iroh")]
            Self::Iroh(conn) => conn.close(0u32.into(), b"test disconnect"),
        }
    }

    async fn closed(&self) {
        match self {
            Self::Noq(conn) => {
                conn.closed().await;
            }
            #[cfg(feature = "iroh")]
            Self::Iroh(conn) => {
                conn.closed().await;
            }
        }
    }
}

/// Closes on any ordinary request before auth, or on repeated auth.
async fn serve(
    conn: impl IncomingRemoteConnection + Send + Sync + 'static,
    id: usize,
    events: mpsc::UnboundedSender<Event>,
) {
    let mut authenticated = false;
    while let Ok(Some(msg)) = read_request::<AuthProtocol>(&conn).await {
        match msg {
            AuthMessage::Auth(WithChannels { inner, tx, .. }) => {
                events.send(Event::Auth).ok();
                if authenticated {
                    conn.close(400u32.into(), b"already authenticated");
                    return;
                }
                authenticated = inner.0 == TOKEN;
                tx.send(authenticated).await.ok();
                if !authenticated {
                    return;
                }
            }
            msg => {
                if !authenticated {
                    conn.close(401u32.into(), b"auth must come first");
                    return;
                }
                match msg {
                    AuthMessage::Ping(WithChannels { tx, .. }) => {
                        events.send(Event::Ping).ok();
                        tx.send(id).await.ok();
                    }
                    AuthMessage::Hold(WithChannels { tx, .. }) => {
                        events.send(Event::Hold).ok();
                        std::future::pending::<()>().await;
                        drop(tx);
                    }
                    AuthMessage::Auth(_) => unreachable!(),
                }
            }
        }
    }
}

struct Server {
    builder: Option<RemoteClientBuilder<AuthProtocol>>,
    connections: mpsc::UnboundedReceiver<ServerConnection>,
    events: mpsc::UnboundedReceiver<Event>,
    task: Option<tokio::task::JoinHandle<()>>,
    #[cfg(feature = "iroh")]
    _router: Option<iroh::protocol::Router>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl Server {
    fn noq() -> TestResult<Self> {
        let (endpoint, client_endpoint, addr) = common::create_connected_endpoints()?;
        let (connections_tx, connections) = mpsc::unbounded_channel();
        let (events_tx, events) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            let mut id = 0;
            while let Some(incoming) = endpoint.accept().await {
                let conn = incoming.await.expect("accept connection");
                connections_tx
                    .send(ServerConnection::Noq(conn.clone()))
                    .ok();
                tasks.spawn(serve(conn, id, events_tx.clone()));
                id += 1;
            }
        });
        Ok(Self {
            builder: Some(Client::noq_builder(client_endpoint, addr)),
            connections,
            events,
            task: Some(task),
            #[cfg(feature = "iroh")]
            _router: None,
        })
    }

    fn builder(&mut self) -> RemoteClientBuilder<AuthProtocol> {
        self.builder.take().expect("builder is used once")
    }
}

async fn authenticate(client: Client<AuthProtocol>, token: String) -> io::Result<()> {
    if client.rpc(Auth(token)).await.map_err(io::Error::from)? {
        Ok(())
    } else {
        Err(io::Error::other("invalid token"))
    }
}

async fn bounded<T>(fut: impl Future<Output = TestResult<T>>) -> TestResult<T> {
    tokio::time::timeout(Duration::from_secs(15), fut).await?
}

/// Checks initial and replacement connections over either transport.
async fn reconnect_barrier(mut server: Server) -> TestResult<()> {
    let gate = Arc::new(Semaphore::new(0));
    let runs = Arc::new(AtomicUsize::new(0));
    let (entered_tx, mut entered) = mpsc::unbounded_channel();
    let client = server
        .builder()
        .on_connect({
            let gate = gate.clone();
            let runs = runs.clone();
            move |pinned| {
                let gate = gate.clone();
                runs.fetch_add(1, Ordering::SeqCst);
                entered_tx
                    .send(pinned.clone())
                    .expect("test receives entry");
                async move {
                    gate.acquire().await.expect("gate is open").forget();
                    authenticate(pinned, TOKEN.into()).await
                }
            }
        })
        .build();

    // Building is lazy.
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    for generation in 0..2 {
        let connect_client = client.clone();
        let connect = tokio::spawn(async move { connect_client.connect().await });
        let pinned = entered.recv().await.expect("initializer entered");

        // Poll each clone while the hook is paused. request() would immediately
        // allocate a stream on a prematurely published connection.
        let mut requests: Vec<_> = (0..4).map(|_| Box::pin(client.clone().request())).collect();
        for request in &mut requests {
            assert!(poll_once(request.as_mut()).await.is_none());
        }
        assert!(server.events.try_recv().is_err());
        assert_eq!(runs.load(Ordering::SeqCst), generation + 1);

        gate.add_permits(1);
        connect.await??;
        assert_eq!(server.events.recv().await, Some(Event::Auth));
        for request in requests {
            let Request::Remote(request) = request.await? else {
                panic!("remote request");
            };
            let (_, recv) = request.write(Ping).await?;
            assert_eq!(oneshot::Receiver::<usize>::from(recv).await?, generation);
            assert_eq!(server.events.recv().await, Some(Event::Ping));
        }
        client.connect().await?;
        assert_eq!(runs.load(Ordering::SeqCst), generation + 1);

        let connection = server
            .connections
            .recv()
            .await
            .expect("accepted connection");
        connection.close();
        // This fixed client cannot reconnect. Its failure ensures that the
        // transport has observed the close before we start the next generation.
        assert!(pinned.rpc(Ping).await.is_err());
    }
    Ok(())
}

#[tokio::test]
async fn noq_reconnect_initializes_before_opening_application_streams() -> TestResult<()> {
    bounded(reconnect_barrier(Server::noq()?)).await
}

#[tokio::test]
async fn failed_initializer_closes_candidate_and_allows_another_attempt() -> TestResult<()> {
    bounded(async {
        let mut server = Server::noq()?;
        let token = Arc::new(Mutex::new("wrong".to_string()));
        let client = server
            .builder()
            .on_connect({
                let token = token.clone();
                move |client| authenticate(client, token.lock().unwrap().clone())
            })
            .build();

        let err = client.rpc(Ping).await.expect_err("auth is rejected");
        assert!(
            matches!(
                err,
                irpc::Error::Request {
                    source: RequestError::Initialization { .. },
                    ..
                }
            ),
            "{err:?}"
        );
        let connection = server.connections.recv().await.unwrap();
        connection.closed().await;
        assert_eq!(server.events.recv().await, Some(Event::Auth));
        assert!(server.events.try_recv().is_err());

        *token.lock().unwrap() = TOKEN.into();
        assert_eq!(client.rpc(Ping).await?, 1);
        assert_eq!(server.events.recv().await, Some(Event::Auth));
        assert_eq!(server.events.recv().await, Some(Event::Ping));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn cancelling_initialization_closes_even_a_retained_pinned_client() -> TestResult<()> {
    bounded(async {
        let mut server = Server::noq()?;
        let attempts = Arc::new(AtomicUsize::new(0));
        let (entered_tx, mut entered) = mpsc::unbounded_channel();
        let (authenticated_tx, mut authenticated) = mpsc::unbounded_channel();
        let client = server
            .builder()
            .on_connect(move |pinned| {
                let first = attempts.fetch_add(1, Ordering::SeqCst) == 0;
                entered_tx.send(pinned.clone()).unwrap();
                let authenticated_tx = authenticated_tx.clone();
                async move {
                    if first {
                        // Cancellation after Auth has succeeded must still close the
                        // candidate, rather than leave partially completed setup usable.
                        authenticate(pinned, TOKEN.into()).await?;
                        authenticated_tx.send(()).unwrap();
                        std::future::pending::<()>().await;
                        Ok(())
                    } else {
                        authenticate(pinned, TOKEN.into()).await
                    }
                }
            })
            .build();

        let connecting_client = client.clone();
        let connect = tokio::spawn(async move { connecting_client.connect().await });
        let pinned = entered.recv().await.unwrap();
        let connection = server.connections.recv().await.unwrap();
        assert_eq!(server.events.recv().await, Some(Event::Auth));
        authenticated.recv().await.unwrap();
        let mut waiting_request = pin!(client.rpc(Ping));
        assert!(poll_once(waiting_request.as_mut()).await.is_none());
        connect.abort();
        assert!(connect.await.unwrap_err().is_cancelled());
        connection.closed().await;
        assert!(pinned.rpc(Ping).await.is_err());
        assert_eq!(waiting_request.await?, 1);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn connection_closed_during_initialization_is_not_cached() -> TestResult<()> {
    bounded(async {
        let mut server = Server::noq()?;
        let (entered_tx, mut entered) = mpsc::unbounded_channel();
        let gate = Arc::new(Semaphore::new(0));
        let client = server
            .builder()
            .on_connect({
                let gate = gate.clone();
                move |pinned| {
                    let gate = gate.clone();
                    entered_tx.send(pinned).unwrap();
                    async move {
                        gate.acquire().await.unwrap().forget();
                        Ok::<_, io::Error>(())
                    }
                }
            })
            .build();
        let connecting_client = client.clone();
        let connect = tokio::spawn(async move { connecting_client.connect().await });
        let pinned = entered.recv().await.unwrap();
        server.connections.recv().await.unwrap().close();
        assert!(pinned.rpc(Ping).await.is_err());
        gate.add_permits(1);
        assert!(matches!(
            connect.await?,
            Err(RequestError::Connection { .. })
        ));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn cancelling_a_dial_releases_the_connection_gate() -> TestResult<()> {
    bounded(async {
        let (server, endpoint, addr) = common::create_connected_endpoints()?;
        let runs = Arc::new(AtomicUsize::new(0));
        let client = Client::noq_builder(endpoint, addr)
            .on_connect({
                let runs = runs.clone();
                move |client| {
                    runs.fetch_add(1, Ordering::SeqCst);
                    authenticate(client, TOKEN.into())
                }
            })
            .build();
        // The server is not accepting connections yet, so the handshake waits.
        let mut connecting = Box::pin(client.connect());
        assert!(poll_once(connecting.as_mut()).await.is_none());
        drop(connecting);
        assert_eq!(runs.load(Ordering::SeqCst), 0);

        let server = n0_future::task::AbortOnDropHandle::new(tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            while let Some(incoming) = server.accept().await {
                // The cancelled attempt may also have reached the server.
                if let Ok(conn) = incoming.await {
                    let (events, _rx) = mpsc::unbounded_channel();
                    tasks.spawn(serve(conn, 0, events));
                }
            }
        }));
        assert_eq!(client.rpc(Ping).await?, 0);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        drop(server);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn fixed_connection_initializes_once_and_never_redials() -> TestResult<()> {
    bounded(async {
        let (server, endpoint, addr) = common::create_connected_endpoints()?;
        let serve = tokio::spawn(async move {
            let conn = server.accept().await.unwrap().await.unwrap();
            let (events, _rx) = mpsc::unbounded_channel();
            serve(conn, 0, events).await;
        });
        let conn = endpoint.connect(addr, "localhost")?.await?;
        let runs = Arc::new(AtomicUsize::new(0));
        let client = RemoteClientBuilder::from_noq(conn.clone())
            .on_connect({
                let runs = runs.clone();
                move |client| {
                    runs.fetch_add(1, Ordering::SeqCst);
                    authenticate(client, TOKEN.into())
                }
            })
            .build();
        client.connect().await?;
        client.connect().await?;
        assert_eq!(client.rpc(Ping).await?, 0);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        conn.close(0u32.into(), b"test disconnect");
        assert!(client.connect().await.is_err());
        assert!(client.rpc(Ping).await.is_err());
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        serve.await?;
        Ok(())
    })
    .await
}

#[tokio::test]
async fn in_flight_requests_are_not_replayed() -> TestResult<()> {
    bounded(async {
        let mut server = Server::noq()?;
        let runs = Arc::new(AtomicUsize::new(0));
        let client = server
            .builder()
            .on_connect({
                let runs = runs.clone();
                move |client| {
                    runs.fetch_add(1, Ordering::SeqCst);
                    authenticate(client, TOKEN.into())
                }
            })
            .build();
        client.connect().await?;
        assert_eq!(server.events.recv().await, Some(Event::Auth));
        let mut request = pin!(client.rpc(Hold));
        assert!(poll_once(request.as_mut()).await.is_none());
        // Drive the request until the server receives it while also watching events.
        tokio::select! {
            event = server.events.recv() => assert_eq!(event, Some(Event::Hold)),
            result = &mut request => panic!("request should be pending: {result:?}"),
        }
        server.connections.recv().await.unwrap().close();
        assert!(request.await.is_err());
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(server.connections.try_recv().is_err());
        Ok(())
    })
    .await
}

#[derive(Debug, Clone)]
struct CustomConnection;

impl RemoteConnection for CustomConnection {
    fn clone_boxed(&self) -> Box<dyn RemoteConnection> {
        Box::new(self.clone())
    }
    fn open_bi(&self) -> BoxFuture<Result<(noq::SendStream, noq::RecvStream), RequestError>> {
        Box::pin(std::future::pending())
    }
    fn zero_rtt_rejected(&self) -> BoxFuture<bool> {
        Box::pin(async { false })
    }
}

#[tokio::test]
async fn local_connect_is_ready_and_custom_connect_reports_unsupported() -> TestResult<()> {
    let (tx, _rx) = tokio::sync::mpsc::channel::<AuthMessage>(1);
    Client::<AuthProtocol>::local(tx).connect().await?;
    let client = Client::<AuthProtocol>::boxed(CustomConnection);
    assert!(matches!(
        client.connect().await,
        Err(RequestError::ConnectUnsupported { .. })
    ));
    Ok(())
}

#[cfg(feature = "iroh")]
mod iroh_tests {
    use iroh::{
        Endpoint, RelayMode,
        endpoint::{Connection, presets},
        protocol::{AcceptError, ProtocolHandler, Router},
    };

    use super::*;

    #[tokio::test]
    async fn completed_zero_rtt_handshake_does_not_hide_a_closed_connection() -> TestResult<()> {
        bounded(async {
            let endpoint = Endpoint::builder(presets::Minimal)
                .alpns(vec![b"auth-test".to_vec()])
                .bind()
                .await?;
            let addr = endpoint.addr();
            let server_endpoint = endpoint.clone();
            let server = tokio::spawn(async move {
                let conn = server_endpoint.accept().await.unwrap().await?;
                let (mut send, mut recv) = conn.accept_bi().await?;
                let data = recv.read_to_end(16).await?;
                send.write_all(&data).await?;
                send.finish()?;
                conn.closed().await;

                let conn = server_endpoint.accept().await.unwrap().await?;
                conn.closed().await;
                TestResult::Ok(())
            });
            let client_endpoint = Endpoint::bind(presets::Minimal).await?;

            // A completed exchange lets the client receive a resumption ticket.
            let conn = client_endpoint.connect(addr.clone(), b"auth-test").await?;
            let (mut send, mut recv) = conn.open_bi().await?;
            send.write_all(b"hello").await?;
            send.finish()?;
            assert_eq!(recv.read_to_end(16).await?, b"hello");
            conn.close(0u32.into(), b"resumption ticket received");

            let conn = client_endpoint
                .connect_with_opts(addr, b"auth-test", Default::default())
                .await?
                .into_0rtt()
                .expect("the first connection supplied a resumption ticket");
            let client = Client::<AuthProtocol>::boxed(conn.clone());
            client.connect().await?;
            conn.close(0u32.into(), b"test disconnect");
            assert!(matches!(
                client.connect().await,
                Err(RequestError::Connection { .. })
            ));

            server.await??;
            client_endpoint.close().await;
            endpoint.close().await;
            Ok(())
        })
        .await
    }

    #[derive(Debug)]
    struct AuthServer {
        connections: mpsc::UnboundedSender<ServerConnection>,
        events: mpsc::UnboundedSender<Event>,
        next_id: AtomicUsize,
    }

    impl ProtocolHandler for AuthServer {
        async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            self.connections
                .send(ServerConnection::Iroh(conn.clone()))
                .ok();
            serve(conn, id, self.events.clone()).await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn iroh_reconnect_initializes_before_opening_application_streams() -> TestResult<()> {
        bounded(async {
            let endpoint = Endpoint::builder(presets::N0)
                .relay_mode(RelayMode::Disabled)
                .clear_address_lookup()
                .bind()
                .await?;
            let addr = endpoint.addr();
            let (connections_tx, connections) = mpsc::unbounded_channel();
            let (events_tx, events) = mpsc::unbounded_channel();
            let router = Router::builder(endpoint)
                .accept(
                    b"auth-test",
                    AuthServer {
                        connections: connections_tx,
                        events: events_tx,
                        next_id: AtomicUsize::new(0),
                    },
                )
                .spawn();
            let client = Endpoint::builder(presets::N0)
                .relay_mode(RelayMode::Disabled)
                .clear_address_lookup()
                .bind()
                .await?;
            reconnect_barrier(Server {
                builder: Some(Client::iroh_builder(client, addr, b"auth-test")),
                connections,
                events,
                task: None,
                _router: Some(router),
            })
            .await
        })
        .await
    }
}
