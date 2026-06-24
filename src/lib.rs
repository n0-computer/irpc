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
//! noq streams, specifically streams from the [noq].
//!
//! This restricts the possible rpc transports to noq (QUIC with dial by
//! socket address) and iroh (QUIC with dial by endpoint id).
//!
//! An upside of this is that the noq streams can be tuned for each rpc
//! request, e.g. by setting the stream priority or by directly using more
//! advanced part of the noq SendStream and RecvStream APIs such as out of
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
//! - `derive`: Enable the [`rpc_requests`] macro.
//! - `rpc`: Enable the rpc features. Enabled by default.
//!   By disabling this feature, all rpc related dependencies are removed.
//!   The remaining dependencies are just serde, tokio and tokio-util.
//! - `spans`: Enable tracing spans for messages. Enabled by default.
//!   This is useful even without rpc, to not lose tracing context when message
//!   passing. This is frequently done manually. This obviously requires
//!   a dependency on tracing.
//! - `noq_endpoint_setup`: Easy way to create noq endpoints. This is useful
//!   both for testing and for rpc on localhost. Enabled by default.
//!
//! # Example
//!
//! ```
//! use irpc::{
//!     Client, WithChannels,
//!     channel::{mpsc, oneshot},
//!     rpc_requests,
//! };
//! use serde::{Deserialize, Serialize};
//!
//! #[tokio::main]
//! async fn main() -> n0_error::Result<()> {
//!     let client = spawn_server();
//!     let res = client.rpc(Multiply(3, 7)).await?;
//!     assert_eq!(res, 21);
//!
//!     let (tx, mut rx) = client.bidi_streaming(Sum, 4, 4).await?;
//!     tx.send(4).await?;
//!     assert_eq!(rx.recv().await?, Some(4));
//!     tx.send(6).await?;
//!     assert_eq!(rx.recv().await?, Some(10));
//!     tx.send(11).await?;
//!     assert_eq!(rx.recv().await?, Some(21));
//!     Ok(())
//! }
//!
//! /// We define a simple protocol using the derive macro.
//! #[rpc_requests(message = ComputeMessage)]
//! #[derive(Debug, Serialize, Deserialize)]
//! enum ComputeProtocol {
//!     /// Multiply two numbers, return the result over a oneshot channel.
//!     #[rpc(tx=oneshot::Sender<i64>)]
//!     #[wrap(Multiply)]
//!     Multiply(i64, i64),
//!     /// Sum all numbers received via the `rx` stream,
//!     /// reply with the updating sum over the `tx` stream.
//!     #[rpc(tx=mpsc::Sender<i64>, rx=mpsc::Receiver<i64>)]
//!     #[wrap(Sum)]
//!     Sum,
//! }
//!
//! fn spawn_server() -> Client<ComputeProtocol> {
//!     let (tx, rx) = tokio::sync::mpsc::channel(16);
//!     // Spawn an actor task to handle incoming requests.
//!     tokio::task::spawn(server_actor(rx));
//!     // Return a local client to talk to our actor.
//!     irpc::Client::local(tx)
//! }
//!
//! async fn server_actor(mut rx: tokio::sync::mpsc::Receiver<ComputeMessage>) {
//!     while let Some(msg) = rx.recv().await {
//!         match msg {
//!             ComputeMessage::Multiply(msg) => {
//!                 let WithChannels { inner, tx, .. } = msg;
//!                 let Multiply(a, b) = inner;
//!                 tx.send(a * b).await.ok();
//!             }
//!             ComputeMessage::Sum(msg) => {
//!                 let WithChannels { tx, mut rx, .. } = msg;
//!                 // Spawn a separate task for this potentially long-running request.
//!                 tokio::task::spawn(async move {
//!                     let mut sum = 0;
//!                     while let Ok(Some(number)) = rx.recv().await {
//!                         sum += number;
//!                         if tx.send(sum).await.is_err() {
//!                             break;
//!                         }
//!                     }
//!                 });
//!             }
//!         }
//!     }
//! }
//! ```
//!
//! # History
//!
//! This crate evolved out of the [quic-rpc](https://docs.rs/quic-rpc/latest/quic-rpc/index.html) crate, which is a generic RPC
//! framework for any transport with cheap streams such as QUIC. Compared to
//! quic-rpc, this crate does not abstract over the stream type and is focused
//! on [iroh](https://docs.rs/iroh/latest/iroh/index.html) and our [noq](https://docs.rs/noq/latest/noq/index.html).
#![cfg_attr(quicrpc_docsrs, feature(doc_cfg))]
use std::{fmt::Debug, future::Future, io, marker::PhantomData, ops::Deref};

/// Processes an RPC request enum and generates trait implementations for use with `irpc`.
///
/// This attribute macro may be applied to an enum where each variant represents
/// a different RPC request type. Each variant of the enum must contain a single unnamed field
/// of a distinct type (unless the `wrap` attribute is used on a variant, see below).
///
/// Basic usage example:
/// ```
/// use irpc::{
///     channel::{mpsc, oneshot},
///     rpc_requests,
/// };
/// use serde::{Deserialize, Serialize};
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
/// * A `From` implementation to convert from each request type to the protocol enum.
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
///     * Generates a [`rpc::RemoteService`] implementation for the protocol enum.
/// * `alias = "<suffix>"` *(optional)*: Generate type aliases with the given suffix for each `WithChannels<T, Service>`.
/// * `rpc_feature = "<feature>"` *(optional)*: If set, the `RemoteService` implementation will be feature-flagged
///   with this feature. Set this if your crate only optionally enables the `rpc` feature
///   of `irpc`.
/// * `no_rpc` *(optional, no value)*: If set, no implementation of `RemoteService` will be generated and the generated
///   code works without the `rpc` feature of `irpc`.
/// * `no_spans` *(optional, no value)*: If set, the generated code works without the `spans` feature of `irpc`.
/// * `span_propagation` *(optional, no value)*: If set, enables OpenTelemetry span context propagation
///   across remote connections. When enabled, span context is included in the wire format as
///   `(Option<SpanContextCarrier>, Message)`, and the generated `RemoteService` implementation
///   will set the parent span from the propagated remote context. Requires the `tracing-opentelemetry`
///   feature to be enabled for actual OpenTelemetry integration; without it, the context is
///   still serialized but has no effect.
///
/// ## Variant attributes
///
/// #### `#[rpc]` attribute
///
/// Individual enum variants are annotated with the `#[rpc(...)]` attribute to specify channel types.
/// The `rpc` attribute contains two optional arguments:
///
/// * `tx = SomeType`: Set the kind of channel for sending responses from the server to the client.
///   Must be a `Sender` type from the [`channel`] module.
///   If `tx` is not set, it defaults to [`channel::none::NoSender`].
/// * `rx = OtherType`: Set the kind of channel for receiving updates from the client at the server.
///   Must be a `Receiver` type from the [`channel`] module.
///   If `rx` is not set, it defaults to [`channel::none::NoReceiver`].
///
/// #### `#[wrap]` attribute
///
/// The attribute has the syntax `#[wrap(TypeName, derive(Foo, Bar))]`
///
/// If set, a struct `TypeName` will be generated from the variant's fields, and the variant
/// will be changed to have a single, unnamed field of `TypeName`.
///
/// * `TypeName` is the name of the generated type.
///   By default it will inherit the visibility of the protocol enum. You can set a different
///   visibility by prefixing it with the visibility (e.g. `pub(crate) TypeName`).
/// * `derive(Foo, Bar)` is optional and allows to set additional derives for the generated struct.
///   By default, the struct will get `Serialize`, `Deserialize`, and `Debug` derives.
///
/// ## Examples
///
/// With `wrap`:
/// ```
/// use irpc::{
///     Client,
///     channel::{mpsc, oneshot},
///     rpc_requests,
/// };
/// use serde::{Deserialize, Serialize};
///
/// #[rpc_requests(message = StoreMessage)]
/// #[derive(Debug, Serialize, Deserialize)]
/// enum StoreProtocol {
///     /// Doc comment for `GetRequest`.
///     #[rpc(tx=oneshot::Sender<String>)]
///     #[wrap(GetRequest, derive(Clone))]
///     Get(String),
///
///     /// Doc comment for `SetRequest`.
///     #[rpc(tx=oneshot::Sender<()>)]
///     #[wrap(SetRequest)]
///     Set { key: String, value: String },
/// }
///
/// async fn client_usage(client: Client<StoreProtocol>) -> n0_error::Result<()> {
///     client
///         .rpc(SetRequest {
///             key: "foo".to_string(),
///             value: "bar".to_string(),
///         })
///         .await?;
///     let value = client.rpc(GetRequest("foo".to_string())).await?;
///     Ok(())
/// }
/// ```
///
/// With type aliases:
/// ```no_compile
/// #[rpc_requests(message = ComputeMessage, alias = "Msg")]
/// enum ComputeProtocol {
///     #[rpc(tx=oneshot::Sender<u128>)]
///     Sqr(Sqr), // Generates type SqrMsg = WithChannels<Sqr, ComputeProtocol>
///     #[rpc(tx=mpsc::Sender<i64>)]
///     Sum(Sum), // Generates type SumMsg = WithChannels<Sum, ComputeProtocol>
/// }
/// ```
///
/// [`RemoteService`]: rpc::RemoteService
/// [`WithChannels<T, Service>`]: WithChannels
/// [`Channels<S>`]: Channels
#[cfg(feature = "derive")]
#[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "derive")))]
pub use irpc_derive::rpc_requests;
#[cfg(feature = "rpc")]
use n0_error::AnyError;
use n0_error::stack_error;
use serde::{Serialize, de::DeserializeOwned};

use self::{
    channel::{
        mpsc,
        none::{NoReceiver, NoSender},
        oneshot,
    },
    sealed::Sealed,
};
use crate::channel::SendError;

pub mod channel;
#[cfg(feature = "rpc")]
#[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "rpc")))]
pub mod span_propagation;
#[cfg(test)]
mod tests;
pub mod util;
#[cfg(not(feature = "rpc"))]
pub mod rpc {
    pub struct RemoteSender<S>(std::marker::PhantomData<S>);
}
#[cfg(feature = "rpc")]
#[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "rpc")))]
pub mod rpc;

mod sealed {
    pub trait Sealed {}
}

/// Requirements for a RPC message
///
/// Even when just using the mem transport, we require messages to be Serializable and Deserializable.
/// Likewise, even when using the noq transport, we require messages to be Send.
///
/// This does not seem like a big restriction. If you want a pure memory channel without the possibility
/// to also use the noq transport, you might want to use a mpsc channel directly.
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

    /// Whether this protocol includes span context in the wire format.
    ///
    /// When `true`, messages are serialized as `(Option<SpanContextCarrier>, Message)`.
    /// When `false` (default), messages are serialized directly without span context wrapper.
    ///
    /// This is controlled by the `span_propagation` attribute on the `rpc_requests` macro.
    const SPAN_PROPAGATION: bool = false;
}

/// Sealed marker trait for a sender
pub trait Sender: Debug + Sealed {}

/// Sealed marker trait for a receiver
pub trait Receiver: Debug + Sealed {}

/// Trait to specify channels for a message and service
pub trait Channels<S: Service>: Send + 'static {
    /// The sender type, can be either mpsc, oneshot or none
    type Tx: Sender;
    /// The receiver type, can be either mpsc, oneshot or none
    ///
    /// For many services, the receiver is not needed, so it can be set to [`NoReceiver`].
    type Rx: Receiver;
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
            span: tracing::Span::current(),
        }
    }
}

/// Tuple conversion from inner message to a WithChannels struct without channels
impl<I, S> From<(I,)> for WithChannels<I, S>
where
    I: Channels<S, Rx = NoReceiver, Tx = NoSender>,
    S: Service,
{
    fn from(inner: (I,)) -> Self {
        let (inner,) = inner;
        Self {
            inner,
            tx: NoSender,
            rx: NoReceiver,
            #[cfg(feature = "spans")]
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

impl<S: Service> From<mpsc::Sender<S::Message>> for Client<S> {
    fn from(tx: mpsc::Sender<S::Message>) -> Self {
        Self(ClientInner::Local(tx), PhantomData)
    }
}

impl<S: Service> From<tokio::sync::mpsc::Sender<S::Message>> for Client<S> {
    fn from(tx: tokio::sync::mpsc::Sender<S::Message>) -> Self {
        LocalSender::from(tx).into()
    }
}

impl<S: Service> Client<S> {
    /// Create a new client to a remote service using the given noq `endpoint`
    /// and a socket `addr` of the remote service.
    #[cfg(feature = "rpc")]
    pub fn noq(endpoint: noq::Endpoint, addr: std::net::SocketAddr) -> Self {
        Self::boxed(rpc::NoqLazyRemoteConnection::new(endpoint, addr))
    }

    /// Create a new client from a `rpc::RemoteConnection` trait object.
    /// This is used from crates that want to provide other transports than noq,
    /// such as the iroh transport.
    #[cfg(feature = "rpc")]
    pub fn boxed(remote: impl rpc::RemoteConnection) -> Self {
        Self(ClientInner::Remote(Box::new(remote)), PhantomData)
    }

    /// Creates a new client from a `tokio::sync::mpsc::Sender`.
    pub fn local(tx: impl Into<crate::channel::mpsc::Sender<S::Message>>) -> Self {
        let tx: crate::channel::mpsc::Sender<S::Message> = tx.into();
        Self(ClientInner::Local(tx), PhantomData)
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
    /// [`noq`] or iroh connection.
    ///
    /// In both cases, the returned sender is fully self contained.
    #[allow(clippy::type_complexity)]
    pub fn request(
        &self,
    ) -> impl Future<Output = Result<Request<LocalSender<S>, rpc::RemoteSender<S>>, RequestError>> + use<S>
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

    /// Performs a request for which the client can send updates.
    pub fn client_streaming<Req, Update, Res>(
        &self,
        msg: Req,
        local_update_cap: usize,
    ) -> impl Future<Output = Result<(mpsc::Sender<Update>, oneshot::Receiver<Res>)>>
    + use<Req, Update, Res, S>
    where
        S: From<Req>,
        S::Message: From<WithChannels<Req, S>>,
        Req: Channels<S, Tx = oneshot::Sender<Res>, Rx = mpsc::Receiver<Update>>,
        Update: RpcMessage,
        Res: RpcMessage,
    {
        let request = self.request();
        async move {
            let (update_tx, res_rx): (mpsc::Sender<Update>, oneshot::Receiver<Res>) =
                match request.await? {
                    Request::Local(request) => {
                        let (req_tx, req_rx) = mpsc::channel(local_update_cap);
                        let (res_tx, res_rx) = oneshot::channel();
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
    ) -> impl Future<Output = Result<(mpsc::Sender<Update>, mpsc::Receiver<Res>)>>
    + Send
    + 'static
    + use<Req, Update, Res, S>
    where
        S: From<Req>,
        S::Message: From<WithChannels<Req, S>>,
        Req: Channels<S, Tx = mpsc::Sender<Res>, Rx = mpsc::Receiver<Update>>,
        Update: RpcMessage,
        Res: RpcMessage,
    {
        let request = self.request();
        async move {
            let (update_tx, res_rx): (mpsc::Sender<Update>, mpsc::Receiver<Res>) =
                match request.await? {
                    Request::Local(request) => {
                        let (update_tx, update_rx) = mpsc::channel(local_update_cap);
                        let (res_tx, res_rx) = mpsc::channel(local_response_cap);
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

    /// Performs a request for which the server returns nothing.
    ///
    /// The purpose of notify is to send messages to the remote without waiting
    /// for the remote to respond.
    ///
    /// The returned future completes once the message is written *locally*.
    /// Therefore we have no guarantee that the remote has received the message.
    ///
    /// If we close the connection immediately after the future returns, the
    /// connection might be closed *before* the message is on the wire, so the
    /// remote might never receive it.
    ///
    /// If you need to send a message with unit result but want to wait until the
    /// remote has received it, consider using [`rpc`] with a unit `()` return
    /// type instead.
    ///
    /// This method is safe to use with both regular and 0-RTT connections.
    /// If 0-RTT data is rejected, the message will be automatically re-sent.
    pub fn notify<Req>(&self, msg: Req) -> impl Future<Output = Result<()>> + Send + 'static
    where
        S: From<Req>,
        S::Message: From<WithChannels<Req, S>>,
        Req: Channels<S, Tx = NoSender, Rx = NoReceiver>,
    {
        let this = self.clone();
        async move {
            match this.request().await? {
                Request::Local(request) => {
                    request.send((msg,)).await?;
                }
                #[cfg(not(feature = "rpc"))]
                Request::Remote(_request) => unreachable!(),
                #[cfg(feature = "rpc")]
                Request::Remote(request) => {
                    // see https://www.iroh.computer/blog/0rtt-api#connect-side
                    let buf = rpc::prepare_write::<S>(msg)?;
                    let (_tx, _rx) = request.write_raw(&buf).await?;
                    if this.0.zero_rtt_rejected().await {
                        // 0rtt was not accepted, the data is lost, send it again!
                        let Request::Remote(request) = this.request().await? else {
                            unreachable!()
                        };
                        let (_tx, _rx) = request.write_raw(&buf).await?;
                    }
                }
            };
            Ok(())
        }
    }

    /// Performs a request for which the server returns a oneshot receiver.
    ///
    /// This method is safe to use with both regular and 0-RTT connections.
    /// If 0-RTT data is rejected, the message will be automatically re-sent.
    pub fn rpc<Req, Res>(&self, msg: Req) -> impl Future<Output = Result<Res>> + Send + 'static
    where
        S: From<Req>,
        S::Message: From<WithChannels<Req, S>>,
        Req: Channels<S, Tx = oneshot::Sender<Res>, Rx = NoReceiver>,
        Res: RpcMessage,
    {
        let this = self.clone();
        async move {
            let recv: oneshot::Receiver<Res> = match this.request().await? {
                Request::Local(request) => {
                    let (tx, rx) = oneshot::channel();
                    request.send((msg, tx)).await?;
                    rx
                }
                #[cfg(not(feature = "rpc"))]
                Request::Remote(_request) => unreachable!(),
                #[cfg(feature = "rpc")]
                Request::Remote(request) => {
                    // see https://www.iroh.computer/blog/0rtt-api#connect-side
                    let buf = rpc::prepare_write::<S>(msg)?;
                    let (_tx, rx) = request.write_raw(&buf).await?;
                    if this.0.zero_rtt_rejected().await {
                        // 0rtt was not accepted, the data is lost, send it again!
                        let Request::Remote(request) = this.request().await? else {
                            unreachable!()
                        };
                        let (_tx, rx) = request.write_raw(&buf).await?;
                        rx
                    } else {
                        rx
                    }
                    .into()
                }
            };
            let res = recv.await?;
            Ok(res)
        }
    }

    /// Performs a request for which the server returns a mpsc receiver.
    ///
    /// This method is safe to use with both regular and 0-RTT connections.
    /// If 0-RTT data is rejected, the message will be automatically re-sent.
    pub fn server_streaming<Req, Res>(
        &self,
        msg: Req,
        local_response_cap: usize,
    ) -> impl Future<Output = Result<mpsc::Receiver<Res>>> + Send + 'static + use<Req, Res, S>
    where
        S: From<Req>,
        S::Message: From<WithChannels<Req, S>>,
        Req: Channels<S, Tx = mpsc::Sender<Res>, Rx = NoReceiver>,
        Res: RpcMessage,
    {
        let this = self.clone();
        async move {
            let recv: mpsc::Receiver<Res> = match this.request().await? {
                Request::Local(request) => {
                    let (tx, rx) = mpsc::channel(local_response_cap);
                    request.send((msg, tx)).await?;
                    rx
                }
                #[cfg(not(feature = "rpc"))]
                Request::Remote(_request) => unreachable!(),
                #[cfg(feature = "rpc")]
                Request::Remote(request) => {
                    // see https://www.iroh.computer/blog/0rtt-api#connect-side
                    let buf = rpc::prepare_write::<S>(msg)?;
                    let (_tx, rx) = request.write_raw(&buf).await?;
                    if this.0.zero_rtt_rejected().await {
                        // 0rtt was not accepted, the data is lost, send it again!
                        let Request::Remote(request) = this.request().await? else {
                            unreachable!()
                        };
                        let (_tx, rx) = request.write_raw(&buf).await?;
                        rx
                    } else {
                        rx
                    }
                    .into()
                }
            };
            Ok(recv)
        }
    }

    /// Deprecated: use [`Self::notify`] instead, it handles 0rtt automatically.
    #[deprecated(note = "use `notify` instead, it handles 0rtt automatically")]
    pub fn notify_0rtt<Req>(&self, msg: Req) -> impl Future<Output = Result<()>> + Send + 'static
    where
        S: From<Req>,
        S::Message: From<WithChannels<Req, S>>,
        Req: Channels<S, Tx = NoSender, Rx = NoReceiver>,
    {
        self.notify(msg)
    }

    /// Deprecated: use [`Self::rpc`] instead, it handles 0rtt automatically.
    #[deprecated(note = "use `rpc` instead, it handles 0rtt automatically")]
    pub fn rpc_0rtt<Req, Res>(&self, msg: Req) -> impl Future<Output = Result<Res>> + Send + 'static
    where
        S: From<Req>,
        S::Message: From<WithChannels<Req, S>>,
        Req: Channels<S, Tx = oneshot::Sender<Res>, Rx = NoReceiver>,
        Res: RpcMessage,
    {
        self.rpc(msg)
    }

    /// Deprecated: use [`Self::server_streaming`] instead, it handles 0rtt automatically.
    #[deprecated(note = "use `server_streaming` instead, it handles 0rtt automatically")]
    pub fn server_streaming_0rtt<Req, Res>(
        &self,
        msg: Req,
        local_response_cap: usize,
    ) -> impl Future<Output = Result<mpsc::Receiver<Res>>> + Send + 'static
    where
        S: From<Req>,
        S::Message: From<WithChannels<Req, S>>,
        Req: Channels<S, Tx = mpsc::Sender<Res>, Rx = NoReceiver>,
        Res: RpcMessage,
    {
        self.server_streaming(msg, local_response_cap)
    }
}

#[derive(Debug)]
pub(crate) enum ClientInner<M> {
    Local(crate::channel::mpsc::Sender<M>),
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

impl<M> ClientInner<M> {
    #[allow(dead_code)]
    async fn zero_rtt_rejected(&self) -> bool {
        match self {
            ClientInner::Local(_sender) => false,
            #[cfg(feature = "rpc")]
            ClientInner::Remote(remote_connection) => remote_connection.zero_rtt_rejected().await,
            #[cfg(not(feature = "rpc"))]
            Self::Remote(_) => unreachable!(),
        }
    }
}

/// Error when opening a request. When cross-process rpc is disabled, this is
/// an empty enum since local requests can not fail.
#[stack_error(derive, add_meta, from_sources)]
pub enum RequestError {
    /// Error in noq during connect
    #[cfg(feature = "rpc")]
    #[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "rpc")))]
    #[error("Error establishing connection")]
    Connect {
        #[error(std_err)]
        source: noq::ConnectError,
    },
    /// Error in noq when the connection already exists, when opening a stream pair
    #[cfg(feature = "rpc")]
    #[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "rpc")))]
    #[error("Error opening stream")]
    Connection {
        #[error(std_err)]
        source: noq::ConnectionError,
    },
    /// Generic error for non-noq transports
    #[cfg(feature = "rpc")]
    #[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "rpc")))]
    #[error("Error opening stream")]
    Other { source: AnyError },

    #[cfg(not(feature = "rpc"))]
    #[error("(Without the rpc feature, requests cannot fail")]
    Unreachable,
}

/// Error type that subsumes all possible errors in this crate, for convenience.
#[stack_error(derive, add_meta, from_sources)]
pub enum Error {
    #[error("Request error")]
    Request { source: RequestError },
    #[error("Send error")]
    Send { source: channel::SendError },
    #[error("Mpsc recv error")]
    MpscRecv { source: channel::mpsc::RecvError },
    #[error("Oneshot recv error")]
    OneshotRecv { source: channel::oneshot::RecvError },
    #[cfg(feature = "rpc")]
    #[error("Recv error")]
    Write { source: rpc::WriteError },
}

/// Type alias for a result with an irpc error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        match e {
            Error::Request { source, .. } => source.into(),
            Error::Send { source, .. } => source.into(),
            Error::MpscRecv { source, .. } => source.into(),
            Error::OneshotRecv { source, .. } => source.into(),
            #[cfg(feature = "rpc")]
            Error::Write { source, .. } => source.into(),
        }
    }
}

impl From<RequestError> for io::Error {
    fn from(e: RequestError) -> Self {
        match e {
            #[cfg(feature = "rpc")]
            RequestError::Connect { source, .. } => io::Error::other(source),
            #[cfg(feature = "rpc")]
            RequestError::Connection { source, .. } => source.into(),
            #[cfg(feature = "rpc")]
            RequestError::Other { source, .. } => io::Error::other(source),
            #[cfg(not(feature = "rpc"))]
            RequestError::Unreachable { .. } => unreachable!(),
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
pub struct LocalSender<S: Service>(crate::channel::mpsc::Sender<S::Message>);

impl<S: Service> Clone for LocalSender<S> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<S: Service> From<tokio::sync::mpsc::Sender<S::Message>> for LocalSender<S> {
    fn from(tx: tokio::sync::mpsc::Sender<S::Message>) -> Self {
        Self(tx.into())
    }
}

impl<S: Service> From<crate::channel::mpsc::Sender<S::Message>> for LocalSender<S> {
    fn from(tx: crate::channel::mpsc::Sender<S::Message>) -> Self {
        Self(tx)
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
    pub fn send<T>(
        &self,
        value: impl Into<WithChannels<T, S>>,
    ) -> impl Future<Output = Result<(), SendError>> + Send + 'static
    where
        T: Channels<S>,
        S::Message: From<WithChannels<T, S>>,
    {
        let value: S::Message = value.into().into();
        self.send_raw(value)
    }

    /// Send a message to the service without the type conversion magic
    pub fn send_raw(
        &self,
        value: S::Message,
    ) -> impl Future<Output = Result<(), SendError>> + Send + 'static + use<S> {
        let x = self.0.clone();
        async move { x.send(value).await }
    }
}
