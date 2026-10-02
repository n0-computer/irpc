//! OpenTelemetry span propagation for irpc.
//!
//! irpc carries the span context of a request in text headers, but does not
//! know any tracing backend. This crate connects it to OpenTelemetry: the
//! client writes the context of the current span with the global text map
//! propagator of `opentelemetry`, and the server sets the parent of the request
//! span from it.
//!
//! Install it once at startup, after the `opentelemetry` setup:
//!
//! ```no_run
//! opentelemetry::global::set_text_map_propagator(
//!     opentelemetry_sdk::propagation::TraceContextPropagator::new(),
//! );
//! irpc_opentelemetry::install().expect("no other propagator is installed");
//! ```
//!
//! Spans reach OpenTelemetry through the `tracing-opentelemetry` layer. This
//! crate uses the versions of `opentelemetry` and `tracing-opentelemetry` that
//! it re-exports. The application must use the same versions, because both
//! crates keep their state in statics.

use irpc::span_propagation::{Propagator, PropagatorAlreadySet, SpanContextCarrier};
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

/// Installs [`OtelPropagator`] as the span propagator of irpc.
///
/// # Errors
///
/// Returns [`PropagatorAlreadySet`] if a propagator is installed already.
pub fn install() -> Result<(), PropagatorAlreadySet> {
    irpc::span_propagation::set_propagator(OtelPropagator)
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
