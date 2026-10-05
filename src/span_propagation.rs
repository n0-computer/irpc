//! Span context propagation across remote connections.
//!
//! A protocol opts in with the `span_propagation` argument of the
//! [`rpc_requests`](crate::rpc_requests) macro. Each request then carries a
//! [`SpanContextCarrier`], a map of text headers, in front of the request.
//!
//! irpc does not know any tracing backend. A [`Propagator`] writes the context
//! of a span into the headers on the client, and sets the parent of a span from
//! the headers on the server.
//!
//! irpc finds the propagator through the tracing subscriber: add the [`layer`]
//! for it to the subscriber. The client asks the subscriber of the current
//! thread, and the server asks the subscriber of the request span. Without the
//! layer, irpc does not propagate span context.
//!
//! The propagator and the layer need the `span-propagation` feature. Neither
//! the feature nor the layer changes the wire format: a protocol with
//! `span_propagation` always sends an `Option<SpanContextCarrier>`, which is
//! `None` when irpc does not propagate span context.
//!
//! The `irpc-opentelemetry` crate has a propagator for OpenTelemetry.

use std::{collections::HashMap, future::Future};

use serde::{Deserialize, Serialize};

#[cfg(feature = "span-propagation")]
tokio::task_local! {
    static SPAN_CONTEXT: SpanContextCarrier;
}

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

    /// Returns the context of the current span, if the subscriber has a propagator.
    pub(crate) fn from_current() -> Option<Self> {
        #[cfg(not(feature = "span-propagation"))]
        return None;
        #[cfg(feature = "span-propagation")]
        tracing::dispatcher::get_default(|dispatch| {
            let propagator = dispatch.downcast_ref::<PropagatorLayer>()?;
            let mut carrier = Self::default();
            propagator.0.inject(&tracing::Span::current(), &mut carrier);
            Some(carrier)
        })
    }
}

/// Connects span propagation to a tracing backend.
#[cfg(feature = "span-propagation")]
pub trait Propagator: Send + Sync + 'static {
    /// Writes the context of `span` into `carrier`.
    fn inject(&self, span: &tracing::Span, carrier: &mut SpanContextCarrier);

    /// Sets the parent of `span` from the context in `carrier`.
    fn set_parent(&self, span: &tracing::Span, carrier: &SpanContextCarrier);
}

/// Returns a tracing layer that makes irpc propagate span context with `propagator`.
///
/// The layer records nothing. irpc finds it with [`tracing::Dispatch::downcast_ref`].
///
/// The layer has a per-layer filter that disables all spans and events. Without
/// it, the layer would enable all levels for the subscriber when other layers
/// use per-layer filters, and every `debug!` and `trace!` would reach them.
/// Another layer must enable the request spans, for example the layer of the
/// tracing backend.
#[cfg(feature = "span-propagation")]
pub fn layer<S>(propagator: impl Propagator) -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    use tracing_subscriber::Layer;
    PropagatorLayer(Box::new(propagator)).with_filter(tracing::level_filters::LevelFilter::OFF)
}

/// The layer from [`layer`], without its filter.
#[cfg(feature = "span-propagation")]
struct PropagatorLayer(Box<dyn Propagator>);

#[cfg(feature = "span-propagation")]
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for PropagatorLayer {}

/// Runs `fut` with `carrier` in scope for the request spans.
///
/// The server loop calls this for each request. Most users do not call it.
///
/// The carrier lives in a task-local of `fut`, so concurrent requests do not see
/// each other's context, and the context moves with `fut` across threads.
pub async fn scope_remote<F: Future>(carrier: Option<SpanContextCarrier>, fut: F) -> F::Output {
    #[cfg(not(feature = "span-propagation"))]
    let _ = carrier;
    #[cfg(not(feature = "span-propagation"))]
    return fut.await;
    #[cfg(feature = "span-propagation")]
    match carrier {
        Some(carrier) => SPAN_CONTEXT.scope(carrier, fut).await,
        None => fut.await,
    }
}

/// Sets the parent of `span` from the span context of the current request.
///
/// The code from `rpc_requests(span_propagation)` calls this through
/// `__macro_exports`. It does nothing outside of [`scope_remote`], if the
/// subscriber of `span` has no propagator, or without the `span-propagation`
/// feature.
pub(crate) fn set_span_parent_from_remote(span: &tracing::Span) {
    #[cfg(not(feature = "span-propagation"))]
    let _ = span;
    #[cfg(feature = "span-propagation")]
    span.with_subscriber(|(_id, dispatch)| {
        let Some(propagator) = dispatch.downcast_ref::<PropagatorLayer>() else {
            return;
        };
        let _ = SPAN_CONTEXT.try_with(|carrier| propagator.0.set_parent(span, carrier));
    });
}
