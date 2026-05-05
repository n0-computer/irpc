//! Demonstrates how to enable OpenTelemetry span context propagation across
//! irpc-iroh remote calls.
//!
//! Run with:
//!
//! ```sh
//! cargo run --example span_propagation -p irpc-iroh \
//!     --features tracing-opentelemetry
//! ```
//!
//! What this shows:
//!
//! 1. Setting up `tracing-opentelemetry` with a `TextMapPropagator` so that
//!    trace context can be injected into / extracted from RPC payloads.
//! 2. Declaring a service with `#[rpc_requests(..., span_propagation)]`. Only
//!    services that opt in pay the wire-format cost; everything else is
//!    unchanged.
//! 3. Entering the per-request span that `WithChannels` carries. The macro
//!    creates a span named after the protocol variant (here `"Get"`) and
//!    attaches the propagated remote context as its parent. Entering it makes
//!    any work the handler does — including child spans — part of the same
//!    distributed trace.

use std::sync::Arc;

use anyhow::Result;
use iroh::{endpoint::presets, protocol::Router, Endpoint};
use irpc::{channel::oneshot, rpc::RemoteService, rpc_requests, WithChannels};
use irpc_iroh::IrohProtocol;
use serde::{Deserialize, Serialize};
use tracing::{info, info_span, Instrument};
use tracing_subscriber::{layer::SubscriberExt, EnvFilter, Layer, Registry};

#[rpc_requests(message = Message, span_propagation)]
#[derive(Debug, Serialize, Deserialize)]
enum Proto {
    #[rpc(tx = oneshot::Sender<String>)]
    #[wrap(GetRequest)]
    Get(String),
}

const ALPN: &[u8] = b"irpc-iroh/span_propagation/1";

#[tokio::main]
async fn main() -> Result<()> {
    // (1) Install a W3C trace-context propagator. Without this, the carrier
    //     written by the client and read by the server has no fields to
    //     inject into / extract from.
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    // A no-op global tracer is enough for this example. In a real service
    // you'd wire this up to OTLP / Jaeger / etc.
    let tracer = opentelemetry::global::tracer("irpc-iroh-example");
    let env = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("span_propagation=info"));
    let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = Registry::default().with(telemetry).with(
        tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_filter(env),
    );
    tracing::subscriber::set_global_default(subscriber)?;

    // (2) Server: a handler that opts into span propagation via the
    //     `span_propagation` flag on `rpc_requests` (above).
    let endpoint = Endpoint::bind(presets::N0).await?;
    let protocol = IrohProtocol::<Proto>::new(Arc::new(|req, rx, tx| {
        Box::pin(async move {
            let msg: Message = <Proto as RemoteService>::with_remote_channels(req, rx, tx);
            match msg {
                Message::Get(msg) => {
                    // (3) `WithChannels` carries a `span` field. The derive macro
                    //     created it as `info_span!("Get")` and set its parent
                    //     from the propagated remote context. Entering it makes
                    //     the rest of the handler — and any child spans it
                    //     creates — part of the same distributed trace.
                    let WithChannels {
                        inner, tx, span, ..
                    } = msg;
                    let _guard = span.enter();
                    info!(?inner, "handling request");
                    tx.send(inner.0.to_uppercase()).await.ok();
                }
            }
            Ok(())
        })
    }));
    let server = Router::builder(endpoint).accept(ALPN, protocol).spawn();

    // Client: every request lives inside a `req` span.
    let client_ep = Endpoint::bind(presets::N0).await?;
    let server_addr = server.endpoint().addr();
    for req_id in 0..3 {
        let client = irpc_iroh::client::<Proto>(client_ep.clone(), server_addr.clone(), ALPN);
        let payload = format!("hello-{req_id}");
        async {
            info!(%payload, "sending request");
            let res = client.rpc(GetRequest(payload)).await?;
            info!(%res, "got response");
            anyhow::Ok(())
        }
        .instrument(info_span!("req", req_id))
        .await?;
    }

    server.shutdown().await?;
    client_ep.close().await;
    Ok(())
}
