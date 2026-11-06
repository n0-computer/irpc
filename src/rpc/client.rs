//! Remote client construction and connection initialization.

use std::{fmt, future::Future, marker::PhantomData, net::SocketAddr, sync::Arc};

use n0_error::{AnyError, e};
use n0_future::future::Boxed as BoxFuture;
use noq::{RecvStream, SendStream};

use super::RemoteConnection;
use crate::{Client, ClientInner, RequestError, Service};

/// A builder for remote clients with connection initialization.
///
/// Use [`Client::noq_builder`] or `Client::iroh_builder` to configure a client
/// that connects on demand and reconnects after a disconnect. Use
/// [`Self::from_noq`] or `Self::from_iroh` for a client that uses one existing
/// connection and cannot reconnect.
///
/// [`Self::on_connect`] sets the function that authenticates or initializes each
/// connection. Setup runs on the first request or a call to [`Client::connect`].
/// Building the client does not run setup or perform network I/O.
#[derive(Debug)]
#[must_use = "call build() to create the client"]
pub struct RemoteClientBuilder<S: Service> {
    source: ConnectionSource,
    hook: Option<ConnectHook>,
    _service: PhantomData<fn() -> S>,
}

impl<S: Service> RemoteClientBuilder<S> {
    fn new(source: ConnectionSource) -> Self {
        Self {
            source,
            hook: None,
            _service: PhantomData,
        }
    }

    pub(crate) fn noq(endpoint: noq::Endpoint, addr: SocketAddr) -> Self {
        Self::new(ConnectionSource::Noq { endpoint, addr })
    }

    #[cfg(feature = "iroh")]
    pub(crate) fn iroh(endpoint: iroh::Endpoint, addr: iroh::EndpointAddr, alpn: Vec<u8>) -> Self {
        Self::new(ConnectionSource::Iroh {
            endpoint,
            addr,
            alpn,
        })
    }

    /// Creates a builder for an existing noq connection.
    ///
    /// The initializer runs once, on the first request or a call to
    /// [`Client::connect`]. If setup fails or is cancelled, the connection is
    /// closed. The client cannot reconnect.
    ///
    /// Requests sent through other handles to this connection bypass the
    /// initializer. Configure and use the client before sending other requests.
    pub fn from_noq(connection: noq::Connection) -> Self {
        Self::new(ConnectionSource::Fixed(Connection::Noq(connection)))
    }

    /// Creates a builder for an iroh connection whose handshake has completed.
    ///
    /// The initializer runs once, on the first request or a call to
    /// [`Client::connect`]. If setup fails or is cancelled, the connection is
    /// closed. The client cannot reconnect.
    ///
    /// Requests sent through other handles to this connection bypass the
    /// initializer. Configure and use the client before sending other requests.
    #[cfg(feature = "iroh")]
    pub fn from_iroh(connection: iroh::endpoint::Connection) -> Self {
        Self::new(ConnectionSource::Fixed(Connection::Iroh(connection)))
    }

    /// Sets the function that initializes each new connection.
    ///
    /// The built client and all its clones wait for `hook` to return `Ok(())`
    /// before opening application streams. The hook receives a client for the
    /// new connection only. Use that client for setup requests, and await their
    /// responses before returning. It never reconnects or runs the hook itself.
    /// Calling the outer client from the hook would wait for the hook to finish
    /// and deadlock.
    ///
    /// If the hook returns an error, the operation that started setup fails with
    /// [`RequestError::Initialization`] and the connection is closed. Cancelling
    /// that operation during setup also closes the connection, even if the hook
    /// retained a client clone. A later operation can try again if the client
    /// was built with an endpoint rather than a fixed connection.
    ///
    /// Use a timeout if setup may take too long. Reconnecting does not resume
    /// failed streams or replay application requests. Calling this method again
    /// replaces the hook.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # #[cfg(feature = "derive")]
    /// # fn example(endpoint: noq::Endpoint, addr: std::net::SocketAddr, token: String) {
    /// use irpc::{Client, channel::oneshot, rpc_requests};
    /// use serde::{Deserialize, Serialize};
    ///
    /// #[rpc_requests(message = Message)]
    /// #[derive(Debug, Serialize, Deserialize)]
    /// enum Protocol {
    ///     #[rpc(tx = oneshot::Sender<bool>)]
    ///     #[wrap(Auth)]
    ///     Auth(String),
    /// }
    ///
    /// let client = Client::<Protocol>::noq_builder(endpoint, addr)
    ///     .on_connect(move |connection_client| {
    ///         let token = token.clone();
    ///         async move {
    ///             let accepted = connection_client
    ///                 .rpc(Auth(token))
    ///                 .await
    ///                 .map_err(std::io::Error::from)?;
    ///             if accepted {
    ///                 Ok(())
    ///             } else {
    ///                 Err(std::io::Error::other("authentication failed"))
    ///             }
    ///         }
    ///     })
    ///     .build();
    /// # let _ = client;
    /// # }
    /// ```
    pub fn on_connect<F, Fut, E>(mut self, hook: F) -> Self
    where
        F: Fn(Client<S>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
        E: std::error::Error + Send + Sync + 'static,
    {
        self.hook = Some(ConnectHook(Box::new(move |connection| {
            let client = Client(ClientInner::Remote(connection), PhantomData);
            let fut = hook(client);
            Box::pin(async move { fut.await.map_err(AnyError::from_std) })
        })));
        self
    }

    /// Builds the client without running connection setup.
    ///
    /// The first request or a call to [`Client::connect`] runs setup. All clones
    /// share the same connection and initializer.
    pub fn build(self) -> Client<S> {
        Client::boxed(ManagedConnection::new(self.source, self.hook))
    }
}

type ConnectFn = dyn Fn(Box<dyn RemoteConnection>) -> BoxFuture<Result<(), AnyError>> + Send + Sync;

struct ConnectHook(Box<ConnectFn>);

impl fmt::Debug for ConnectHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectHook").finish_non_exhaustive()
    }
}

#[derive(Debug)]
enum ConnectionSource {
    Noq {
        endpoint: noq::Endpoint,
        addr: SocketAddr,
    },
    #[cfg(feature = "iroh")]
    Iroh {
        endpoint: iroh::Endpoint,
        addr: iroh::EndpointAddr,
        alpn: Vec<u8>,
    },
    Fixed(Connection),
}

impl ConnectionSource {
    async fn connect(&self) -> Result<Connection, RequestError> {
        match self {
            Self::Noq { endpoint, addr } => Ok(Connection::Noq(
                endpoint.connect(*addr, "localhost")?.await?,
            )),
            #[cfg(feature = "iroh")]
            Self::Iroh {
                endpoint,
                addr,
                alpn,
            } => {
                let connection = endpoint
                    .connect(addr.clone(), alpn)
                    .await
                    .map_err(|err| e!(RequestError::Other, err.into()))?;
                Ok(Connection::Iroh(connection))
            }
            Self::Fixed(connection) => Ok(connection.clone()),
        }
    }
}

/// A native connection whose transport handshake has completed.
#[derive(Debug, Clone)]
enum Connection {
    Noq(noq::Connection),
    #[cfg(feature = "iroh")]
    Iroh(iroh::endpoint::Connection),
}

impl Connection {
    fn boxed(&self) -> Box<dyn RemoteConnection> {
        match self {
            Self::Noq(connection) => Box::new(connection.clone()),
            #[cfg(feature = "iroh")]
            Self::Iroh(connection) => Box::new(connection.clone()),
        }
    }

    fn close_reason(&self) -> Option<noq::ConnectionError> {
        match self {
            Self::Noq(connection) => connection.close_reason(),
            #[cfg(feature = "iroh")]
            Self::Iroh(connection) => connection.close_reason(),
        }
    }

    fn close(&self) {
        match self {
            Self::Noq(connection) => {
                connection.close(0u32.into(), b"initialization did not finish")
            }
            #[cfg(feature = "iroh")]
            Self::Iroh(connection) => {
                connection.close(0u32.into(), b"initialization did not finish")
            }
        }
    }

    async fn open_bi(&self) -> Result<(SendStream, RecvStream), RequestError> {
        match self {
            Self::Noq(connection) => Ok(connection.open_bi().await?),
            #[cfg(feature = "iroh")]
            Self::Iroh(connection) => Ok(connection.open_bi().await?),
        }
    }
}

/// Closes an unpublished connection on errors, cancellation, or unwinding.
struct Initializing(Option<Connection>);

impl Drop for Initializing {
    fn drop(&mut self) {
        if let Some(connection) = &self.0 {
            connection.close();
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ManagedConnection(Arc<ManagedConnectionInner>);

#[derive(Debug)]
struct ManagedConnectionInner {
    source: ConnectionSource,
    hook: Option<ConnectHook>,
    /// Holds the initialized connection and serializes setup and stream opening.
    connection: tokio::sync::Mutex<Option<Connection>>,
}

impl ManagedConnection {
    fn new(source: ConnectionSource, hook: Option<ConnectHook>) -> Self {
        Self(Arc::new(ManagedConnectionInner {
            source,
            hook,
            connection: Default::default(),
        }))
    }

    #[cfg(feature = "iroh")]
    pub(crate) fn iroh(endpoint: iroh::Endpoint, addr: iroh::EndpointAddr, alpn: Vec<u8>) -> Self {
        Self::new(
            ConnectionSource::Iroh {
                endpoint,
                addr,
                alpn,
            },
            None,
        )
    }
}

impl ManagedConnectionInner {
    async fn ensure_connected<'a>(
        &self,
        current: &'a mut Option<Connection>,
    ) -> Result<&'a Connection, RequestError> {
        if current
            .as_ref()
            .is_some_and(|conn| conn.close_reason().is_some())
        {
            *current = None;
        }
        if current.is_none() {
            let mut candidate = Initializing(Some(self.source.connect().await?));
            let connection = candidate
                .0
                .as_ref()
                .expect("candidate is not yet published");
            if let Some(reason) = connection.close_reason() {
                return Err(reason.into());
            }
            if let Some(hook) = &self.hook {
                (hook.0)(connection.boxed())
                    .await
                    .map_err(|err| e!(RequestError::Initialization, err))?;
            }
            if let Some(reason) = connection.close_reason() {
                return Err(reason.into());
            }
            *current = candidate.0.take();
        }
        Ok(current.as_ref().expect("connection was initialized"))
    }
}

impl RemoteConnection for ManagedConnection {
    fn clone_boxed(&self) -> Box<dyn RemoteConnection> {
        Box::new(self.clone())
    }

    fn open_bi(&self) -> BoxFuture<Result<(SendStream, RecvStream), RequestError>> {
        let this = self.0.clone();
        Box::pin(async move {
            let mut current = this.connection.lock().await;
            if let Some(connection) = current.as_ref() {
                match connection.open_bi().await {
                    Ok(pair) => return Ok(pair),
                    Err(err) if matches!(this.source, ConnectionSource::Fixed(_)) => {
                        return Err(err);
                    }
                    Err(_) => *current = None,
                }
            }
            let connection = this.ensure_connected(&mut current).await?;
            connection.open_bi().await
        })
    }

    fn zero_rtt_rejected(&self) -> BoxFuture<bool> {
        Box::pin(async { false })
    }

    fn connect(&self) -> BoxFuture<Result<(), RequestError>> {
        let this = self.0.clone();
        Box::pin(async move {
            let mut current = this.connection.lock().await;
            this.ensure_connected(&mut current).await?;
            Ok(())
        })
    }
}
