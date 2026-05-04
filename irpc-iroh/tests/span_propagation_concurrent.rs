//! Demonstrates that the thread-local approach to span propagation breaks
//! when handlers yield before calling `with_remote_channels` and there are
//! concurrent connections.
//!
//! Triggers two failure modes the current `thread_local!`-based design has:
//!
//! 1. The handler yields (`tokio::task::yield_now().await`) before calling
//!    `with_remote_channels`. The thread-local was set in `read_request_raw`
//!    on whichever thread completed the read; after a yield the task can
//!    resume on a different thread, so the value isn't there.
//! 2. While the first handler is yielded, another connection's
//!    `handle_connection` task can run on the same OS thread and overwrite
//!    the thread-local with its own context. The first handler then reads
//!    the wrong value (or `None`).
//!
//! With a task-local (or any per-task scoped storage) neither of these
//! manifests.

#![cfg(feature = "tracing-opentelemetry")]

use std::sync::Arc;

use iroh::{endpoint::presets, protocol::Router, Endpoint};
use irpc::{channel::oneshot, rpc::RemoteService, rpc_requests, WithChannels};
use n0_error::Result;
use opentelemetry::trace::TraceId;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use serde::{Deserialize, Serialize};
use tracing::{info_span, Instrument};
use tracing_subscriber::{layer::SubscriberExt, EnvFilter, Layer, Registry};

use irpc_iroh::IrohProtocol;

fn trace_id_for_req(spans: &[SpanData], req_id: i64) -> Option<TraceId> {
    spans
        .iter()
        .find(|s| {
            s.name == "req"
                && s.attributes
                    .iter()
                    .any(|kv| kv.key.as_str() == "req_id" && kv.value == req_id.into())
        })
        .map(|s| s.span_context.trace_id())
}

fn server_span_for_trace(spans: &[SpanData], trace_id: TraceId) -> Option<&SpanData> {
    spans
        .iter()
        .find(|s| s.name == "Get" && s.span_context.trace_id() == trace_id)
}

#[rpc_requests(message = Message, span_propagation)]
#[derive(Debug, Serialize, Deserialize)]
enum Proto {
    #[rpc(tx = oneshot::Sender<String>)]
    #[wrap(GetRequest)]
    Get(String),
}

const ALPN: &[u8] = b"test-concurrent-span-propagation";
const N: i64 = 8;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_span_propagation() -> Result<()> {
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    opentelemetry::global::set_tracer_provider(provider.clone());
    let tracer = opentelemetry::global::tracer("test");

    let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = Registry::default()
        .with(telemetry)
        .with(tracing_subscriber::fmt::layer().with_filter(EnvFilter::from_default_env()));
    tracing::subscriber::set_global_default(subscriber).expect("global already set");

    // Server with a handler that yields BEFORE calling `with_remote_channels`.
    // This is the key bit: the thread-local set by `read_request_raw` may be
    // gone (or wrong) by the time we read it.
    let server_endpoint = Endpoint::bind(presets::N0).await?;
    let server_addr = server_endpoint.addr();
    let protocol = IrohProtocol::<Proto>::new(Arc::new(|req, rx, tx| {
        Box::pin(async move {
            tokio::task::yield_now().await;

            let msg: Message = <Proto as RemoteService>::with_remote_channels(req, rx, tx);
            match msg {
                Message::Get(msg) => {
                    let WithChannels {
                        inner, tx, span, ..
                    } = msg;
                    let _guard = span.enter();
                    tx.send(inner.0.to_uppercase()).await.ok();
                }
            }
            Ok(())
        })
    }));
    let server = Router::builder(server_endpoint).accept(ALPN, protocol).spawn();

    // Spawn N concurrent clients, each on its own iroh endpoint (so each
    // gets its own server-side connection / `handle_connection` task).
    let mut tasks = Vec::new();
    for i in 0..N {
        let server_addr = server_addr.clone();
        let task = tokio::spawn(
            async move {
                let client_ep = Endpoint::bind(presets::N0).await.unwrap();
                let client = irpc_iroh::client::<Proto>(client_ep.clone(), server_addr, ALPN);
                let res = client
                    .rpc(GetRequest(format!("hello{i}")))
                    .await
                    .unwrap();
                assert_eq!(res, format!("HELLO{i}"));
                client_ep.close().await;
            }
            .instrument(info_span!("req", req_id = i)),
        );
        tasks.push(task);
    }

    for task in tasks {
        task.await.unwrap();
    }

    server.shutdown().await.unwrap();

    let _ = provider.force_flush();
    let spans = exporter.get_finished_spans().unwrap();

    let mut broken = Vec::new();
    for req_id in 0..N {
        let client_trace_id = trace_id_for_req(&spans, req_id)
            .unwrap_or_else(|| panic!("no client 'req' span for req_id={req_id}"));

        if server_span_for_trace(&spans, client_trace_id).is_none() {
            broken.push(req_id);
        }
    }

    assert!(
        broken.is_empty(),
        "Expected every server 'Get' span to share its client request's trace_id. \
         req_ids with broken propagation: {broken:?}. \
         This is the symptom of the thread-local design: when handlers yield \
         before `with_remote_channels`, concurrent connections clobber each \
         other's stored context."
    );

    Ok(())
}
