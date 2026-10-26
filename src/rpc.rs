//! Module for cross-process RPC.
//!
//! The code in this module works with all connections that use [`noq`]
//! streams. The [`crate::noq`] module has the transport with dial by socket
//! address. The [`crate::iroh`] module has the transport with dial by endpoint id.
//!
//! # Requests and channel messages
//!
//! Each request has its own pair of streams. The client writes the request as
//! the first message on its stream. After the request, the streams carry the
//! channel messages of the request: the response, and for some requests,
//! updates from the client. A `Request` error code is about the request, and a
//! `Message` error code is about a channel message, see [`ErrorCode`].
//!
//! # Bad requests
//!
//! A request is bad if it is larger than [`MAX_MESSAGE_SIZE`], or if it does
//! not decode. For example, a client of a newer protocol version sent a request
//! type that the server does not know. A stream that ends before its request is
//! complete also counts as a request that does not decode. The server stops and
//! resets the streams of a bad request with
//! [`ErrorCode::RequestMaxSizeExceeded`] or [`ErrorCode::RequestDecodeFailed`].
//!
//! By default, a [`Handler`] then closes the connection with the same code, as
//! for any other protocol violation. A protocol that adds request types at the
//! end of its enum can use [`Handler::skip_bad_requests`]: the handler then
//! reads the next request, so an old server rejects the new requests and keeps
//! serving the old ones on the same connection.
//!
//! The client gets the code in a `Remote` error from the receiver of the
//! response, for example [`oneshot::RecvError::Remote`] from [`Client::rpc`],
//! and from the sender of its updates, [`SendError::Remote`].
//! When the server also closes the connection, as a [`Handler`] does by
//! default, the close usually arrives first, and the client gets an `Io` error
//! with the connection error instead. In both cases, `remote_code` returns the
//! code, for example [`crate::Error::remote_code`].
//!
//! [`read_request`] returns a bad request as a [`ReadRequestError`]. A server
//! with its own loop decides what to do: close the connection, or read the
//! next request.
//!
//! # Abandoned requests
//!
//! A client that drops a request before it wrote all of it resets the stream
//! with [`ErrorCode::RequestAborted`]. For example, it dropped the future of
//! [`Client::rpc`] while it wrote a large request. A client that cannot send a
//! request resets the stream with [`ErrorCode::RequestMaxSizeExceeded`] or
//! [`ErrorCode::RequestEncodeFailed`].
//!
//! When the client resets the stream of a request before the server read the
//! request, the server skips the request and reads the next one. It resets its
//! side of the stream with [`ErrorCode::RequestAborted`], so that the receiver
//! of the response gets an error instead of the end of the stream.
//!
//! # Channel messages
//!
//! A sender resets its stream with [`ErrorCode::MessageMaxSizeExceeded`] if a
//! message is larger than [`MAX_MESSAGE_SIZE`], and with
//! [`ErrorCode::MessageEncodeFailed`] if a message does not encode. A receiver
//! stops its stream with [`ErrorCode::MessageMaxSizeExceeded`] if a message is
//! larger than [`MAX_MESSAGE_SIZE`], and with [`ErrorCode::MessageDecodeFailed`]
//! if a message does not decode. The channel then ends. The side that found the
//! error gets its own variant, for example [`mpsc::RecvError::DecodeFailed`],
//! and the other side gets the code in a `Remote` error, for example
//! [`SendError::Remote`]. A reset can drop messages that the receiver did not
//! read yet.
//!
//! # Error codes
//!
//! irpc closes connections, and stops and resets streams, with the codes of
//! [`ErrorCode`]. irpc reserves the codes 0 to 15, and can add more codes in
//! this range in a later version.
//!
//! An application should use codes from 16 upward, so that a peer can tell its
//! codes apart from the codes of irpc. irpc does not check this. A code below
//! 16 works, but a later version of irpc can give it a different meaning.
//!
//! [`Client::rpc`]: crate::Client::rpc
use std::{fmt::Debug, future::Future, io, marker::PhantomData, pin::Pin, sync::Arc};

use n0_error::{e, stack_error};
use n0_future::future::Boxed as BoxFuture;
use noq::{ConnectionError, VarInt};
use serde::{Serialize, de::DeserializeOwned};
use smallvec::SmallVec;
use tokio::io::AsyncReadExt;
use tracing::{Instrument, debug, trace, warn};

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

/// An error code that irpc uses on streams and connections.
///
/// A `Request` code is about the request, and a `Message` code is about a
/// channel message, see [Requests and channel messages]. An application should
/// use codes from 16 upward, see [Error codes].
///
/// # Examples
///
/// ```
/// use irpc::rpc::ErrorCode;
///
/// let code = ErrorCode::RequestDecodeFailed.to_code();
/// assert_eq!(
///     ErrorCode::from_code(code),
///     Some(ErrorCode::RequestDecodeFailed)
/// );
/// ```
///
/// [Requests and channel messages]: self#requests-and-channel-messages
/// [Error codes]: self#error-codes
#[derive(Debug, derive_more::Display, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
#[non_exhaustive]
pub enum ErrorCode {
    /// No error, for example a normal close or a dropped receiver.
    #[display("closed")]
    Closed = 0,
    /// The client reset the stream of the request before the server read the request.
    ///
    /// For example, the client dropped the request before it wrote all of it.
    /// The server skips such a request, and resets its side of the stream with
    /// this code, see [Abandoned requests].
    ///
    /// [Abandoned requests]: self#abandoned-requests
    #[display("request aborted")]
    RequestAborted = 1,
    /// The request is larger than [`MAX_MESSAGE_SIZE`].
    ///
    /// The server rejects such a request, see [Bad requests]. A client that
    /// finds its request too large resets the stream with this code.
    ///
    /// [Bad requests]: self#bad-requests
    #[display("request too large")]
    RequestMaxSizeExceeded = 2,
    /// The request does not encode.
    ///
    /// A client that cannot encode its request resets the stream with this code.
    #[display("request does not encode")]
    RequestEncodeFailed = 3,
    /// The request does not decode, or its stream ended before it was complete.
    ///
    /// The server rejects such a request, see [Bad requests].
    ///
    /// [Bad requests]: self#bad-requests
    #[display("request does not decode")]
    RequestDecodeFailed = 4,
    /// A channel message is larger than [`MAX_MESSAGE_SIZE`].
    #[display("message too large")]
    MessageMaxSizeExceeded = 5,
    /// A channel message does not encode.
    #[display("message does not encode")]
    MessageEncodeFailed = 6,
    /// A channel message does not decode.
    #[display("message does not decode")]
    MessageDecodeFailed = 7,
}

impl ErrorCode {
    /// Every error code, for [`Self::from_code`].
    const ALL: [Self; 8] = [
        Self::Closed,
        Self::RequestAborted,
        Self::RequestMaxSizeExceeded,
        Self::RequestEncodeFailed,
        Self::RequestDecodeFailed,
        Self::MessageMaxSizeExceeded,
        Self::MessageEncodeFailed,
        Self::MessageDecodeFailed,
    ];

    /// Returns the number of this error code.
    pub const fn to_code(self) -> u32 {
        self as u32
    }

    /// Returns the error code for `code`, or `None` if irpc does not use `code`.
    pub fn from_code(code: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.to_code() == code)
    }

    fn from_varint(code: VarInt) -> Option<Self> {
        u32::try_from(code.into_inner())
            .ok()
            .and_then(Self::from_code)
    }
}

impl From<ErrorCode> for VarInt {
    fn from(code: ErrorCode) -> Self {
        VarInt::from_u32(code.to_code())
    }
}

/// Error when reading a request with [`read_request`].
///
/// For [`MaxSizeExceeded`], [`Malformed`], and [`DecodeFailed`], irpc stops
/// and resets the streams of the request with
/// [`ErrorCode::RequestMaxSizeExceeded`] or [`ErrorCode::RequestDecodeFailed`],
/// so its client gets an error. The connection is still usable, so a server
/// can read the next request.
///
/// [`MaxSizeExceeded`]: Self::MaxSizeExceeded
/// [`Malformed`]: Self::Malformed
/// [`DecodeFailed`]: Self::DecodeFailed
#[stack_error(derive, add_meta)]
#[non_exhaustive]
pub enum ReadRequestError {
    /// The request is larger than [`MAX_MESSAGE_SIZE`].
    #[error("Request too large")]
    MaxSizeExceeded,
    /// The request is not framed correctly.
    ///
    /// For example, the stream ended before the request was complete.
    #[error("Request malformed")]
    Malformed {
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
    fn error_code(&self) -> Option<ErrorCode> {
        match self {
            Self::MaxSizeExceeded { .. } => Some(ErrorCode::RequestMaxSizeExceeded),
            Self::Malformed { .. } | Self::DecodeFailed { .. } => {
                Some(ErrorCode::RequestDecodeFailed)
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

/// Error when a client writes a request.
#[stack_error(derive, add_meta, from_sources)]
#[non_exhaustive]
pub enum WriteError {
    /// The request is larger than [`MAX_MESSAGE_SIZE`], so this client did not send it.
    #[error("Request too large")]
    MaxSizeExceeded,
    /// The request does not encode, so this client did not send it.
    #[error("Request does not encode")]
    EncodeFailed {
        #[error(std_err)]
        source: postcard::Error,
    },
    /// The server stopped the stream with an irpc error code while this client wrote the request.
    ///
    /// The `code` says why, see [`ErrorCode`], and the [`rpc`](self) module for an overview.
    #[error("Server rejected the request: {code}")]
    Remote { code: ErrorCode },
    /// An io error, for example because the connection was lost.
    #[error("Io error")]
    Io {
        #[error(std_err)]
        source: io::Error,
    },
}

impl WriteError {
    /// Returns the code that this client resets its stream with, if it found the error itself.
    fn reset_code(&self) -> Option<ErrorCode> {
        match self {
            Self::MaxSizeExceeded { .. } => Some(ErrorCode::RequestMaxSizeExceeded),
            Self::EncodeFailed { .. } => Some(ErrorCode::RequestEncodeFailed),
            _ => None,
        }
    }
}

/// Returns the irpc error code that the peer closed the connection with, if any, other than 0.
pub(crate) fn close_code(err: &ConnectionError) -> Option<ErrorCode> {
    match err {
        ConnectionError::ApplicationClosed(close) => ErrorCode::from_varint(close.error_code),
        _ => None,
    }
    .filter(|code| *code != ErrorCode::Closed)
}

/// Returns the irpc error code that the peer closed the connection with, if `err` comes from that close.
pub(crate) fn io_close_code(err: &io::Error) -> Option<ErrorCode> {
    let err = err.get_ref()?;
    if let Some(noq::ReadError::ConnectionLost(err)) = err.downcast_ref() {
        close_code(err)
    } else if let Some(noq::WriteError::ConnectionLost(err)) = err.downcast_ref() {
        close_code(err)
    } else {
        None
    }
}

/// Returns the irpc error code that the receiver stopped the stream with, if any.
pub(crate) fn stop_code(err: &noq::WriteError) -> Option<ErrorCode> {
    match err {
        noq::WriteError::Stopped(code) => ErrorCode::from_varint(*code),
        _ => None,
    }
}

impl From<noq::WriteError> for WriteError {
    fn from(err: noq::WriteError) -> Self {
        match stop_code(&err).filter(|code| *code != ErrorCode::Closed) {
            Some(code) => e!(WriteError::Remote { code }),
            None => e!(WriteError::Io, err.into()),
        }
    }
}

impl From<WriteError> for io::Error {
    fn from(e: WriteError) -> Self {
        match e {
            WriteError::Io { source, .. } => source,
            WriteError::Remote { .. } => io::Error::new(io::ErrorKind::ConnectionReset, e),
            _ => io::Error::new(io::ErrorKind::InvalidData, e),
        }
    }
}

impl WriteError {
    /// Returns the irpc error code that the peer sent, if any.
    ///
    /// This is the code of a [`Remote`](Self::Remote) error, or the code that
    /// the peer closed the connection with.
    pub fn remote_code(&self) -> Option<ErrorCode> {
        match self {
            Self::Remote { code, .. } => Some(*code),
            Self::Io { source, .. } => io_close_code(source),
            _ => None,
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
/// [`ErrorCode::RequestAborted`].
#[derive(Debug)]
pub struct RemoteSender<S>(ResetOnDrop, noq::RecvStream, std::marker::PhantomData<S>);

/// A send stream that resets with [`ErrorCode::RequestAborted`] if it drops before [`Self::into_inner`].
#[derive(Debug)]
struct ResetOnDrop(Option<noq::SendStream>);

impl ResetOnDrop {
    fn get_mut(&mut self) -> &mut noq::SendStream {
        self.0
            .as_mut()
            .expect("only `into_inner` and `reset` take the stream")
    }

    fn into_inner(mut self) -> noq::SendStream {
        self.0
            .take()
            .expect("only `into_inner` and `reset` take the stream")
    }

    /// Resets the stream with `code`, instead of [`ErrorCode::RequestAborted`] on drop.
    fn reset(&mut self, code: ErrorCode) {
        if let Some(mut send) = self.0.take() {
            send.reset(code.into()).ok();
        }
    }
}

impl Drop for ResetOnDrop {
    fn drop(&mut self) {
        if let Some(send) = &mut self.0 {
            send.reset(ErrorCode::RequestAborted.into()).ok();
        }
    }
}

/// Serialize a message for sending over the wire.
///
/// When `S::SPAN_PROPAGATION` is true, the message is wrapped in a tuple with
/// the context of `span`: `(Option<SpanContextCarrier>, msg)`.
/// When false, the message is serialized directly.
fn prepare_write<S: Service>(
    msg: impl Into<S>,
    span: &tracing::Span,
) -> Result<SmallVec<[u8; 128]>, WriteError> {
    let msg = msg.into();
    let mut buf = SmallVec::new();
    let res = if S::SPAN_PROPAGATION {
        // Include span context in wire format
        let span_ctx = crate::span_propagation::SpanContextCarrier::from_span_or_current(span);
        encode(&(span_ctx, msg), &mut buf)
    } else {
        // Original wire format without span context
        encode(&msg, &mut buf)
    };
    res.map_err(|err| match err {
        EncodeError::MaxSizeExceeded => e!(WriteError::MaxSizeExceeded),
        EncodeError::Failed(source) => e!(WriteError::EncodeFailed, source),
    })?;
    Ok(buf)
}

/// Why a message did not encode.
enum EncodeError {
    /// The message is larger than [`MAX_MESSAGE_SIZE`].
    MaxSizeExceeded,
    /// The message does not encode.
    Failed(postcard::Error),
}

/// Encodes `value` with its size prefix into `buf`.
fn encode(value: &impl Serialize, buf: &mut SmallVec<[u8; 128]>) -> Result<(), EncodeError> {
    let size = postcard::experimental::serialized_size(value).map_err(EncodeError::Failed)? as u64;
    if size > MAX_MESSAGE_SIZE {
        return Err(EncodeError::MaxSizeExceeded);
    }
    buf.clear();
    buf.write_varint_u64(size)
        .expect("a buffer does not fail to write");
    postcard::to_io(value, buf).map_err(EncodeError::Failed)?;
    Ok(())
}

/// Encodes the channel message `value` with its size prefix into `buf`.
///
/// Resets `send` if `value` is too large or does not encode, so the receiver
/// gets an error instead of the end of the stream.
fn encode_message(
    value: &impl Serialize,
    buf: &mut SmallVec<[u8; 128]>,
    send: &mut noq::SendStream,
) -> Result<(), SendError> {
    encode(value, buf).map_err(|err| {
        let (code, err) = match err {
            EncodeError::MaxSizeExceeded => (
                ErrorCode::MessageMaxSizeExceeded,
                e!(SendError::MaxSizeExceeded),
            ),
            EncodeError::Failed(source) => (
                ErrorCode::MessageEncodeFailed,
                e!(SendError::EncodeFailed, source),
            ),
        };
        send.reset(code.into()).ok();
        err
    })
}

impl<S: Service> RemoteSender<S> {
    pub fn new(send: noq::SendStream, recv: noq::RecvStream) -> Self {
        Self(ResetOnDrop(Some(send)), recv, PhantomData)
    }

    /// Writes the request `msg` in its span from [`Service::new_request_span`],
    /// and returns the streams of the request.
    ///
    /// If `msg` is too large or does not encode, this resets the stream with the
    /// code of the error.
    pub async fn write(
        self,
        msg: impl Into<S>,
    ) -> std::result::Result<(noq::SendStream, noq::RecvStream), WriteError> {
        let (send, recv, _span) = self.write_in_span(msg).await?;
        Ok((send, recv))
    }

    /// Writes the request like [`Self::write`], and also returns its span.
    pub(crate) async fn write_in_span(
        mut self,
        msg: impl Into<S>,
    ) -> std::result::Result<(noq::SendStream, noq::RecvStream, tracing::Span), WriteError> {
        let (span, buf) = self.prepare(msg)?;
        let (send, recv) = self.write_raw(&buf).instrument(span.clone()).await?;
        Ok((send, recv, span))
    }

    /// Returns the index of the stream, which the server also uses as the request id.
    pub(crate) fn req_id(&self) -> u64 {
        self.1.id().index()
    }

    /// Creates the span of the request, and encodes the request.
    ///
    /// With span propagation, the request carries the context of this span, or
    /// of the current span if the tracing backend does not record this span.
    /// If `msg` is too large or does not encode, this resets the stream with
    /// the code of the error, so the server learns why the request is missing.
    pub(crate) fn prepare(
        &mut self,
        msg: impl Into<S>,
    ) -> std::result::Result<(tracing::Span, SmallVec<[u8; 128]>), WriteError> {
        let msg = msg.into();
        let span = msg.new_request_span(self.req_id());
        let buf = prepare_write::<S>(msg, &span).inspect_err(|err| {
            if let Some(code) = err.reset_code() {
                self.0.reset(code);
            }
        })?;
        Ok((span, buf))
    }

    pub(crate) async fn write_raw(
        self,
        buf: &[u8],
    ) -> std::result::Result<(noq::SendStream, noq::RecvStream), WriteError> {
        let RemoteSender(mut send, recv, _) = self;
        send.get_mut().write_all(buf).await?;
        debug!("request sent");
        Ok((send.into_inner(), recv))
    }
}

impl<T: DeserializeOwned> From<noq::RecvStream> for oneshot::Receiver<T> {
    fn from(mut read: noq::RecvStream) -> Self {
        // A client creates the receiver in the request span, and logs the response in it.
        let span = tracing::Span::current();
        let fut = async move {
            // A sender that is dropped without sending finishes the stream.
            let Some(size) = read.read_varint_u64().await? else {
                return Err(e!(oneshot::RecvError::SenderClosed));
            };
            if size > MAX_MESSAGE_SIZE {
                read.stop(ErrorCode::MessageMaxSizeExceeded.into()).ok();
                return Err(e!(oneshot::RecvError::MaxSizeExceeded));
            }
            let mut buf = vec![0; size as usize];
            AsyncReadExt::read_exact(&mut read, &mut buf).await?;
            let msg: T = postcard::from_bytes(&buf).map_err(|err| {
                read.stop(ErrorCode::MessageDecodeFailed.into()).ok();
                e!(oneshot::RecvError::DecodeFailed, err)
            })?;
            span.in_scope(|| debug!("response received"));
            Ok(msg)
        };
        oneshot::Receiver::from(|| fut)
    }
}

/// Returns the noq read error in `err`, if the stream failed.
fn read_error(err: &io::Error) -> Option<&noq::ReadError> {
    err.get_ref()?.downcast_ref()
}

/// Returns the irpc error code that the sender reset the stream with, if any, other than 0.
pub(crate) fn reset_code(err: &io::Error) -> Option<ErrorCode> {
    match read_error(err)? {
        noq::ReadError::Reset(code) => ErrorCode::from_varint(*code),
        _ => None,
    }
    .filter(|code| *code != ErrorCode::Closed)
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
            span: tracing::Span::current(),
            received: 0,
            finished: false,
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
        // A server creates the sender in the request span, and logs the response in it.
        let span = tracing::Span::current();
        oneshot::Sender::Boxed(Box::new(move |value| {
            Box::pin(async move {
                // write via a small buffer to avoid allocation for small values
                let mut buf = SmallVec::new();
                encode_message(&value, &mut buf, &mut writer)?;
                writer.write_all(&buf).await?;
                span.in_scope(|| debug!("response sent"));
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
                span: tracing::Span::current(),
                sent: 0,
                _marker: PhantomData,
            }),
        ))))
    }
}

/// The receiver of a remote [`mpsc`] channel.
///
/// It logs the first message and the end of the stream in the span in which it was created.
struct NoqReceiver<T> {
    recv: noq::RecvStream,
    span: tracing::Span,
    received: u64,
    finished: bool,
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
                if !self.finished {
                    self.finished = true;
                    let received = self.received;
                    self.span.in_scope(|| debug!(received, "stream finished"));
                }
                return Ok(None);
            };
            if size > MAX_MESSAGE_SIZE {
                self.recv
                    .stop(ErrorCode::MessageMaxSizeExceeded.into())
                    .ok();
                return Err(e!(mpsc::RecvError::MaxSizeExceeded));
            }
            let mut buf = vec![0; size as usize];
            AsyncReadExt::read_exact(read, &mut buf).await?;
            let msg: T = postcard::from_bytes(&buf).map_err(|err| {
                read.stop(ErrorCode::MessageDecodeFailed.into()).ok();
                e!(mpsc::RecvError::DecodeFailed, err)
            })?;
            self.received += 1;
            if self.received == 1 {
                self.span.in_scope(|| debug!("first message received"));
            }
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
    /// The span in which the sender was created, for the logs of the first message and the end.
    span: tracing::Span,
    sent: u64,
    _marker: std::marker::PhantomData<T>,
}

impl<T: RpcMessage> NoqSenderInner<T> {
    /// Counts a sent message, and logs the first one.
    fn record_sent(&mut self) {
        self.sent += 1;
        if self.sent == 1 {
            self.span.in_scope(|| debug!("first message sent"));
        }
    }

    fn send(
        &mut self,
        value: T,
    ) -> Pin<Box<dyn Future<Output = Result<(), SendError>> + Send + Sync + '_>> {
        Box::pin(async move {
            encode_message(&value, &mut self.buffer, &mut self.send)?;
            self.send.write_all(&self.buffer).await?;
            self.buffer.clear();
            self.record_sent();
            Ok(())
        })
    }

    fn try_send(
        &mut self,
        value: T,
    ) -> Pin<Box<dyn Future<Output = Result<bool, SendError>> + Send + Sync + '_>> {
        Box::pin(async move {
            // todo: move the non-async part out of the box. Will require a new return type.
            encode_message(&value, &mut self.buffer, &mut self.send)?;
            let Some(n) = now_or_never(self.send.write(&self.buffer)) else {
                return Ok(false);
            };
            let n = n?;
            self.send.write_all(&self.buffer[n..]).await?;
            self.buffer.clear();
            self.record_sent();
            Ok(true)
        })
    }
}

// A sender has one state, so the size of the open state costs nothing.
#[allow(clippy::large_enum_variant)]
enum NoqSenderState<T> {
    Open(NoqSenderInner<T>),
    /// Closed by an error, with the code that the receiver stopped the stream with if it did.
    Closed(Option<ErrorCode>),
}

impl<T> NoqSenderState<T> {
    /// Takes the state for a send, and leaves the sender closed until the send puts it back.
    ///
    /// A cancelled send never puts it back, so the sender stays closed: the
    /// stream can hold part of a message.
    fn take_for_send(&mut self) -> Self {
        std::mem::replace(self, Self::Closed(None))
    }

    /// Returns the state after a send, so that later sends fail with the same error.
    fn after_send<R>(sender: NoqSenderInner<T>, res: &Result<R, SendError>) -> Self {
        match res {
            Ok(_) => Self::Open(sender),
            Err(SendError::ReceiverClosed { .. }) => Self::Closed(Some(ErrorCode::Closed)),
            Err(err) => Self::Closed(err.remote_code()),
        }
    }
}

struct NoqSender<T>(tokio::sync::Mutex<NoqSenderState<T>>);

impl<T> Drop for NoqSender<T> {
    /// Logs the end of the stream, which the send stream finishes when it drops.
    ///
    /// A sender that failed is closed, and logs nothing: the failed send returned the error.
    fn drop(&mut self) {
        if let NoqSenderState::Open(sender) = self.0.get_mut() {
            let sent = sender.sent;
            sender.span.in_scope(|| debug!(sent, "stream finished"));
        }
    }
}

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
                NoqSenderState::Closed(code) => {
                    *guard = NoqSenderState::Closed(code);
                    Err(SendError::closed(code))
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
                NoqSenderState::Closed(code) => {
                    *guard = NoqSenderState::Closed(code);
                    Err(SendError::closed(code))
                }
            }
        })
    }

    fn closed(&self) -> Pin<Box<dyn Future<Output = ()> + Send + Sync + '_>> {
        Box::pin(async {
            // Wait without the lock, so that other clones can send meanwhile.
            let stopped = match &*self.0.lock().await {
                NoqSenderState::Open(sender) => sender.send.stopped(),
                NoqSenderState::Closed(_) => return,
            };
            stopped.await.ok();
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
    /// The code should be 16 or higher, see [Error codes]. irpc
    /// closes a connection with code 0 for a normal close, which
    /// [`Handler::from_sender`] also uses when its receiver is gone. For a bad
    /// request, irpc closes the connection with [`ErrorCode::RequestMaxSizeExceeded`]
    /// or [`ErrorCode::RequestDecodeFailed`].
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
    ///
    /// [Error codes]: self#error-codes
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
    /// The requests of a connection run one after the other, each in its span
    /// from [`Service::new_request_span`]. If `f` calls
    /// [`RemoteService::with_remote_channels`], the message gets this span.
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
    /// time, see [Bad requests].
    ///
    /// [Bad requests]: self#bad-requests
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
        loop {
            let Some((msg, carrier, rx, tx)) = self.read_next(connection).await? else {
                return Ok(());
            };
            let span = new_request_span(&msg, &rx, carrier.as_ref());
            let fut = span
                .in_scope(|| {
                    debug!("request received");
                    (self.f)(msg, rx, tx)
                })
                .instrument(span);
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
                    HandlerError::close_connection(ErrorCode::Closed.to_code(), b"")
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
            async move { f(S::with_remote_channels(msg, rx, tx)).await }
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
/// Returns [`ReadRequestError::MaxSizeExceeded`],
/// [`ReadRequestError::Malformed`], or [`ReadRequestError::DecodeFailed`]
/// for a bad request. The connection is still open after these errors, see
/// [Bad requests].
///
/// [Bad requests]: self#bad-requests
pub async fn read_request<S: RemoteService>(
    connection: &impl IncomingRemoteConnection,
) -> Result<Option<S::Message>, ReadRequestError> {
    let Some((msg, carrier, rx, tx)) = read_request_inner::<S>(connection).await? else {
        return Ok(None);
    };
    let span = new_request_span(&msg, &rx, carrier.as_ref());
    Ok(Some(span.in_scope(|| {
        debug!("request received");
        S::with_remote_channels(msg, rx, tx)
    })))
}

/// Creates the span of a request, and sets its parent from the span context of the client.
fn new_request_span<S: Service>(
    msg: &S,
    recv: &noq::RecvStream,
    carrier: Option<&crate::span_propagation::SpanContextCarrier>,
) -> tracing::Span {
    let span = msg.new_request_span(recv.id().index());
    if let Some(carrier) = carrier {
        carrier.set_parent_of(&span);
    }
    span
}

/// Reads a single request from the connection.
///
/// This accepts a bi-directional stream from the connection and reads and parses the request.
///
/// Unlike [`read_request`] and a [`Handler`], this creates no request span, and
/// drops the span context of the client.
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
                if cause.error_code == ErrorCode::Closed.into() =>
            {
                trace!("remote side closed connection {cause:?}");
                return Ok(None);
            }
            Err(ConnectionError::LocallyClosed) => return Ok(None),
            Err(cause) => return Err(e!(ReadRequestError::Connection, cause)),
        };
        let err = match read_request_frame::<S>(&mut recv).await {
            ReadFrame::Request(carrier, msg) => return Ok(Some((msg, carrier, recv, send))),
            ReadFrame::MaxSizeExceeded => e!(ReadRequestError::MaxSizeExceeded),
            ReadFrame::Malformed(source) => e!(ReadRequestError::Malformed, source),
            ReadFrame::DecodeFailed(source) => e!(ReadRequestError::DecodeFailed, source),
            ReadFrame::Reset(reason) => {
                debug!(?reason, "skipped request: the client reset its stream");
                // A dropped stream finishes, which a client with an mpsc
                // response would take for the end of the response.
                send.reset(ErrorCode::RequestAborted.into()).ok();
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
    MaxSizeExceeded,
    /// The stream ended before the request was complete, or the size prefix is too long.
    Malformed(io::Error),
    /// The request does not decode.
    DecodeFailed(postcard::Error),
    /// The client reset the stream before the request was complete, with the irpc code if it sent one.
    Reset(Option<ErrorCode>),
    /// The connection was lost before the request was complete.
    ConnectionLost,
}

impl<S> ReadFrame<S> {
    /// Returns the frame for a stream that failed before the request was complete.
    fn from_read_err(err: &noq::ReadError) -> Self {
        match err {
            // Any reset, not only `ErrorCode::RequestAborted`: the update sender of a
            // request resets the stream if an update does not encode. noq drops
            // unread data on a reset, so this reset can arrive before the request.
            noq::ReadError::Reset(code) => Self::Reset(ErrorCode::from_varint(*code)),
            noq::ReadError::ConnectionLost(_) => Self::ConnectionLost,
            // A server cannot get these: irpc stops a request stream only after
            // its read failed, and only a client gets `ZeroRttRejected`.
            noq::ReadError::ClosedStream | noq::ReadError::ZeroRttRejected => Self::Reset(None),
        }
    }
}

/// Reads and decodes a request.
async fn read_request_frame<S: Service>(recv: &mut noq::RecvStream) -> ReadFrame<S> {
    let size = match recv.read_varint_u64().await {
        Ok(Some(size)) => size,
        Ok(None) => {
            return ReadFrame::Malformed(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "stream ended before the request",
            ));
        }
        Err(err) => {
            return match read_error(&err) {
                Some(read_err) => ReadFrame::from_read_err(read_err),
                // The varint is longer than a u64, or the stream ended inside it.
                None => ReadFrame::Malformed(err),
            };
        }
    };
    if size > MAX_MESSAGE_SIZE {
        return ReadFrame::MaxSizeExceeded;
    }
    let mut buf = vec![0; size as usize];
    match recv.read_exact(&mut buf).await {
        Ok(()) => {}
        Err(noq::ReadExactError::ReadError(err)) => return ReadFrame::from_read_err(&err),
        Err(err @ noq::ReadExactError::FinishedEarly(_)) => {
            return ReadFrame::Malformed(io::Error::new(io::ErrorKind::UnexpectedEof, err));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_round_trips() {
        for code in ErrorCode::ALL {
            assert_eq!(ErrorCode::from_code(code.to_code()), Some(code));
        }
        assert_eq!(ErrorCode::from_code(16), None);
    }

    #[test]
    fn write_error_maps_stop_codes() {
        let err = WriteError::from(noq::WriteError::Stopped(
            ErrorCode::RequestDecodeFailed.into(),
        ));
        assert_eq!(err.remote_code(), Some(ErrorCode::RequestDecodeFailed));
        assert_eq!(io::Error::from(err).kind(), io::ErrorKind::ConnectionReset);
        let err = WriteError::from(noq::WriteError::Stopped(16u32.into()));
        assert!(matches!(err, WriteError::Io { .. }), "{err:?}");
    }
}
