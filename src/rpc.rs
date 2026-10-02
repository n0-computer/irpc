//! Module for cross-process RPC.
//!
//! The code in this module works with all connections that use [`noq`]
//! streams. The [`crate::noq`] module has the transport with dial by socket
//! address. The [`crate::iroh`] module has the transport with dial by endpoint id.
use std::{
    fmt::Debug,
    future::Future,
    io,
    marker::PhantomData,
    ops::DerefMut,
    pin::{Pin, pin},
    sync::Arc,
};

use n0_error::{e, stack_error};
use n0_future::{future::Boxed as BoxFuture, task::JoinSet};
use noq::{ConnectionError, VarInt};
use serde::de::DeserializeOwned;
use smallvec::SmallVec;
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

/// The function inside a [`Handler`].
type HandlerFn<S> = Arc<
    dyn Fn(
            S,
            noq::RecvStream,
            noq::SendStream,
        ) -> BoxFuture<std::result::Result<(), CloseConnection>>
        + Send
        + Sync
        + 'static,
>;

/// A request to close the connection, with a code and a reason.
///
/// A handler function returns `Err(CloseConnection)` to close the connection,
/// for example when the remote violates the protocol. The server loop then
/// closes the connection with the code and the reason. It reads no more
/// requests from the connection.
///
/// irpc itself uses these codes to close a connection:
///
/// - `0`: a normal close. The server loop does not treat a close with code 0
///   as an error. A connection that is dropped also closes with code 0.
///   [`Handler::from_sender`] uses code 0 when its receiver is gone.
/// - [`ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED`] (1): a request is larger than
///   [`MAX_MESSAGE_SIZE`].
///
/// An application can use these codes too. Streams have their own codes,
/// separate from the codes of a connection.
///
/// # Examples
///
/// ```
/// use irpc::{
///     channel::oneshot,
///     rpc::{CloseConnection, Handler},
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
///         Err(CloseConnection::new(401, "permission denied"))
///     } else {
///         msg.tx.send(()).await.ok();
///         Ok(())
///     }
/// });
/// ```
#[derive(Debug, Clone)]
pub struct CloseConnection {
    code: VarInt,
    reason: Vec<u8>,
}

impl CloseConnection {
    /// Creates a request to close the connection with `code` and `reason`.
    pub fn new(code: u32, reason: impl AsRef<[u8]>) -> Self {
        Self {
            code: VarInt::from_u32(code),
            reason: reason.as_ref().to_vec(),
        }
    }
}

/// How the server loop runs the requests of one connection.
#[derive(Debug, Clone, Copy)]
enum Mode {
    /// Runs each request in the loop, before the loop reads the next request.
    Sequential,
    /// Runs each request in a task, with at most this many tasks per connection.
    Concurrent(usize),
}

/// Handles the requests that a server reads from a connection.
///
/// A server uses one handler for all its connections. Create a handler with
/// one of these functions:
///
/// - [`Handler::from_sender`] sends each request to a [`LocalSender`], for
///   example the sender of an actor.
/// - [`Handler::concurrent`] runs a function for each request. It runs at
///   most a given number of requests of a connection at the same time.
/// - [`Handler::sequential`] runs a function for each request. It runs the
///   requests of a connection one after the other.
/// - [`Handler::raw`] runs a function on the protocol enum and the two
///   streams of each request. The function can pass the request to another
///   handler with [`Handler::call`].
///
/// The function of [`Handler::concurrent`] and [`Handler::sequential`] takes
/// the message enum. The compiler cannot get the protocol type from it. If no
/// other code names the protocol type, write it, for example
/// `Handler::<MyProtocol>::concurrent(..)`.
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
/// let handler: Handler<EchoProtocol> = Handler::concurrent(16, |msg| async move {
///     match msg {
///         EchoMessage::Echo(msg) => {
///             let WithChannels { inner, tx, .. } = msg;
///             tx.send(inner.0).await.ok();
///         }
///     }
///     Ok(())
/// });
/// ```
pub struct Handler<S> {
    f: HandlerFn<S>,
    mode: Mode,
}

impl<S> Clone for Handler<S> {
    fn clone(&self) -> Self {
        Self {
            f: self.f.clone(),
            mode: self.mode,
        }
    }
}

impl<S> Debug for Handler<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handler")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl<S: Service> Handler<S> {
    /// Creates a handler that runs `f` on the protocol enum and the streams of each request.
    ///
    /// The server loop runs the requests of a connection one after the other.
    /// It waits for the future of `f` before it reads the next request. The
    /// future returns `Err(CloseConnection)` to close the connection.
    ///
    /// Use this handler to pass a request to another handler, see
    /// [`Handler::call`]. If `f` needs the message enum, call
    /// [`RemoteService::with_remote_channels`] inside the future. With span
    /// propagation, the remote span context is only available while the future
    /// runs.
    pub fn raw<F, Fut>(f: F) -> Self
    where
        F: Fn(S, noq::RecvStream, noq::SendStream) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::result::Result<(), CloseConnection>> + Send + 'static,
    {
        let f: HandlerFn<S> = Arc::new(move |msg, rx, tx| Box::pin(f(msg, rx, tx)));
        Self {
            f,
            mode: Mode::Sequential,
        }
    }
}

impl<S: Service> Handler<S> {
    /// Runs this handler on one request, in the current task.
    ///
    /// Use it inside a [`Handler::raw`] to pass a request to another handler.
    /// In the example, the inner protocols are variants of `AppProtocol`. The
    /// returned future ends when the request ends. The limit of a
    /// [`Handler::concurrent`] does not apply here.
    ///
    /// # Examples
    ///
    /// ```
    /// use irpc::{Service, channel::oneshot, rpc::Handler, rpc_requests};
    /// use serde::{Deserialize, Serialize};
    ///
    /// #[rpc_requests(message = PingMessage)]
    /// #[derive(Debug, Serialize, Deserialize)]
    /// enum PingProtocol {
    ///     #[rpc(tx = oneshot::Sender<()>)]
    ///     #[wrap(Ping)]
    ///     Ping,
    /// }
    ///
    /// #[rpc_requests(message = EchoMessage)]
    /// #[derive(Debug, Serialize, Deserialize)]
    /// enum EchoProtocol {
    ///     #[rpc(tx = oneshot::Sender<String>)]
    ///     #[wrap(Echo)]
    ///     Echo(String),
    /// }
    ///
    /// /// Holds a request of `PingProtocol` or of `EchoProtocol`.
    /// #[derive(Debug, Serialize, Deserialize)]
    /// enum AppProtocol {
    ///     Ping(PingProtocol),
    ///     Echo(EchoProtocol),
    /// }
    ///
    /// impl Service for AppProtocol {
    ///     // The server passes the inner requests to other handlers, so it
    ///     // needs no message enum.
    ///     type Message = ();
    /// }
    ///
    /// fn app_handler(
    ///     ping: Handler<PingProtocol>,
    ///     echo: Handler<EchoProtocol>,
    /// ) -> Handler<AppProtocol> {
    ///     Handler::raw(move |request, rx, tx| {
    ///         let (ping, echo) = (ping.clone(), echo.clone());
    ///         async move {
    ///             match request {
    ///                 AppProtocol::Ping(request) => ping.call(request, rx, tx).await,
    ///                 AppProtocol::Echo(request) => echo.call(request, rx, tx).await,
    ///             }
    ///         }
    ///     })
    /// }
    /// ```
    pub fn call(
        &self,
        request: S,
        rx: noq::RecvStream,
        tx: noq::SendStream,
    ) -> impl Future<Output = std::result::Result<(), CloseConnection>> + Send + 'static {
        (self.f)(request, rx, tx)
    }

    /// Handles the requests of `connection` until the connection ends.
    ///
    /// The wire format depends on `S::SPAN_PROPAGATION`. If it is true, each
    /// request carries a span context.
    ///
    /// The handler decides if the requests of the connection run one after the
    /// other or at the same time. When the connection ends, this function waits
    /// for the requests that still run. If the caller drops the future of this
    /// function, the requests that still run are aborted.
    pub async fn handle_connection(
        &self,
        connection: &impl IncomingRemoteConnection,
    ) -> io::Result<()> {
        debug!("connection accepted");
        match self.mode {
            Mode::Sequential => self.run_sequential(connection).await,
            Mode::Concurrent(max_concurrent) => {
                self.run_concurrent(connection, max_concurrent).await
            }
        }
    }

    /// Runs the requests of `connection` one after the other.
    async fn run_sequential(&self, connection: &impl IncomingRemoteConnection) -> io::Result<()> {
        loop {
            // `None` means that the remote closed the connection gracefully.
            let Some((msg, carrier, rx, tx)) = read_request_inner::<S>(connection).await? else {
                return Ok(());
            };
            let fut = (self.f)(msg, rx, tx);
            let fut = crate::span_propagation::scope_remote(carrier, fut);
            if let Err(close) = fut.await {
                close_connection(connection, close);
                return Ok(());
            }
        }
    }

    /// Runs each request of `connection` in a task, at most `max_concurrent` at a time.
    async fn run_concurrent(
        &self,
        connection: &impl IncomingRemoteConnection,
        max_concurrent: usize,
    ) -> io::Result<()> {
        // The tasks of the running requests. When the set is dropped, its tasks
        // are aborted.
        let mut tasks = JoinSet::new();
        // The loop keeps this future across iterations: a task that ends must
        // not cancel a request that is only partly read.
        let mut read = pin!(read_request_inner::<S>(connection));
        let res = loop {
            tokio::select! {
                biased;
                // A task ended. If its handler asks to close the connection, stop.
                Some(task) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Err(close) = task.expect("handler task panicked") {
                        close_connection(connection, close);
                        break Ok(());
                    }
                }
                // A request arrived, and fewer than `max_concurrent` tasks run.
                next = &mut read, if tasks.len() < max_concurrent => {
                    read.set(read_request_inner::<S>(connection));
                    match next {
                        Err(err) => break Err(err),
                        // The remote closed the connection gracefully.
                        Ok(None) => break Ok(()),
                        Ok(Some((msg, carrier, rx, tx))) => {
                            let fut = crate::span_propagation::scope_remote(carrier, (self.f)(msg, rx, tx));
                            tasks.spawn(fut.instrument(tracing::Span::current()));
                        }
                    }
                }
            }
        };
        // Wait for all tasks, however the connection ended. If the remote closes
        // the connection right after a request, for example after
        // `Client::notify`, that request still completes. The connection is
        // over, so the loop ignores a `CloseConnection` from these tasks.
        while let Some(task) = tasks.join_next().await {
            task.expect("handler task panicked").ok();
        }
        res
    }
}

impl<S: RemoteService> Handler<S> {
    /// Creates a handler that sends each request to `local_sender`.
    ///
    /// The server loop waits until the request is in the channel of the
    /// sender. Then it reads the next request of the connection. So a full
    /// channel applies backpressure to the connection.
    pub fn from_sender(local_sender: impl Into<LocalSender<S>>) -> Self {
        let local_sender = local_sender.into();
        Self::sequential(move |msg| {
            let local_sender = local_sender.clone();
            async move {
                local_sender.send_raw(msg).await.map_err(|err| {
                    // The receiver is gone, so the handler cannot handle any
                    // request. Close with code 0, as a dropped connection does.
                    warn!("handler stopped: {err:#}");
                    CloseConnection::new(0, b"")
                })
            }
        })
    }

    /// Creates a handler that runs `f` for each request, at most `max_concurrent` at a time.
    ///
    /// Each call of `f` runs in its own task. The limit applies to each
    /// connection separately. When `max_concurrent` tasks of a connection run,
    /// the server loop waits until one of them ends. Then it reads the next
    /// request.
    ///
    /// When the connection ends, the server loop waits for the tasks that still
    /// run. If a task panics, the server loop panics too.
    ///
    /// The server uses the handler for all its connections, so `f` must clone
    /// the state that it needs into its future. The future returns
    /// `Err(CloseConnection)` to close the connection.
    ///
    /// # Panics
    ///
    /// Panics if `max_concurrent` is zero.
    pub fn concurrent<F, Fut>(max_concurrent: usize, f: F) -> Self
    where
        F: Fn(S::Message) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::result::Result<(), CloseConnection>> + Send + 'static,
    {
        assert!(max_concurrent > 0, "max_concurrent must not be zero");
        Self {
            mode: Mode::Concurrent(max_concurrent),
            ..Self::sequential(f)
        }
    }

    /// Creates a handler that runs `f` for each request, one request at a time.
    ///
    /// The server loop waits for the future of `f` before it reads the next
    /// request of the connection. Requests of other connections run at the
    /// same time. `f` runs in the task of the connection, so a panic ends the
    /// connection.
    ///
    /// The future returns `Err(CloseConnection)` to close the connection.
    pub fn sequential<F, Fut>(f: F) -> Self
    where
        F: Fn(S::Message) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::result::Result<(), CloseConnection>> + Send + 'static,
    {
        let f = Arc::new(f);
        Self::raw(move |msg, rx, tx| {
            let f = f.clone();
            async move {
                // Create the message inside the future: the remote span context
                // is only available while the future runs.
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

/// Closes `connection` with the code and the reason from a handler.
fn close_connection(connection: &impl IncomingRemoteConnection, close: CloseConnection) {
    debug!(code = %close.code, "handler closed the connection");
    connection.close(close.code, &close.reason);
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
/// silently dropped. Use [`Handler::handle_connection`] (or [`read_request`]) if you need
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
