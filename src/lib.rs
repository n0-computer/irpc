//! # A minimal RPC library for use with [iroh](https://docs.rs/iroh/latest/iroh/index.html).
//!
//! ## Goals
//!
//! The main goal of this library is to provide an rpc framework that is so
//! lightweight that it can be also used for async boundaries within a single
//! process without any overhead, instead of the usual practice of a mpsc channel
//! with a giant message enum where each enum case contains mpsc or oneshot
//! backchannels.
//!
//! The second goal is to lightly abstract over remote and local communication,
//! so that a system can be interacted with cross process or even across networks.
//!
//! ## Non-goals
//!
//! - Cross language interop. This is for talking from rust to rust
//! - Any kind of versioning. You have to do this yourself
//! - Making remote message passing look like local async function calls
//! - Being runtime agnostic. This is for tokio
//!
//! ## Interaction patterns
//!
//! For each request, there can be a reply and update channel. Each channel
//! can be either oneshot, carry multiple messages, or be disabled. This enables
//! the typical interaction patterns known from libraries like grpc:
//!
//! - rpc: 1 request, 1 reply
//! - server streaming: 1 request, multiple replys
//! - client streaming: multiple requests, 1 reply
//! - bidi streaming: multiple requests, multiple replys
//!
//! as well as more complex patterns. It is however not possible to have multiple
//! differently typed reply channels for a single message type.
//!
//! ## Transports
//!
//! We don't abstract over the send and receive stream. These must always be
//! quinn streams, specifically streams from the [iroh quinn fork].
//!
//! This restricts the possible rpc transports to quinn (QUIC with dial by
//! socket address) and iroh (QUIC with dial by node id).
//!
//! An upside of this is that the quinn streams can be tuned for each rpc
//! request, e.g. by setting the stream priority or by directy using more
//! advanced part of the quinn SendStream and RecvStream APIs such as out of
//! order receiving.
//!
//! ## Serialization
//!
//! Serialization is currently done using [postcard]. Messages are always
//! length prefixed with postcard varints, even in the case of oneshot
//! channels.
//!
//! Serialization only happens for cross process rpc communication.
//!
//! However, the requirement for message enums to be serializable is present even
//! when disabling the `rpc` feature. Due to the fact that the channels live
//! outside the message, this is not a big restriction.
//!
//! ## Features
//!
//! - `rpc`: Enable the rpc features. Enabled by default.
//!   By disabling this feature, all rpc related dependencies are removed.
//!   The remaining dependencies are just serde, tokio and tokio-util.
//! - `message_spans`: Enable tracing spans for messages. Enabled by default.
//!   This is useful even without rpc, to not lose tracing context when message
//!   passing. This is frequently done manually. This obviously requires
//!   a dependency on tracing.
//! - `quinn_endpoint_setup`: Easy way to create quinn endpoints. This is useful
//!   both for testing and for rpc on localhost. Enabled by default.
//!
//! # History
//!
//! This crate evolved out of the [quic-rpc](https://docs.rs/quic-rpc/latest/quic-rpc/index.html) crate, which is a generic RPC
//! framework for any transport with cheap streams such as QUIC. Compared to
//! quic-rpc, this crate does not abstract over the stream type and is focused
//! on [iroh](https://docs.rs/iroh/latest/iroh/index.html) and our [iroh quinn fork](https://docs.rs/iroh-quinn/latest/iroh-quinn/index.html).
#![cfg_attr(quicrpc_docsrs, feature(doc_cfg))]
use std::{fmt::Debug, future::Future, io, marker::PhantomData, ops::Deref, result};

#[cfg(feature = "derive")]
#[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "derive")))]
pub use irpc_derive::rpc_requests;
use sealed::Sealed;
use serde::{de::DeserializeOwned, Serialize};

use self::channel::none::NoReceiver;

#[cfg(feature = "rpc")]
#[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "rpc")))]
pub mod util;
#[cfg(not(feature = "rpc"))]
mod util;

/// Requirements for a RPC message
///
/// Even when just using the mem transport, we require messages to be Serializable and Deserializable.
/// Likewise, even when using the quinn transport, we require messages to be Send.
///
/// This does not seem like a big restriction. If you want a pure memory channel without the possibility
/// to also use the quinn transport, you might want to use a mpsc channel directly.
pub trait RpcMessage: Debug + Serialize + DeserializeOwned + Send + Sync + Unpin + 'static {}

impl<T> RpcMessage for T where
    T: Debug + Serialize + DeserializeOwned + Send + Sync + Unpin + 'static
{
}

/// Marker trait for a service
///
/// This is usually implemented by a zero-sized struct.
/// It has various bounds to make derives easier.
///
/// A service acts as a scope for defining the reply and request channels for each
/// message type, and provides some type safety when sending messages.
pub trait Service: Send + Sync + Debug + Clone + 'static {}

mod sealed {
    pub trait Sealed {}
}

/// Sealed marker trait for a sender
pub trait Sender: Debug + Sealed {}

/// Sealed marker trait for a receiver
pub trait Receiver: Debug + Sealed {}

/// Trait to specify channels for a message and service
pub trait Channels<S: Service> {
    /// The sender type, can be either mpsc, oneshot or none
    type Response: Sender;
    /// The receiver type, can be either mpsc, oneshot or none
    ///
    /// For many services, the receiver is not needed, so it can be set to [`NoReceiver`].
    type Request: Receiver;
}

/// Channels that abstract over local or remote sending
pub mod channel {
    use std::io;

    /// Oneshot channel, similar to tokio's oneshot channel
    pub mod oneshot {
        use std::{fmt::Debug, future::Future, io, pin::Pin, task};

        use n0_future::future::Boxed as BoxFuture;

        use super::{RecvError, SendError};
        use crate::util::FusedOneshotReceiver;

        /// Create a local oneshot sender and receiver pair.
        ///
        /// This is currently using a tokio channel pair internally.
        pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
            let (reply, request) = tokio::sync::oneshot::channel();
            (reply.into(), request.into())
        }

        /// A generic boxed sender.
        ///
        /// Remote senders are always boxed, since for remote communication the boxing
        /// overhead is negligible. However, boxing can also be used for local communication,
        /// e.g. when applying a transform or filter to the message before sending it.
        pub type BoxedSender<T> =
            Box<dyn FnOnce(T) -> BoxFuture<io::Result<()>> + Send + Sync + 'static>;

        /// A sender that can be wrapped in a `Box<dyn DynSender<T>>`.
        ///
        /// In addition to implementing `Future`, this provides a fn to check if the sender is
        /// an rpc sender.
        ///
        /// Remote receivers are always boxed, since for remote communication the boxing
        /// overhead is negligible. However, boxing can also be used for local communication,
        /// e.g. when applying a transform or filter to the message before receiving it.
        pub trait DynSender<T>: Future<Output = io::Result<()>> + Send + Sync + 'static {
            fn is_rpc(&self) -> bool;
        }

        /// A generic boxed receiver
        ///
        /// Remote receivers are always boxed, since for remote communication the boxing
        /// overhead is negligible. However, boxing can also be used for local communication,
        /// e.g. when applying a transform or filter to the message before receiving it.
        pub type BoxedReceiver<T> = BoxFuture<io::Result<T>>;

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
            fn from(reply: tokio::sync::oneshot::Sender<T>) -> Self {
                Self::Tokio(reply)
            }
        }

        impl<T> TryFrom<Sender<T>> for tokio::sync::oneshot::Sender<T> {
            type Error = Sender<T>;

            fn try_from(value: Sender<T>) -> Result<Self, Self::Error> {
                match value {
                    Sender::Tokio(reply) => Ok(reply),
                    Sender::Boxed(_) => Err(value),
                }
            }
        }

        impl<T> Sender<T> {
            /// Send a message
            ///
            /// If this is a boxed sender that represents a remote connection, sending may yield or fail with an io error.
            /// Local senders will never yield, but can fail if the receiver has been closed.
            pub async fn send(self, value: T) -> std::result::Result<(), SendError> {
                match self {
                    Sender::Tokio(reply) => {
                        reply.send(value).map_err(|_| SendError::ReceiverClosed)
                    }
                    Sender::Boxed(f) => f(value).await.map_err(SendError::from),
                }
            }
        }

        impl<T> Sender<T> {
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
            type Output = std::result::Result<T, RecvError>;

            fn poll(self: Pin<&mut Self>, cx: &mut task::Context) -> task::Poll<Self::Output> {
                match self.get_mut() {
                    Self::Tokio(request) => Pin::new(request)
                        .poll(cx)
                        .map_err(|_| RecvError::SenderClosed),
                    Self::Boxed(request) => Pin::new(request).poll(cx).map_err(RecvError::Io),
                }
            }
        }

        /// Convert a tokio oneshot receiver to a receiver for this crate
        impl<T> From<tokio::sync::oneshot::Receiver<T>> for Receiver<T> {
            fn from(request: tokio::sync::oneshot::Receiver<T>) -> Self {
                Self::Tokio(FusedOneshotReceiver(request))
            }
        }

        impl<T> TryFrom<Receiver<T>> for tokio::sync::oneshot::Receiver<T> {
            type Error = Receiver<T>;

            fn try_from(value: Receiver<T>) -> Result<Self, Self::Error> {
                match value {
                    Receiver::Tokio(reply) => Ok(reply.0),
                    Receiver::Boxed(_) => Err(value),
                }
            }
        }

        /// Convert a function that produces a future to a receiver for this crate
        impl<T, F, Fut> From<F> for Receiver<T>
        where
            F: FnOnce() -> Fut,
            Fut: Future<Output = io::Result<T>> + Send + 'static,
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
    }

    /// SPSC channel, similar to tokio's mpsc channel
    ///
    /// For the rpc case, the send side can not be cloned, hence mpsc instead of mpsc.
    pub mod mpsc {
        use std::{fmt::Debug, future::Future, io, pin::Pin, sync::Arc};

        use super::{RecvError, SendError};
        use crate::RpcMessage;

        /// Create a local mpsc sender and receiver pair, with the given buffer size.
        ///
        /// This is currently using a tokio channel pair internally.
        pub fn channel<T>(buffer: usize) -> (Sender<T>, Receiver<T>) {
            let (reply, request) = tokio::sync::mpsc::channel(buffer);
            (reply.into(), request.into())
        }

        /// Single producer, single consumer sender.
        ///
        /// For the local case, this wraps a tokio::sync::mpsc::Sender.
        #[derive(Clone)]
        pub enum Sender<T> {
            Tokio(tokio::sync::mpsc::Sender<T>),
            Boxed(Arc<dyn DynSender<T>>),
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

            pub async fn closed(&self)
            where
                T: RpcMessage,
            {
                match self {
                    Sender::Tokio(reply) => reply.closed().await,
                    Sender::Boxed(sink) => sink.closed().await,
                }
            }

            #[cfg(feature = "stream")]
            pub fn into_sink(self) -> impl n0_future::Sink<T, Error = SendError> + Send + 'static
            where
                T: RpcMessage,
            {
                futures_util::sink::unfold(self, |sink, value| async move {
                    sink.send(value).await?;
                    Ok(sink)
                })
            }
        }

        impl<T> From<tokio::sync::mpsc::Sender<T>> for Sender<T> {
            fn from(reply: tokio::sync::mpsc::Sender<T>) -> Self {
                Self::Tokio(reply)
            }
        }

        impl<T> TryFrom<Sender<T>> for tokio::sync::mpsc::Sender<T> {
            type Error = Sender<T>;

            fn try_from(value: Sender<T>) -> Result<Self, Self::Error> {
                match value {
                    Sender::Tokio(reply) => Ok(reply),
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
            fn send(
                &self,
                value: T,
            ) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + Sync + '_>>;

            /// Try to send a message, returning as fast as possible if sending
            /// is not currently possible.
            ///
            /// For the remote case, it must be guaranteed that the message is
            /// either completely sent or not at all.
            fn try_send(
                &self,
                value: T,
            ) -> Pin<Box<dyn Future<Output = io::Result<bool>> + Send + Sync + '_>>;

            /// Await the sender close
            fn closed(&self) -> Pin<Box<dyn Future<Output = ()> + Send + Sync + '_>>;

            /// True if this is a remote sender
            fn is_rpc(&self) -> bool;
        }

        /// A receiver that can be wrapped in a `Box<dyn DynReceiver<T>>`.
        pub trait DynReceiver<T>: Debug + Send + Sync + 'static {
            fn recv(
                &mut self,
            ) -> Pin<
                Box<
                    dyn Future<Output = std::result::Result<Option<T>, RecvError>>
                        + Send
                        + Sync
                        + '_,
                >,
            >;
        }

        impl<T> Debug for Sender<T> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    Self::Tokio(x) => f
                        .debug_struct("Tokio")
                        .field("avail", &x.capacity())
                        .field("cap", &x.max_capacity())
                        .finish(),
                    Self::Boxed(message) => f.debug_tuple("Boxed").field(&message).finish(),
                }
            }
        }

        impl<T: RpcMessage> Sender<T> {
            /// Send a message and yield until either it is sent or an error occurs.
            ///
            /// ## Cancellation safety
            ///
            /// If the future is dropped before completion, and if this is a remote sender,
            /// then the sender will be closed and further sends will return an [`io::Error`]
            /// with [`io::ErrorKind::BrokenPipe`]. Therefore, make sure to always poll the
            /// future until completion if you want to reuse the sender or any clone afterwards.
            pub async fn send(&self, value: T) -> std::result::Result<(), SendError> {
                match self {
                    Sender::Tokio(reply) => reply
                        .send(value)
                        .await
                        .map_err(|_| SendError::ReceiverClosed),
                    Sender::Boxed(sink) => sink.send(value).await.map_err(SendError::from),
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
            /// then the sender will be closed and further sends will return an [`io::Error`]
            /// with [`io::ErrorKind::BrokenPipe`]. Therefore, make sure to always poll the
            /// future until completion if you want to reuse the sender or any clone afterwards.
            pub async fn try_send(&mut self, value: T) -> std::result::Result<bool, SendError> {
                match self {
                    Sender::Tokio(reply) => match reply.try_send(value) {
                        Ok(()) => Ok(true),
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                            Err(SendError::ReceiverClosed)
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Ok(false),
                    },
                    Sender::Boxed(sink) => sink.try_send(value).await.map_err(SendError::from),
                }
            }
        }

        impl<T> crate::sealed::Sealed for Sender<T> {}
        impl<T> crate::Sender for Sender<T> {}

        pub enum Receiver<T> {
            Tokio(tokio::sync::mpsc::Receiver<T>),
            Boxed(Box<dyn DynReceiver<T>>),
        }

        impl<T: RpcMessage> Receiver<T> {
            /// Receive a message
            ///
            /// Returns Ok(None) if the sender has been dropped or the remote end has
            /// cleanly closed the connection.
            ///
            /// Returns an an io error if there was an error receiving the message.
            pub async fn recv(&mut self) -> std::result::Result<Option<T>, RecvError> {
                match self {
                    Self::Tokio(request) => Ok(request.recv().await),
                    Self::Boxed(request) => Ok(request.recv().await?),
                }
            }

            #[cfg(feature = "stream")]
            pub fn into_stream(
                self,
            ) -> impl n0_future::Stream<Item = std::result::Result<T, RecvError>> + Send + Sync + 'static
            {
                n0_future::stream::unfold(self, |mut recv| async move {
                    recv.recv().await.transpose().map(|msg| (msg, recv))
                })
            }
        }

        impl<T> From<tokio::sync::mpsc::Receiver<T>> for Receiver<T> {
            fn from(request: tokio::sync::mpsc::Receiver<T>) -> Self {
                Self::Tokio(request)
            }
        }

        impl<T> TryFrom<Receiver<T>> for tokio::sync::mpsc::Receiver<T> {
            type Error = Receiver<T>;

            fn try_from(value: Receiver<T>) -> Result<Self, Self::Error> {
                match value {
                    Receiver::Tokio(reply) => Ok(reply),
                    Receiver::Boxed(_) => Err(value),
                }
            }
        }

        impl<T> Debug for Receiver<T> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    Self::Tokio(message) => f
                        .debug_struct("Tokio")
                        .field("avail", &message.capacity())
                        .field("cap", &message.max_capacity())
                        .finish(),
                    Self::Boxed(message) => f.debug_tuple("Boxed").field(&message).finish(),
                }
            }
        }

        impl<T> crate::sealed::Sealed for Receiver<T> {}
        impl<T> crate::Receiver for Receiver<T> {}
    }

    /// No channels, used when no communication is needed
    pub mod none {
        use crate::sealed::Sealed;

        /// A sender that does nothing. This is used when no communication is needed.
        #[derive(Debug)]
        pub struct NoSender;
        impl Sealed for NoSender {}
        impl crate::Sender for NoSender {}

        /// A receiver that does nothing. This is used when no communication is needed.
        #[derive(Debug)]
        pub struct NoReceiver;

        impl Sealed for NoReceiver {}
        impl crate::Receiver for NoReceiver {}
    }

    /// Error when sending a oneshot or mpsc message. For local communication,
    /// the only thing that can go wrong is that the receiver has been dropped.
    ///
    /// For rpc communication, there can be any number of errors, so this is a
    /// generic io error.
    #[derive(Debug, thiserror::Error)]
    pub enum SendError {
        /// The receiver has been closed. This is the only error that can occur
        /// for local communication.
        #[error("receiver closed")]
        ReceiverClosed,
        /// The underlying io error. This can occur for remote communication,
        /// due to a network error or serialization error.
        #[error("io error: {0}")]
        Io(#[from] io::Error),
    }

    impl From<SendError> for io::Error {
        fn from(e: SendError) -> Self {
            match e {
                SendError::ReceiverClosed => io::Error::new(io::ErrorKind::BrokenPipe, e),
                SendError::Io(e) => e,
            }
        }
    }

    /// Error when receiving a oneshot or mpsc message. For local communication,
    /// the only thing that can go wrong is that the sender has been closed.
    ///
    /// For rpc communication, there can be any number of errors, so this is a
    /// generic io error.
    #[derive(Debug, thiserror::Error)]
    pub enum RecvError {
        /// The sender has been closed. This is the only error that can occur
        /// for local communication.
        #[error("sender closed")]
        SenderClosed,
        /// An io error occurred. This can occur for remote communication,
        /// due to a network error or deserialization error.
        #[error("io error: {0}")]
        Io(#[from] io::Error),
    }

    impl From<RecvError> for io::Error {
        fn from(e: RecvError) -> Self {
            match e {
                RecvError::Io(e) => e,
                RecvError::SenderClosed => io::Error::new(io::ErrorKind::BrokenPipe, e),
            }
        }
    }
}

/// A wrapper for a message with channels to send and receive it.
/// This expands the protocol message to a full message that includes the
/// active and unserializable channels.
///
/// The channel kind for request and reply is defined by implementing the `Channels`
/// trait, either manually or using a macro.
///
/// When the `message_spans` feature is enabled, this also includes a tracing
/// span to carry the tracing context during message passing.
pub struct Request<I: Channels<S>, S: Service> {
    /// The request message.
    pub message: I,
    /// The return channel to send the reply to. Can be set to [`crate::channel::none::NoSender`] if not needed.
    pub reply: <I as Channels<S>>::Response,
    /// The request channel to receive the request from. Can be set to [`NoReceiver`] if not needed.
    pub updates: <I as Channels<S>>::Request,
    /// The current span where the full message was created.
    #[cfg(feature = "message_spans")]
    #[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "message_spans")))]
    pub span: tracing::Span,
}

impl<I: Channels<S> + Debug, S: Service> Debug for Request<I, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("")
            .field(&self.message)
            .field(&self.reply)
            .field(&self.updates)
            .finish()
    }
}

impl<I: Channels<S>, S: Service> Request<I, S> {
    /// Get the parent span
    #[cfg(feature = "message_spans")]
    pub fn parent_span_opt(&self) -> Option<&tracing::Span> {
        Some(&self.span)
    }
}

/// Tuple conversion from message message and reply/request channels to a Request struct
///
/// For the case where you want both reply and request channels.
impl<I: Channels<S>, S: Service, Response, Updates> From<(I, Response, Updates)> for Request<I, S>
where
    I: Channels<S>,
    <I as Channels<S>>::Response: From<Response>,
    <I as Channels<S>>::Request: From<Updates>,
{
    fn from(message: (I, Response, Updates)) -> Self {
        let (message, reply, updates) = message;
        Self {
            message,
            reply: reply.into(),
            updates: updates.into(),
            #[cfg(feature = "message_spans")]
            #[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "message_spans")))]
            span: tracing::Span::current(),
        }
    }
}

/// Tuple conversion from message message and reply channel to a Request struct
///
/// For the very common case where you just need a reply channel to send the reply to.
impl<I, S, Response> From<(I, Response)> for Request<I, S>
where
    I: Channels<S, Request = NoReceiver>,
    S: Service,
    <I as Channels<S>>::Response: From<Response>,
{
    fn from(message: (I, Response)) -> Self {
        let (message, reply) = message;
        Self {
            message,
            reply: reply.into(),
            updates: NoReceiver,
            #[cfg(feature = "message_spans")]
            #[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "message_spans")))]
            span: tracing::Span::current(),
        }
    }
}

/// Deref so you can access the message fields directly.
///
/// If the message message has fields named `reply`, `request` or `span`, you need to use the
/// `message` field to access them.
impl<I: Channels<S>, S: Service> Deref for Request<I, S> {
    type Target = I;

    fn deref(&self) -> &Self::Target {
        &self.message
    }
}

/// A client to the service `S` using the local message type `M` and the remote
/// message type `R`.
///
/// `R` is typically a serializable enum with a case for each possible message
/// type. It can be thought of as the definition of the protocol.
///
/// `M` is typically an enum with a case for each possible message type, where
/// each case is a `Request` struct that extends the message protocol message
/// with a local reply and request channel as well as a tracing span to allow for
/// keeping tracing context across async boundaries.
///
/// In some cases, `M` and `R` can be enums for a subset of the protocol. E.g.
/// if you have a subsystem that only handles a part of the messages.
///
/// The service type `S` provides a scope for the protocol messages. It exists
/// so you can use the same message with multiple services.
#[derive(Debug)]
pub struct Client<M, R, S>(ClientInner<M>, PhantomData<(R, S)>);

impl<M, R, S> Clone for Client<M, R, S> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), PhantomData)
    }
}

impl<M, R, S> From<LocalSender<M, S>> for Client<M, R, S> {
    fn from(reply: LocalSender<M, S>) -> Self {
        Self(ClientInner::Local(reply.0), PhantomData)
    }
}

impl<M, R, S> From<tokio::sync::mpsc::Sender<M>> for Client<M, R, S> {
    fn from(reply: tokio::sync::mpsc::Sender<M>) -> Self {
        LocalSender::from(reply).into()
    }
}

impl<M, R, S> Client<M, R, S> {
    /// Create a new client to a remote service using the given quinn `endpoint`
    /// and a socket `addr` of the remote service.
    #[cfg(feature = "rpc")]
    pub fn quinn(endpoint: quinn::Endpoint, addr: std::net::SocketAddr) -> Self {
        Self::boxed(rpc::QuinnRemoteConnection::new(endpoint, addr))
    }

    /// Create a new client from a `rpc::RemoteConnection` trait object.
    /// This is used from crates that want to provide other transports than quinn,
    /// such as the iroh transport.
    #[cfg(feature = "rpc")]
    pub fn boxed(remote: impl rpc::RemoteConnection) -> Self {
        Self(ClientInner::Remote(Box::new(remote)), PhantomData)
    }

    /// Get the local sender. This is useful if you don't care about remote
    /// requests.
    pub fn local(&self) -> Option<LocalSender<M, S>> {
        match &self.0 {
            ClientInner::Local(reply) => Some(reply.clone().into()),
            ClientInner::Remote(..) => None,
        }
    }

    /// Start a request by creating a sender that can be used to send the initial
    /// message to the local or remote service.
    ///
    /// In the local case, this is just a clone which has almost zero overhead.
    /// Creating a local sender can not fail.
    ///
    /// In the remote case, this involves lazily creating a connection to the
    /// remote side and then creating a new stream on the underlying
    /// [`quinn`] or iroh connection.
    ///
    /// In both cases, the returned sender is fully self contained.
    #[allow(clippy::type_complexity)]
    pub fn request(
        &self,
    ) -> impl Future<
        Output = result::Result<
            RequestSender<LocalSender<M, S>, rpc::RemoteSender<R, S>>,
            RequestError,
        >,
    > + 'static
    where
        S: Service,
        M: Send + Sync + 'static,
        R: 'static,
    {
        #[cfg(feature = "rpc")]
        {
            let cloned = match &self.0 {
                ClientInner::Local(reply) => RequestSender::Local(reply.clone()),
                ClientInner::Remote(connection) => RequestSender::Remote(connection.clone_boxed()),
            };
            async move {
                match cloned {
                    RequestSender::Local(reply) => Ok(RequestSender::Local(reply.into())),
                    RequestSender::Remote(conn) => {
                        let (send, recv) = conn.open_bi().await?;
                        Ok(RequestSender::Remote(rpc::RemoteSender::new(send, recv)))
                    }
                }
            }
        }
        #[cfg(not(feature = "rpc"))]
        {
            let ClientInner::Local(reply) = &self.0 else {
                unreachable!()
            };
            let reply = reply.clone().into();
            async move { Ok(RequestSender::Local(reply)) }
        }
    }

    /// Performs a request for which the server returns a oneshot receiver.
    pub fn rpc<Req, Res>(&self, msg: Req) -> impl Future<Output = Result<Res>> + Send + 'static
    where
        S: Service,
        M: From<Request<Req, S>> + Send + Sync + Unpin + 'static,
        R: From<Req> + Serialize + Send + Sync + 'static,
        Req: Channels<S, Response = channel::oneshot::Sender<Res>, Request = NoReceiver>
            + Send
            + 'static,
        Res: RpcMessage,
    {
        let request = self.request();
        async move {
            let recv: channel::oneshot::Receiver<Res> = match request.await? {
                RequestSender::Local(tx) => {
                    let (reply, request) = channel::oneshot::channel();
                    tx.send((msg, reply)).await?;
                    request
                }
                #[cfg(not(feature = "rpc"))]
                RequestSender::Remote(_request) => unreachable!(),
                #[cfg(feature = "rpc")]
                RequestSender::Remote(tx) => {
                    let (_reply, request) = tx.write(msg).await?;
                    request.into()
                }
            };
            let res = recv.await?;
            Ok(res)
        }
    }

    /// Performs a request for which the server returns a mpsc receiver.
    pub fn server_streaming<Req, Res>(
        &self,
        msg: Req,
        local_reply_cap: usize,
    ) -> impl Future<Output = Result<channel::mpsc::Receiver<Res>>> + Send + 'static
    where
        S: Service,
        M: From<Request<Req, S>> + Send + Sync + Unpin + 'static,
        R: From<Req> + Serialize + Send + Sync + 'static,
        Req: Channels<S, Response = channel::mpsc::Sender<Res>, Request = NoReceiver>
            + Send
            + 'static,
        Res: RpcMessage,
    {
        let request = self.request();
        async move {
            let recv: channel::mpsc::Receiver<Res> = match request.await? {
                RequestSender::Local(tx) => {
                    let (reply, request) = channel::mpsc::channel(local_reply_cap);
                    tx.send((msg, reply)).await?;
                    request
                }
                #[cfg(not(feature = "rpc"))]
                RequestSender::Remote(_request) => unreachable!(),
                #[cfg(feature = "rpc")]
                RequestSender::Remote(tx) => {
                    let (_reply, request) = tx.write(msg).await?;
                    request.into()
                }
            };
            Ok(recv)
        }
    }

    /// Performs a request for which the client can send updates.
    pub fn client_streaming<Req, Update, Res>(
        &self,
        msg: Req,
        local_update_cap: usize,
    ) -> impl Future<
        Output = Result<(
            channel::mpsc::Sender<Update>,
            channel::oneshot::Receiver<Res>,
        )>,
    >
    where
        S: Service,
        M: From<Request<Req, S>> + Send + Sync + Unpin + 'static,
        R: From<Req> + Serialize + 'static,
        Req: Channels<
            S,
            Response = channel::oneshot::Sender<Res>,
            Request = channel::mpsc::Receiver<Update>,
        >,
        Update: RpcMessage,
        Res: RpcMessage,
    {
        let request = self.request();
        async move {
            let (update_reply, res_request): (
                channel::mpsc::Sender<Update>,
                channel::oneshot::Receiver<Res>,
            ) = match request.await? {
                RequestSender::Local(request) => {
                    let (req_reply, req_request) = channel::mpsc::channel(local_update_cap);
                    let (res_reply, res_request) = channel::oneshot::channel();
                    request.send((msg, res_reply, req_request)).await?;
                    (req_reply, res_request)
                }
                #[cfg(not(feature = "rpc"))]
                RequestSender::Remote(_request) => unreachable!(),
                #[cfg(feature = "rpc")]
                RequestSender::Remote(request) => {
                    let (reply, request) = request.write(msg).await?;
                    (reply.into(), request.into())
                }
            };
            Ok((update_reply, res_request))
        }
    }

    /// Performs a request for which the client can send updates, and the server returns a mpsc receiver.
    pub fn bidi_streaming<Req, Update, Res>(
        &self,
        msg: Req,
        local_update_cap: usize,
        local_reply_cap: usize,
    ) -> impl Future<Output = Result<(channel::mpsc::Sender<Update>, channel::mpsc::Receiver<Res>)>>
           + Send
           + 'static
    where
        S: Service,
        M: From<Request<Req, S>> + Send + Sync + Unpin + 'static,
        R: From<Req> + Serialize + Send + 'static,
        Req: Channels<
                S,
                Response = channel::mpsc::Sender<Res>,
                Request = channel::mpsc::Receiver<Update>,
            > + Send
            + 'static,
        Update: RpcMessage,
        Res: RpcMessage,
    {
        let request = self.request();
        async move {
            let (update_reply, res_request): (
                channel::mpsc::Sender<Update>,
                channel::mpsc::Receiver<Res>,
            ) = match request.await? {
                RequestSender::Local(request) => {
                    let (update_reply, update_request) = channel::mpsc::channel(local_update_cap);
                    let (res_reply, res_request) = channel::mpsc::channel(local_reply_cap);
                    request.send((msg, res_reply, update_request)).await?;
                    (update_reply, res_request)
                }
                #[cfg(not(feature = "rpc"))]
                RequestSender::Remote(_request) => unreachable!(),
                #[cfg(feature = "rpc")]
                RequestSender::Remote(request) => {
                    let (reply, request) = request.write(msg).await?;
                    (reply.into(), request.into())
                }
            };
            Ok((update_reply, res_request))
        }
    }
}

#[derive(Debug)]
pub(crate) enum ClientInner<M> {
    Local(tokio::sync::mpsc::Sender<M>),
    #[cfg(feature = "rpc")]
    #[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "rpc")))]
    Remote(Box<dyn rpc::RemoteConnection>),
    #[cfg(not(feature = "rpc"))]
    #[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "rpc")))]
    #[allow(dead_code)]
    Remote(PhantomData<M>),
}

impl<M> Clone for ClientInner<M> {
    fn clone(&self) -> Self {
        match self {
            Self::Local(reply) => Self::Local(reply.clone()),
            #[cfg(feature = "rpc")]
            Self::Remote(conn) => Self::Remote(conn.clone_boxed()),
            #[cfg(not(feature = "rpc"))]
            Self::Remote(_) => unreachable!(),
        }
    }
}

/// Error when opening a request. When cross-process rpc is disabled, this is
/// an empty enum since local requests can not fail.
#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    /// Error in quinn during connect
    #[cfg(feature = "rpc")]
    #[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "rpc")))]
    #[error("error establishing connection: {0}")]
    Connect(#[from] quinn::ConnectError),
    /// Error in quinn when the connection already exists, when opening a stream pair
    #[cfg(feature = "rpc")]
    #[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "rpc")))]
    #[error("error opening stream: {0}")]
    Connection(#[from] quinn::ConnectionError),
    /// Generic error for non-quinn transports
    #[cfg(feature = "rpc")]
    #[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "rpc")))]
    #[error("error opening stream: {0}")]
    Other(#[from] anyhow::Error),
}

/// Error type that subsumes all possible errors in this crate, for convenience.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("request error: {0}")]
    Request(#[from] RequestError),
    #[error("send error: {0}")]
    Send(#[from] channel::SendError),
    #[error("recv error: {0}")]
    Recv(#[from] channel::RecvError),
    #[cfg(feature = "rpc")]
    #[error("recv error: {0}")]
    Write(#[from] rpc::WriteError),
}

/// Type alias for a result with an irpc error type.
pub type Result<T> = std::result::Result<T, Error>;

impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        match e {
            Error::Request(e) => e.into(),
            Error::Send(e) => e.into(),
            Error::Recv(e) => e.into(),
            #[cfg(feature = "rpc")]
            Error::Write(e) => e.into(),
        }
    }
}

impl From<RequestError> for io::Error {
    fn from(e: RequestError) -> Self {
        match e {
            #[cfg(feature = "rpc")]
            RequestError::Connect(e) => io::Error::other(e),
            #[cfg(feature = "rpc")]
            RequestError::Connection(e) => e.into(),
            #[cfg(feature = "rpc")]
            RequestError::Other(e) => io::Error::other(e),
        }
    }
}

/// A local sender for the service `S` using the message type `M`.
///
/// This is a wrapper around an in-memory channel (currently [`tokio::sync::mpsc::Sender`]),
/// that adds nice syntax for sending messages that can be converted into
/// [`Request`].
#[derive(Debug)]
#[repr(transparent)]
pub struct LocalSender<M, S>(tokio::sync::mpsc::Sender<M>, std::marker::PhantomData<S>);

impl<M, S> Clone for LocalSender<M, S> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), PhantomData)
    }
}

impl<M, S> From<tokio::sync::mpsc::Sender<M>> for LocalSender<M, S> {
    fn from(reply: tokio::sync::mpsc::Sender<M>) -> Self {
        Self(reply, PhantomData)
    }
}

#[cfg(not(feature = "rpc"))]
pub mod rpc {
    pub struct RemoteSender<R, S>(std::marker::PhantomData<(R, S)>);
}

#[cfg(feature = "rpc")]
#[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "rpc")))]
pub mod rpc {
    //! Module for cross-process RPC using [`quinn`].
    use std::{
        fmt::Debug, future::Future, io, marker::PhantomData, ops::DerefMut, pin::Pin, sync::Arc,
    };

    use n0_future::{future::Boxed as BoxFuture, task::JoinSet};
    use quinn::ConnectionError;
    use serde::{de::DeserializeOwned, Serialize};
    use smallvec::SmallVec;
    use tracing::{trace, trace_span, warn, Instrument};

    use crate::{
        channel::{
            mpsc::{self, DynReceiver, DynSender},
            none::NoSender,
            oneshot, RecvError, SendError,
        },
        util::{now_or_never, AsyncReadVarintExt, WriteVarintExt},
        RequestError, RpcMessage,
    };

    /// Error that can occur when writing the initial message when doing a
    /// cross-process RPC.
    #[derive(Debug, thiserror::Error)]
    pub enum WriteError {
        /// Error writing to the stream with quinn
        #[error("error writing to stream: {0}")]
        Quinn(#[from] quinn::WriteError),
        /// Generic IO error, e.g. when serializing the message or when using
        /// other transports.
        #[error("error serializing: {0}")]
        Io(#[from] io::Error),
    }

    impl From<WriteError> for io::Error {
        fn from(e: WriteError) -> Self {
            match e {
                WriteError::Io(e) => e,
                WriteError::Quinn(e) => e.into(),
            }
        }
    }

    /// Trait to abstract over a client connection to a remote service.
    ///
    /// This isn't really that much abstracted, since the result of open_bi must
    /// still be a quinn::SendStream and quinn::RecvStream. This is just so we
    /// can have different connection implementations for normal quinn connections,
    /// iroh connections, and possibly quinn connections with disabled encryption
    /// for performance.
    ///
    /// This is done as a trait instead of an enum, so we don't need an iroh
    /// dependency in the main crate.
    pub trait RemoteConnection: Send + Sync + Debug + 'static {
        /// Boxed clone so the trait is dynable.
        fn clone_boxed(&self) -> Box<dyn RemoteConnection>;

        /// Open a bidirectional stream to the remote service.
        fn open_bi(
            &self,
        ) -> BoxFuture<std::result::Result<(quinn::SendStream, quinn::RecvStream), RequestError>>;
    }

    /// A connection to a remote service.
    ///
    /// Initially this does just have the endpoint and the address. Once a
    /// connection is established, it will be stored.
    #[derive(Debug, Clone)]
    pub(crate) struct QuinnRemoteConnection(Arc<QuinnRemoteConnectionInner>);

    #[derive(Debug)]
    struct QuinnRemoteConnectionInner {
        pub endpoint: quinn::Endpoint,
        pub addr: std::net::SocketAddr,
        pub connection: tokio::sync::Mutex<Option<quinn::Connection>>,
    }

    impl QuinnRemoteConnection {
        pub fn new(endpoint: quinn::Endpoint, addr: std::net::SocketAddr) -> Self {
            Self(Arc::new(QuinnRemoteConnectionInner {
                endpoint,
                addr,
                connection: Default::default(),
            }))
        }
    }

    impl RemoteConnection for QuinnRemoteConnection {
        fn clone_boxed(&self) -> Box<dyn RemoteConnection> {
            Box::new(self.clone())
        }

        fn open_bi(
            &self,
        ) -> BoxFuture<std::result::Result<(quinn::SendStream, quinn::RecvStream), RequestError>>
        {
            let this = self.0.clone();
            Box::pin(async move {
                let mut guard = this.connection.lock().await;
                let pair = match guard.as_mut() {
                    Some(conn) => {
                        // try to reuse the connection
                        match conn.open_bi().await {
                            Ok(pair) => pair,
                            Err(_) => {
                                // try with a new connection, just once
                                *guard = None;
                                connect_and_open_bi(&this.endpoint, &this.addr, guard).await?
                            }
                        }
                    }
                    None => connect_and_open_bi(&this.endpoint, &this.addr, guard).await?,
                };
                Ok(pair)
            })
        }
    }

    async fn connect_and_open_bi(
        endpoint: &quinn::Endpoint,
        addr: &std::net::SocketAddr,
        mut guard: tokio::sync::MutexGuard<'_, Option<quinn::Connection>>,
    ) -> Result<(quinn::SendStream, quinn::RecvStream), RequestError> {
        let conn = endpoint.connect(*addr, "localhost")?.await?;
        let (send, recv) = conn.open_bi().await?;
        *guard = Some(conn);
        Ok((send, recv))
    }

    /// A connection to a remote service that can be used to send the initial message.
    #[derive(Debug)]
    pub struct RemoteSender<R, S>(
        quinn::SendStream,
        quinn::RecvStream,
        std::marker::PhantomData<(R, S)>,
    );

    impl<R, S> RemoteSender<R, S> {
        pub fn new(send: quinn::SendStream, recv: quinn::RecvStream) -> Self {
            Self(send, recv, PhantomData)
        }

        pub async fn write(
            self,
            msg: impl Into<R>,
        ) -> std::result::Result<(quinn::SendStream, quinn::RecvStream), WriteError>
        where
            R: Serialize,
        {
            let RemoteSender(mut send, recv, _) = self;
            let msg = msg.into();
            let mut buf = SmallVec::<[u8; 128]>::new();
            buf.write_length_prefixed(msg)?;
            send.write_all(&buf).await?;
            Ok((send, recv))
        }
    }

    impl<T: DeserializeOwned> From<quinn::RecvStream> for oneshot::Receiver<T> {
        fn from(mut read: quinn::RecvStream) -> Self {
            let fut = async move {
                let size = read.read_varint_u64().await?.ok_or(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "failed to read size",
                ))?;
                let rest = read
                    .read_to_end(size as usize)
                    .await
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                let msg: T = postcard::from_bytes(&rest)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                io::Result::Ok(msg)
            };
            oneshot::Receiver::from(|| fut)
        }
    }

    impl From<quinn::RecvStream> for crate::channel::none::NoReceiver {
        fn from(read: quinn::RecvStream) -> Self {
            drop(read);
            Self
        }
    }

    impl<T: RpcMessage> From<quinn::RecvStream> for mpsc::Receiver<T> {
        fn from(read: quinn::RecvStream) -> Self {
            mpsc::Receiver::Boxed(Box::new(QuinnReceiver {
                recv: read,
                _marker: PhantomData,
            }))
        }
    }

    impl From<quinn::SendStream> for NoSender {
        fn from(write: quinn::SendStream) -> Self {
            let _ = write;
            NoSender
        }
    }

    impl<T: RpcMessage> From<quinn::SendStream> for oneshot::Sender<T> {
        fn from(mut writer: quinn::SendStream) -> Self {
            oneshot::Sender::Boxed(Box::new(move |value| {
                Box::pin(async move {
                    // write via a small buffer to avoid allocation for small values
                    let mut buf = SmallVec::<[u8; 128]>::new();
                    buf.write_length_prefixed(value)?;
                    writer.write_all(&buf).await?;
                    io::Result::Ok(())
                })
            }))
        }
    }

    impl<T: RpcMessage> From<quinn::SendStream> for mpsc::Sender<T> {
        fn from(write: quinn::SendStream) -> Self {
            mpsc::Sender::Boxed(Arc::new(QuinnSender(tokio::sync::Mutex::new(
                QuinnSenderState::Open(QuinnSenderInner {
                    send: write,
                    buffer: SmallVec::new(),
                    _marker: PhantomData,
                }),
            ))))
        }
    }

    struct QuinnReceiver<T> {
        recv: quinn::RecvStream,
        _marker: std::marker::PhantomData<T>,
    }

    impl<T> Debug for QuinnReceiver<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("QuinnReceiver").finish()
        }
    }

    impl<T: RpcMessage> DynReceiver<T> for QuinnReceiver<T> {
        fn recv(
            &mut self,
        ) -> Pin<
            Box<dyn Future<Output = std::result::Result<Option<T>, RecvError>> + Send + Sync + '_>,
        > {
            Box::pin(async {
                let read = &mut self.recv;
                let Some(size) = read.read_varint_u64().await? else {
                    return Ok(None);
                };
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

    impl<T> Drop for QuinnReceiver<T> {
        fn drop(&mut self) {}
    }

    struct QuinnSenderInner<T> {
        send: quinn::SendStream,
        buffer: SmallVec<[u8; 128]>,
        _marker: std::marker::PhantomData<T>,
    }

    impl<T: RpcMessage> QuinnSenderInner<T> {
        fn send(
            &mut self,
            value: T,
        ) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + Sync + '_>> {
            Box::pin(async {
                let value = value;
                self.buffer.clear();
                self.buffer.write_length_prefixed(value)?;
                self.send.write_all(&self.buffer).await?;
                self.buffer.clear();
                Ok(())
            })
        }

        fn try_send(
            &mut self,
            value: T,
        ) -> Pin<Box<dyn Future<Output = io::Result<bool>> + Send + Sync + '_>> {
            Box::pin(async {
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
    enum QuinnSenderState<T> {
        Open(QuinnSenderInner<T>),
        #[default]
        Closed,
    }

    struct QuinnSender<T>(tokio::sync::Mutex<QuinnSenderState<T>>);

    impl<T> Debug for QuinnSender<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("QuinnSender").finish()
        }
    }

    impl<T: RpcMessage> DynSender<T> for QuinnSender<T> {
        fn send(
            &self,
            value: T,
        ) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + Sync + '_>> {
            Box::pin(async {
                let mut guard = self.0.lock().await;
                let sender = std::mem::take(guard.deref_mut());
                match sender {
                    QuinnSenderState::Open(mut sender) => {
                        let res = sender.send(value).await;
                        if res.is_ok() {
                            *guard = QuinnSenderState::Open(sender);
                        }
                        res
                    }
                    QuinnSenderState::Closed => Err(io::ErrorKind::BrokenPipe.into()),
                }
            })
        }

        fn try_send(
            &self,
            value: T,
        ) -> Pin<Box<dyn Future<Output = io::Result<bool>> + Send + Sync + '_>> {
            Box::pin(async {
                let mut guard = self.0.lock().await;
                let sender = std::mem::take(guard.deref_mut());
                match sender {
                    QuinnSenderState::Open(mut sender) => {
                        let res = sender.try_send(value).await;
                        if res.is_ok() {
                            *guard = QuinnSenderState::Open(sender);
                        }
                        res
                    }
                    QuinnSenderState::Closed => Err(io::ErrorKind::BrokenPipe.into()),
                }
            })
        }

        fn closed(&self) -> Pin<Box<dyn Future<Output = ()> + Send + Sync + '_>> {
            Box::pin(async {
                let mut guard = self.0.lock().await;
                match guard.deref_mut() {
                    QuinnSenderState::Open(sender) => sender.closed().await,
                    QuinnSenderState::Closed => {}
                }
            })
        }

        fn is_rpc(&self) -> bool {
            true
        }
    }

    /// Type alias for a handler fn for remote requests
    pub type Handler<R> = Arc<
        dyn Fn(
                R,
                quinn::RecvStream,
                quinn::SendStream,
            ) -> BoxFuture<std::result::Result<(), SendError>>
            + Send
            + Sync
            + 'static,
    >;

    /// Utility function to listen for incoming connections and handle them with the provided handler
    pub async fn listen<R: DeserializeOwned + 'static>(
        endpoint: quinn::Endpoint,
        handler: Handler<R>,
    ) {
        let mut request_id = 0u64;
        let mut tasks = JoinSet::new();
        while let Some(incoming) = endpoint.accept().await {
            let handler = handler.clone();
            let fut = async move {
                let connection = match incoming.await {
                    Ok(connection) => connection,
                    Err(cause) => {
                        warn!("failed to accept connection {cause:?}");
                        return io::Result::Ok(());
                    }
                };
                loop {
                    let (send, mut recv) = match connection.accept_bi().await {
                        Ok((s, r)) => (s, r),
                        Err(ConnectionError::ApplicationClosed(cause))
                            if cause.error_code.into_inner() == 0 =>
                        {
                            trace!("remote side closed connection {cause:?}");
                            return Ok(());
                        }
                        Err(cause) => {
                            warn!("failed to accept bi stream {cause:?}");
                            return Err(cause.into());
                        }
                    };
                    let size = recv.read_varint_u64().await?.ok_or_else(|| {
                        io::Error::new(io::ErrorKind::UnexpectedEof, "failed to read size")
                    })?;
                    let mut buf = vec![0; size as usize];
                    recv.read_exact(&mut buf)
                        .await
                        .map_err(|e| io::Error::new(io::ErrorKind::UnexpectedEof, e))?;
                    let msg: R = postcard::from_bytes(&buf)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    let request = recv;
                    let reply = send;
                    handler(msg, request, reply).await?;
                }
            };
            let span = trace_span!("rpc", id = request_id);
            tasks.spawn(fut.instrument(span));
            request_id += 1;
        }
    }
}

/// A request to a service. This can be either local or remote.
#[derive(Debug)]
pub enum RequestSender<L, R> {
    /// Local in memory request
    Local(L),
    /// Remote cross process request
    Remote(R),
}

impl<M: Send, S: Service> LocalSender<M, S> {
    /// Send a message to the service
    pub fn send<T>(&self, value: impl Into<Request<T, S>>) -> SendFut<M>
    where
        T: Channels<S>,
        M: From<Request<T, S>>,
    {
        let value: M = value.into().into();
        SendFut::new(self.0.clone(), value)
    }

    /// Send a message to the service without the type conversion magic
    pub fn send_raw(&self, value: M) -> SendFut<M> {
        SendFut::new(self.0.clone(), value)
    }
}

mod send_fut {
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    };

    use tokio::sync::mpsc::Sender;
    use tokio_util::sync::PollSender;

    use crate::channel::SendError;

    pub struct SendFut<T: Send> {
        poll_sender: PollSender<T>,
        value: Option<T>,
    }

    impl<T: Send> SendFut<T> {
        pub fn new(sender: Sender<T>, value: T) -> Self {
            Self {
                poll_sender: PollSender::new(sender),
                value: Some(value),
            }
        }
    }

    impl<T: Send + Unpin> Future for SendFut<T> {
        type Output = std::result::Result<(), SendError>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.get_mut();

            // Safely extract the value
            let value = match this.value.take() {
                Some(v) => v,
                None => return Poll::Ready(Ok(())), // Already completed
            };

            // Try to reserve capacity
            match this.poll_sender.poll_reserve(cx) {
                Poll::Ready(Ok(())) => {
                    // Send the item
                    this.poll_sender.send_item(value).ok();
                    Poll::Ready(Ok(()))
                }
                Poll::Ready(Err(_)) => {
                    // Channel is closed
                    Poll::Ready(Err(SendError::ReceiverClosed))
                }
                Poll::Pending => {
                    // Restore the value and wait
                    this.value = Some(value);
                    Poll::Pending
                }
            }
        }
    }
}
use send_fut::SendFut;
