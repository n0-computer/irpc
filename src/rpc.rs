//! Module for cross-process RPC.
//!
//! The code in this module works with all connections that use [`noq`]
//! streams. The [`crate::noq`] module has the transport with dial by socket
//! address. The [`crate::iroh`] module has the transport with dial by endpoint id.
use std::{
    fmt::Debug, future::Future, io, marker::PhantomData, ops::DerefMut, pin::Pin, sync::Arc,
};

use n0_error::{AnyError, e, stack_error};
use n0_future::future::Boxed as BoxFuture;
use noq::{ConnectionError, VarInt};
use serde::de::DeserializeOwned;
use smallvec::SmallVec;
use tracing::{debug, trace, warn};

#[cfg(feature = "iroh")]
pub(crate) mod iroh_impl;
pub(crate) mod noq_impl;

use crate::{
    LocalSender, RequestError, RpcMessage, Service,
    channel::{
        SendError,
        mpsc::{self, DynReceiver, DynSender},
        none::NoSender,
        oneshot,
    },
    util::{AsyncReadVarintExt, WriteVarintExt, now_or_never},
};

/// Default max message size (16 MiB).
pub const MAX_MESSAGE_SIZE: u64 = 1024 * 1024 * 16;

/// Error code on streams if the max message size was exceeded.
pub const ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED: u32 = 1;

/// Error code on streams if the sender tried to send an message that could not be postcard serialized.
pub const ERROR_CODE_INVALID_POSTCARD: u32 = 2;

/// Error that can occur when writing the initial message when doing a
/// cross-process RPC.
#[stack_error(derive, add_meta, from_sources)]
pub enum WriteError {
    /// Error writing to the stream with noq
    #[error("Error writing to stream")]
    Noq {
        #[error(std_err)]
        source: noq::WriteError,
    },
    /// The message exceeded the maximum allowed message size (see [`MAX_MESSAGE_SIZE`]).
    #[error("Maximum message size exceeded")]
    MaxMessageSizeExceeded,
    /// Generic IO error, e.g. when serializing the message or when using
    /// other transports.
    #[error("Error serializing")]
    Io {
        #[error(std_err)]
        source: io::Error,
    },
}

impl From<postcard::Error> for WriteError {
    fn from(value: postcard::Error) -> Self {
        e!(Self::Io, io::Error::new(io::ErrorKind::InvalidData, value))
    }
}

impl From<postcard::Error> for SendError {
    fn from(value: postcard::Error) -> Self {
        e!(Self::Io, io::Error::new(io::ErrorKind::InvalidData, value))
    }
}

impl From<WriteError> for io::Error {
    fn from(e: WriteError) -> Self {
        match e {
            WriteError::Io { source, .. } => source,
            WriteError::MaxMessageSizeExceeded { .. } => {
                io::Error::new(io::ErrorKind::InvalidData, e)
            }
            WriteError::Noq { source, .. } => source.into(),
        }
    }
}

impl From<noq::WriteError> for SendError {
    fn from(err: noq::WriteError) -> Self {
        match err {
            noq::WriteError::Stopped(code)
                if code == ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED.into() =>
            {
                e!(SendError::MaxMessageSizeExceeded)
            }
            _ => e!(SendError::Io, io::Error::from(err)),
        }
    }
}

/// Trait to abstract over a client connection to a remote service.
///
/// This isn't really that much abstracted, since the result of open_bi must
/// still be a noq::SendStream and noq::RecvStream. This is just so we
/// can have different connection implementations for normal noq connections,
/// iroh connections, and possibly noq connections with disabled encryption
/// for performance.
pub trait RemoteConnection: Send + Sync + Debug + 'static {
    /// Boxed clone so the trait is dynable.
    fn clone_boxed(&self) -> Box<dyn RemoteConnection>;

    /// Open a bidirectional stream to the remote service.
    fn open_bi(
        &self,
    ) -> BoxFuture<std::result::Result<(noq::SendStream, noq::RecvStream), RequestError>>;

    /// Returns whether 0-RTT data was rejected by the server.
    ///
    /// For connections that were fully authenticated before allowing to send any data, this should return `false`.
    fn zero_rtt_rejected(&self) -> BoxFuture<bool>;

    /// Returns a copy of this connection that runs `hook` on each new connection.
    ///
    /// Returns `None` if this connection does not open connections itself.
    fn with_connect_hook(&self, hook: ConnectHook) -> Option<Box<dyn RemoteConnection>> {
        let _ = hook;
        None
    }

    /// Opens the connection now, if it is not open.
    fn connect(&self) -> BoxFuture<std::result::Result<(), RequestError>> {
        Box::pin(async { Ok(()) })
    }
}

/// A function that a client runs on each new connection, before any request.
///
/// Create it with [`Client::on_connect`](crate::Client::on_connect).
#[derive(Clone)]
pub struct ConnectHook(pub(crate) Arc<ConnectFn>);

type ConnectFn =
    dyn Fn(Box<dyn RemoteConnection>) -> BoxFuture<std::result::Result<(), AnyError>> + Send + Sync;

impl Debug for ConnectHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectHook").finish_non_exhaustive()
    }
}

impl ConnectHook {
    /// Runs the hook for `connection`.
    pub(crate) async fn run(
        &self,
        connection: Box<dyn RemoteConnection>,
    ) -> std::result::Result<(), RequestError> {
        (self.0)(connection)
            .await
            .map_err(|err| e!(RequestError::Other, err))
    }
}

/// A connection to a remote service that can be used to send the initial message.
#[derive(Debug)]
pub struct RemoteSender<S>(
    noq::SendStream,
    noq::RecvStream,
    std::marker::PhantomData<S>,
);

/// Serialize a message for sending over the wire.
///
/// When `S::SPAN_PROPAGATION` is true, the message is wrapped in a tuple with
/// span context: `(Option<SpanContextCarrier>, msg)`.
/// When false, the message is serialized directly.
pub(crate) fn prepare_write<S: Service>(
    msg: impl Into<S>,
) -> Result<SmallVec<[u8; 128]>, WriteError> {
    let msg = msg.into();
    let mut buf = SmallVec::<[u8; 128]>::new();

    if S::SPAN_PROPAGATION {
        // Include span context in wire format
        let span_ctx = Some(crate::span_propagation::SpanContextCarrier::from_current());
        let payload = (span_ctx, msg);
        if postcard::experimental::serialized_size(&payload)? as u64 > MAX_MESSAGE_SIZE {
            return Err(e!(WriteError::MaxMessageSizeExceeded));
        }
        buf.write_length_prefixed(&payload)?;
    } else {
        // Original wire format without span context
        if postcard::experimental::serialized_size(&msg)? as u64 > MAX_MESSAGE_SIZE {
            return Err(e!(WriteError::MaxMessageSizeExceeded));
        }
        buf.write_length_prefixed(&msg)?;
    }

    Ok(buf)
}

impl<S: Service> RemoteSender<S> {
    pub fn new(send: noq::SendStream, recv: noq::RecvStream) -> Self {
        Self(send, recv, PhantomData)
    }

    pub async fn write(
        self,
        msg: impl Into<S>,
    ) -> std::result::Result<(noq::SendStream, noq::RecvStream), WriteError> {
        let buf = prepare_write(msg)?;
        self.write_raw(&buf).await
    }

    pub(crate) async fn write_raw(
        self,
        buf: &[u8],
    ) -> std::result::Result<(noq::SendStream, noq::RecvStream), WriteError> {
        let RemoteSender(mut send, recv, _) = self;
        send.write_all(buf).await?;
        Ok((send, recv))
    }
}

impl<T: DeserializeOwned> From<noq::RecvStream> for oneshot::Receiver<T> {
    fn from(mut read: noq::RecvStream) -> Self {
        let fut = async move {
            let size = read.read_varint_u64().await?.ok_or(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "failed to read size",
            ))?;
            if size > MAX_MESSAGE_SIZE {
                read.stop(ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED.into()).ok();
                return Err(e!(oneshot::RecvError::MaxMessageSizeExceeded));
            }
            let rest = read
                .read_to_end(size as usize)
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let msg: T = postcard::from_bytes(&rest)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(msg)
        };
        oneshot::Receiver::from(|| fut)
    }
}

impl From<noq::RecvStream> for crate::channel::none::NoReceiver {
    fn from(read: noq::RecvStream) -> Self {
        drop(read);
        Self
    }
}

impl<T: RpcMessage> From<noq::RecvStream> for mpsc::Receiver<T> {
    fn from(read: noq::RecvStream) -> Self {
        mpsc::Receiver::Boxed(Box::new(NoqReceiver {
            recv: read,
            _marker: PhantomData,
        }))
    }
}

impl From<noq::SendStream> for NoSender {
    fn from(write: noq::SendStream) -> Self {
        let _ = write;
        NoSender
    }
}

impl<T: RpcMessage> From<noq::SendStream> for oneshot::Sender<T> {
    fn from(mut writer: noq::SendStream) -> Self {
        oneshot::Sender::Boxed(Box::new(move |value| {
            Box::pin(async move {
                let size = match postcard::experimental::serialized_size(&value) {
                    Ok(size) => size,
                    Err(e) => {
                        writer.reset(ERROR_CODE_INVALID_POSTCARD.into()).ok();
                        return Err(e!(
                            SendError::Io,
                            io::Error::new(io::ErrorKind::InvalidData, e,)
                        ));
                    }
                };
                if size as u64 > MAX_MESSAGE_SIZE {
                    writer
                        .reset(ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED.into())
                        .ok();
                    return Err(e!(SendError::MaxMessageSizeExceeded));
                }
                // write via a small buffer to avoid allocation for small values
                let mut buf = SmallVec::<[u8; 128]>::new();
                if let Err(e) = buf.write_length_prefixed(value) {
                    writer.reset(ERROR_CODE_INVALID_POSTCARD.into()).ok();
                    return Err(e.into());
                }
                writer.write_all(&buf).await?;
                Ok(())
            })
        }))
    }
}

impl<T: RpcMessage> From<noq::SendStream> for mpsc::Sender<T> {
    fn from(write: noq::SendStream) -> Self {
        mpsc::Sender::Boxed(Arc::new(NoqSender(tokio::sync::Mutex::new(
            NoqSenderState::Open(NoqSenderInner {
                send: write,
                buffer: SmallVec::new(),
                _marker: PhantomData,
            }),
        ))))
    }
}

struct NoqReceiver<T> {
    recv: noq::RecvStream,
    _marker: std::marker::PhantomData<T>,
}

impl<T> Debug for NoqReceiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoqReceiver").finish()
    }
}

impl<T: RpcMessage> DynReceiver<T> for NoqReceiver<T> {
    fn recv(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<T>, mpsc::RecvError>> + Send + Sync + '_>> {
        Box::pin(async {
            let read = &mut self.recv;
            let Some(size) = read.read_varint_u64().await? else {
                return Ok(None);
            };
            if size > MAX_MESSAGE_SIZE {
                self.recv
                    .stop(ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED.into())
                    .ok();
                return Err(e!(mpsc::RecvError::MaxMessageSizeExceeded));
            }
            let mut buf = vec![0; size as usize];
            read.read_exact(&mut buf)
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::UnexpectedEof, e))?;
            let msg: T = postcard::from_bytes(&buf)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(Some(msg))
        })
    }
}

impl<T> Drop for NoqReceiver<T> {
    fn drop(&mut self) {}
}

struct NoqSenderInner<T> {
    send: noq::SendStream,
    buffer: SmallVec<[u8; 128]>,
    _marker: std::marker::PhantomData<T>,
}

impl<T: RpcMessage> NoqSenderInner<T> {
    fn send(
        &mut self,
        value: T,
    ) -> Pin<Box<dyn Future<Output = Result<(), SendError>> + Send + Sync + '_>> {
        Box::pin(async {
            let size = match postcard::experimental::serialized_size(&value) {
                Ok(size) => size,
                Err(e) => {
                    self.send.reset(ERROR_CODE_INVALID_POSTCARD.into()).ok();
                    return Err(e!(
                        SendError::Io,
                        io::Error::new(io::ErrorKind::InvalidData, e)
                    ));
                }
            };
            if size as u64 > MAX_MESSAGE_SIZE {
                self.send
                    .reset(ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED.into())
                    .ok();
                return Err(e!(SendError::MaxMessageSizeExceeded));
            }
            let value = value;
            self.buffer.clear();
            if let Err(e) = self.buffer.write_length_prefixed(value) {
                self.send.reset(ERROR_CODE_INVALID_POSTCARD.into()).ok();
                return Err(e.into());
            }
            self.send.write_all(&self.buffer).await?;
            self.buffer.clear();
            Ok(())
        })
    }

    fn try_send(
        &mut self,
        value: T,
    ) -> Pin<Box<dyn Future<Output = Result<bool, SendError>> + Send + Sync + '_>> {
        Box::pin(async {
            if postcard::experimental::serialized_size(&value)? as u64 > MAX_MESSAGE_SIZE {
                return Err(e!(SendError::MaxMessageSizeExceeded));
            }
            // todo: move the non-async part out of the box. Will require a new return type.
            let value = value;
            self.buffer.clear();
            self.buffer.write_length_prefixed(value)?;
            let Some(n) = now_or_never(self.send.write(&self.buffer)) else {
                return Ok(false);
            };
            let n = n?;
            self.send.write_all(&self.buffer[n..]).await?;
            self.buffer.clear();
            Ok(true)
        })
    }

    fn closed(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + Sync + '_>> {
        Box::pin(async move {
            self.send.stopped().await.ok();
        })
    }
}

#[derive(Default)]
enum NoqSenderState<T> {
    Open(NoqSenderInner<T>),
    #[default]
    Closed,
}

struct NoqSender<T>(tokio::sync::Mutex<NoqSenderState<T>>);

impl<T> Debug for NoqSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoqSender").finish()
    }
}

impl<T: RpcMessage> DynSender<T> for NoqSender<T> {
    fn send(&self, value: T) -> Pin<Box<dyn Future<Output = Result<(), SendError>> + Send + '_>> {
        Box::pin(async {
            let mut guard = self.0.lock().await;
            let sender = std::mem::take(guard.deref_mut());
            match sender {
                NoqSenderState::Open(mut sender) => {
                    let res = sender.send(value).await;
                    if res.is_ok() {
                        *guard = NoqSenderState::Open(sender);
                    }
                    res
                }
                NoqSenderState::Closed => Err(io::Error::from(io::ErrorKind::BrokenPipe).into()),
            }
        })
    }

    fn try_send(
        &self,
        value: T,
    ) -> Pin<Box<dyn Future<Output = Result<bool, SendError>> + Send + '_>> {
        Box::pin(async {
            let mut guard = self.0.lock().await;
            let sender = std::mem::take(guard.deref_mut());
            match sender {
                NoqSenderState::Open(mut sender) => {
                    let res = sender.try_send(value).await;
                    if res.is_ok() {
                        *guard = NoqSenderState::Open(sender);
                    }
                    res
                }
                NoqSenderState::Closed => Err(io::Error::from(io::ErrorKind::BrokenPipe).into()),
            }
        })
    }

    fn closed(&self) -> Pin<Box<dyn Future<Output = ()> + Send + Sync + '_>> {
        Box::pin(async {
            let mut guard = self.0.lock().await;
            match guard.deref_mut() {
                NoqSenderState::Open(sender) => sender.closed().await,
                NoqSenderState::Closed => {}
            }
        })
    }

    fn is_rpc(&self) -> bool {
        true
    }
}

/// Type alias for a handler fn for remote requests
pub type Handler<R> = Arc<
    dyn Fn(R, noq::RecvStream, noq::SendStream) -> BoxFuture<std::result::Result<(), SendError>>
        + Send
        + Sync
        + 'static,
>;

/// Extension trait to [`Service`] to create a [`Service::Message`] from a [`Service`]
/// and a pair of QUIC streams.
///
/// This trait is auto-implemented when using the [`crate::rpc_requests`] macro.
pub trait RemoteService: Service + Sized {
    /// Returns the message enum for this request by combining `self` (the protocol enum)
    /// with a pair of QUIC streams for `tx` and `rx` channels.
    fn with_remote_channels(self, rx: noq::RecvStream, tx: noq::SendStream) -> Self::Message;

    /// Creates a [`Handler`] that forwards all messages to a [`LocalSender`].
    fn remote_handler(local_sender: LocalSender<Self>) -> Handler<Self> {
        Arc::new(move |msg, rx, tx| {
            // `with_remote_channels` reads the task-local span context installed by
            // the dispatch loop, so it must run inside the future (which is polled
            // within that scope) rather than eagerly here.
            let local_sender = local_sender.clone();
            Box::pin(async move {
                let msg = Self::with_remote_channels(msg, rx, tx);
                local_sender.send_raw(msg).await
            })
        })
    }
}

/// Abstracts over the connections that a server can read requests from.
///
/// This is implemented for noq connections, and for iroh connections with and
/// without 0-RTT. You don't need to implement this trait yourself. It is used by
/// [`read_request`] and [`handle_connection`] to work with all of these.
pub trait IncomingRemoteConnection {
    /// Accepts a single bidirectional stream.
    fn accept_bi(
        &self,
    ) -> impl Future<Output = Result<(noq::SendStream, noq::RecvStream), ConnectionError>> + Send;

    /// Closes the connection.
    fn close(&self, error_code: VarInt, reason: &[u8]);

    /// Returns a label for the remote side, for use in tracing spans.
    ///
    /// Returns `None` if the remote is not known yet, which can happen for
    /// 0-RTT connections.
    fn remote_label(&self) -> Option<String>;
}

/// Handles a single connection with the provided `handler`.
///
/// This function handles requests for a service `S`. The wire format used depends on
/// `S::SPAN_PROPAGATION` - if true, span context is expected in the wire format.
pub async fn handle_connection<S: Service>(
    connection: &impl IncomingRemoteConnection,
    handler: Handler<S>,
) -> io::Result<()> {
    if let Some(remote) = connection.remote_label() {
        tracing::Span::current().record("remote", tracing::field::display(remote));
    }
    debug!("connection accepted");
    loop {
        let Some((msg, carrier, rx, tx)) = read_request_inner::<S>(connection).await? else {
            return Ok(());
        };
        crate::span_propagation::scope_remote(carrier, handler(msg, rx, tx)).await?;
    }
}

/// Reads a request from a connection and converts it to a message enum.
///
/// This combines `read_request_raw` with `RemoteService::with_remote_channels`.
pub async fn read_request<S: RemoteService>(
    connection: &impl IncomingRemoteConnection,
) -> std::io::Result<Option<S::Message>> {
    let Some((msg, carrier, rx, tx)) = read_request_inner::<S>(connection).await? else {
        return Ok(None);
    };
    Ok(Some(
        crate::span_propagation::scope_remote(carrier, async move {
            S::with_remote_channels(msg, rx, tx)
        })
        .await,
    ))
}

/// Reads a single request from the connection.
///
/// This accepts a bi-directional stream from the connection and reads and parses the request.
///
/// When `S::SPAN_PROPAGATION` is true, any propagated span context on the wire is
/// silently dropped. Use [`handle_connection`] (or [`read_request`]) if you need
/// the propagated context to reach the generated handler spans.
///
/// Returns the parsed request and the stream pair if reading and parsing the request succeeded.
/// Returns None if the remote closed the connection with error code `0`.
/// Returns an error for all other failure cases.
pub async fn read_request_raw<S: Service>(
    connection: &impl IncomingRemoteConnection,
) -> std::io::Result<Option<(S, noq::RecvStream, noq::SendStream)>> {
    Ok(read_request_inner::<S>(connection)
        .await?
        .map(|(msg, _carrier, rx, tx)| (msg, rx, tx)))
}

/// Internal: read a request and also return the propagated span context carrier.
///
/// The carrier is `Some` iff `S::SPAN_PROPAGATION` is true and the remote sent one.
async fn read_request_inner<S: Service>(
    connection: &impl IncomingRemoteConnection,
) -> std::io::Result<
    Option<(
        S,
        Option<crate::span_propagation::SpanContextCarrier>,
        noq::RecvStream,
        noq::SendStream,
    )>,
> {
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
        return Err(e!(mpsc::RecvError::MaxMessageSizeExceeded).into());
    }
    let mut buf = vec![0; size as usize];
    recv.read_exact(&mut buf)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::UnexpectedEof, e))?;

    let (carrier, msg): (Option<crate::span_propagation::SpanContextCarrier>, S) =
        if S::SPAN_PROPAGATION {
            postcard::from_bytes(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        } else {
            let msg = postcard::from_bytes(&buf)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            (None, msg)
        };

    Ok(Some((msg, carrier, recv, send)))
}
