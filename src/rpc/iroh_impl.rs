//! RPC over [`iroh`] connections, with dial by endpoint id.
use std::{fmt, sync::Arc};

use iroh::{
    endpoint::{
        Accepting, Connection, ConnectionError, IncomingZeroRttConnection,
        OutgoingZeroRttConnection, RecvStream, SendStream, VarInt, ZeroRttStatus,
    },
    protocol::{AcceptError, ProtocolHandler},
};
use n0_error::{Result, e};
use n0_future::{TryFutureExt, future::Boxed as BoxFuture};
// portable-atomic provides AtomicU64 on 32-bit targets (e.g. Xtensa ESP32) that
// lack native 64-bit atomics, same as iroh itself.
use portable_atomic::{AtomicU64, Ordering};
use tracing::{Instrument, debug, error_span, trace_span, warn};

use crate::{
    LocalSender, RequestError, Service,
    rpc::{Handler, IncomingRemoteConnection, RemoteConnection, RemoteService, handle_connection},
};

impl RemoteConnection for Connection {
    fn clone_boxed(&self) -> Box<dyn RemoteConnection> {
        Box::new(self.clone())
    }

    fn open_bi(&self) -> BoxFuture<std::result::Result<(SendStream, RecvStream), RequestError>> {
        let conn = self.clone();
        Box::pin(async move {
            let (send, recv) = conn.open_bi().await?;
            Ok((send, recv))
        })
    }

    fn zero_rtt_rejected(&self) -> BoxFuture<bool> {
        Box::pin(async { false })
    }
}

impl RemoteConnection for OutgoingZeroRttConnection {
    fn clone_boxed(&self) -> Box<dyn RemoteConnection> {
        Box::new(self.clone())
    }

    fn open_bi(&self) -> BoxFuture<std::result::Result<(SendStream, RecvStream), RequestError>> {
        let conn = self.clone();
        Box::pin(async move {
            let (send, recv) = conn.open_bi().await?;
            Ok((send, recv))
        })
    }

    fn zero_rtt_rejected(&self) -> BoxFuture<bool> {
        let conn = self.clone();
        Box::pin(async move {
            match conn.handshake_completed().await {
                Err(_) => true,
                Ok(ZeroRttStatus::Accepted(_)) => false,
                Ok(ZeroRttStatus::Rejected(_)) => true,
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

    fn zero_rtt_rejected(&self) -> BoxFuture<bool> {
        Box::pin(async { false })
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
pub struct IrohProtocol<S> {
    handler: Handler<S>,
    request_id: AtomicU64,
}

impl<T> fmt::Debug for IrohProtocol<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RpcProtocol")
    }
}

impl<S: Service> IrohProtocol<S> {
    pub fn with_sender(local_sender: impl Into<LocalSender<S>>) -> Self
    where
        S: RemoteService,
    {
        let handler = S::remote_handler(local_sender.into());
        Self::new(handler)
    }

    /// Creates a new [`IrohProtocol`] for the `handler`.
    pub fn new(handler: Handler<S>) -> Self {
        Self {
            handler,
            request_id: Default::default(),
        }
    }
}

impl<S: Service> ProtocolHandler for IrohProtocol<S> {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let handler = self.handler.clone();
        let request_id = self.request_id.fetch_add(1, Ordering::AcqRel);
        let fut = handle_connection::<S>(&connection, handler).map_err(AcceptError::from_err);
        let span = trace_span!("rpc", id = request_id);
        fut.instrument(span).await
    }
}

/// A [`ProtocolHandler`] for an irpc protocol that supports 0rtt connections.
///
/// Can be added to an [`iroh::protocol::Router`] to handle incoming connections for an ALPN string.
///
/// For details about when it is safe to use 0rtt, see <https://www.iroh.computer/blog/0rtt-api>
/// For details about when it is safe to use 0rtt, see <https://www.iroh.computer/blog/0rtt-api>
pub struct Iroh0RttProtocol<S> {
    handler: Handler<S>,
    request_id: AtomicU64,
}

impl<T> fmt::Debug for Iroh0RttProtocol<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RpcProtocol")
    }
}

impl<S: Service> Iroh0RttProtocol<S> {
    pub fn with_sender(local_sender: impl Into<LocalSender<S>>) -> Self
    where
        S: RemoteService,
    {
        let handler = S::remote_handler(local_sender.into());
        Self::new(handler)
    }

    /// Creates a new [`Iroh0RttProtocol`] for the `handler`.
    pub fn new(handler: Handler<S>) -> Self {
        Self {
            handler,
            request_id: Default::default(),
        }
    }
}

impl<S: Service> ProtocolHandler for Iroh0RttProtocol<S> {
    async fn on_accepting(&self, accepting: Accepting) -> Result<Connection, AcceptError> {
        let zrtt_conn = accepting.into_0rtt();
        let handler = self.handler.clone();
        let request_id = self.request_id.fetch_add(1, Ordering::AcqRel);
        handle_connection::<S>(&zrtt_conn, handler)
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

impl IncomingRemoteConnection for IncomingZeroRttConnection {
    async fn accept_bi(&self) -> Result<(SendStream, RecvStream), ConnectionError> {
        self.accept_bi().await
    }

    fn close(&self, error_code: VarInt, reason: &[u8]) {
        self.close(error_code, reason)
    }

    fn remote_label(&self) -> Option<String> {
        let remote = self.remote_id().ok()?;
        Some(remote.fmt_short().to_string())
    }
}

impl IncomingRemoteConnection for Connection {
    async fn accept_bi(&self) -> Result<(SendStream, RecvStream), ConnectionError> {
        self.accept_bi().await
    }

    fn close(&self, error_code: VarInt, reason: &[u8]) {
        self.close(error_code, reason)
    }

    fn remote_label(&self) -> Option<String> {
        Some(self.remote_id().fmt_short().to_string())
    }
}

/// Utility function to listen for incoming connections and handle them with the provided handler.
///
/// The wire format used depends on `S::SPAN_PROPAGATION` - if true, span context is expected.
pub async fn listen<S: Service>(endpoint: iroh::Endpoint, handler: Handler<S>) {
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
                Ok(connection) => match handle_connection::<S>(&connection, handler).await {
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
