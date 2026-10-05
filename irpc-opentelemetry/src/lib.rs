//! OpenTelemetry span propagation for irpc.
//!
//! irpc carries the span context of a request in text headers, but does not
//! know any tracing backend. This crate connects it to OpenTelemetry: the
//! client writes the context of the current span with the global text map
//! propagator of `opentelemetry`, and the server sets the parent of the request
//! span from it.
//!
//! Add its [`layer`] to the tracing subscriber, next to the
//! `tracing-opentelemetry` layer:
//!
//! ```no_run
//! use tracing_subscriber::{Registry, layer::SubscriberExt};
//!
//! opentelemetry::global::set_text_map_propagator(
//!     opentelemetry_sdk::propagation::TraceContextPropagator::new(),
//! );
//! let tracer = opentelemetry::global::tracer("app");
//! let subscriber = Registry::default()
//!     .with(tracing_opentelemetry::layer().with_tracer(tracer))
//!     .with(irpc_opentelemetry::layer());
//! tracing::subscriber::set_global_default(subscriber).expect("no other subscriber is set");
//! ```
//!
//! Spans reach OpenTelemetry through the `tracing-opentelemetry` layer. The
//! application must use semver-compatible versions of `opentelemetry` and
//! `tracing-opentelemetry` with this crate: both keep their state in statics,
//! and with a second copy of either, irpc silently propagates nothing.

use irpc::span_propagation::{Propagator, PropagatorLayer, SpanContextCarrier};
use opentelemetry::{
    propagation::{Extractor, Injector},
    trace::TraceContextExt,
};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// A [`Propagator`] that uses the global text map propagator of `opentelemetry`.
#[derive(Debug, Default, Clone, Copy)]
pub struct OtelPropagator;

impl Propagator for OtelPropagator {
    fn inject(&self, span: &tracing::Span, carrier: &mut SpanContextCarrier) {
        // The `tracing-opentelemetry` layer keeps the context in the tracing span.
        let context = span.context();
        opentelemetry::global::get_text_map_propagator(|propagator| {
            propagator.inject_context(&context, &mut Headers(carrier));
        });
    }

    fn set_parent(&self, span: &tracing::Span, carrier: &SpanContextCarrier) {
        let context = opentelemetry::global::get_text_map_propagator(|propagator| {
            propagator
                .extract_with_context(&opentelemetry::Context::current(), &HeadersRef(carrier))
        });
        // If the headers have no span, `context` is the current context. Without
        // context activation in `tracing-opentelemetry`, it has no span either, and
        // setting it as parent would make `span` a new root.
        if context.span().span_context().is_valid() {
            let _ = span.set_parent(context);
        }
    }
}

/// Returns a tracing layer that makes irpc propagate span context with [`OtelPropagator`].
pub fn layer() -> PropagatorLayer {
    PropagatorLayer::new(OtelPropagator)
}

/// Writes headers into a [`SpanContextCarrier`].
struct Headers<'a>(&'a mut SpanContextCarrier);

impl Injector for Headers<'_> {
    fn set(&mut self, key: &str, value: String) {
        self.0.set(key, value);
    }
}

/// Reads headers from a [`SpanContextCarrier`].
struct HeadersRef<'a>(&'a SpanContextCarrier);

impl Extractor for HeadersRef<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key)
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().collect()
    }
}
