//! Span context propagation across remote connections.
//!
//! A protocol opts in with the `span_propagation` argument of the
//! [`rpc_requests`](crate::rpc_requests) macro. Each request then carries a
//! [`SpanContextCarrier`], a map of text headers, in front of the request.
//!
//! irpc does not know any tracing backend. A [`Propagator`] writes the context
//! of a span into the headers on the client, and sets the parent of a span from
//! the headers on the server. Install one per process with [`set_propagator`].
//!
//! The propagator does not change the wire format: a protocol with
//! `span_propagation` always sends the `Option<SpanContextCarrier>`. Without a
//! propagator, its value is `None`.
//!
//! The `irpc-opentelemetry` crate has a propagator for OpenTelemetry.

use std::{collections::HashMap, future::Future, sync::OnceLock};

use n0_error::stack_error;
use serde::{Deserialize, Serialize};

tokio::task_local! {
    static SPAN_CONTEXT: SpanContextCarrier;
}

/// The propagator of this process.
static PROPAGATOR: OnceLock<Box<dyn Propagator>> = OnceLock::new();

/// Text headers that carry the context of a span to the remote.
///
/// On the wire, this is a map from string to string. Propagators decide which
/// headers they use, for example `traceparent` and `tracestate` for W3C trace
/// context.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SpanContextCarrier {
    headers: HashMap<String, String>,
}

impl SpanContextCarrier {
    /// Returns the value of the header `key`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.headers.get(key).map(String::as_str)
    }

    /// Sets the header `key` to `value`.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.headers.insert(key.into(), value.into());
    }

    /// Returns the keys of all headers.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.headers.keys().map(String::as_str)
    }

    /// Returns the context of the current span, if a propagator is installed.
    pub(crate) fn from_current() -> Option<Self> {
        let propagator = PROPAGATOR.get()?;
        let mut carrier = Self::default();
        propagator.inject(&tracing::Span::current(), &mut carrier);
        Some(carrier)
    }
}

/// Connects span propagation to a tracing backend.
pub trait Propagator: Send + Sync + 'static {
    /// Writes the context of `span` into `carrier`.
    fn inject(&self, span: &tracing::Span, carrier: &mut SpanContextCarrier);

    /// Sets the parent of `span` from the context in `carrier`.
    fn set_parent(&self, span: &tracing::Span, carrier: &SpanContextCarrier);
}

/// The error of [`set_propagator`] when a propagator is installed already.
#[stack_error(derive)]
#[error("a span propagator is already installed")]
pub struct PropagatorAlreadySet;

/// Installs the propagator for this process.
///
/// # Errors
///
/// Returns [`PropagatorAlreadySet`] if a propagator is installed already.
pub fn set_propagator(propagator: impl Propagator) -> Result<(), PropagatorAlreadySet> {
    PROPAGATOR
        .set(Box::new(propagator))
        .map_err(|_| PropagatorAlreadySet)
}

/// Runs `fut` with `carrier` in scope for [`set_span_parent_from_remote`].
///
/// The server loop calls this for each request. Most users do not call it.
pub async fn scope_remote<F: Future>(carrier: Option<SpanContextCarrier>, fut: F) -> F::Output {
    match carrier {
        Some(carrier) => SPAN_CONTEXT.scope(carrier, fut).await,
        None => fut.await,
    }
}

/// Sets the parent of `span` from the span context of the current request.
///
/// The code from `rpc_requests(span_propagation)` calls this. It does nothing
/// outside of [`scope_remote`] or without a propagator.
pub fn set_span_parent_from_remote(span: &tracing::Span) {
    let Some(propagator) = PROPAGATOR.get() else {
        return;
    };
    let _ = SPAN_CONTEXT.try_with(|carrier| propagator.set_parent(span, carrier));
}
