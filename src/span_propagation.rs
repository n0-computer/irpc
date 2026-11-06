//! Span context propagation across remote connections.
//!
//! A protocol opts in with the `span_propagation` argument of the
//! `rpc_requests` macro. Each request then carries a
//! [`SpanContextCarrier`], a map of text headers, in front of the request.
//!
//! irpc does not know any tracing backend. A `Propagator` writes the context
//! of a span into the headers on the client, and sets the parent of a span from
//! the headers on the server.
//!
//! irpc finds the propagator through the tracing subscriber: add the `layer`
//! for it to the subscriber. The client asks the subscriber of the current
//! thread, and the server asks the subscriber of the request span. Without the
//! layer, irpc does not propagate span context.
//!
//! The propagator and the layer need the `tracing-subscriber-03` feature,
//! because the layer is a layer of `tracing-subscriber` 0.3. A later version
//! of `tracing-subscriber` would get its own feature. Neither the feature nor
//! the layer changes the wire format: a protocol with
//! `span_propagation` always sends an `Option<SpanContextCarrier>`, which is
//! `None` when irpc does not propagate span context.
//!
//! irpc creates the request spans at debug level, see
//! [`Service::new_request_span`](crate::Service::new_request_span). The server
//! can only set the parent of a request span that the tracing backend records,
//! so the filter of the backend on the server must let the request spans
//! through. If it drops them, the server starts a new trace. The client sends
//! the context of the span that it is in if the backend drops its request span.
//!
//! The `irpc-opentelemetry` crate has a propagator for OpenTelemetry, and
//! documents how to filter for it.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

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

    /// Sets the parent of `span` from this context, if the subscriber of `span` has a propagator.
    pub(crate) fn set_parent_of(&self, span: &tracing::Span) {
        #[cfg(not(feature = "tracing-subscriber-03"))]
        let _ = span;
        #[cfg(feature = "tracing-subscriber-03")]
        span.with_subscriber(|(_id, dispatch)| {
            if let Some(propagator) = dispatch.downcast_ref::<PropagatorLayer>() {
                propagator.0.set_parent(span, self);
            }
        });
    }

    /// Returns the context of `span`, or of the current span if `span` has none.
    ///
    /// Returns `None` if the subscriber has no propagator. A span has no
    /// context if the tracing backend does not record it, for example because
    /// a filter of the backend drops it. The client calls this outside the
    /// request span, so the current span is the parent of the request span,
    /// and the server still joins the trace of the caller.
    pub(crate) fn from_span_or_current(span: &tracing::Span) -> Option<Self> {
        #[cfg(not(feature = "tracing-subscriber-03"))]
        {
            let _ = span;
            None
        }
        #[cfg(feature = "tracing-subscriber-03")]
        {
            // Inside `get_default`, the dispatcher of a scoped subscriber is busy, and
            // `Span::current` would return no span.
            let current = tracing::Span::current();
            tracing::dispatcher::get_default(|dispatch| {
                let propagator = dispatch.downcast_ref::<PropagatorLayer>()?;
                let mut carrier = Self::default();
                propagator.0.inject(span, &mut carrier);
                if carrier.headers.is_empty() {
                    propagator.0.inject(&current, &mut carrier);
                }
                Some(carrier)
            })
        }
    }
}

/// Connects span propagation to a tracing backend.
#[cfg(feature = "tracing-subscriber-03")]
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
#[cfg(feature = "tracing-subscriber-03")]
pub fn layer<S>(propagator: impl Propagator) -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    use tracing_subscriber::Layer;
    PropagatorLayer(Box::new(propagator)).with_filter(tracing::level_filters::LevelFilter::OFF)
}

/// The layer from [`layer`], without its filter.
#[cfg(feature = "tracing-subscriber-03")]
struct PropagatorLayer(Box<dyn Propagator>);

#[cfg(feature = "tracing-subscriber-03")]
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for PropagatorLayer {}
