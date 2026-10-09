//! Module for cross-process RPC.
//!
//! The code in this module works with all connections that use [`noq`]
//! streams. The [`crate::noq`] module has the transport with dial by socket
//! address. The [`crate::iroh`] module has the transport with dial by endpoint id.
//!
//! # Bad requests
//!
//! Each request has its own stream. If a request is bad, the server stops and
//! resets the streams of that request:
//!
//! * with [`ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED`] if the request is larger
//!   than [`MAX_MESSAGE_SIZE`],
//! * with [`ERROR_CODE_DECODE_FAILED`] if the request does not decode. For
//!   example, a client of a newer protocol version sent a request type that
//!   the server does not know. A stream that ends before its request is
//!   complete is also a request that does not decode.
//!
//! By default, a [`Handler`] then closes the connection with the same code, as
//! for any other protocol violation. A protocol that adds request types at the
//! end of its enum can use [`Handler::skip_bad_requests`]: the handler then
//! reads the next request, so an old server rejects the new requests and keeps
//! serving the old ones on the same connection.
//!
//! [`read_request`] returns a bad request as a [`ReadRequestError`]. A server
//! with its own loop decides what to do: close the connection, or read the
//! next request.
//!
//! A client that drops a request before it wrote all of it resets the stream
//! with [`ERROR_CODE_ABORTED`]. For example, it dropped the future of
//! [`Client::rpc`](crate::Client::rpc) while it wrote a large request. The
//! server skips a request whose stream was reset. It resets its side of the
//! stream with code 0, and reads the next request.
//!
//! A remote channel receiver also stops its stream with
//! [`ERROR_CODE_DECODE_FAILED`] if a message does not decode. Its sender then
//! gets an error on the next send.
//!
//! # Error codes
//!
//! irpc reserves the codes 0 to 15 for streams and connections. Code 0 means
//! no error, for example a normal close. irpc also uses
//! [`ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED`], [`ERROR_CODE_ENCODE_FAILED`],
//! [`ERROR_CODE_DECODE_FAILED`], and [`ERROR_CODE_ABORTED`], and can add more
//! codes in this range in a later version.
//!
//! An application should use codes from 16 upward, so that a peer can tell its
//! codes apart from the codes of irpc. irpc does not check this. A code below
//! 16 works, but a later version of irpc can give it a different meaning.
use std::{
    fmt::Debug, future::Future, io, marker::PhantomData, ops::DerefMut, pin::Pin, sync::Arc,
};

use n0_error::{e, stack_error};
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

/// Error code on streams and connections if a message is larger than [`MAX_MESSAGE_SIZE`].
///
/// See [Error codes](self#error-codes) for the codes that irpc reserves.
pub const ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED: u32 = 1;

/// Error code on streams if the sender could not encode a message.
pub const ERROR_CODE_ENCODE_FAILED: u32 = 2;

/// Error code on streams and connections if the receiver could not decode a message.
///
/// A server stops and resets the streams of a request that does not decode,
/// and by default closes the connection with this code. A remote channel
/// receiver stops its stream if a message does not decode.
pub const ERROR_CODE_DECODE_FAILED: u32 = 3;

/// Error code on a request stream if the client drops the request before it is written.
///
/// The server skips such a request, see [Bad requests](self#bad-requests).
pub const ERROR_CODE_ABORTED: u32 = 4;

/// Error when reading a request with [`read_request`].
///
/// For [`MaxMessageSizeExceeded`](Self::MaxMessageSizeExceeded),
/// [`InvalidRequest`](Self::InvalidRequest), and
/// [`DecodeFailed`](Self::DecodeFailed), irpc resets the streams of the
/// request, so its client gets an error. The connection is still usable, so a
/// server can read the next request.
#[stack_error(derive, add_meta)]
#[non_exhaustive]
pub enum ReadRequestError {
    /// The request is larger than [`MAX_MESSAGE_SIZE`].
    #[error("Maximum message size exceeded")]
    MaxMessageSizeExceeded,
    /// The request is not framed correctly.
    ///
    /// For example, the stream ended before the request was complete.
    #[error("Invalid request")]
    InvalidRequest {
        #[error(std_err)]
        source: io::Error,
    },
    /// The request does not decode.
    ///
    /// For example, a newer client sent a request type that this server does
    /// not know.
    #[error("Request does not decode")]
    DecodeFailed {
        #[error(std_err)]
        source: postcard::Error,
    },
    /// The connection failed.
    #[error("Connection failed")]
    Connection {
        #[error(std_err)]
        source: ConnectionError,
    },
}

impl ReadRequestError {
    /// Returns the irpc error code for a bad request, or `None` for a connection error.
    fn error_code(&self) -> Option<u32> {
        match self {
            Self::MaxMessageSizeExceeded { .. } => Some(ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED),
            Self::InvalidRequest { .. } | Self::DecodeFailed { .. } => {
                Some(ERROR_CODE_DECODE_FAILED)
            }
            Self::Connection { .. } => None,
        }
    }
}

impl From<ReadRequestError> for io::Error {
    fn from(err: ReadRequestError) -> Self {
        match err {
            ReadRequestError::Connection { source, .. } => source.into(),
            err => io::Error::new(io::ErrorKind::InvalidData, err),
        }
    }
}

/// Error that can occur when writing the initial message when doing a
/// cross-process RPC.
#[stack_error(derive, add_meta, from_sources)]
#[non_exhaustive]
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
            // A receiver that is dropped stops the stream with code 0.
            noq::WriteError::Stopped(code) if code == 0u32.into() => e!(SendError::ReceiverClosed),
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
}

/// A connection to a remote service that can be used to send the initial message.
///
/// If it drops before the message is written, it resets the stream with
/// [`ERROR_CODE_ABORTED`].
#[derive(Debug)]
pub struct RemoteSender<S>(ResetOnDrop, noq::RecvStream, std::marker::PhantomData<S>);

/// A send stream that resets with [`ERROR_CODE_ABORTED`] if it drops before [`Self::into_inner`].
#[derive(Debug)]
struct ResetOnDrop(Option<noq::SendStream>);

impl ResetOnDrop {
    fn get_mut(&mut self) -> &mut noq::SendStream {
        self.0.as_mut().expect("only `into_inner` takes the stream")
    }

    fn into_inner(mut self) -> noq::SendStream {
        self.0.take().expect("only `into_inner` takes the stream")
    }
}

impl Drop for ResetOnDrop {
    fn drop(&mut self) {
        if let Some(send) = &mut self.0 {
            send.reset(ERROR_CODE_ABORTED.into()).ok();
        }
    }
}

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
        Self(ResetOnDrop(Some(send)), recv, PhantomData)
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
        send.get_mut().write_all(buf).await?;
        Ok((send.into_inner(), recv))
    }
}

impl<T: DeserializeOwned> From<noq::RecvStream> for oneshot::Receiver<T> {
    fn from(mut read: noq::RecvStream) -> Self {
        let fut = async move {
            // A sender that is dropped without sending finishes the stream.
            let Some(size) = read.read_varint_u64().await? else {
                return Err(e!(oneshot::RecvError::SenderClosed));
            };
            if size > MAX_MESSAGE_SIZE {
                read.stop(ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED.into()).ok();
                return Err(e!(oneshot::RecvError::MaxMessageSizeExceeded));
            }
            let rest = read
                .read_to_end(size as usize)
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let msg: T = postcard::from_bytes(&rest).map_err(|e| {
                read.stop(ERROR_CODE_DECODE_FAILED.into()).ok();
                io::Error::new(io::ErrorKind::InvalidData, e)
            })?;
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
                        writer.reset(ERROR_CODE_ENCODE_FAILED.into()).ok();
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
                    writer.reset(ERROR_CODE_ENCODE_FAILED.into()).ok();
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
            let msg: T = postcard::from_bytes(&buf).map_err(|e| {
                read.stop(ERROR_CODE_DECODE_FAILED.into()).ok();
                io::Error::new(io::ErrorKind::InvalidData, e)
            })?;
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
    /// Encodes `value` into the buffer.
    ///
    /// Resets the stream if `value` is too large or does not encode, so the
    /// receiver gets an error instead of the end of the stream.
    fn encode(&mut self, value: T) -> Result<(), SendError> {
        let size = match postcard::experimental::serialized_size(&value) {
            Ok(size) => size,
            Err(e) => {
                self.send.reset(ERROR_CODE_ENCODE_FAILED.into()).ok();
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
        self.buffer.clear();
        if let Err(e) = self.buffer.write_length_prefixed(value) {
            self.send.reset(ERROR_CODE_ENCODE_FAILED.into()).ok();
            return Err(e.into());
        }
        Ok(())
    }

    fn send(
        &mut self,
        value: T,
    ) -> Pin<Box<dyn Future<Output = Result<(), SendError>> + Send + Sync + '_>> {
        Box::pin(async {
            self.encode(value)?;
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
            // todo: move the non-async part out of the box. Will require a new return type.
            self.encode(value)?;
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

enum NoqSenderState<T> {
    Open(NoqSenderInner<T>),
    Closed(CloseReason),
}

#[derive(Debug, Clone, Copy)]
enum CloseReason {
    ReceiverClosed,
    Other,
}

impl CloseReason {
    fn error(self) -> SendError {
        match self {
            Self::ReceiverClosed => e!(SendError::ReceiverClosed),
            Self::Other => io::Error::from(io::ErrorKind::BrokenPipe).into(),
        }
    }
}

impl<T> NoqSenderState<T> {
    /// Takes the state for a send, and leaves the sender closed until the send puts it back.
    ///
    /// A cancelled send never puts it back, so the sender stays closed: the
    /// stream can hold part of a message.
    fn take_for_send(&mut self) -> Self {
        std::mem::replace(self, Self::Closed(CloseReason::Other))
    }

    /// Returns the state after a send, so that later sends fail with the same kind of error.
    fn after_send<R>(sender: NoqSenderInner<T>, res: &Result<R, SendError>) -> Self {
        match res {
            Ok(_) => Self::Open(sender),
            Err(SendError::ReceiverClosed { .. }) => Self::Closed(CloseReason::ReceiverClosed),
            Err(_) => Self::Closed(CloseReason::Other),
        }
    }
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
            let sender = guard.take_for_send();
            match sender {
                NoqSenderState::Open(mut sender) => {
                    let res = sender.send(value).await;
                    *guard = NoqSenderState::after_send(sender, &res);
                    res
                }
                NoqSenderState::Closed(reason) => {
                    *guard = NoqSenderState::Closed(reason);
                    Err(reason.error())
                }
            }
        })
    }

    fn try_send(
        &self,
        value: T,
    ) -> Pin<Box<dyn Future<Output = Result<bool, SendError>> + Send + '_>> {
        Box::pin(async {
            let mut guard = self.0.lock().await;
            let sender = guard.take_for_send();
            match sender {
                NoqSenderState::Open(mut sender) => {
                    let res = sender.try_send(value).await;
                    *guard = NoqSenderState::after_send(sender, &res);
                    res
                }
                NoqSenderState::Closed(reason) => {
                    *guard = NoqSenderState::Closed(reason);
                    Err(reason.error())
                }
            }
        })
    }

    fn closed(&self) -> Pin<Box<dyn Future<Output = ()> + Send + Sync + '_>> {
        Box::pin(async {
            let mut guard = self.0.lock().await;
            match guard.deref_mut() {
                NoqSenderState::Open(sender) => sender.closed().await,
                NoqSenderState::Closed(_) => {}
            }
        })
    }

    fn is_rpc(&self) -> bool {
        true
    }
}

/// The function inside a [`Handler`].
type HandlerFn<S> = Arc<
    dyn Fn(S, noq::RecvStream, noq::SendStream) -> BoxFuture<std::result::Result<(), HandlerError>>
        + Send
        + Sync
        + 'static,
>;

/// The error that a handler function returns to close the connection.
///
/// Currently the only constructor is [`Self::close_connection`]. The server
/// then closes the connection.
#[derive(Debug)]
pub struct HandlerError {
    inner: HandlerErrorInner,
}

#[derive(Debug)]
enum HandlerErrorInner {
    CloseConnection { code: VarInt, reason: Vec<u8> },
}

impl std::error::Error for HandlerError {}

impl std::fmt::Display for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.inner {
            HandlerErrorInner::CloseConnection { code, reason } => write!(
                f,
                "handler asked to close the connection with code {code}: {}",
                String::from_utf8_lossy(reason)
            ),
        }
    }
}

impl HandlerError {
    /// Creates a [`HandlerError`] that closes the connection with `code` and `reason`.
    ///
    /// When a handler function returns this error, the server closes the connection
    /// immediately and reads no more requests from it. This also ends the streams of
    /// all other requests on the connection, including streams that a handler moved
    /// into a spawned task.
    ///
    /// The code should be 16 or higher, see [Error codes](self#error-codes). irpc
    /// closes a connection with code 0 for a normal close, which
    /// [`Handler::from_sender`] also uses when its receiver is gone. For a bad
    /// request, irpc closes the connection with
    /// [`ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED`] or [`ERROR_CODE_DECODE_FAILED`].
    ///
    /// # Examples
    ///
    /// ```
    /// use irpc::{
    ///     channel::oneshot,
    ///     rpc::{Handler, HandlerError},
    ///     rpc_requests,
    /// };
    /// use serde::{Deserialize, Serialize};
    ///
    /// #[rpc_requests(message = AuthMessage)]
    /// #[derive(Debug, Serialize, Deserialize)]
    /// enum AuthProtocol {
    ///     #[rpc(tx = oneshot::Sender<()>)]
    ///     #[wrap(Auth)]
    ///     Auth(String),
    /// }
    ///
    /// let handler = Handler::<AuthProtocol>::sequential(|msg| async move {
    ///     let AuthMessage::Auth(msg) = msg;
    ///     if msg.inner.0 != "secret" {
    ///         Err(HandlerError::close_connection(401, "permission denied"))
    ///     } else {
    ///         msg.tx.send(()).await.ok();
    ///         Ok(())
    ///     }
    /// });
    /// ```
    pub fn close_connection(code: u32, reason: impl AsRef<[u8]>) -> Self {
        Self {
            inner: HandlerErrorInner::CloseConnection {
                code: VarInt::from_u32(code),
                reason: reason.as_ref().to_vec(),
            },
        }
    }
}

/// Handles the requests that a server reads from a connection.
///
/// A server uses one handler for all its connections. Create one with:
///
/// - [`Handler::from_sender`]: sends each request to a [`LocalSender`].
/// - [`Handler::sequential`]: runs a function on each request, one at a time.
/// - [`Handler::raw`]: runs a function on the protocol enum and the streams.
///
/// A handler function can return `Err(HandlerError)` to close the connection.
/// In all other cases, handle errors internally and return `Ok(())` from
/// the handler function to read the next request from the connection.
/// A bad request also closes the connection, unless
/// [`Handler::skip_bad_requests`] is set.
///
/// Often, the protocol type cannot be inferred from the message enum,
/// so you may have to name it: `Handler::<MyProtocol>::sequential(..)`.
///
/// # Examples
///
/// ```
/// use irpc::{WithChannels, channel::oneshot, rpc::Handler, rpc_requests};
/// use serde::{Deserialize, Serialize};
///
/// #[rpc_requests(message = EchoMessage)]
/// #[derive(Debug, Serialize, Deserialize)]
/// enum EchoProtocol {
///     #[rpc(tx = oneshot::Sender<String>)]
///     #[wrap(Echo)]
///     Echo(String),
/// }
///
/// let handler: Handler<EchoProtocol> = Handler::sequential(|msg| async move {
///     match msg {
///         EchoMessage::Echo(msg) => {
///             let WithChannels { inner, tx, .. } = msg;
///             tx.send(inner.0).await.ok();
///         }
///     }
///     Ok(())
/// });
/// ```
#[derive(derive_more::Debug)]
pub struct Handler<S> {
    #[debug(skip)]
    f: HandlerFn<S>,
    skip_bad_requests: bool,
}

impl<S> Clone for Handler<S> {
    fn clone(&self) -> Self {
        Self {
            f: self.f.clone(),
            skip_bad_requests: self.skip_bad_requests,
        }
    }
}

impl<S: Service> Handler<S> {
    /// Creates a handler that runs `f` on the protocol enum and the streams of each request.
    ///
    /// The requests of a connection run one after the other. If `f` needs the
    /// message enum, call [`RemoteService::with_remote_channels`] inside its
    /// future: the remote span context is only set while the future runs.
    pub fn raw<F, Fut>(f: F) -> Self
    where
        F: Fn(S, noq::RecvStream, noq::SendStream) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::result::Result<(), HandlerError>> + Send + 'static,
    {
        let f: HandlerFn<S> = Arc::new(move |msg, rx, tx| Box::pin(f(msg, rx, tx)));
        Self {
            f,
            skip_bad_requests: false,
        }
    }

    /// Runs this handler on one request.
    ///
    /// Use it in a [`Handler::raw`] to pass a request to another handler.
    pub fn call(
        &self,
        request: S,
        rx: noq::RecvStream,
        tx: noq::SendStream,
    ) -> impl Future<Output = std::result::Result<(), HandlerError>> + Send + 'static {
        (self.f)(request, rx, tx)
    }

    /// Sets whether a bad request is skipped instead of closing the connection.
    ///
    /// By default, a request that is too large or does not decode closes the
    /// connection. With `true`, only the request fails, and the handler reads
    /// the next request. Use it for a protocol that adds request types over
    /// time, see [Bad requests](self#bad-requests).
    pub fn skip_bad_requests(mut self, skip: bool) -> Self {
        self.skip_bad_requests = skip;
        self
    }

    /// Handles the requests of `connection` until the connection ends.
    ///
    /// Returns an error if the connection fails, or if a bad request closed it.
    pub async fn handle_connection(
        &self,
        connection: &impl IncomingRemoteConnection,
    ) -> io::Result<()> {
        debug!("connection accepted");
        loop {
            let Some((msg, carrier, rx, tx)) = self.read_next(connection).await? else {
                return Ok(());
            };
            let fut = (self.f)(msg, rx, tx);
            let fut = crate::span_propagation::scope_remote(carrier, fut);
            if let Err(err) = fut.await {
                match err.inner {
                    HandlerErrorInner::CloseConnection { code, reason } => {
                        debug!(%code, "handler closed the connection");
                        connection.close(code, &reason);
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Reads the next request, and closes the connection or skips it on a bad request.
    async fn read_next(
        &self,
        connection: &impl IncomingRemoteConnection,
    ) -> io::Result<Option<Request<S>>> {
        loop {
            match read_request_inner::<S>(connection).await {
                Ok(request) => return Ok(request),
                Err(err) => match err.error_code() {
                    None => return Err(err.into()), // A connection error.
                    Some(_) if self.skip_bad_requests => debug!("skipped bad request: {err:#}"),
                    Some(code) => {
                        connection.close(code.into(), err.to_string().as_bytes());
                        return Err(err.into());
                    }
                },
            }
        }
    }
}

impl<S: RemoteService> Handler<S> {
    /// Creates a handler that sends each request to `local_sender`.
    ///
    /// A full channel applies backpressure to the connection.
    pub fn from_sender(local_sender: impl Into<LocalSender<S>>) -> Self {
        let local_sender = local_sender.into();
        Self::sequential(move |msg| {
            let local_sender = local_sender.clone();
            async move {
                local_sender.send_raw(msg).await.map_err(|err| {
                    // The receiver is gone. Close as a dropped connection does.
                    warn!("handler stopped: {err:#}");
                    HandlerError::close_connection(0, b"")
                })
            }
        })
    }

    /// Creates a handler that runs `f` on each request, one at a time per connection.
    pub fn sequential<F, Fut>(f: F) -> Self
    where
        F: Fn(S::Message) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::result::Result<(), HandlerError>> + Send + 'static,
    {
        let f = Arc::new(f);
        Self::raw(move |msg, rx, tx| {
            let f = f.clone();
            async move {
                // Inside the future: the remote span context is only set while it runs.
                f(S::with_remote_channels(msg, rx, tx)).await
            }
        })
    }
}

impl<S: RemoteService> From<LocalSender<S>> for Handler<S> {
    fn from(local_sender: LocalSender<S>) -> Self {
        Self::from_sender(local_sender)
    }
}

/// Extension trait to [`Service`] to create a [`Service::Message`] from a [`Service`]
/// and a pair of QUIC streams.
///
/// This trait is auto-implemented when using the [`crate::rpc_requests`] macro.
pub trait RemoteService: Service + Sized {
    /// Returns the message enum for this request by combining `self` (the protocol enum)
    /// with a pair of QUIC streams for `tx` and `rx` channels.
    fn with_remote_channels(self, rx: noq::RecvStream, tx: noq::SendStream) -> Self::Message;
}

/// Abstracts over the connections that a server can read requests from.
///
/// This is implemented for noq connections, and for iroh connections with and
/// without 0-RTT. It is used by [`read_request`] and [`Handler::handle_connection`]
/// to work with all of these.
///
/// This trait is sealed: only irpc can implement it. So irpc can add methods to
/// it without a breaking change.
pub trait IncomingRemoteConnection: crate::sealed::Sealed {
    /// Accepts a single bidirectional stream.
    fn accept_bi(
        &self,
    ) -> impl Future<Output = Result<(noq::SendStream, noq::RecvStream), ConnectionError>> + Send;

    /// Closes the connection.
    fn close(&self, error_code: VarInt, reason: &[u8]);
}

/// Reads a request from a connection and converts it to a message enum.
///
/// This combines `read_request_raw` with `RemoteService::with_remote_channels`.
///
/// Returns `None` if the remote closed the connection with code 0, or if this
/// side closed it. Skips a request that the client abandoned.
///
/// Returns [`ReadRequestError::MaxMessageSizeExceeded`],
/// [`ReadRequestError::InvalidRequest`], or [`ReadRequestError::DecodeFailed`]
/// for a bad request. The connection is still open after these errors, see
/// [Bad requests](self#bad-requests).
pub async fn read_request<S: RemoteService>(
    connection: &impl IncomingRemoteConnection,
) -> Result<Option<S::Message>, ReadRequestError> {
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
/// silently dropped. Use [`Handler::handle_connection`] (or [`read_request`]) if you need
/// the propagated context to reach the generated handler spans.
///
/// Returns `None` in the same cases as [`read_request`].
pub async fn read_request_raw<S: Service>(
    connection: &impl IncomingRemoteConnection,
) -> Result<Option<(S, noq::RecvStream, noq::SendStream)>, ReadRequestError> {
    Ok(read_request_inner::<S>(connection)
        .await?
        .map(|(msg, _carrier, rx, tx)| (msg, rx, tx)))
}

/// A request with its span context carrier and its streams.
type Request<S> = (
    S,
    Option<crate::span_propagation::SpanContextCarrier>,
    noq::RecvStream,
    noq::SendStream,
);

/// Internal: read a request and also return the propagated span context carrier.
///
/// The carrier is `Some` iff `S::SPAN_PROPAGATION` is true and the remote sent one.
async fn read_request_inner<S: Service>(
    connection: &impl IncomingRemoteConnection,
) -> Result<Option<Request<S>>, ReadRequestError> {
    loop {
        let (mut send, mut recv) = match connection.accept_bi().await {
            Ok(streams) => streams,
            Err(ConnectionError::ApplicationClosed(cause))
                if cause.error_code.into_inner() == 0 =>
            {
                trace!("remote side closed connection {cause:?}");
                return Ok(None);
            }
            Err(ConnectionError::LocallyClosed) => return Ok(None),
            Err(cause) => return Err(e!(ReadRequestError::Connection, cause)),
        };
        let err = match read_request_frame::<S>(&mut recv).await {
            ReadFrame::Request(carrier, msg) => return Ok(Some((msg, carrier, recv, send))),
            ReadFrame::MaxMessageSizeExceeded => e!(ReadRequestError::MaxMessageSizeExceeded),
            ReadFrame::InvalidRequest(source) => e!(ReadRequestError::InvalidRequest, source),
            ReadFrame::DecodeFailed(source) => e!(ReadRequestError::DecodeFailed, source),
            ReadFrame::Reset => {
                debug!("skipped request: the client reset its stream");
                // A drop finishes the stream, which looks like an empty response.
                send.reset(0u32.into()).ok();
                continue;
            }
            // The next `accept_bi` returns why.
            ReadFrame::ConnectionLost => continue,
        };
        if let Some(code) = err.error_code() {
            recv.stop(code.into()).ok();
            send.reset(code.into()).ok();
        }
        return Err(err);
    }
}

/// What the server reads from the stream of a request.
enum ReadFrame<S> {
    /// A complete request.
    Request(Option<crate::span_propagation::SpanContextCarrier>, S),
    /// The request is larger than [`MAX_MESSAGE_SIZE`].
    MaxMessageSizeExceeded,
    /// The stream ended before the request was complete, or the size prefix is too long.
    InvalidRequest(io::Error),
    /// The request does not decode.
    DecodeFailed(postcard::Error),
    /// The client reset the stream before the request was complete.
    Reset,
    /// The connection was lost before the request was complete.
    ConnectionLost,
}

impl<S> ReadFrame<S> {
    /// Returns the frame for a stream that failed before the request was complete.
    fn from_read_err(err: &noq::ReadError) -> Self {
        match err {
            // Any reset, not only `ERROR_CODE_ABORTED`: the update sender of a
            // request resets the stream if an update does not encode. noq drops
            // unread data on a reset, so this reset can arrive before the request.
            noq::ReadError::Reset(_) => Self::Reset,
            noq::ReadError::ConnectionLost(_) => Self::ConnectionLost,
            // A server cannot get these: irpc stops a request stream only after
            // its read failed, and only a client gets `ZeroRttRejected`.
            noq::ReadError::ClosedStream | noq::ReadError::ZeroRttRejected => Self::Reset,
        }
    }
}

/// Reads and decodes a request.
async fn read_request_frame<S: Service>(recv: &mut noq::RecvStream) -> ReadFrame<S> {
    let size = match recv.read_varint_u64().await {
        Ok(Some(size)) => size,
        Ok(None) => {
            return ReadFrame::InvalidRequest(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "stream ended before the request",
            ));
        }
        Err(err) => {
            let read_err = err
                .get_ref()
                .and_then(|err| err.downcast_ref::<noq::ReadError>());
            return match read_err {
                Some(read_err) => ReadFrame::from_read_err(read_err),
                // The varint is longer than a u64, or the stream ended inside it.
                None => ReadFrame::InvalidRequest(err),
            };
        }
    };
    if size > MAX_MESSAGE_SIZE {
        return ReadFrame::MaxMessageSizeExceeded;
    }
    let mut buf = vec![0; size as usize];
    match recv.read_exact(&mut buf).await {
        Ok(()) => {}
        Err(noq::ReadExactError::ReadError(err)) => return ReadFrame::from_read_err(&err),
        Err(err @ noq::ReadExactError::FinishedEarly(_)) => {
            return ReadFrame::InvalidRequest(io::Error::new(io::ErrorKind::UnexpectedEof, err));
        }
    }
    let decoded = if S::SPAN_PROPAGATION {
        postcard::from_bytes(&buf)
    } else {
        postcard::from_bytes(&buf).map(|msg| (None, msg))
    };
    match decoded {
        Ok((carrier, msg)) => ReadFrame::Request(carrier, msg),
        Err(err) => ReadFrame::DecodeFailed(err),
    }
}
