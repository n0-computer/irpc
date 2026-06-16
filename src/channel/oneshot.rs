//! Oneshot channel, similar to tokio's oneshot channel

use std::{fmt::Debug, future::Future, io, pin::Pin, task};

use n0_error::{e, stack_error};
use n0_future::future::Boxed as BoxFuture;

use super::SendError;
use crate::util::FusedOneshotReceiver;

/// Error when receiving a oneshot or mpsc message. For local communication,
/// the only thing that can go wrong is that the sender has been closed.
///
/// For rpc communication, there can be any number of errors, so this is a
/// generic io error.
#[stack_error(derive, add_meta, from_sources)]
pub enum RecvError {
    /// The sender has been closed. This is the only error that can occur
    /// for local communication.
    #[error("Sender closed")]
    SenderClosed,
    /// The message exceeded the maximum allowed message size (see [`MAX_MESSAGE_SIZE`]).
    ///
    /// [`MAX_MESSAGE_SIZE`]: crate::rpc::MAX_MESSAGE_SIZE
    #[error("Maximum message size exceeded")]
    MaxMessageSizeExceeded,
    /// An io error occurred. This can occur for remote communication,
    /// due to a network error or deserialization error.
    #[error("Io error")]
    Io {
        #[error(std_err)]
        source: io::Error,
    },
}

impl From<RecvError> for io::Error {
    fn from(e: RecvError) -> Self {
        match e {
            RecvError::Io { source, .. } => source,
            RecvError::SenderClosed { .. } => io::Error::new(io::ErrorKind::BrokenPipe, e),
            RecvError::MaxMessageSizeExceeded { .. } => {
                io::Error::new(io::ErrorKind::InvalidData, e)
            }
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

/// A generic boxed sender.
///
/// Remote senders are always boxed, since for remote communication the boxing
/// overhead is negligible. However, boxing can also be used for local communication,
/// e.g. when applying a transform or filter to the message before sending it.
pub type BoxedSender<T> =
    Box<dyn FnOnce(T) -> BoxFuture<Result<(), SendError>> + Send + Sync + 'static>;

/// A sender that can be wrapped in a `Box<dyn DynSender<T>>`.
///
/// In addition to implementing `Future`, this provides a fn to check if the sender is
/// an rpc sender.
///
/// Remote receivers are always boxed, since for remote communication the boxing
/// overhead is negligible. However, boxing can also be used for local communication,
/// e.g. when applying a transform or filter to the message before receiving it.
pub trait DynSender<T>: Future<Output = Result<(), SendError>> + Send + Sync + 'static {
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
    /// we can't yet distinguish between local and remote boxed oneshot senders.
    /// If we ever want to have local boxed oneshot senders, we need to add a
    /// third variant here.
    Boxed(BoxedSender<T>),
}

impl<T> Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tokio(_) => f.debug_tuple("Tokio").finish(),
            Self::Boxed(_) => f.debug_tuple("Boxed").finish(),
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
    pub async fn send(self, value: T) -> Result<(), SendError> {
        match self {
            Sender::Tokio(tx) => tx.send(value).map_err(|_| e!(SendError::ReceiverClosed)),
            Sender::Boxed(f) => f(value).await,
        }
    }

    /// Check if this is a remote sender
    pub fn is_rpc(&self) -> bool
    where
        T: 'static,
    {
        match self {
            Sender::Tokio(_) => false,
            Sender::Boxed(_) => true,
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
        let inner: BoxedSender<U> = Box::new(move |value| {
            let opt = f(value);
            Box::pin(async move {
                if let Some(v) = opt {
                    self.send(v).await
                } else {
                    Ok(())
                }
            })
        });
        Sender::Boxed(inner)
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
