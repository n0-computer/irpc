//! Checks the parent of the server request span when OpenTelemetry does not record the client request span.
//!
//! The OpenTelemetry layer drops the client request span, and another layer
//! keeps the span enabled. The request span then has no OpenTelemetry context,
//! so the client sends the context of the span it is in, or an empty carrier.

use std::net::{Ipv4Addr, SocketAddrV4};

use irpc::{
    Client, WithChannels,
    channel::oneshot,
    noq::{listen, make_client_endpoint, make_server_endpoint},
    rpc::Handler,
    rpc_requests,
};
use n0_error::Result;
use n0_future::task::AbortOnDropHandle;
use opentelemetry::trace::{SpanId, TracerProvider};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use serde::{Deserialize, Serialize};
use tracing::{Instrument, Metadata, info_span};
use tracing_subscriber::{
    Layer, Registry,
    filter::dynamic_filter_fn,
    layer::{Context, SubscriberExt},
};

#[rpc_requests(message = EchoMessage, span_propagation)]
#[derive(Debug, Serialize, Deserialize)]
enum EchoProtocol {
    #[rpc(tx = oneshot::Sender<String>)]
    #[wrap(Echo)]
    Echo(String),
}

/// A client without a recorded span sends an empty carrier.
///
/// The server must not set the parent of the request span from it. With context
/// activation off, the current OpenTelemetry context is empty, and setting it as
/// parent would make the request span a new root.
#[tokio::test]
async fn request_without_client_span_keeps_local_parent() -> Result<()> {
    check_local_parent(true).await
}

#[tokio::test]
async fn request_without_client_span_keeps_local_parent_without_context_activation() -> Result<()> {
    check_local_parent(false).await
}

async fn check_local_parent(context_activation: bool) -> Result<()> {
    let spans = exported_spans(context_activation, false).await?;
    let handler_span = find(&spans, "server-handler");
    let request_span = find(&spans, "Echo");
    // The request span keeps its local parent: the empty carrier does not make it a new root.
    assert_ne!(request_span.parent_span_id, SpanId::INVALID);
    assert!(!request_span.parent_span_is_remote);
    // irpc runs the handler in the request span.
    assert_eq!(
        handler_span.parent_span_id,
        request_span.span_context.span_id()
    );
    Ok(())
}

/// A client in a caller span sends the context of the caller span.
#[tokio::test]
async fn request_in_caller_span_has_caller_as_parent() -> Result<()> {
    let spans = exported_spans(true, true).await?;
    let caller_span = find(&spans, "caller");
    let request_span = find(&spans, "Echo");
    assert!(request_span.parent_span_is_remote);
    assert_eq!(
        request_span.span_context.trace_id(),
        caller_span.span_context.trace_id()
    );
    assert_eq!(
        request_span.parent_span_id,
        caller_span.span_context.span_id()
    );
    Ok(())
}

/// Returns the first span named `name`.
fn find<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
    spans
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no span named {name}"))
}

/// Sends a request, in a span named `caller` if `in_caller`, and returns the spans that OpenTelemetry exported.
async fn exported_spans(context_activation: bool, in_caller: bool) -> Result<Vec<SpanData>> {
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    // The server creates its request span in the connection span `rpc`, the client outside.
    let drop_client_request_span =
        dynamic_filter_fn(|meta: &Metadata<'_>, cx: &Context<'_, Registry>| {
            meta.name() != "Echo" || cx.lookup_current().is_some_and(|s| s.name() == "rpc")
        });
    let subscriber = Registry::default()
        .with(
            tracing_opentelemetry::layer()
                .with_tracer(provider.tracer("test"))
                .with_context_activation(context_activation)
                .with_filter(drop_client_request_span),
        )
        .with(irpc_opentelemetry::layer())
        // This layer keeps the client request span enabled.
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::sink));
    // The test runtime has one thread, so the server task sees this subscriber too.
    let _guard = tracing::subscriber::set_default(subscriber);

    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into();
    let (server, cert) = make_server_endpoint(addr)?;
    let client = make_client_endpoint(addr, &[cert.as_slice()])?;
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server.local_addr()?.port()).into();

    // Spans are exported when they close, so the handler reports when its spans are closed.
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel();
    let handler = Handler::sequential(move |msg: EchoMessage| {
        let done_tx = done_tx.clone();
        async move {
            async move {
                let EchoMessage::Echo(WithChannels { inner, tx, .. }) = msg;
                tx.send(inner.0).await.ok();
            }
            .instrument(info_span!("server-handler"))
            .await;
            done_tx.send(()).ok();
            Ok(())
        }
    });
    let _server = AbortOnDropHandle::new(tokio::spawn(listen::<EchoProtocol>(server, handler)));
    let client = Client::<EchoProtocol>::noq(client, server_addr);
    // The client connects on its first request. The connection spans would keep a caller
    // span open, so the client connects outside of it.
    client.rpc(Echo("connect".into())).await?;
    done_rx.recv().await.expect("handler is alive");
    exporter.reset();

    let caller = if in_caller {
        info_span!("caller")
    } else {
        tracing::Span::none()
    };
    let res = client.rpc(Echo("a".into())).instrument(caller).await?;
    assert_eq!(res, "a");
    done_rx.recv().await.expect("handler is alive");

    let _ = provider.force_flush();
    Ok(exporter.get_finished_spans().expect("exporter is open"))
}
