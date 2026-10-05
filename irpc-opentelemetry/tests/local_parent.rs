//! Checks that a request without a client span keeps its local parent on the server.
//!
//! A client with the irpc layer but no active span sends an empty carrier. The
//! server must not set the parent of the request span from it. With context
//! activation off, the current OpenTelemetry context is empty, and setting it as
//! parent would make the request span a new root.

use std::{
    net::{Ipv4Addr, SocketAddrV4},
    sync::Arc,
};

use irpc::{
    Client, WithChannels,
    channel::oneshot,
    noq::{listen, make_client_endpoint, make_server_endpoint},
    rpc::{Handler, RemoteService},
    rpc_requests,
};
use n0_error::Result;
use n0_future::task::AbortOnDropHandle;
use opentelemetry::trace::TracerProvider;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use serde::{Deserialize, Serialize};
use tracing::{Instrument, info_span};
use tracing_subscriber::{Registry, layer::SubscriberExt};

#[rpc_requests(message = EchoMessage, span_propagation)]
#[derive(Debug, Serialize, Deserialize)]
enum EchoProtocol {
    #[rpc(tx = oneshot::Sender<String>)]
    #[wrap(Echo)]
    Echo(String),
}

#[tokio::test]
async fn request_without_client_span_keeps_local_parent() -> Result<()> {
    check_local_parent(true).await
}

#[tokio::test]
async fn request_without_client_span_keeps_local_parent_without_context_activation() -> Result<()> {
    check_local_parent(false).await
}

async fn check_local_parent(context_activation: bool) -> Result<()> {
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = Registry::default()
        .with(
            tracing_opentelemetry::layer()
                .with_tracer(provider.tracer("test"))
                .with_context_activation(context_activation),
        )
        .with(irpc_opentelemetry::layer());
    // The test runtime has one thread, so the server task sees this subscriber too.
    let _guard = tracing::subscriber::set_default(subscriber);

    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into();
    let (server, cert) = make_server_endpoint(addr)?;
    let client = make_client_endpoint(addr, &[cert.as_slice()])?;
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server.local_addr()?.port()).into();

    // Spans are exported when they close, so the handler reports when its spans are closed.
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel();
    let handler: Handler<EchoProtocol> = Arc::new(move |request, rx, tx| {
        let done_tx = done_tx.clone();
        Box::pin(async move {
            async move {
                let EchoMessage::Echo(WithChannels {
                    inner, tx, span, ..
                }) = request.with_remote_channels(rx, tx);
                let _guard = span.enter();
                tx.send(inner.0).await.ok();
            }
            .instrument(info_span!("server-handler"))
            .await;
            done_tx.send(()).ok();
            Ok(())
        })
    });
    let _server = AbortOnDropHandle::new(tokio::spawn(listen::<EchoProtocol>(server, handler)));
    let client = Client::<EchoProtocol>::noq(client, server_addr);
    // No span is active here, so the client sends an empty carrier.
    assert_eq!(client.rpc(Echo("a".into())).await?, "a");
    done_rx.recv().await.expect("handler is alive");

    let _ = provider.force_flush();
    let spans = exporter.get_finished_spans().expect("exporter is open");
    let find = |name: &str| {
        spans
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no span named {name}"))
    };
    let handler_span = find("server-handler");
    let request_span = find("Echo");
    assert_eq!(
        request_span.span_context.trace_id(),
        handler_span.span_context.trace_id()
    );
    assert_eq!(
        request_span.parent_span_id,
        handler_span.span_context.span_id()
    );
    assert!(!request_span.parent_span_is_remote);
    Ok(())
}
