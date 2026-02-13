use std::{
    fmt,
    future::Future,
    io,
    sync::{atomic::AtomicU64, Arc},
};

use iroh::{
    endpoint::{
        Accepting, Connection, ConnectionError, IncomingZeroRttConnection,
        OutgoingZeroRttConnection, RecvStream, RemoteEndpointIdError, SendStream, VarInt,
        ZeroRttStatus,
    },
    protocol::{AcceptError, ProtocolHandler},
    EndpointId,
};
use irpc::{
    channel::oneshot,
    rpc::{
        Handler, RemoteConnection, RemoteService, RequestFilter,
        ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED, ERROR_CODE_REQUEST_LIMITED, MAX_MESSAGE_SIZE,
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

    fn zero_rtt_accepted(&self) -> BoxFuture<bool> {
        Box::pin(async { true })
    }
}

#[derive(Debug, Clone)]
pub struct IrohZrttRemoteConnection(OutgoingZeroRttConnection);

impl IrohZrttRemoteConnection {
    pub fn new(connection: OutgoingZeroRttConnection) -> Self {
        Self(connection)
    }
}

impl irpc::rpc::RemoteConnection for IrohZrttRemoteConnection {
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

    fn zero_rtt_accepted(&self) -> BoxFuture<bool> {
        let conn = self.0.clone();
        Box::pin(async move {
            match conn.handshake_completed().await {
                Err(_) => false,
                Ok(ZeroRttStatus::Accepted(_)) => true,
                Ok(ZeroRttStatus::Rejected(_)) => false,
            }
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

    fn zero_rtt_accepted(&self) -> BoxFuture<bool> {
        Box::pin(async { true })
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
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let handler = self.handler.clone();
        let request_id = self
            .request_id
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let fut = handle_connection(&connection, handler).map_err(AcceptError::from_err);
        let span = trace_span!("rpc", id = request_id);
        fut.instrument(span).await
    }
}

/// A [`ProtocolHandler`] for an irpc protocol that supports 0rtt connections.
///
/// Can be added to an [`iroh::protocol::Router`] to handle incoming connections for an ALPN string.
///
/// For details about when it is safe to use 0rtt, see <https://www.iroh.computer/blog/0rtt-api>
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
    async fn on_accepting(&self, accepting: Accepting) -> Result<Connection, AcceptError> {
        let zrtt_conn = accepting.into_0rtt();
        let handler = self.handler.clone();
        let request_id = self
            .request_id
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        handle_connection(&zrtt_conn, handler)
            .map_err(AcceptError::from_err)
            .instrument(trace_span!("rpc", id = request_id))
            .await?;
        let conn = zrtt_conn
            .handshake_completed()
            .await
            .map_err(AcceptError::from)?;
        Ok(conn)
    }

    async fn accept(&self, _connection: Connection) -> Result<(), AcceptError> {
        // Noop, handled in [`Self::on_accepting`]
        Ok(())
    }
}

/// Handles a single iroh connection with the provided `handler`.
pub async fn handle_connection<R: DeserializeOwned + 'static>(
    connection: &impl IncomingRemoteConnection,
    handler: Handler<R>,
) -> io::Result<()> {
    if let Ok(remote) = connection.remote_id() {
        tracing::Span::current().record("remote", tracing::field::display(remote.fmt_short()));
    }
    debug!("connection accepted");
    loop {
        let Some((msg, rx, tx)) = read_request_raw(connection).await? else {
            return Ok(());
        };
        handler(msg, rx, tx).await?;
    }
}

/// Reads a single request from a connection, and a message with channels.
pub async fn read_request<S: RemoteService>(
    connection: &impl IncomingRemoteConnection,
) -> std::io::Result<Option<S::Message>> {
    Ok(read_request_raw::<S>(connection)
        .await?
        .map(|(msg, rx, tx)| S::with_remote_channels(msg, rx, tx)))
}

/// Abstracts over [`Connection`] and [`IncomingZeroRttConnection`].
///
/// You don't need to implement this trait yourself. It is used by [`read_request`] and
/// [`handle_connection`] to work with both fully authenticated connections and with
/// 0-RTT connections.
pub trait IncomingRemoteConnection {
    /// Accepts a single bidirectional stream.
    fn accept_bi(
        &self,
    ) -> impl Future<Output = Result<(SendStream, RecvStream), ConnectionError>> + Send;
    /// Close the connection.
    fn close(&self, error_code: VarInt, reason: &[u8]);
    /// Returns the remote's endpoint id.
    ///
    /// This may only fail for 0-RTT connections.
    fn remote_id(&self) -> Result<EndpointId, RemoteEndpointIdError>;
}

impl IncomingRemoteConnection for IncomingZeroRttConnection {
    async fn accept_bi(&self) -> Result<(SendStream, RecvStream), ConnectionError> {
        self.accept_bi().await
    }

    fn close(&self, error_code: VarInt, reason: &[u8]) {
        self.close(error_code, reason)
    }
    fn remote_id(&self) -> Result<EndpointId, RemoteEndpointIdError> {
        self.remote_id()
    }
}

impl IncomingRemoteConnection for Connection {
    async fn accept_bi(&self) -> Result<(SendStream, RecvStream), ConnectionError> {
        self.accept_bi().await
    }

    fn close(&self, error_code: VarInt, reason: &[u8]) {
        self.close(error_code, reason)
    }
    fn remote_id(&self) -> Result<EndpointId, RemoteEndpointIdError> {
        Ok(self.remote_id())
    }
}

/// Reads a single request from the connection.
///
/// This accepts a bi-directional stream from the connection and reads and parses the request.
///
/// Returns the parsed request and the stream pair if reading and parsing the request succeeded.
/// Returns None if the remote closed the connection with error code `0`.
/// Returns an error for all other failure cases.
pub async fn read_request_raw<R: DeserializeOwned + 'static>(
    connection: &impl IncomingRemoteConnection,
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

/// Filter for incoming iroh connections.
///
/// Like [`irpc::rpc::ConnectionFilter`], but additionally allows filtering by
/// [`EndpointId`] — the remote node's cryptographic identity, available after
/// the QUIC handshake completes.
///
/// Most implementations only need [`Self::accept`] and/or [`Self::accept_endpoint_id`].
/// Override [`Self::accept_unvalidated`] for coarse pre-handshake flood protection.
pub trait IrohConnectionFilter: Send + Sync + 'static {
    /// Check whether to accept a connection from the given validated address.
    ///
    /// Returns `true` to accept, `false` to refuse.
    fn accept(&self, _addr: &std::net::SocketAddr) -> bool {
        true
    }

    /// Check whether to accept a connection before address validation.
    ///
    /// # Security
    ///
    /// The address has **not** been validated at this stage and can be
    /// freely spoofed by an attacker. It usually should not be used for
    /// access-control decisions. It is mainly useful for coarse,
    /// high-threshold flood protection (e.g. blocking known-bad IPs).
    ///
    /// Returns `true` to accept, `false` to refuse. Defaults to `true`.
    fn accept_unvalidated(&self, _addr: &std::net::SocketAddr) -> bool {
        true
    }

    /// Check whether to accept a connection from the given endpoint ID.
    ///
    /// Called after the QUIC handshake, when the remote node's identity is known.
    ///
    /// Returns `true` to accept, `false` to refuse.
    fn accept_endpoint_id(&self, _id: &EndpointId) -> bool {
        true
    }
}

/// An [`IrohConnectionFilter`] that accepts all connections.
#[derive(Debug, Clone, Default)]
pub struct AcceptAll;

impl IrohConnectionFilter for AcceptAll {}

fn wrap_request_filter<R: Send + 'static>(
    handler: Handler<R>,
    filter: Arc<dyn RequestFilter<R>>,
) -> Handler<R> {
    Arc::new(move |r, rx, mut tx| {
        if filter.accept(&r) {
            handler(r, rx, tx)
        } else {
            tx.reset(ERROR_CODE_REQUEST_LIMITED.into()).ok();
            drop(rx);
            Box::pin(async { Ok(()) })
        }
    })
}

/// Builder for configuring and running an iroh listener with optional filters.
pub struct IrohListenerBuilder<R> {
    endpoint: iroh::Endpoint,
    handler: Handler<R>,
    connection_filter: Arc<dyn IrohConnectionFilter>,
}

impl<R: DeserializeOwned + Send + 'static> IrohListenerBuilder<R> {
    /// Creates a new listener builder.
    pub fn new(endpoint: iroh::Endpoint, handler: Handler<R>) -> Self {
        Self {
            endpoint,
            handler,
            connection_filter: Arc::new(AcceptAll),
        }
    }

    /// Sets a connection filter for per-IP and per-endpoint-ID rate limiting.
    pub fn connection_filter(mut self, filter: impl IrohConnectionFilter) -> Self {
        self.connection_filter = Arc::new(filter);
        self
    }

    /// Sets a per-request filter.
    ///
    /// The filter is called with `&R` (the deserialized protocol enum)
    /// before the handler. If it returns `false`, the request is dropped.
    pub fn request_filter(mut self, filter: impl RequestFilter<R>) -> Self {
        self.handler = wrap_request_filter(self.handler, Arc::new(filter));
        self
    }

    /// Runs the listener, accepting connections until the endpoint is closed.
    pub async fn listen(self) {
        IrohListener {
            endpoint: self.endpoint,
            handler: self.handler,
            filter: self.connection_filter,
        }
        .run()
        .await
    }
}

struct IrohListener<R> {
    endpoint: iroh::Endpoint,
    handler: Handler<R>,
    filter: Arc<dyn IrohConnectionFilter>,
}

impl<R: DeserializeOwned + 'static> IrohListener<R> {
    async fn run(self) {
        let mut request_id = 0u64;
        let mut tasks = n0_future::task::JoinSet::new();
        loop {
            let incoming = tokio::select! {
                Some(res) = tasks.join_next(), if !tasks.is_empty() => {
                    res.expect("irpc connection task panicked");
                    continue;
                }
                incoming = self.endpoint.accept() => {
                    match incoming {
                        None => break,
                        Some(incoming) => incoming
                    }
                }
            };
            let addr = incoming.remote_address();
            let validated = incoming.remote_address_validated();
            let refused = if validated {
                !self.filter.accept(&addr)
            } else {
                !self.filter.accept_unvalidated(&addr)
            };
            if refused {
                incoming.refuse();
                continue;
            }
            let handler = self.handler.clone();
            let filter = self.filter.clone();
            let fut = async move {
                let accepting = match incoming.accept() {
                    Ok(accepting) => accepting,
                    Err(cause) => {
                        warn!("failed to accept connection: {cause:?}");
                        return;
                    }
                };
                match accepting.await {
                    Ok(connection) => {
                        // Deferred validated-address check for initially-unvalidated connections
                        if !validated && !filter.accept(&addr) {
                            connection.close(ERROR_CODE_REQUEST_LIMITED.into(), b"rate limited");
                            return;
                        }
                        // Endpoint ID check (only available after handshake)
                        if !filter.accept_endpoint_id(&connection.remote_id()) {
                            connection.close(ERROR_CODE_REQUEST_LIMITED.into(), b"rate limited");
                            return;
                        }
                        match handle_connection(&connection, handler).await {
                            Err(err) => warn!("connection closed with error: {err:?}"),
                            Ok(()) => debug!("connection closed"),
                        }
                    }
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
}

/// Utility function to listen for incoming connections and handle them with the provided handler
pub async fn listen<R: DeserializeOwned + Send + 'static>(
    endpoint: iroh::Endpoint,
    handler: Handler<R>,
) {
    IrohListenerBuilder::new(endpoint, handler).listen().await
}
