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
//! For each request, there can be a response and update channel. Each channel
//! can be either oneshot, carry multiple messages, or be disabled. This enables
//! the typical interaction patterns known from libraries like grpc:
//!
//! - rpc: 1 request, 1 response
//! - server streaming: 1 request, multiple responses
//! - client streaming: multiple requests, 1 response
//! - bidi streaming: multiple requests, multiple responses
//!
//! as well as more complex patterns. It is however not possible to have multiple
//! differently typed tx channels for a single message type.
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
//! - `spans`: Enable tracing spans for messages. Enabled by default.
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

/// Processes an RPC request enum and generates trait implementations for use with `irpc`.
///
/// This attribute macro may be applied to an enum where each variant represents
/// a different RPC request type. Each variant of the enum must contain a single unnamed field
/// of a distinct type, otherwise compilation fails.
///
/// Basic usage example:
/// ```
/// use serde::{Serialize, Deserialize};
/// use irpc::{rpc_requests, channel::{oneshot, mpsc}};
///
/// #[rpc_requests(message = ComputeMessage)]
/// #[derive(Debug, Serialize, Deserialize)]
/// enum ComputeProtocol {
///     /// Multiply two numbers, return the result over a oneshot channel.
///     #[rpc(tx=oneshot::Sender<i64>)]
///     Multiply(Multiply),
///     /// Sum all numbers received via the `rx` stream,
///     /// reply with the updating sum over the `tx` stream.
///     #[rpc(tx=mpsc::Sender<i64>, rx=mpsc::Receiver<i64>)]
///     Sum(Sum),
/// }
///
/// #[derive(Debug, Serialize, Deserialize)]
/// struct Multiply(i64, i64);
///
/// #[derive(Debug, Serialize, Deserialize)]
/// struct Sum;
/// ```
///
/// ## Generated code
///
/// If no further arguments are set, the macro generates:
///
/// * A [`Channels<S>`] implementation for each request type (i.e. the type of the variant's
///   single unnamed field).
///   The `Tx` and `Rx` types are set to the types provided via the variant's `rpc` attribute.
/// * A [`From`] implementation to convert from each request type to the protocol enum.
///
/// When the `message` argument is set, the macro will also create a message enum and implement the
/// [`Service`] and [`RemoteService`] traits for the protocol enum. This is recommended for the
/// typical use of the macro.
///
/// ## Macro arguments
///
/// * `message = <name>` *(optional but recommended)*:
///     * Generates an extended enum wrapping each type in [`WithChannels<T, Service>`].
///       The attribute value is the name of the message enum type.
///     * Generates a [`Service`] implementation for the protocol enum, with the `Message`
///       type set to the message enum.
///     * Generates a [`RemoteService`] implementation for the protocol enum.
/// * `alias = "<suffix>"` *(optional)*: Generate type aliases with the given suffix for each [`WithChannels<T, Service>`].
/// * `rpc_feature = "<feature>"` *(optional)*: If set, the [`RemoteService`] implementation will be feature-flagged
///   with this feature. Set this if your crate only optionally enables the `rpc` feature
///   of [`irpc`].
/// * `no_rpc` *(optional, no value)*: If set, no implementation of [`RemoteService`] will be generated and the generated
///   code works without the `rpc` feature of `irpc`.
/// * `no_spans` *(optional, no value)*: If set, the generated code works without the `spans` feature of `irpc`.
///
/// ## Variant attributes
///
/// Individual enum variants are annotated with the `#[rpc(...)]` attribute to specify channel types.
/// The `rpc` attribute contains a key-value list with these arguments:
///
/// * `tx = SomeType` *(required)*: Set the kind of channel for sending responses from the server to the client.
///    Must be a `Sender` type from the [`crate::channel`] module.
/// * `rx = OtherType` *(optional)*: Set the kind of channel for receiving updates from the client at the server.
///    Must be a `Receiver` type from the [`crate::channel`] module. If `rx` is not set,
///    it defaults to [`crate::channel::none::NoReceiver`].
///
/// ## Examples
///
/// With type aliases:
/// ```no_compile
/// #[rpc_requests(message = ComputeMessage, alias = "Msg")]
/// enum ComputeProtocol {
///     #[rpc(tx=oneshot::Sender<u128>)]
///     Sqr(Sqr), // Generates type SqrMsg = WithChannels<Sqr, ComputeService>
///     #[rpc(tx=mpsc::Sender<i64>)]
///     Sum(Sum), // Generates type SumMsg = WithChannels<Sum, ComputeService>
/// }
/// ```
///
/// [`irpc`]: crate
/// [`RemoteService`]: rpc::RemoteService
/// [`WithChannels<T, Service>`]: WithChannels
/// [`Channels<S>`]: Channels
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

/// Trait for a service
///
/// This is implemented on the protocol enum.
/// It is usually auto-implemented via the [`rpc_requests] macro.
///
/// A service acts as a scope for defining the tx and rx channels for each
/// message type, and provides some type safety when sending messages.
pub trait Service: Serialize + DeserializeOwned + Send + Sync + Debug + 'static {
    /// Message enum for this protocol.
    ///
    /// This is expected to be an enum with identical variant names than the
    /// protocol enum, but its single unit field is the [`WithChannels`] struct
    /// that contains the inner request plus the `tx` and `rx` channels.
    type Message: Send + Unpin + 'static;
}

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
    type Tx: Sender;
    /// The receiver type, can be either mpsc, oneshot or none
    ///
    /// For many services, the receiver is not needed, so it can be set to [`NoReceiver`].
    type Rx: Receiver;
}

/// Channels that abstract over local or remote sending
pub mod channel {
    use std::io;

    /// Oneshot channel, similar to tokio's oneshot channel
    pub mod oneshot {
        use std::{fmt::Debug, future::Future, pin::Pin, task};

        use n0_future::future::Boxed as BoxFuture;

        use super::{RecvError, SendError};
        use crate::util::FusedOneshotReceiver;

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
        pub trait DynSender<T>:
            Future<Output = Result<(), SendError>> + Send + Sync + 'static
        {
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
            pub async fn send(self, value: T) -> std::result::Result<(), SendError> {
                match self {
                    Sender::Tokio(tx) => tx.send(value).map_err(|_| SendError::ReceiverClosed),
                    Sender::Boxed(f) => f(value).await,
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
                    Self::Tokio(rx) => Pin::new(rx).poll(cx).map_err(|_| RecvError::SenderClosed),
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
    }

    /// SPSC channel, similar to tokio's mpsc channel
    ///
    /// For the rpc case, the send side can not be cloned, hence mpsc instead of mpsc.
    pub mod mpsc {
        use std::{fmt::Debug, future::Future, pin::Pin, sync::Arc};

        use super::{RecvError, SendError};
        use crate::RpcMessage;

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
                    Sender::Tokio(tx) => tx.closed().await,
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
            fn send(
                &self,
                value: T,
            ) -> Pin<Box<dyn Future<Output = Result<(), SendError>> + Send + '_>>;

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
                    Self::Boxed(inner) => f.debug_tuple("Boxed").field(&inner).finish(),
                }
            }
        }

        impl<T: RpcMessage> Sender<T> {
            /// Send a message and yield until either it is sent or an error occurs.
            ///
            /// ## Cancellation safety
            ///
            /// If the future is dropped before completion, and if this is a remote sender,
            /// then the sender will be closed and further sends will return an [`SendError::Io`]
            /// with [`std::io::ErrorKind::BrokenPipe`]. Therefore, make sure to always poll the
            /// future until completion if you want to reuse the sender or any clone afterwards.
            pub async fn send(&self, value: T) -> std::result::Result<(), SendError> {
                match self {
                    Sender::Tokio(tx) => {
                        tx.send(value).await.map_err(|_| SendError::ReceiverClosed)
                    }
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
            /// then the sender will be closed and further sends will return an [`SendError::Io`]
            /// with [`std::io::ErrorKind::BrokenPipe`]. Therefore, make sure to always poll the
            /// future until completion if you want to reuse the sender or any clone afterwards.
            pub async fn try_send(&mut self, value: T) -> std::result::Result<bool, SendError> {
                match self {
                    Sender::Tokio(tx) => match tx.try_send(value) {
                        Ok(()) => Ok(true),
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                            Err(SendError::ReceiverClosed)
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

        impl<T: RpcMessage> Receiver<T> {
            /// Receive a message
            ///
            /// Returns Ok(None) if the sender has been dropped or the remote end has
            /// cleanly closed the connection.
            ///
            /// Returns an an io error if there was an error receiving the message.
            pub async fn recv(&mut self) -> std::result::Result<Option<T>, RecvError> {
                match self {
                    Self::Tokio(rx) => Ok(rx.recv().await),
                    Self::Boxed(rx) => Ok(rx.recv().await?),
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
        /// The message exceeded the maximum allowed message size (see [`MAX_MESSAGE_SIZE`]).
        ///
        /// [`MAX_MESSAGE_SIZE`]: crate::rpc::MAX_MESSAGE_SIZE
        #[error("maximum message size exceeded")]
        MaxMessageSizeExceeded,
        /// The underlying io error. This can occur for remote communication,
        /// due to a network error or serialization error.
        #[error("io error: {0}")]
        Io(#[from] io::Error),
    }

    impl From<SendError> for io::Error {
        fn from(e: SendError) -> Self {
            match e {
                SendError::ReceiverClosed => io::Error::new(io::ErrorKind::BrokenPipe, e),
                SendError::MaxMessageSizeExceeded => io::Error::new(io::ErrorKind::InvalidData, e),
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
        /// The message exceeded the maximum allowed message size (see [`MAX_MESSAGE_SIZE`]).
        ///
        /// [`MAX_MESSAGE_SIZE`]: crate::rpc::MAX_MESSAGE_SIZE
        #[error("maximum message size exceeded")]
        MaxMessageSizeExceeded,
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
                RecvError::MaxMessageSizeExceeded => io::Error::new(io::ErrorKind::InvalidData, e),
            }
        }
    }
}

/// A wrapper for a message with channels to send and receive it.
/// This expands the protocol message to a full message that includes the
/// active and unserializable channels.
///
/// The channel kind for rx and tx is defined by implementing the `Channels`
/// trait, either manually or using a macro.
///
/// When the `spans` feature is enabled, this also includes a tracing
/// span to carry the tracing context during message passing.
pub struct WithChannels<I: Channels<S>, S: Service> {
    /// The inner message.
    pub inner: I,
    /// The return channel to send the response to. Can be set to [`crate::channel::none::NoSender`] if not needed.
    pub tx: <I as Channels<S>>::Tx,
    /// The request channel to receive the request from. Can be set to [`NoReceiver`] if not needed.
    pub rx: <I as Channels<S>>::Rx,
    /// The current span where the full message was created.
    #[cfg(feature = "spans")]
    #[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "spans")))]
    pub span: tracing::Span,
}

impl<I: Channels<S> + Debug, S: Service> Debug for WithChannels<I, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("")
            .field(&self.inner)
            .field(&self.tx)
            .field(&self.rx)
            .finish()
    }
}

impl<I: Channels<S>, S: Service> WithChannels<I, S> {
    /// Get the parent span
    #[cfg(feature = "spans")]
    pub fn parent_span_opt(&self) -> Option<&tracing::Span> {
        Some(&self.span)
    }
}

/// Tuple conversion from inner message and tx/rx channels to a WithChannels struct
///
/// For the case where you want both tx and rx channels.
impl<I: Channels<S>, S: Service, Tx, Rx> From<(I, Tx, Rx)> for WithChannels<I, S>
where
    I: Channels<S>,
    <I as Channels<S>>::Tx: From<Tx>,
    <I as Channels<S>>::Rx: From<Rx>,
{
    fn from(inner: (I, Tx, Rx)) -> Self {
        let (inner, tx, rx) = inner;
        Self {
            inner,
            tx: tx.into(),
            rx: rx.into(),
            #[cfg(feature = "spans")]
            #[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "spans")))]
            span: tracing::Span::current(),
        }
    }
}

/// Tuple conversion from inner message and tx channel to a WithChannels struct
///
/// For the very common case where you just need a tx channel to send the response to.
impl<I, S, Tx> From<(I, Tx)> for WithChannels<I, S>
where
    I: Channels<S, Rx = NoReceiver>,
    S: Service,
    <I as Channels<S>>::Tx: From<Tx>,
{
    fn from(inner: (I, Tx)) -> Self {
        let (inner, tx) = inner;
        Self {
            inner,
            tx: tx.into(),
            rx: NoReceiver,
            #[cfg(feature = "spans")]
            #[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "spans")))]
            span: tracing::Span::current(),
        }
    }
}

/// Deref so you can access the inner fields directly.
///
/// If the inner message has fields named `tx`, `rx` or `span`, you need to use the
/// `inner` field to access them.
impl<I: Channels<S>, S: Service> Deref for WithChannels<I, S> {
    type Target = I;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// A client to the service `S` using the local message type `M` and the remote
/// message type `R`.
///
/// `R` is typically a serializable enum with a case for each possible message
/// type. It can be thought of as the definition of the protocol.
///
/// `M` is typically an enum with a case for each possible message type, where
/// each case is a `WithChannels` struct that extends the inner protocol message
/// with a local tx and rx channel as well as a tracing span to allow for
/// keeping tracing context across async boundaries.
///
/// In some cases, `M` and `R` can be enums for a subset of the protocol. E.g.
/// if you have a subsystem that only handles a part of the messages.
///
/// The service type `S` provides a scope for the protocol messages. It exists
/// so you can use the same message with multiple services.
#[derive(Debug)]
pub struct Client<S: Service>(ClientInner<S::Message>, PhantomData<S>);

impl<S: Service> Clone for Client<S> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), PhantomData)
    }
}

impl<S: Service> From<LocalSender<S>> for Client<S> {
    fn from(tx: LocalSender<S>) -> Self {
        Self(ClientInner::Local(tx.0), PhantomData)
    }
}

impl<S: Service> From<tokio::sync::mpsc::Sender<S::Message>> for Client<S> {
    fn from(tx: tokio::sync::mpsc::Sender<S::Message>) -> Self {
        LocalSender::from(tx).into()
    }
}

impl<S: Service> Client<S> {
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

    /// Creates a new client from a `tokio::sync::mpsc::Sender`.
    pub fn local(tx: tokio::sync::mpsc::Sender<S::Message>) -> Self {
        tx.into()
    }

    /// Get the local sender. This is useful if you don't care about remote
    /// requests.
    pub fn as_local(&self) -> Option<LocalSender<S>> {
        match &self.0 {
            ClientInner::Local(tx) => Some(tx.clone().into()),
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
        Output = result::Result<Request<LocalSender<S>, rpc::RemoteSender<S>>, RequestError>,
    > + 'static
    where
        S: Service,
    {
        #[cfg(feature = "rpc")]
        {
            let cloned = match &self.0 {
                ClientInner::Local(tx) => Request::Local(tx.clone()),
                ClientInner::Remote(connection) => Request::Remote(connection.clone_boxed()),
            };
            async move {
                match cloned {
                    Request::Local(tx) => Ok(Request::Local(tx.into())),
                    Request::Remote(conn) => {
                        let (send, recv) = conn.open_bi().await?;
                        Ok(Request::Remote(rpc::RemoteSender::new(send, recv)))
                    }
                }
            }
        }
        #[cfg(not(feature = "rpc"))]
        {
            let ClientInner::Local(tx) = &self.0 else {
                unreachable!()
            };
            let tx = tx.clone().into();
            async move { Ok(Request::Local(tx)) }
        }
    }

    /// Performs a request for which the server returns a oneshot receiver.
    pub fn rpc<Req, Res>(&self, msg: Req) -> impl Future<Output = Result<Res>> + Send + 'static
    where
        S: Service + From<Req>,
        S::Message: From<WithChannels<Req, S>> + Send + Sync + Unpin + 'static,
        Req: Channels<S, Tx = channel::oneshot::Sender<Res>, Rx = NoReceiver> + Send + 'static,
        Res: RpcMessage,
    {
        let request = self.request();
        async move {
            let recv: channel::oneshot::Receiver<Res> = match request.await? {
                Request::Local(request) => {
                    let (tx, rx) = channel::oneshot::channel();
                    request.send((msg, tx)).await?;
                    rx
                }
                #[cfg(not(feature = "rpc"))]
                Request::Remote(_request) => unreachable!(),
                #[cfg(feature = "rpc")]
                Request::Remote(request) => {
                    let (_tx, rx) = request.write(msg).await?;
                    rx.into()
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
        local_response_cap: usize,
    ) -> impl Future<Output = Result<channel::mpsc::Receiver<Res>>> + Send + 'static
    where
        S: Service + From<Req>,
        S::Message: From<WithChannels<Req, S>> + Send + Sync + Unpin + 'static,
        Req: Channels<S, Tx = channel::mpsc::Sender<Res>, Rx = NoReceiver> + Send + 'static,
        Res: RpcMessage,
    {
        let request = self.request();
        async move {
            let recv: channel::mpsc::Receiver<Res> = match request.await? {
                Request::Local(request) => {
                    let (tx, rx) = channel::mpsc::channel(local_response_cap);
                    request.send((msg, tx)).await?;
                    rx
                }
                #[cfg(not(feature = "rpc"))]
                Request::Remote(_request) => unreachable!(),
                #[cfg(feature = "rpc")]
                Request::Remote(request) => {
                    let (_tx, rx) = request.write(msg).await?;
                    rx.into()
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
        S: Service + From<Req>,
        S::Message: From<WithChannels<Req, S>> + Send + Sync + Unpin + 'static,
        Req: Channels<S, Tx = channel::oneshot::Sender<Res>, Rx = channel::mpsc::Receiver<Update>>,
        Update: RpcMessage,
        Res: RpcMessage,
    {
        let request = self.request();
        async move {
            let (update_tx, res_rx): (
                channel::mpsc::Sender<Update>,
                channel::oneshot::Receiver<Res>,
            ) = match request.await? {
                Request::Local(request) => {
                    let (req_tx, req_rx) = channel::mpsc::channel(local_update_cap);
                    let (res_tx, res_rx) = channel::oneshot::channel();
                    request.send((msg, res_tx, req_rx)).await?;
                    (req_tx, res_rx)
                }
                #[cfg(not(feature = "rpc"))]
                Request::Remote(_request) => unreachable!(),
                #[cfg(feature = "rpc")]
                Request::Remote(request) => {
                    let (tx, rx) = request.write(msg).await?;
                    (tx.into(), rx.into())
                }
            };
            Ok((update_tx, res_rx))
        }
    }

    /// Performs a request for which the client can send updates, and the server returns a mpsc receiver.
    pub fn bidi_streaming<Req, Update, Res>(
        &self,
        msg: Req,
        local_update_cap: usize,
        local_response_cap: usize,
    ) -> impl Future<Output = Result<(channel::mpsc::Sender<Update>, channel::mpsc::Receiver<Res>)>>
           + Send
           + 'static
    where
        S: Service + From<Req>,
        S::Message: From<WithChannels<Req, S>> + Send + Sync + Unpin + 'static,
        Req: Channels<S, Tx = channel::mpsc::Sender<Res>, Rx = channel::mpsc::Receiver<Update>>
            + Send
            + 'static,
        Update: RpcMessage,
        Res: RpcMessage,
    {
        let request = self.request();
        async move {
            let (update_tx, res_rx): (channel::mpsc::Sender<Update>, channel::mpsc::Receiver<Res>) =
                match request.await? {
                    Request::Local(request) => {
                        let (update_tx, update_rx) = channel::mpsc::channel(local_update_cap);
                        let (res_tx, res_rx) = channel::mpsc::channel(local_response_cap);
                        request.send((msg, res_tx, update_rx)).await?;
                        (update_tx, res_rx)
                    }
                    #[cfg(not(feature = "rpc"))]
                    Request::Remote(_request) => unreachable!(),
                    #[cfg(feature = "rpc")]
                    Request::Remote(request) => {
                        let (tx, rx) = request.write(msg).await?;
                        (tx.into(), rx.into())
                    }
                };
            Ok((update_tx, res_rx))
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
            Self::Local(tx) => Self::Local(tx.clone()),
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
/// [`WithChannels`].
#[derive(Debug)]
#[repr(transparent)]
pub struct LocalSender<S: Service>(tokio::sync::mpsc::Sender<S::Message>);

impl<S: Service> Clone for LocalSender<S> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<S: Service> From<tokio::sync::mpsc::Sender<S::Message>> for LocalSender<S> {
    fn from(tx: tokio::sync::mpsc::Sender<S::Message>) -> Self {
        Self(tx)
    }
}

#[cfg(not(feature = "rpc"))]
pub mod rpc {
    pub struct RemoteSender<S>(std::marker::PhantomData<S>);
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
    use serde::de::DeserializeOwned;
    use smallvec::SmallVec;
    use tracing::{trace, trace_span, warn, Instrument};

    use crate::{
        channel::{
            mpsc::{self, DynReceiver, DynSender},
            none::NoSender,
            oneshot, RecvError, SendError,
        },
        util::{now_or_never, AsyncReadVarintExt, WriteVarintExt},
        LocalSender, RequestError, RpcMessage, Service,
    };

    /// This is used by irpc-derive to refer to quinn types (SendStream and RecvStream)
    /// to make generated code work for users without having to depend on quinn directly
    /// (i.e. when using iroh).
    #[doc(hidden)]
    pub use quinn;

    /// Default max message size (16 MiB).
    pub const MAX_MESSAGE_SIZE: u64 = 1024 * 1024 * 16;

    /// Error code on streams if the max message size was exceeded.
    pub const ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED: u32 = 1;

    /// Error code on streams if the sender tried to send an message that could not be postcard serialized.
    pub const ERROR_CODE_INVALID_POSTCARD: u32 = 2;

    /// Error that can occur when writing the initial message when doing a
    /// cross-process RPC.
    #[derive(Debug, thiserror::Error)]
    pub enum WriteError {
        /// Error writing to the stream with quinn
        #[error("error writing to stream: {0}")]
        Quinn(#[from] quinn::WriteError),
        /// The message exceeded the maximum allowed message size (see [`MAX_MESSAGE_SIZE`]).
        #[error("maximum message size exceeded")]
        MaxMessageSizeExceeded,
        /// Generic IO error, e.g. when serializing the message or when using
        /// other transports.
        #[error("error serializing: {0}")]
        Io(#[from] io::Error),
    }

    impl From<postcard::Error> for WriteError {
        fn from(value: postcard::Error) -> Self {
            Self::Io(io::Error::new(io::ErrorKind::InvalidData, value))
        }
    }

    impl From<postcard::Error> for SendError {
        fn from(value: postcard::Error) -> Self {
            Self::Io(io::Error::new(io::ErrorKind::InvalidData, value))
        }
    }

    impl From<WriteError> for io::Error {
        fn from(e: WriteError) -> Self {
            match e {
                WriteError::Io(e) => e,
                WriteError::MaxMessageSizeExceeded => io::Error::new(io::ErrorKind::InvalidData, e),
                WriteError::Quinn(e) => e.into(),
            }
        }
    }

    impl From<quinn::WriteError> for SendError {
        fn from(err: quinn::WriteError) -> Self {
            match err {
                quinn::WriteError::Stopped(code)
                    if code == ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED.into() =>
                {
                    SendError::MaxMessageSizeExceeded
                }
                _ => SendError::Io(io::Error::from(err)),
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
    pub struct RemoteSender<S>(
        quinn::SendStream,
        quinn::RecvStream,
        std::marker::PhantomData<S>,
    );

    impl<S: Service> RemoteSender<S> {
        pub fn new(send: quinn::SendStream, recv: quinn::RecvStream) -> Self {
            Self(send, recv, PhantomData)
        }

        pub async fn write(
            self,
            msg: impl Into<S>,
        ) -> std::result::Result<(quinn::SendStream, quinn::RecvStream), WriteError> {
            let RemoteSender(mut send, recv, _) = self;
            let msg = msg.into();
            if postcard::experimental::serialized_size(&msg)? as u64 > MAX_MESSAGE_SIZE {
                return Err(WriteError::MaxMessageSizeExceeded);
            }
            let mut buf = SmallVec::<[u8; 128]>::new();
            buf.write_length_prefixed(msg)?;
            send.write_all(&buf).await?;
            Ok((send, recv))
        }
    }

    impl<T: DeserializeOwned> From<quinn::RecvStream> for oneshot::Receiver<T> {
        fn from(mut read: quinn::RecvStream) -> Self {
            let fut = async move {
                let size = read
                    .read_varint_u64()
                    .await?
                    .ok_or(RecvError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "failed to read size",
                    )))?;
                if size > MAX_MESSAGE_SIZE {
                    read.stop(ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED.into()).ok();
                    return Err(RecvError::MaxMessageSizeExceeded);
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
                    let size = match postcard::experimental::serialized_size(&value) {
                        Ok(size) => size,
                        Err(e) => {
                            writer.reset(ERROR_CODE_INVALID_POSTCARD.into()).ok();
                            return Err(SendError::Io(io::Error::new(
                                io::ErrorKind::InvalidData,
                                e,
                            )));
                        }
                    };
                    if size as u64 > MAX_MESSAGE_SIZE {
                        writer
                            .reset(ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED.into())
                            .ok();
                        return Err(SendError::MaxMessageSizeExceeded);
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
                if size > MAX_MESSAGE_SIZE {
                    self.recv
                        .stop(ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED.into())
                        .ok();
                    return Err(RecvError::MaxMessageSizeExceeded);
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
        ) -> Pin<Box<dyn Future<Output = Result<(), SendError>> + Send + Sync + '_>> {
            Box::pin(async {
                let size = match postcard::experimental::serialized_size(&value) {
                    Ok(size) => size,
                    Err(e) => {
                        self.send.reset(ERROR_CODE_INVALID_POSTCARD.into()).ok();
                        return Err(SendError::Io(io::Error::new(io::ErrorKind::InvalidData, e)));
                    }
                };
                if size as u64 > MAX_MESSAGE_SIZE {
                    self.send
                        .reset(ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED.into())
                        .ok();
                    return Err(SendError::MaxMessageSizeExceeded);
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
                    return Err(SendError::MaxMessageSizeExceeded);
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
        ) -> Pin<Box<dyn Future<Output = Result<(), SendError>> + Send + '_>> {
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
                    QuinnSenderState::Closed => {
                        Err(io::Error::from(io::ErrorKind::BrokenPipe).into())
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
                let sender = std::mem::take(guard.deref_mut());
                match sender {
                    QuinnSenderState::Open(mut sender) => {
                        let res = sender.try_send(value).await;
                        if res.is_ok() {
                            *guard = QuinnSenderState::Open(sender);
                        }
                        res
                    }
                    QuinnSenderState::Closed => {
                        Err(io::Error::from(io::ErrorKind::BrokenPipe).into())
                    }
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

    /// Extension trait to [`Service`] to create a [`Service::Message`] from a [`Service`]
    /// and a pair of QUIC streams.
    ///
    /// This trait is auto-implemented when using the [`crate::rpc_requests`] macro.
    pub trait RemoteService: Service + Sized {
        /// Returns the message enum for this request by combining `self` (the protocol enum)
        /// with a pair of QUIC streams for `tx` and `rx` channels.
        fn with_remote_channels(
            self,
            rx: quinn::RecvStream,
            tx: quinn::SendStream,
        ) -> Self::Message;

        /// Creates a [`Handler`] that forwards all messages to a [`LocalSender`].
        fn remote_handler(local_sender: LocalSender<Self>) -> Handler<Self> {
            Arc::new(move |msg, rx, tx| {
                let msg = Self::with_remote_channels(msg, rx, tx);
                Box::pin(local_sender.send_raw(msg))
            })
        }
    }

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
                handle_connection(connection, handler).await
            };
            let span = trace_span!("rpc", id = request_id);
            tasks.spawn(fut.instrument(span));
            request_id += 1;
        }
    }

    /// Handles a quic connection with the provided `handler`.
    pub async fn handle_connection<R: DeserializeOwned + 'static>(
        connection: quinn::Connection,
        handler: Handler<R>,
    ) -> io::Result<()> {
        loop {
            let Some((msg, rx, tx)) = read_request_raw(&connection).await? else {
                return Ok(());
            };
            handler(msg, rx, tx).await?;
        }
    }

    pub async fn read_request<S: RemoteService>(
        connection: &quinn::Connection,
    ) -> std::io::Result<Option<S::Message>> {
        Ok(read_request_raw::<S>(connection)
            .await?
            .map(|(msg, rx, tx)| S::with_remote_channels(msg, rx, tx)))
    }

    /// Reads a single request from the connection.
    ///
    /// This accepts a bi-directional stream from the connection and reads and parses the request.
    ///
    /// Returns the parsed request and the stream pair if reading and parsing the request succeeded.
    /// Returns None if the remote closed the connection with error code `0`.
    /// Returns an error for all other failure cases.
    pub async fn read_request_raw<R: DeserializeOwned + 'static>(
        connection: &quinn::Connection,
    ) -> std::io::Result<Option<(R, quinn::RecvStream, quinn::SendStream)>> {
        let (send, mut recv) = match connection.accept_bi().await {
            Ok((s, r)) => (s, r),
            Err(ConnectionError::ApplicationClosed(cause))
                if cause.error_code.into_inner() == 0 =>
            {
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
            return Err(RecvError::MaxMessageSizeExceeded.into());
        }
        let mut buf = vec![0; size as usize];
        recv.read_exact(&mut buf)
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::UnexpectedEof, e))?;
        let msg: R = postcard::from_bytes(&buf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let rx = recv;
        let tx = send;
        Ok(Some((msg, rx, tx)))
    }
}

/// A request to a service. This can be either local or remote.
#[derive(Debug)]
pub enum Request<L, R> {
    /// Local in memory request
    Local(L),
    /// Remote cross process request
    Remote(R),
}

impl<S: Service> LocalSender<S> {
    /// Send a message to the service
    pub fn send<T>(&self, value: impl Into<WithChannels<T, S>>) -> SendFut<S::Message>
    where
        T: Channels<S>,
        S::Message: From<WithChannels<T, S>>,
    {
        let value: S::Message = value.into().into();
        SendFut::new(self.0.clone(), value)
    }

    /// Send a message to the service without the type conversion magic
    pub fn send_raw(&self, value: S::Message) -> SendFut<S::Message> {
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
