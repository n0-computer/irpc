//! SPSC channel, similar to tokio's mpsc channel
//!
//! For the rpc case, the send side can not be cloned, hence mpsc instead of mpsc.

use std::{
    fmt::Debug,
    future::{Future, poll_fn},
    io,
    marker::PhantomData,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, ready},
};

use n0_error::{e, stack_error};

use super::SendError;
#[cfg(feature = "rpc")]
use crate::rpc::{ErrorCode, RpcRecvError};

/// Error when receiving an mpsc message.
///
/// A local channel has no errors: when all senders are dropped, the receiver
/// gets `Ok(None)`. A remote channel can fail for reasons that a local channel
/// does not have, see [`RpcRecvError`].
///
/// For a remote channel, the sender is on the other side of the connection:
/// the server for a response stream, and the client for the updates of a request.
#[stack_error(derive, add_meta, from_sources)]
#[non_exhaustive]
pub enum RecvError {
    /// The remote channel failed for a reason that a local channel does not have.
    ///
    /// If the sender resets the stream, even with code 0, the receiver gets
    /// this error instead of `Ok(None)`. A reset can discard messages that
    /// were sent but not read yet, so the channel did not end cleanly.
    #[cfg(feature = "rpc")]
    #[error("Remote channel failed")]
    Rpc { source: RpcRecvError },
}

/// Converts an io error from reading the stream of a remote channel.
///
/// See [`RpcRecvError`]'s conversion from an io error.
#[cfg(feature = "rpc")]
impl From<io::Error> for RecvError {
    fn from(err: io::Error) -> Self {
        RpcRecvError::from(err).into()
    }
}

impl From<RecvError> for io::Error {
    fn from(e: RecvError) -> Self {
        match e {
            #[cfg(feature = "rpc")]
            RecvError::Rpc { source, .. } => source.into(),
        }
    }
}

#[cfg(feature = "rpc")]
impl RecvError {
    /// Returns the code that the peer sent, if any.
    ///
    /// See [`RpcRecvError::remote_code`].
    pub fn remote_code(&self) -> Option<ErrorCode> {
        match self {
            Self::Rpc { source, .. } => source.remote_code(),
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
    pub(crate) fn from_boxed(inner: Arc<dyn DynSender<T>>) -> Self {
        Self(SenderInner::Boxed(inner))
    }

    /// Returns true if sending can fail in more ways than the receiver being dropped.
    ///
    /// This is about which failures can occur, not about where the receiver is.
    /// A purely in-memory sender is not rpc, even if it is boxed, e.g. after
    /// [`Self::with_map`] or [`Self::with_filter_map`]: it only fails with
    /// [`SendError::ReceiverClosed`]. A sender that goes through a connection
    /// that can break on its own is rpc, even if both ends are on the same
    /// machine, e.g. shared memory between processes.
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
        Sender::from_boxed(inner)
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

impl<T> TryFrom<Sender<T>> for tokio::sync::mpsc::Sender<T> {
    type Error = Sender<T>;

    fn try_from(value: Sender<T>) -> Result<Self, Self::Error> {
        match value.0 {
            SenderInner::Tokio(tx) => Ok(tx),
            SenderInner::Boxed(_) => Err(value),
        }
    }
}

/// A sender that can be wrapped in a `Arc<dyn DynSender<T>>`.
pub(crate) trait DynSender<T>: Debug + Send + Sync + 'static {
    /// Send a message.
    ///
    /// For the remote case, the receiver gets a message either completely or
    /// not at all. If the send is cancelled, the message must not be
    /// delivered, and the sender must stay usable. A receiver from irpc 0.17 or
    /// earlier is the exception, see [`Sender::send`].
    fn send(&self, value: T) -> Pin<Box<dyn Future<Output = Result<(), SendError>> + Send + '_>>;

    /// Try to send a message, returning as fast as possible if sending
    /// is not currently possible.
    ///
    /// For the remote case, it must be guaranteed that the message is
    /// either completely sent or not at all. If the send is cancelled, the
    /// same rules as for [`Self::send`] apply.
    fn try_send(
        &self,
        value: T,
    ) -> Pin<Box<dyn Future<Output = Result<bool, SendError>> + Send + '_>>;

    /// Await the sender close
    fn closed(&self) -> Pin<Box<dyn Future<Output = ()> + Send + Sync + '_>>;

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

/// A receiver that can be wrapped in a `Box<dyn DynReceiver<T>>`.
pub(crate) trait DynReceiver<T>: Debug + Send + Sync + 'static {
    /// Polls for the next message, keeping a message read in part for the next poll.
    ///
    /// Returns `Ok(None)` once the sender has closed the channel.
    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<T>, RecvError>>;
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
    /// If the future is dropped before completion, the message is not
    /// delivered, and the sender stays usable.
    ///
    /// The exception is a remote receiver from irpc 0.17 or earlier, which does
    /// not know that a message can be marked as aborted. If the cancelled send
    /// wrote part of the message, the next send finishes it, and such a receiver
    /// delivers it. So a retry after a cancelled send can deliver the message twice.
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
    /// If the future is dropped before completion, the message is not
    /// delivered, and the sender stays usable.
    ///
    /// The exception is a remote receiver from irpc 0.17 or earlier, which does
    /// not know that a message can be marked as aborted. If the cancelled send
    /// wrote part of the message, the next send finishes it, and such a receiver
    /// delivers it. So a retry after a cancelled send can deliver the message twice.
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

/// A receiver for local or remote messages.
pub struct Receiver<T>(ReceiverInner<T>);

enum ReceiverInner<T> {
    Tokio(tokio::sync::mpsc::Receiver<T>),
    Boxed(Box<dyn DynReceiver<T>>),
}

impl<T> Receiver<T> {
    pub(crate) fn from_boxed(inner: Box<dyn DynReceiver<T>>) -> Self {
        Self(ReceiverInner::Boxed(inner))
    }
}

impl<T: Send + Sync + 'static> Receiver<T> {
    /// Receives a message.
    ///
    /// Returns `Ok(None)` after the last complete message, when all local
    /// senders are dropped or the remote sender finishes its stream.
    ///
    /// Returns [`RecvError`] when receiving fails. Receiving on a local
    /// channel never fails. An error ends the channel, and later calls
    /// return `Ok(None)`.
    ///
    /// ## Cancellation safety
    ///
    /// This method is cancellation safe. A message read in part when the future
    /// drops stays with the receiver, and the next call returns it whole.
    pub async fn recv(&mut self) -> Result<Option<T>, RecvError> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// Polls for the next message, keeping a message read in part for the next poll.
    ///
    /// Returns `Ok(None)` after the last complete message when all local
    /// senders are dropped or the remote sender finishes its stream.
    ///
    /// See [`Self::recv`] for errors.
    /// After returning an error, later polls return `Ok(None)`.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<T>, RecvError>> {
        match &mut self.0 {
            ReceiverInner::Tokio(rx) => rx.poll_recv(cx).map(Ok),
            ReceiverInner::Boxed(rx) => rx.poll_recv(cx),
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
        Receiver::from_boxed(inner)
    }

    #[cfg(feature = "stream")]
    pub fn into_stream(
        self,
    ) -> impl n0_future::Stream<Item = Result<T, RecvError>> + Send + Sync + 'static {
        self
    }
}

#[cfg(feature = "stream")]
impl<T: Send + Sync + 'static> n0_future::Stream for Receiver<T> {
    type Item = Result<T, RecvError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().poll_recv(cx).map(Result::transpose)
    }
}

impl<T> From<tokio::sync::mpsc::Receiver<T>> for Receiver<T> {
    fn from(rx: tokio::sync::mpsc::Receiver<T>) -> Self {
        Self(ReceiverInner::Tokio(rx))
    }
}

impl<T> TryFrom<Receiver<T>> for tokio::sync::mpsc::Receiver<T> {
    type Error = Receiver<T>;

    fn try_from(value: Receiver<T>) -> Result<Self, Self::Error> {
        match value.0 {
            ReceiverInner::Tokio(tx) => Ok(tx),
            ReceiverInner::Boxed(_) => Err(value),
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
    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<U>, RecvError>> {
        loop {
            let Some(msg) = ready!(self.receiver.poll_recv(cx))? else {
                return Poll::Ready(Ok(None));
            };
            if let Some(v) = (self.f)(msg) {
                return Poll::Ready(Ok(Some(v)));
            }
        }
    }
}

impl<T> crate::sealed::Sealed for Receiver<T> {}
impl<T> crate::Receiver for Receiver<T> {}
