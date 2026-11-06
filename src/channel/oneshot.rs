//! Oneshot channel, similar to tokio's oneshot channel

use std::{fmt::Debug, future::Future, io, pin::Pin, task};

use n0_error::{e, stack_error};
use n0_future::future::Boxed as BoxFuture;

use super::SendError;
#[cfg(feature = "rpc")]
use crate::rpc::{self, ErrorCode, RpcError};
use crate::util::FusedOneshotReceiver;

/// Error when receiving a oneshot message.
///
/// For a local channel, the only error is [`SenderClosed`]. A remote channel
/// can also fail for reasons that a local channel does not have, see
/// [`RpcError`].
///
/// For a remote channel, the sender is on the other side of the connection,
/// for example the server for a response.
///
/// [`SenderClosed`]: Self::SenderClosed
#[stack_error(derive, add_meta, from_sources)]
#[non_exhaustive]
pub enum RecvError {
    /// The sender has been closed.
    ///
    /// This is the only error that can occur for local communication. For
    /// remote communication, this means that the sender finished the stream
    /// without sending, for example because it was dropped, or reset it with
    /// code 0 or [`ErrorCode::MessageAborted`]. A sender whose send was
    /// cancelled in the middle of the message is also closed, like a sender
    /// that was dropped before it sent.
    ///
    /// [`ErrorCode::MessageAborted`]: crate::rpc::ErrorCode::MessageAborted
    #[error("Sender closed")]
    SenderClosed,
    /// The remote channel failed for a reason that a local channel does not have.
    #[cfg(feature = "rpc")]
    #[error("Remote channel failed")]
    Rpc { source: RpcError },
}

/// Converts an io error from reading the stream of a remote channel.
///
/// A reset with code 0 or [`ErrorCode::MessageAborted`], and the end of the
/// stream in the middle of a message, become [`RecvError::SenderClosed`].
/// Anything else becomes [`RecvError::Rpc`], see [`RpcError`]'s conversion
/// from an io error.
#[cfg(feature = "rpc")]
impl From<io::Error> for RecvError {
    fn from(err: io::Error) -> Self {
        if let Some(None | Some(ErrorCode::MessageAborted)) = rpc::reset_code(&err) {
            return e!(RecvError::SenderClosed);
        }
        if rpc::is_truncated(&err) {
            return e!(RecvError::SenderClosed);
        }
        RpcError::from(err).into()
    }
}

impl From<RecvError> for io::Error {
    fn from(e: RecvError) -> Self {
        match e {
            RecvError::SenderClosed { .. } => io::Error::new(io::ErrorKind::BrokenPipe, e),
            #[cfg(feature = "rpc")]
            RecvError::Rpc { source, .. } => source.into(),
        }
    }
}

#[cfg(feature = "rpc")]
impl RecvError {
    /// Returns the code that the peer sent, if any.
    ///
    /// See [`RpcError::remote_code`].
    pub fn remote_code(&self) -> Option<ErrorCode> {
        match self {
            Self::Rpc { source, .. } => source.remote_code(),
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
pub trait DynSender<T>: Debug + Send + Sync + 'static {
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
pub type BoxedReceiver<T> = BoxFuture<Result<T, RecvError>>;

/// A oneshot sender.
///
/// Compared to a local onehsot sender, sending a message is async since in the case
/// of remote communication, sending over the wire is async. Other than that it
/// behaves like a local oneshot sender and has no overhead in the local case.
pub enum Sender<T> {
    Tokio(tokio::sync::oneshot::Sender<T>),
    Boxed(Box<dyn DynSender<T>>),
}

impl<T> Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tokio(_) => f.debug_tuple("Tokio").finish(),
            Self::Boxed(sender) => f.debug_tuple("Boxed").field(sender).finish(),
        }
    }
}

impl<T> From<tokio::sync::oneshot::Sender<T>> for Sender<T> {
    fn from(tx: tokio::sync::oneshot::Sender<T>) -> Self {
        Self::Tokio(tx)
    }
}

impl<T> TryFrom<Sender<T>> for tokio::sync::oneshot::Sender<T> {
    type Error = Sender<T>;

    fn try_from(value: Sender<T>) -> Result<Self, Self::Error> {
        match value {
            Sender::Tokio(tx) => Ok(tx),
            Sender::Boxed(_) => Err(value),
        }
    }
}

impl<T> Sender<T> {
    /// Send a message
    ///
    /// If this is a boxed sender that represents a remote connection, sending may yield or fail with an io error.
    /// Local senders will never yield, but can fail if the receiver has been closed.
    pub async fn send(self, value: T) -> Result<(), SendError>
    where
        T: 'static,
    {
        match self {
            Sender::Tokio(tx) => tx.send(value).map_err(|_| e!(SendError::ReceiverClosed)),
            Sender::Boxed(sender) => sender.send(value).await,
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
        match self {
            Sender::Tokio(_) => false,
            Sender::Boxed(sender) => sender.is_rpc(),
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
        Sender::Boxed(Box::new(FilterMapSender { inner: self, f }))
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
pub enum Receiver<T> {
    Tokio(FusedOneshotReceiver<T>),
    Boxed(BoxedReceiver<T>),
}

impl<T> Future for Receiver<T> {
    type Output = Result<T, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context) -> task::Poll<Self::Output> {
        match self.get_mut() {
            Self::Tokio(rx) => Pin::new(rx)
                .poll(cx)
                .map_err(|_| e!(RecvError::SenderClosed)),
            Self::Boxed(rx) => Pin::new(rx).poll(cx),
        }
    }
}

/// Convert a tokio oneshot receiver to a receiver for this crate
impl<T> From<tokio::sync::oneshot::Receiver<T>> for Receiver<T> {
    fn from(rx: tokio::sync::oneshot::Receiver<T>) -> Self {
        Self::Tokio(FusedOneshotReceiver(rx))
    }
}

impl<T> TryFrom<Receiver<T>> for tokio::sync::oneshot::Receiver<T> {
    type Error = Receiver<T>;

    fn try_from(value: Receiver<T>) -> Result<Self, Self::Error> {
        match value {
            Receiver::Tokio(tx) => Ok(tx.0),
            Receiver::Boxed(_) => Err(value),
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
        Self::Boxed(Box::pin(f()))
    }
}

impl<T> Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tokio(_) => f.debug_tuple("Tokio").finish(),
            Self::Boxed(_) => f.debug_tuple("Boxed").finish(),
        }
    }
}

impl<T> crate::sealed::Sealed for Receiver<T> {}
impl<T> crate::Receiver for Receiver<T> {}
