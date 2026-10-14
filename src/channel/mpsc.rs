//! SPSC channel, similar to tokio's mpsc channel
//!
//! For the rpc case, the send side can not be cloned, hence mpsc instead of mpsc.

use std::{fmt::Debug, future::Future, io, marker::PhantomData, pin::Pin, sync::Arc};

use n0_error::{e, stack_error};

use super::SendError;
#[cfg(feature = "rpc")]
use crate::rpc::{self, ConnectionClose, RemoteCode};

/// Error when receiving an mpsc message.
///
/// A local channel has no errors.
///
/// For a remote channel, the sender is on the other side of the connection:
/// the server for a response stream, and the client for the updates of a request.
///
/// If the server rejects the request and also closes the connection, the close
/// usually arrives first, and the receiver gets [`ConnectionClosed`] instead of [`Remote`].
/// [`Self::remote_code`] returns the code in both cases.
///
/// [`Io`]: Self::Io
/// [`Remote`]: Self::Remote
/// [`ConnectionClosed`]: Self::ConnectionClosed
#[stack_error(derive, add_meta)]
#[non_exhaustive]
pub enum RecvError {
    /// The sender reset the stream with a code, and the channel is closed.
    ///
    /// The `code` says why, see [`RemoteCode`], and the [`rpc`] module for an overview.
    ///
    /// [`RemoteCode`]: crate::rpc::RemoteCode
    /// [`rpc`]: crate::rpc
    #[cfg(feature = "rpc")]
    #[error("Sender closed the channel: {code}")]
    Remote { code: crate::rpc::RemoteCode },
    /// The sender reset the stream with code 0, and the channel is closed.
    ///
    /// Unlike the end of the stream, which is `Ok(None)`, a reset can drop
    /// messages that this receiver did not read yet.
    #[cfg(feature = "rpc")]
    #[error("Sender reset the channel")]
    SenderClosed,
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
    /// The connection was closed on purpose, by the peer or by this side.
    #[cfg(feature = "rpc")]
    #[error("Connection {close}")]
    ConnectionClosed { close: crate::rpc::ConnectionClose },
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
/// A reset becomes [`RecvError::Remote`], or [`RecvError::SenderClosed`]
/// for code 0. The close of the connection becomes
/// [`RecvError::ConnectionClosed`], and any other io error becomes [`RecvError::Io`].
impl From<io::Error> for RecvError {
    fn from(err: io::Error) -> Self {
        #[cfg(feature = "rpc")]
        match rpc::reset_code(&err) {
            Some(Some(code)) => return e!(RecvError::Remote { code }),
            Some(None) => return e!(RecvError::SenderClosed),
            None => {}
        }
        #[cfg(feature = "rpc")]
        if let Some(close) = ConnectionClose::from_io(&err) {
            return e!(RecvError::ConnectionClosed { close });
        }
        e!(RecvError::Io, err)
    }
}

impl From<RecvError> for io::Error {
    fn from(e: RecvError) -> Self {
        match e {
            RecvError::Io { source, .. } => source,
            #[cfg(feature = "rpc")]
            RecvError::ConnectionClosed { .. } => io::Error::new(io::ErrorKind::NotConnected, e),
            #[cfg(feature = "rpc")]
            RecvError::Remote { .. } | RecvError::SenderClosed { .. } => {
                io::Error::new(io::ErrorKind::ConnectionReset, e)
            }
            _ => io::Error::new(io::ErrorKind::InvalidData, e),
        }
    }
}

#[cfg(feature = "rpc")]
impl RecvError {
    /// Returns the code that the peer sent, if any.
    ///
    /// This is the code of a [`Remote`](Self::Remote) error, or the code that
    /// the peer closed the connection with.
    pub fn remote_code(&self) -> Option<RemoteCode> {
        match self {
            Self::Remote { code, .. } => Some(*code),
            Self::ConnectionClosed { close, .. } => close.code(),
            _ => None,
        }
    }
}

/// Create a local mpsc sender and receiver pair, with the given buffer size.
///
/// This is currently using a tokio channel pair internally.
pub fn channel<T>(buffer: usize) -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = tokio::sync::mpsc::channel(buffer);
    (tx.into(), rx.into())
}

/// Single producer, single consumer sender.
///
/// For the local case, this wraps a tokio::sync::mpsc::Sender.
pub enum Sender<T> {
    Tokio(tokio::sync::mpsc::Sender<T>),
    Boxed(Arc<dyn DynSender<T>>),
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Tokio(tx) => Self::Tokio(tx.clone()),
            Self::Boxed(inner) => Self::Boxed(inner.clone()),
        }
    }
}

impl<T> Sender<T> {
    pub fn is_rpc(&self) -> bool
    where
        T: 'static,
    {
        match self {
            Sender::Tokio(_) => false,
            Sender::Boxed(x) => x.is_rpc(),
        }
    }

    #[cfg(feature = "stream")]
    pub fn into_sink(self) -> impl n0_future::Sink<T, Error = SendError> + Send + 'static
    where
        T: Send + Sync + 'static,
    {
        futures_util::sink::unfold(self, |sink, value| async move {
            sink.send(value).await?;
            Ok(sink)
        })
    }
}

impl<T: Send + Sync + 'static> Sender<T> {
    /// Applies a filter before sending.
    ///
    /// Messages that don't pass the filter are dropped.
    ///
    /// If you want to combine multiple filters and maps with minimal
    /// overhead, use `with_filter_map` directly.
    pub fn with_filter<F>(self, f: F) -> Sender<T>
    where
        F: Fn(&T) -> bool + Send + Sync + 'static,
    {
        self.with_filter_map(move |u| if f(&u) { Some(u) } else { None })
    }

    /// Applies a transform before sending.
    ///
    /// If you want to combine multiple filters and maps with minimal
    /// overhead, use `with_filter_map` directly.
    pub fn with_map<U, F>(self, f: F) -> Sender<U>
    where
        F: Fn(U) -> T + Send + Sync + 'static,
        U: Send + Sync + 'static,
    {
        self.with_filter_map(move |u| Some(f(u)))
    }

    /// Applies a filter and transform before sending.
    ///
    /// Any combination of filters and maps can be expressed using
    /// a single filter_map.
    pub fn with_filter_map<U, F>(self, f: F) -> Sender<U>
    where
        F: Fn(U) -> Option<T> + Send + Sync + 'static,
        U: Send + Sync + 'static,
    {
        let inner: Arc<dyn DynSender<U>> = Arc::new(FilterMapSender {
            f,
            sender: self,
            _p: PhantomData,
        });
        Sender::Boxed(inner)
    }

    /// Future that resolves when the sender is closed
    pub async fn closed(&self) {
        match self {
            Sender::Tokio(tx) => tx.closed().await,
            Sender::Boxed(sink) => sink.closed().await,
        }
    }
}

impl<T> From<tokio::sync::mpsc::Sender<T>> for Sender<T> {
    fn from(tx: tokio::sync::mpsc::Sender<T>) -> Self {
        Self::Tokio(tx)
    }
}

impl<T> TryFrom<Sender<T>> for tokio::sync::mpsc::Sender<T> {
    type Error = Sender<T>;

    fn try_from(value: Sender<T>) -> Result<Self, Self::Error> {
        match value {
            Sender::Tokio(tx) => Ok(tx),
            Sender::Boxed(_) => Err(value),
        }
    }
}

/// A sender that can be wrapped in a `Arc<dyn DynSender<T>>`.
pub trait DynSender<T>: Debug + Send + Sync + 'static {
    /// Send a message.
    ///
    /// For the remote case, if the message can not be completely sent,
    /// this must return an error and disable the channel.
    fn send(&self, value: T) -> Pin<Box<dyn Future<Output = Result<(), SendError>> + Send + '_>>;

    /// Try to send a message, returning as fast as possible if sending
    /// is not currently possible.
    ///
    /// For the remote case, it must be guaranteed that the message is
    /// either completely sent or not at all.
    fn try_send(
        &self,
        value: T,
    ) -> Pin<Box<dyn Future<Output = Result<bool, SendError>> + Send + '_>>;

    /// Await the sender close
    fn closed(&self) -> Pin<Box<dyn Future<Output = ()> + Send + Sync + '_>>;

    /// True if this is a remote sender
    fn is_rpc(&self) -> bool;
}

/// A receiver that can be wrapped in a `Box<dyn DynReceiver<T>>`.
pub trait DynReceiver<T>: Debug + Send + Sync + 'static {
    fn recv(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<T>, RecvError>> + Send + Sync + '_>>;
}

impl<T> Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tokio(x) => f
                .debug_struct("Tokio")
                .field("avail", &x.capacity())
                .field("cap", &x.max_capacity())
                .finish(),
            Self::Boxed(inner) => f.debug_tuple("Boxed").field(&inner).finish(),
        }
    }
}

impl<T: Send + 'static> Sender<T> {
    /// Send a message and yield until either it is sent or an error occurs.
    ///
    /// ## Cancellation safety
    ///
    /// If the future is dropped before completion, and if this is a remote sender,
    /// then the sender will be closed and further sends will return
    /// `SendError::Closed`. Therefore, make sure to always poll the
    /// future until completion if you want to reuse the sender or any clone afterwards.
    pub async fn send(&self, value: T) -> Result<(), SendError> {
        match self {
            Sender::Tokio(tx) => tx
                .send(value)
                .await
                .map_err(|_| e!(SendError::ReceiverClosed)),
            Sender::Boxed(sink) => sink.send(value).await,
        }
    }

    /// Try to send a message, returning as fast as possible if sending
    /// is not currently possible. This can be used to send ephemeral
    /// messages.
    ///
    /// For the local case, this will immediately return false if the
    /// channel is full.
    ///
    /// For the remote case, it will attempt to send the message and
    /// return false if sending the first byte fails, otherwise yield
    /// until the message is completely sent or an error occurs. This
    /// guarantees that the message is sent either completely or not at
    /// all.
    ///
    /// Returns true if the message was sent.
    ///
    /// ## Cancellation safety
    ///
    /// If the future is dropped before completion, and if this is a remote sender,
    /// then the sender will be closed and further sends will return
    /// `SendError::Closed`. Therefore, make sure to always poll the
    /// future until completion if you want to reuse the sender or any clone afterwards.
    pub async fn try_send(&self, value: T) -> Result<bool, SendError> {
        match self {
            Sender::Tokio(tx) => match tx.try_send(value) {
                Ok(()) => Ok(true),
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    Err(e!(SendError::ReceiverClosed))
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Ok(false),
            },
            Sender::Boxed(sink) => sink.try_send(value).await,
        }
    }
}

impl<T> crate::sealed::Sealed for Sender<T> {}
impl<T> crate::Sender for Sender<T> {}

pub enum Receiver<T> {
    Tokio(tokio::sync::mpsc::Receiver<T>),
    Boxed(Box<dyn DynReceiver<T>>),
}

impl<T: Send + Sync + 'static> Receiver<T> {
    /// Receive a message
    ///
    /// Returns Ok(None) if the sender has been dropped or the remote end has
    /// cleanly closed the connection.
    ///
    /// Returns an an io error if there was an error receiving the message.
    pub async fn recv(&mut self) -> Result<Option<T>, RecvError> {
        match self {
            Self::Tokio(rx) => Ok(rx.recv().await),
            Self::Boxed(rx) => Ok(rx.recv().await?),
        }
    }

    /// Map messages, transforming them from type T to type U.
    pub fn map<U, F>(self, f: F) -> Receiver<U>
    where
        F: Fn(T) -> U + Send + Sync + 'static,
        U: Send + Sync + 'static,
    {
        self.filter_map(move |u| Some(f(u)))
    }

    /// Filter messages, only passing through those for which the predicate returns true.
    ///
    /// Messages that don't pass the filter are dropped.
    pub fn filter<F>(self, f: F) -> Receiver<T>
    where
        F: Fn(&T) -> bool + Send + Sync + 'static,
    {
        self.filter_map(move |u| if f(&u) { Some(u) } else { None })
    }

    /// Filter and map messages, only passing through those for which the function returns Some.
    ///
    /// Messages that don't pass the filter are dropped.
    pub fn filter_map<F, U>(self, f: F) -> Receiver<U>
    where
        U: Send + Sync + 'static,
        F: Fn(T) -> Option<U> + Send + Sync + 'static,
    {
        let inner: Box<dyn DynReceiver<U>> = Box::new(FilterMapReceiver {
            f,
            receiver: self,
            _p: PhantomData,
        });
        Receiver::Boxed(inner)
    }

    #[cfg(feature = "stream")]
    pub fn into_stream(
        self,
    ) -> impl n0_future::Stream<Item = Result<T, RecvError>> + Send + Sync + 'static {
        n0_future::stream::unfold(self, |mut recv| async move {
            recv.recv().await.transpose().map(|msg| (msg, recv))
        })
    }
}

impl<T> From<tokio::sync::mpsc::Receiver<T>> for Receiver<T> {
    fn from(rx: tokio::sync::mpsc::Receiver<T>) -> Self {
        Self::Tokio(rx)
    }
}

impl<T> TryFrom<Receiver<T>> for tokio::sync::mpsc::Receiver<T> {
    type Error = Receiver<T>;

    fn try_from(value: Receiver<T>) -> Result<Self, Self::Error> {
        match value {
            Receiver::Tokio(tx) => Ok(tx),
            Receiver::Boxed(_) => Err(value),
        }
    }
}

impl<T> Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tokio(inner) => f
                .debug_struct("Tokio")
                .field("avail", &inner.capacity())
                .field("cap", &inner.max_capacity())
                .finish(),
            Self::Boxed(inner) => f.debug_tuple("Boxed").field(&inner).finish(),
        }
    }
}

struct FilterMapSender<F, T, U> {
    f: F,
    sender: Sender<T>,
    _p: PhantomData<U>,
}

impl<F, T, U> Debug for FilterMapSender<F, T, U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilterMapSender").finish_non_exhaustive()
    }
}

impl<F, T, U> DynSender<U> for FilterMapSender<F, T, U>
where
    F: Fn(U) -> Option<T> + Send + Sync + 'static,
    T: Send + Sync + 'static,
    U: Send + Sync + 'static,
{
    fn send(&self, value: U) -> Pin<Box<dyn Future<Output = Result<(), SendError>> + Send + '_>> {
        Box::pin(async move {
            match (self.f)(value) {
                Some(v) => self.sender.send(v).await,
                _ => Ok(()),
            }
        })
    }

    fn try_send(
        &self,
        value: U,
    ) -> Pin<Box<dyn Future<Output = Result<bool, SendError>> + Send + '_>> {
        Box::pin(async move {
            match (self.f)(value) {
                Some(v) => self.sender.try_send(v).await,
                _ => Ok(true),
            }
        })
    }

    fn is_rpc(&self) -> bool {
        self.sender.is_rpc()
    }

    fn closed(&self) -> Pin<Box<dyn Future<Output = ()> + Send + Sync + '_>> {
        match self {
            FilterMapSender {
                sender: Sender::Tokio(tx),
                ..
            } => Box::pin(tx.closed()),
            FilterMapSender {
                sender: Sender::Boxed(sink),
                ..
            } => sink.closed(),
        }
    }
}

struct FilterMapReceiver<F, T, U> {
    f: F,
    receiver: Receiver<T>,
    _p: PhantomData<U>,
}

impl<F, T, U> Debug for FilterMapReceiver<F, T, U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilterMapReceiver").finish_non_exhaustive()
    }
}

impl<F, T, U> DynReceiver<U> for FilterMapReceiver<F, T, U>
where
    F: Fn(T) -> Option<U> + Send + Sync + 'static,
    T: Send + Sync + 'static,
    U: Send + Sync + 'static,
{
    fn recv(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<U>, RecvError>> + Send + Sync + '_>> {
        Box::pin(async move {
            while let Some(msg) = self.receiver.recv().await? {
                if let Some(v) = (self.f)(msg) {
                    return Ok(Some(v));
                }
            }
            Ok(None)
        })
    }
}

impl<T> crate::sealed::Sealed for Receiver<T> {}
impl<T> crate::Receiver for Receiver<T> {}
