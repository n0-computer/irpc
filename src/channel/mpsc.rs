//! SPSC channel, similar to tokio's mpsc channel
//!
//! For the rpc case, the send side can not be cloned, hence mpsc instead of mpsc.

use std::{fmt::Debug, future::Future, io, marker::PhantomData, pin::Pin, sync::Arc};

use n0_error::{e, stack_error};

use super::SendError;

/// Error when receiving a oneshot or mpsc message. For local communication,
/// the only thing that can go wrong is that the sender has been closed.
///
/// For rpc communication, there can be any number of errors, so this is a
/// generic io error.
#[stack_error(derive, add_meta, from_sources)]
#[non_exhaustive]
pub enum RecvError {
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
            RecvError::MaxMessageSizeExceeded { .. } => {
                io::Error::new(io::ErrorKind::InvalidData, e)
            }
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
///
/// Create a local sender with [`channel`] or from a tokio mpsc sender, and a
/// boxed sender from an `Arc<dyn DynSender<T>>`.
pub struct Sender<T>(SenderInner<T>);

enum SenderInner<T> {
    Tokio(tokio::sync::mpsc::Sender<T>),
    Boxed(Arc<dyn DynSender<T>>),
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self(match &self.0 {
            SenderInner::Tokio(tx) => SenderInner::Tokio(tx.clone()),
            SenderInner::Boxed(inner) => SenderInner::Boxed(inner.clone()),
        })
    }
}

impl<T> Sender<T> {
    pub fn is_rpc(&self) -> bool
    where
        T: 'static,
    {
        match &self.0 {
            SenderInner::Tokio(_) => false,
            SenderInner::Boxed(x) => x.is_rpc(),
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
        Sender(SenderInner::Boxed(inner))
    }

    /// Future that resolves when the sender is closed
    pub async fn closed(&self) {
        match &self.0 {
            SenderInner::Tokio(tx) => tx.closed().await,
            SenderInner::Boxed(sink) => sink.closed().await,
        }
    }
}

impl<T> From<tokio::sync::mpsc::Sender<T>> for Sender<T> {
    fn from(tx: tokio::sync::mpsc::Sender<T>) -> Self {
        Self(SenderInner::Tokio(tx))
    }
}

/// Converts a dynamic sender into a sender.
impl<T> From<Arc<dyn DynSender<T>>> for Sender<T> {
    fn from(tx: Arc<dyn DynSender<T>>) -> Self {
        Self(SenderInner::Boxed(tx))
    }
}

impl<T> TryFrom<Sender<T>> for tokio::sync::mpsc::Sender<T> {
    type Error = Sender<T>;

    fn try_from(value: Sender<T>) -> Result<Self, Self::Error> {
        match value.0 {
            SenderInner::Tokio(tx) => Ok(tx),
            inner @ SenderInner::Boxed(_) => Err(Sender(inner)),
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
        match &self.0 {
            SenderInner::Tokio(x) => f
                .debug_struct("Tokio")
                .field("avail", &x.capacity())
                .field("cap", &x.max_capacity())
                .finish(),
            SenderInner::Boxed(inner) => f.debug_tuple("Boxed").field(&inner).finish(),
        }
    }
}

impl<T: Send + 'static> Sender<T> {
    /// Send a message and yield until either it is sent or an error occurs.
    ///
    /// ## Cancellation safety
    ///
    /// If the future is dropped before completion, and if this is a remote sender,
    /// then the sender will be closed and further sends will return an [`SendError::Io`]
    /// with [`std::io::ErrorKind::BrokenPipe`]. Therefore, make sure to always poll the
    /// future until completion if you want to reuse the sender or any clone afterwards.
    pub async fn send(&self, value: T) -> Result<(), SendError> {
        match &self.0 {
            SenderInner::Tokio(tx) => tx
                .send(value)
                .await
                .map_err(|_| e!(SendError::ReceiverClosed)),
            SenderInner::Boxed(sink) => sink.send(value).await,
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
    /// then the sender will be closed and further sends will return an [`SendError::Io`]
    /// with [`std::io::ErrorKind::BrokenPipe`]. Therefore, make sure to always poll the
    /// future until completion if you want to reuse the sender or any clone afterwards.
    pub async fn try_send(&self, value: T) -> Result<bool, SendError> {
        match &self.0 {
            SenderInner::Tokio(tx) => match tx.try_send(value) {
                Ok(()) => Ok(true),
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    Err(e!(SendError::ReceiverClosed))
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Ok(false),
            },
            SenderInner::Boxed(sink) => sink.try_send(value).await,
        }
    }
}

impl<T> crate::sealed::Sealed for Sender<T> {}
impl<T> crate::Sender for Sender<T> {}

/// Single consumer receiver.
///
/// Create a local receiver with [`channel`] or from a tokio mpsc receiver, and
/// a boxed receiver from a `Box<dyn DynReceiver<T>>`.
pub struct Receiver<T>(ReceiverInner<T>);

enum ReceiverInner<T> {
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
        match &mut self.0 {
            ReceiverInner::Tokio(rx) => Ok(rx.recv().await),
            ReceiverInner::Boxed(rx) => Ok(rx.recv().await?),
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
        Receiver(ReceiverInner::Boxed(inner))
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
        Self(ReceiverInner::Tokio(rx))
    }
}

/// Converts a dynamic receiver into a receiver.
impl<T> From<Box<dyn DynReceiver<T>>> for Receiver<T> {
    fn from(rx: Box<dyn DynReceiver<T>>) -> Self {
        Self(ReceiverInner::Boxed(rx))
    }
}

impl<T> TryFrom<Receiver<T>> for tokio::sync::mpsc::Receiver<T> {
    type Error = Receiver<T>;

    fn try_from(value: Receiver<T>) -> Result<Self, Self::Error> {
        match value.0 {
            ReceiverInner::Tokio(rx) => Ok(rx),
            inner @ ReceiverInner::Boxed(_) => Err(Receiver(inner)),
        }
    }
}

impl<T> Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            ReceiverInner::Tokio(inner) => f
                .debug_struct("Tokio")
                .field("avail", &inner.capacity())
                .field("cap", &inner.max_capacity())
                .finish(),
            ReceiverInner::Boxed(inner) => f.debug_tuple("Boxed").field(&inner).finish(),
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
        match &self.sender.0 {
            SenderInner::Tokio(tx) => Box::pin(tx.closed()),
            SenderInner::Boxed(sink) => sink.closed(),
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
