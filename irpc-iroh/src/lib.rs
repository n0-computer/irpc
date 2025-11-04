use std::{
    fmt, io,
    sync::{atomic::AtomicU64, Arc},
};

use iroh::{
    endpoint::{Connecting, Connection, ConnectionError, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler},
};
use irpc::{
    channel::oneshot,
    rpc::{
        Handler, RemoteConnection, RemoteService, ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED,
        MAX_MESSAGE_SIZE,
    },
    util::AsyncReadVarintExt,
    LocalSender, RequestError,
};
use n0_error::{e, Result};
use n0_future::{future::Boxed as BoxFuture, TryFutureExt};
use serde::de::DeserializeOwned;
use tracing::{debug, error_span, trace, trace_span, warn, Instrument};

/// Returns a client that connects to a irpc service using an [`iroh::Endpoint`].
pub fn client<S: irpc::Service>(
    endpoint: iroh::Endpoint,
    addr: impl Into<iroh::EndpointAddr>,
    alpn: impl AsRef<[u8]>,
) -> irpc::Client<S> {
    let conn = IrohLazyRemoteConnection::new(endpoint, addr.into(), alpn.as_ref().to_vec());
    irpc::Client::boxed(conn)
}

/// Wrap an existing iroh connection as an irpc remote connection.
///
/// This will stop working as soon as the underlying iroh connection is closed.
/// If you need to support reconnects, use [`IrohLazyRemoteConnection`] instead.
// TODO: remove this and provide a From instance as soon as iroh is 1.0 and
// we can move irpc-iroh into irpc?
#[derive(Debug, Clone)]
pub struct IrohRemoteConnection(Connection);

impl IrohRemoteConnection {
    pub fn new(connection: Connection) -> Self {
        Self(connection)
    }
}

impl irpc::rpc::RemoteConnection for IrohRemoteConnection {
    fn clone_boxed(&self) -> Box<dyn irpc::rpc::RemoteConnection> {
        Box::new(self.clone())
    }

    fn open_bi(
        &self,
    ) -> n0_future::future::Boxed<std::result::Result<(SendStream, RecvStream), irpc::RequestError>>
    {
        let conn = self.0.clone();
        Box::pin(async move {
            let (send, recv) = conn.open_bi().await?;
            Ok((send, recv))
        })
    }
}

/// A connection to a remote service.
///
/// Initially this does just have the endpoint and the address. Once a
/// connection is established, it will be stored.
#[derive(Debug, Clone)]
pub struct IrohLazyRemoteConnection(Arc<IrohRemoteConnectionInner>);

#[derive(Debug)]
struct IrohRemoteConnectionInner {
    endpoint: iroh::Endpoint,
    addr: iroh::EndpointAddr,
    connection: tokio::sync::Mutex<Option<Connection>>,
    alpn: Vec<u8>,
}

impl IrohLazyRemoteConnection {
    pub fn new(endpoint: iroh::Endpoint, addr: iroh::EndpointAddr, alpn: Vec<u8>) -> Self {
        Self(Arc::new(IrohRemoteConnectionInner {
            endpoint,
            addr,
            connection: Default::default(),
            alpn,
        }))
    }
}

impl RemoteConnection for IrohLazyRemoteConnection {
    fn clone_boxed(&self) -> Box<dyn RemoteConnection> {
        Box::new(self.clone())
    }

    fn open_bi(&self) -> BoxFuture<std::result::Result<(SendStream, RecvStream), RequestError>> {
        let this = self.0.clone();
        Box::pin(async move {
            let mut guard = this.connection.lock().await;
            let pair = match guard.as_mut() {
                Some(conn) => {
                    // try to reuse the connection
                    match conn.open_bi().await {
                        Ok(pair) => pair,
                        Err(_) => {
                            // try with a new connection, just once
                            *guard = None;
                            connect_and_open_bi(&this.endpoint, &this.addr, &this.alpn, guard)
                                .await?
                        }
                    }
                }
                None => connect_and_open_bi(&this.endpoint, &this.addr, &this.alpn, guard).await?,
            };
            Ok(pair)
        })
    }
}

async fn connect_and_open_bi(
    endpoint: &iroh::Endpoint,
    addr: &iroh::EndpointAddr,
    alpn: &[u8],
    mut guard: tokio::sync::MutexGuard<'_, Option<Connection>>,
) -> Result<(SendStream, RecvStream), RequestError> {
    let conn = endpoint
        .connect(addr.clone(), alpn)
        .await
        .map_err(|err| e!(RequestError::Other, err.into()))?;
    let (send, recv) = conn.open_bi().await?;
    *guard = Some(conn);
    Ok((send, recv))
}

/// A [`ProtocolHandler`] for an irpc protocol.
///
/// Can be added to an [`iroh::protocol::Router`] to handle incoming connections for an ALPN string.
pub struct IrohProtocol<R> {
    handler: Handler<R>,
    request_id: AtomicU64,
}

impl<T> fmt::Debug for IrohProtocol<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RpcProtocol")
    }
}

impl<R: DeserializeOwned + Send + 'static> IrohProtocol<R> {
    pub fn with_sender(local_sender: impl Into<LocalSender<R>>) -> Self
    where
        R: RemoteService,
    {
        let handler = R::remote_handler(local_sender.into());
        Self::new(handler)
    }

    /// Creates a new [`IrohProtocol`] for the `handler`.
    pub fn new(handler: Handler<R>) -> Self {
        Self {
            handler,
            request_id: Default::default(),
        }
    }
}

impl<R: DeserializeOwned + Send + 'static> ProtocolHandler for IrohProtocol<R> {
    fn accept(
        &self,
        connection: Connection,
    ) -> impl std::future::Future<Output = Result<(), AcceptError>> + Send {
        let handler = self.handler.clone();
        let request_id = self
            .request_id
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let fut = handle_connection(connection, handler).map_err(AcceptError::from_err);
        let span = trace_span!("rpc", id = request_id);
        Box::pin(fut.instrument(span))
    }
}

/// A [`ProtocolHandler`] for an irpc protocol that supports 0rtt connections.
///
/// Can be added to an [`iroh::protocol::Router`] to handle incoming connections for an ALPN string.
///
/// For details about when it is safe to use 0rtt, see https://www.iroh.computer/blog/0rtt-api
pub struct Iroh0RttProtocol<R> {
    handler: Handler<R>,
    request_id: AtomicU64,
}

impl<T> fmt::Debug for Iroh0RttProtocol<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RpcProtocol")
    }
}

impl<R: DeserializeOwned + Send + 'static> Iroh0RttProtocol<R> {
    pub fn with_sender(local_sender: impl Into<LocalSender<R>>) -> Self
    where
        R: RemoteService,
    {
        let handler = R::remote_handler(local_sender.into());
        Self::new(handler)
    }

    /// Creates a new [`Iroh0RttProtocol`] for the `handler`.
    pub fn new(handler: Handler<R>) -> Self {
        Self {
            handler,
            request_id: Default::default(),
        }
    }
}

impl<R: DeserializeOwned + Send + 'static> ProtocolHandler for Iroh0RttProtocol<R> {
    async fn on_connecting(&self, connecting: Connecting) -> Result<Connection, AcceptError> {
        let (conn, _zero_rtt_accepted) = connecting
            .into_0rtt()
            .expect("accept into 0.5 RTT always succeeds");
        Ok(conn)
    }

    fn accept(
        &self,
        connection: Connection,
    ) -> impl std::future::Future<Output = Result<(), AcceptError>> + Send {
        let handler = self.handler.clone();
        let request_id = self
            .request_id
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let fut = handle_connection(connection, handler).map_err(AcceptError::from_err);
        let span = trace_span!("rpc", id = request_id);
        Box::pin(fut.instrument(span))
    }
}

/// Handles a single iroh connection with the provided `handler`.
pub async fn handle_connection<R: DeserializeOwned + 'static>(
    connection: Connection,
    handler: Handler<R>,
) -> io::Result<()> {
    if let Ok(remote) = connection.remote_id() {
        tracing::Span::current().record("remote", tracing::field::display(remote.fmt_short()));
    }
    debug!("connection accepted");
    loop {
        let Some((msg, rx, tx)) = read_request_raw(&connection).await? else {
            return Ok(());
        };
        handler(msg, rx, tx).await?;
    }
}

pub async fn read_request<S: RemoteService>(
    connection: &Connection,
) -> std::io::Result<Option<S::Message>> {
    Ok(read_request_raw::<S>(connection)
        .await?
        .map(|(msg, rx, tx)| S::with_remote_channels(msg, rx, tx)))
}

/// Reads a single request from the connection.
///
/// This accepts a bi-directional stream from the connection and reads and parses the request.
///
/// Returns the parsed request and the stream pair if reading and parsing the request succeeded.
/// Returns None if the remote closed the connection with error code `0`.
/// Returns an error for all other failure cases.
pub async fn read_request_raw<R: DeserializeOwned + 'static>(
    connection: &Connection,
) -> std::io::Result<Option<(R, RecvStream, SendStream)>> {
    let (send, mut recv) = match connection.accept_bi().await {
        Ok((s, r)) => (s, r),
        Err(ConnectionError::ApplicationClosed(cause)) if cause.error_code.into_inner() == 0 => {
            trace!("remote side closed connection {cause:?}");
            return Ok(None);
        }
        Err(cause) => {
            warn!("failed to accept bi stream {cause:?}");
            return Err(cause.into());
        }
    };
    let size = recv
        .read_varint_u64()
        .await?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "failed to read size"))?;
    if size > MAX_MESSAGE_SIZE {
        connection.close(
            ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED.into(),
            b"request exceeded max message size",
        );
        return Err(e!(oneshot::RecvError::MaxMessageSizeExceeded).into());
    }
    let mut buf = vec![0; size as usize];
    recv.read_exact(&mut buf)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::UnexpectedEof, e))?;
    let msg: R =
        postcard::from_bytes(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let rx = recv;
    let tx = send;
    Ok(Some((msg, rx, tx)))
}

/// Utility function to listen for incoming connections and handle them with the provided handler
pub async fn listen<R: DeserializeOwned + 'static>(endpoint: iroh::Endpoint, handler: Handler<R>) {
    let mut request_id = 0u64;
    let mut tasks = n0_future::task::JoinSet::new();
    loop {
        let incoming = tokio::select! {
            Some(res) = tasks.join_next(), if !tasks.is_empty() => {
                res.expect("irpc connection task panicked");
                continue;
            }
            incoming = endpoint.accept() => {
                match incoming {
                    None => break,
                    Some(incoming) => incoming
                }
            }
        };
        let handler = handler.clone();
        let fut = async move {
            match incoming.await {
                Ok(connection) => match handle_connection(connection, handler).await {
                    Err(err) => warn!("connection closed with error: {err:?}"),
                    Ok(()) => debug!("connection closed"),
                },
                Err(cause) => {
                    warn!("failed to accept connection: {cause:?}");
                }
            };
        };
        let span = error_span!("rpc", id = request_id, remote = tracing::field::Empty);
        tasks.spawn(fut.instrument(span));
        request_id += 1;
    }
}
