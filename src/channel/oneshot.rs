//! Oneshot channel, similar to tokio's oneshot channel

use std::{fmt::Debug, future::Future, io, pin::Pin, task};

use n0_error::{e, stack_error};
use n0_future::future::Boxed as BoxFuture;

use super::SendError;
#[cfg(feature = "rpc")]
use crate::rpc::{self, ErrorCode};
use crate::util::FusedOneshotReceiver;

/// Error when receiving a oneshot message.
///
/// For a local channel, the only error is [`SenderClosed`]. For a remote
/// channel, the sender is on the other side of the connection, for example
/// the server for a response.
///
/// If the server rejects the request and also closes the connection, the close
/// usually arrives first, and the receiver gets [`Io`] instead of [`Remote`].
/// [`Self::remote_code`] returns the code in both cases.
///
/// [`SenderClosed`]: Self::SenderClosed
/// [`Io`]: Self::Io
/// [`Remote`]: Self::Remote
#[stack_error(derive, add_meta)]
#[non_exhaustive]
pub enum RecvError {
    /// The sender has been closed.
    ///
    /// This is the only error that can occur for local communication. For
    /// remote communication, this means that the sender finished the stream
    /// without sending, for example because it was dropped.
    #[error("Sender closed")]
    SenderClosed,
    /// The sender reset the stream with an irpc error code, and the channel is closed.
    ///
    /// The `code` says why, see [`ErrorCode`], and the [`rpc`] module for an overview.
    ///
    /// [`ErrorCode`]: crate::rpc::ErrorCode
    /// [`rpc`]: crate::rpc
    #[cfg(feature = "rpc")]
    #[error("Sender closed the channel: {code}")]
    Remote { code: crate::rpc::ErrorCode },
    /// The message is larger than [`MAX_MESSAGE_SIZE`], so this receiver closed the channel.
    ///
    /// [`MAX_MESSAGE_SIZE`]: crate::rpc::MAX_MESSAGE_SIZE
    #[error("Message too large")]
    MaxSizeExceeded,
    /// The message does not decode, so this receiver closed the channel.
    #[cfg(feature = "rpc")]
    #[error("Message does not decode")]
    DecodeFailed {
        #[error(std_err)]
        source: postcard::Error,
    },
    /// An io error, for example because the connection was lost.
    ///
    /// A stream that ends in the middle of a message also gives an io error.
    #[error("Io error")]
    Io {
        #[error(std_err)]
        source: io::Error,
    },
}

/// Converts an io error from reading the stream of a remote channel.
///
/// A reset with an irpc error code becomes [`RecvError::Remote`], and any
/// other io error becomes [`RecvError::Io`].
impl From<io::Error> for RecvError {
    fn from(err: io::Error) -> Self {
        #[cfg(feature = "rpc")]
        if let Some(code) = rpc::reset_code(&err) {
            return e!(RecvError::Remote { code });
        }
        e!(RecvError::Io, err)
    }
}

impl From<RecvError> for io::Error {
    fn from(e: RecvError) -> Self {
        match e {
            RecvError::Io { source, .. } => source,
            RecvError::SenderClosed { .. } => io::Error::new(io::ErrorKind::BrokenPipe, e),
            #[cfg(feature = "rpc")]
            RecvError::Remote { .. } => io::Error::new(io::ErrorKind::ConnectionReset, e),
            _ => io::Error::new(io::ErrorKind::InvalidData, e),
        }
    }
}

#[cfg(feature = "rpc")]
impl RecvError {
    /// Returns the irpc error code that the peer sent, if any.
    ///
    /// This is the code of a [`Remote`](Self::Remote) error, or the code that
    /// the peer closed the connection with.
    pub fn remote_code(&self) -> Option<ErrorCode> {
        match self {
            Self::Remote { code, .. } => Some(*code),
            Self::Io { source, .. } => rpc::io_close_code(source),
            _ => None,
        }
    }
}

/// Create a local oneshot sender and receiver pair.
///
/// This is currently using a tokio channel pair internally.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    (tx.into(), rx.into())
}

/// A sender that can be wrapped in a `Box<dyn DynSender<T>>`.
///
/// Remote senders are always boxed, since for remote communication the boxing
/// overhead is negligible. However, boxing can also be used for local communication,
/// e.g. when applying a transform or filter to the message before sending it.
pub(crate) trait DynSender<T>: Debug + Send + Sync + 'static {
    /// Send a message, consuming the sender.
    fn send(self: Box<Self>, value: T) -> BoxFuture<Result<(), SendError>>;

    /// True if sending can fail in more ways than the receiver being dropped.
    ///
    /// Return false if this sender only fails with [`SendError::ReceiverClosed`],
    /// e.g. a transform around an in-memory channel. Return true if it goes
    /// through a connection that can break on its own, even a local one such
    /// as shared memory between processes. Wrappers should forward the value
    /// of the sender they wrap.
    ///
    /// See [`Sender::is_rpc`].
    fn is_rpc(&self) -> bool;
}

/// A generic boxed receiver
///
/// Remote receivers are always boxed, since for remote communication the boxing
/// overhead is negligible. However, boxing can also be used for local communication,
/// e.g. when applying a transform or filter to the message before receiving it.
type BoxedReceiver<T> = BoxFuture<Result<T, RecvError>>;

/// A oneshot sender.
///
/// Compared to a local onehsot sender, sending a message is async since in the case
/// of remote communication, sending over the wire is async. Other than that it
/// behaves like a local oneshot sender and has no overhead in the local case.
pub struct Sender<T>(SenderInner<T>);

enum SenderInner<T> {
    Tokio(tokio::sync::oneshot::Sender<T>),
    Boxed(Box<dyn DynSender<T>>),
}

impl<T> Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            SenderInner::Tokio(_) => f.debug_tuple("Tokio").finish(),
            SenderInner::Boxed(sender) => f.debug_tuple("Boxed").field(sender).finish(),
        }
    }
}

impl<T> From<tokio::sync::oneshot::Sender<T>> for Sender<T> {
    fn from(tx: tokio::sync::oneshot::Sender<T>) -> Self {
        Self(SenderInner::Tokio(tx))
    }
}

impl<T> TryFrom<Sender<T>> for tokio::sync::oneshot::Sender<T> {
    type Error = Sender<T>;

    fn try_from(value: Sender<T>) -> Result<Self, Self::Error> {
        match value.0 {
            SenderInner::Tokio(tx) => Ok(tx),
            SenderInner::Boxed(_) => Err(value),
        }
    }
}

impl<T> Sender<T> {
    pub(crate) fn from_boxed(inner: Box<dyn DynSender<T>>) -> Self {
        Self(SenderInner::Boxed(inner))
    }

    /// Send a message
    ///
    /// If this is a boxed sender that represents a remote connection, sending may yield or fail with an io error.
    /// Local senders will never yield, but can fail if the receiver has been closed.
    pub async fn send(self, value: T) -> Result<(), SendError>
    where
        T: 'static,
    {
        match self.0 {
            SenderInner::Tokio(tx) => tx.send(value).map_err(|_| e!(SendError::ReceiverClosed)),
            SenderInner::Boxed(sender) => sender.send(value).await,
        }
    }

    /// Returns true if sending can fail in more ways than the receiver being dropped.
    ///
    /// This is about which failures can occur, not about where the receiver is.
    /// A purely in-memory sender is not rpc: it only fails with
    /// [`SendError::ReceiverClosed`]. A sender that goes through a connection
    /// that can break on its own is rpc, even if both ends are on the same
    /// machine, e.g. shared memory between processes.
    ///
    /// A sender created with [`Self::with_map`] or [`Self::with_filter_map`]
    /// is rpc if the sender it wraps is.
    pub fn is_rpc(&self) -> bool
    where
        T: 'static,
    {
        match &self.0 {
            SenderInner::Tokio(_) => false,
            SenderInner::Boxed(sender) => sender.is_rpc(),
        }
    }
}

impl<T: Send + Sync + 'static> Sender<T> {
    /// Applies a filter before sending.
    ///
    /// Messages that don't pass the filter are dropped.
    pub fn with_filter(self, f: impl Fn(&T) -> bool + Send + Sync + 'static) -> Sender<T> {
        self.with_filter_map(move |u| if f(&u) { Some(u) } else { None })
    }

    /// Applies a transform before sending.
    pub fn with_map<U, F>(self, f: F) -> Sender<U>
    where
        F: Fn(U) -> T + Send + Sync + 'static,
        U: Send + Sync + 'static,
    {
        self.with_filter_map(move |u| Some(f(u)))
    }

    /// Applies a filter and transform before sending.
    ///
    /// Messages that don't pass the filter are dropped.
    pub fn with_filter_map<U, F>(self, f: F) -> Sender<U>
    where
        F: Fn(U) -> Option<T> + Send + Sync + 'static,
        U: Send + Sync + 'static,
    {
        Sender::from_boxed(Box::new(FilterMapSender { inner: self, f }))
    }
}

/// A sender that applies a filter and transform before sending to the inner sender.
struct FilterMapSender<T, F> {
    inner: Sender<T>,
    f: F,
}

impl<T, F> Debug for FilterMapSender<T, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilterMapSender")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl<T, U, F> DynSender<U> for FilterMapSender<T, F>
where
    T: Send + Sync + 'static,
    U: Send + Sync + 'static,
    F: Fn(U) -> Option<T> + Send + Sync + 'static,
{
    fn send(self: Box<Self>, value: U) -> BoxFuture<Result<(), SendError>> {
        let Self { inner, f } = *self;
        let opt = f(value);
        Box::pin(async move {
            if let Some(v) = opt {
                inner.send(v).await
            } else {
                Ok(())
            }
        })
    }

    fn is_rpc(&self) -> bool {
        self.inner.is_rpc()
    }
}

impl<T> crate::sealed::Sealed for Sender<T> {}
impl<T> crate::Sender for Sender<T> {}

/// A oneshot receiver.
///
/// Compared to a local oneshot receiver, receiving a message can fail not just
/// when the sender has been closed, but also when the remote connection fails.
pub struct Receiver<T>(ReceiverInner<T>);

enum ReceiverInner<T> {
    Tokio(FusedOneshotReceiver<T>),
    Boxed(BoxedReceiver<T>),
}

impl<T> Future for Receiver<T> {
    type Output = Result<T, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context) -> task::Poll<Self::Output> {
        match &mut self.get_mut().0 {
            ReceiverInner::Tokio(rx) => Pin::new(rx)
                .poll(cx)
                .map_err(|_| e!(RecvError::SenderClosed)),
            ReceiverInner::Boxed(rx) => Pin::new(rx).poll(cx),
        }
    }
}

/// Convert a tokio oneshot receiver to a receiver for this crate
impl<T> From<tokio::sync::oneshot::Receiver<T>> for Receiver<T> {
    fn from(rx: tokio::sync::oneshot::Receiver<T>) -> Self {
        Self(ReceiverInner::Tokio(FusedOneshotReceiver(rx)))
    }
}

impl<T> TryFrom<Receiver<T>> for tokio::sync::oneshot::Receiver<T> {
    type Error = Receiver<T>;

    fn try_from(value: Receiver<T>) -> Result<Self, Self::Error> {
        match value.0 {
            ReceiverInner::Tokio(tx) => Ok(tx.0),
            ReceiverInner::Boxed(_) => Err(value),
        }
    }
}

/// Convert a function that produces a future to a receiver for this crate
impl<T, F, Fut> From<F> for Receiver<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, RecvError>> + Send + 'static,
{
    fn from(f: F) -> Self {
        Self(ReceiverInner::Boxed(Box::pin(f())))
    }
}

impl<T> Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            ReceiverInner::Tokio(_) => f.debug_tuple("Tokio").finish(),
            ReceiverInner::Boxed(_) => f.debug_tuple("Boxed").finish(),
        }
    }
}

impl<T> crate::sealed::Sealed for Receiver<T> {}
impl<T> crate::Receiver for Receiver<T> {}
