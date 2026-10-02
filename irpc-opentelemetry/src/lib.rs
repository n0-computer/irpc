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
//! Spans reach OpenTelemetry through the `tracing-opentelemetry` layer. This
//! crate uses the versions of `opentelemetry` and `tracing-opentelemetry` that
//! it re-exports. The application must use the same versions, because both
//! crates keep their state in statics.

use irpc::span_propagation::{Propagator, PropagatorLayer, SpanContextCarrier};
pub use opentelemetry;
use opentelemetry::propagation::{Extractor, Injector};
pub use tracing_opentelemetry;
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
        let _ = span.set_parent(context);
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
