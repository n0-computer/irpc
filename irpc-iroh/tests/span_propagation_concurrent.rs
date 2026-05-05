//! Stress test for span context propagation under concurrent load on a
//! multi-threaded runtime.
//!
//! Fires many in-flight requests so that handler futures are very likely to
//! yield and migrate worker threads between dispatch and span creation. With
//! the previous `thread_local!` storage this was racy — a yielding task could
//! lose its slot to another request landing on the same OS thread. The
//! `task_local!` scope is per-task and survives migration, so every server
//! "Get" span must share its client's trace id.
//!
//! Lives in its own integration-test binary because it installs a global
//! tracing subscriber and tracer provider, which would conflict with the
//! sibling unit test `tests::span_propagation::span_propagation`.

#![cfg(feature = "tracing-opentelemetry")]

use std::sync::Arc;

use iroh::{endpoint::presets, protocol::Router, Endpoint};
use irpc::{channel::oneshot, rpc::RemoteService, rpc_requests, WithChannels};
use irpc_iroh::IrohProtocol;
use n0_error::StdResultExt;
use opentelemetry::trace::TraceId;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use serde::{Deserialize, Serialize};
use tracing::{info_span, Instrument};
use tracing_subscriber::{layer::SubscriberExt, EnvFilter, Layer, Registry};

fn trace_id_for_req(spans: &[SpanData], name: &str, val: i64) -> Option<TraceId> {
    spans
        .iter()
        .find(|s| {
            s.name == name
                && s.attributes
                    .iter()
                    .any(|kv| kv.key.as_str() == "req_id" && kv.value == val.into())
        })
        .map(|s| s.span_context.trace_id())
}

fn server_span_for_trace(spans: &[SpanData], trace_id: TraceId) -> Option<&SpanData> {
    spans
        .iter()
        .find(|s| s.name == "Get" && s.span_context.trace_id() == trace_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn span_propagation_concurrent() -> n0_error::Result<()> {
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    opentelemetry::global::set_tracer_provider(provider.clone());
    let tracer = opentelemetry::global::tracer("test-concurrent");
    let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = Registry::default()
        .with(telemetry)
        .with(tracing_subscriber::fmt::layer().with_filter(EnvFilter::from_default_env()));
    tracing::subscriber::set_global_default(subscriber).expect("global already set");

    #[rpc_requests(message = Message, span_propagation)]
    #[derive(Debug, Serialize, Deserialize)]
    enum Proto {
        #[rpc(tx = oneshot::Sender<String>)]
        #[wrap(GetRequest)]
        Get(String),
    }

    const ALPN: &[u8] = b"test-concurrent";

    let endpoint = Endpoint::bind(presets::N0).await?;
    let protocol = IrohProtocol::<Proto>::new(Arc::new(|req, rx, tx| {
        Box::pin(async move {
            // Yield before constructing the WithChannels so the handler is
            // very likely to resume on a different worker thread.
            tokio::task::yield_now().await;
            let msg: Message = <Proto as RemoteService>::with_remote_channels(req, rx, tx);
            match msg {
                Message::Get(msg) => {
                    let WithChannels { inner, tx, .. } = msg;
                    tokio::task::yield_now().await;
                    tx.send(inner.0.to_uppercase()).await.ok();
                }
            }
            Ok(())
        })
    }));
    let server = Router::builder(endpoint).accept(ALPN, protocol).spawn();

    let client_ep = Endpoint::bind(presets::N0).await?;
    let server_addr = server.endpoint().addr();

    const N: i64 = 32;
    let mut handles = Vec::with_capacity(N as usize);
    for req_id in 0..N {
        let client = irpc_iroh::client::<Proto>(client_ep.clone(), server_addr.clone(), ALPN);
        let payload = format!("req-{req_id}");
        let expected = payload.to_uppercase();
        let h = tokio::spawn(
            async move {
                let res = client.rpc(GetRequest(payload)).await.unwrap();
                assert_eq!(res, expected);
            }
            .instrument(info_span!("req", req_id)),
        );
        handles.push(h);
    }
    for h in handles {
        h.await.unwrap();
    }

    server.shutdown().await.anyerr()?;
    client_ep.close().await;

    let _ = provider.force_flush();
    let spans = exporter.get_finished_spans().unwrap();

    for req_id in 0..N {
        let client_trace_id = trace_id_for_req(&spans, "req", req_id)
            .unwrap_or_else(|| panic!("no client span found with req_id={req_id}"));
        let server_span = server_span_for_trace(&spans, client_trace_id).unwrap_or_else(|| {
            panic!("no server 'Get' span with trace_id={client_trace_id} for req_id={req_id}")
        });
        assert!(
            server_span.parent_span_is_remote,
            "server span for req_id={req_id} should have a remote parent"
        );
    }

    Ok(())
}
