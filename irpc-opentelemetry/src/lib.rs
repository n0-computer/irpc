//! OpenTelemetry span propagation for irpc.
//!
//! irpc carries the span context of a request in text headers, but does not
//! know any tracing backend. This crate connects it to OpenTelemetry: the
//! client writes the context of its request span with the global text map
//! propagator of `opentelemetry`, and the server sets the parent of its request
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
//! Spans reach OpenTelemetry through the `tracing-opentelemetry` layer.
//!
//! The application must use semver-compatible versions of `irpc`,
//! `opentelemetry`, and `tracing-opentelemetry` with this crate. With a second
//! copy of one of them, everything compiles, but no span context reaches the
//! remote: `opentelemetry` keeps the registered propagator in a static, and
//! this crate and irpc find their layers in the subscriber by type, so they
//! do not see the static or the layer types of another copy.
//!
//! # Filtering
//!
//! irpc creates its request spans at debug level, so that a console filter at
//! info level does not prefix every log line with them. The macro-generated spans
//! have the target of the module that defines the protocol.
//!
//! The server sets the parent of its request span from the context that the
//! client sent. It can only do that if the `tracing-opentelemetry` layer records
//! the request span. If a filter drops the span, nothing fails: the server starts
//! a new trace instead of joining the trace of the client. So give the
//! `tracing-opentelemetry` layer its own filter that lets the request spans
//! through, and keep the console filter at info:
//!
//! ```no_run
//! # let tracer = opentelemetry::global::tracer("app");
//! use tracing_subscriber::{EnvFilter, Layer, Registry, layer::SubscriberExt};
//!
//! let subscriber = Registry::default()
//!     .with(
//!         tracing_opentelemetry::layer()
//!             .with_tracer(tracer)
//!             .with_filter(EnvFilter::new("info,my_protocol=debug")),
//!     )
//!     .with(irpc_opentelemetry::layer())
//!     .with(tracing_subscriber::fmt::layer().with_filter(EnvFilter::new("info")));
//! ```
//!
//! Replace `my_protocol` with the module path of the protocol. The directive
//! also sends the debug events of that module to OpenTelemetry, so a protocol in a
//! module of its own keeps the traces small. irpc's own debug events have the
//! target `irpc` and stay out.
//!
//! A single filter for the whole subscriber, such as
//! `tracing_subscriber::fmt().with_env_filter("info")`, cannot do both: at info
//! level it drops the request spans, and at debug level the console shows them.
//!
//! The client does not need its request span recorded. If the
//! `tracing-opentelemetry` layer drops it, the client sends the context of the
//! span that it is in.

use irpc::span_propagation::{Propagator, SpanContextCarrier};
use opentelemetry::{
    propagation::{Extractor, Injector},
    trace::TraceContextExt,
};
use tracing::Subscriber;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::{Layer, registry::LookupSpan};

/// A [`Propagator`] that uses the global text map propagator of `opentelemetry`.
///
/// Create one with [`Default::default`].
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
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
pub fn layer<S>() -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    irpc::span_propagation::layer(OtelPropagator)
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
