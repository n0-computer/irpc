use std::{
    fmt, io,
    sync::{atomic::AtomicU64, Arc},
};

use iroh::{
    endpoint::{Connection, ConnectionError, RecvStream, SendStream},
    protocol::ProtocolHandler,
};
use irpc::{
    rpc::{Handler, RemoteConnection},
    util::AsyncReadVarintExt,
    RequestError,
};
use n0_future::{boxed::BoxFuture, TryFutureExt};
use serde::de::DeserializeOwned;
use tracing::{trace, trace_span, warn, Instrument};

/// A connection to a remote service.
///
/// Initially this does just have the endpoint and the address. Once a
/// connection is established, it will be stored.
#[derive(Debug, Clone)]
pub struct IrohRemoteConnection(Arc<IrohRemoteConnectionInner>);

#[derive(Debug)]
struct IrohRemoteConnectionInner {
    endpoint: iroh::Endpoint,
    addr: iroh::NodeAddr,
    connection: tokio::sync::Mutex<Option<Connection>>,
    alpn: Vec<u8>,
}

impl IrohRemoteConnection {
    pub fn new(endpoint: iroh::Endpoint, addr: iroh::NodeAddr, alpn: Vec<u8>) -> Self {
        Self(Arc::new(IrohRemoteConnectionInner {
            endpoint,
            addr,
            connection: Default::default(),
            alpn,
        }))
    }
}

impl RemoteConnection for IrohRemoteConnection {
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
                                .await
                                .map_err(RequestError::Other)?
                        }
                    }
                }
                None => connect_and_open_bi(&this.endpoint, &this.addr, &this.alpn, guard)
                    .await
                    .map_err(RequestError::Other)?,
            };
            Ok(pair)
        })
    }
}

async fn connect_and_open_bi(
    endpoint: &iroh::Endpoint,
    addr: &iroh::NodeAddr,
    alpn: &[u8],
    mut guard: tokio::sync::MutexGuard<'_, Option<Connection>>,
) -> anyhow::Result<(SendStream, RecvStream)> {
    let conn = endpoint.connect(addr.clone(), alpn).await?;
    let (send, recv) = conn.open_bi().await?;
    *guard = Some(conn);
    Ok((send, recv))
}

/// A [`ProtocolHandler`] for an irpc protocol.
///
/// Can be added to an [`iroh::router::Router`] to handle incoming connections for an ALPN string.
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
    /// Creates a new [`IrohProtocol`] for the `handler`.
    pub fn new(handler: Handler<R>) -> Self {
        Self {
            handler,
            request_id: Default::default(),
        }
    }
}

impl<R: DeserializeOwned + Send + 'static> ProtocolHandler for IrohProtocol<R> {
    fn accept(&self, connection: Connection) -> n0_future::future::Boxed<anyhow::Result<()>> {
        let handler = self.handler.clone();
        let request_id = self
            .request_id
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let fut = handle_connection(connection, handler).map_err(anyhow::Error::from);
        let span = trace_span!("rpc", id = request_id);
        Box::pin(fut.instrument(span))
    }
}

/// Handles a single iroh connection with the provided `handler`.
pub async fn handle_connection<R: DeserializeOwned + 'static>(
    connection: Connection,
    handler: Handler<R>,
) -> io::Result<()> {
    loop {
        let Some((msg, updates, reply)) = read_request(&connection).await? else {
            return Ok(());
        };
        handler(msg, updates, reply).await?;
    }
}

/// Reads a single request from the connection.
///
/// This accepts a bi-directional stream from the connection and reads and parses the request.
///
/// Returns the parsed request and the stream pair if reading and parsing the request succeeded.
/// Returns None if the remote closed the connection with error code `0`.
/// Returns an error for all other failure cases.
pub async fn read_request<R: DeserializeOwned + 'static>(
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
    let mut buf = vec![0; size as usize];
    recv.read_exact(&mut buf)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::UnexpectedEof, e))?;
    let msg: R =
        postcard::from_bytes(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let updates = recv;
    let reply = send;
    Ok(Some((msg, updates, reply)))
}

/// Utility function to listen for incoming connections and handle them with the provided handler
pub async fn listen<R: DeserializeOwned + 'static>(endpoint: iroh::Endpoint, handler: Handler<R>) {
    let mut request_id = 0u64;
    let mut tasks = n0_future::task::JoinSet::new();
    while let Some(incoming) = endpoint.accept().await {
        let handler = handler.clone();
        let fut = async move {
            let connection = match incoming.await {
                Ok(connection) => connection,
                Err(cause) => {
                    warn!("failed to accept connection {cause:?}");
                    return io::Result::Ok(());
                }
            };
            handle_connection(connection, handler).await
        };
        let span = trace_span!("rpc", id = request_id);
        tasks.spawn(fut.instrument(span));
        request_id += 1;
    }
}
