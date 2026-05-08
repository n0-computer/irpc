//! Distributed span propagation across irpc-iroh, visualized in Jaeger.
//!
//! Start Jaeger (UI on 16686, OTLP HTTP on 4318):
//!
//! ```sh
//! docker run --rm -p 16686:16686 -p 4318:4318 jaegertracing/jaeger:latest
//! ```
//!
//! Server (prints its endpoint id):
//!
//! ```sh
//! cargo run -p irpc-iroh --features tracing-opentelemetry --example span_propagation -- server
//! ```
//!
//! Client:
//!
//! ```sh
//! cargo run -p irpc-iroh --features tracing-opentelemetry --example span_propagation -- client <ENDPOINT_ID>
//! ```
//!
//! Open <http://localhost:16686>, pick the `example-client` service, and each
//! `req` trace will include the server's child `Get` span.
//!
//! After opening a trace, click "Trace Logs" in the view selector at the top right
//! to open a log view that assembles logs from both client and server.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use clap::{Parser, Subcommand};
use iroh::{Endpoint, EndpointId, endpoint::presets, protocol::Router};
use irpc::{WithChannels, channel::oneshot, rpc::RemoteService, rpc_requests};
use irpc_iroh::IrohProtocol;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{Resource, trace::SdkTracerProvider};
use serde::{Deserialize, Serialize};
use tracing::{Instrument, info, info_span};
use tracing_subscriber::{EnvFilter, Layer, Registry, layer::SubscriberExt};

#[rpc_requests(message = Message, span_propagation)]
#[derive(Debug, Serialize, Deserialize)]
enum Proto {
    #[rpc(tx = oneshot::Sender<String>)]
    #[wrap(GetRequest)]
    Get(String),
}

const ALPN: &[u8] = b"irpc-iroh/span_propagation/1";
const DEFAULT_OTLP_ENDPOINT: &str = "http://localhost:4318/v1/traces";

#[derive(Parser, Debug)]
#[command(about = "Distributed span propagation demo over irpc-iroh")]
struct Cli {
    /// OTLP HTTP/protobuf endpoint to export spans to. Defaults to Jaeger's standard port.
    #[arg(long, global = true, default_value = DEFAULT_OTLP_ENDPOINT)]
    otlp: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the server. Prints its endpoint id; pass that to the client.
    Server,
    /// Send some requests to a server identified by `endpoint_id`.
    Client {
        endpoint_id: EndpointId,
        /// Number of requests to send.
        #[arg(long, default_value_t = 3)]
        count: u32,
    },
}

fn init_tracing(
    service_name: &'static str,
    endpoint_id: EndpointId,
    otlp_endpoint: &str,
) -> Result<SdkTracerProvider> {
    // (1) Install a W3C trace-context propagator. Without this, the
    //     `SpanContextCarrier` injected on the client and extracted on the
    //     server has no header keys to read or write, so trace ids never
    //     cross the wire.
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    // (2) Build the OTLP HTTP/protobuf exporter pointed at the collector
    //     (Jaeger's OTLP ingestor by default). The `reqwest-blocking-client`
    //     transport lets the SDK's batch processor send from its own OS
    //     thread without a tokio runtime in scope.
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(otlp_endpoint)
        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
        .build()?;

    // (3) Wire the exporter into a tracer provider. The `service.name`
    //     resource is what Jaeger lists in its service dropdown, so the
    //     client and server show up as separate boxes joined by trace id;
    //     `service.instance.id` carries the iroh endpoint id so multiple
    //     clients/servers on the same trace are distinguishable.
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            Resource::builder()
                .with_attribute(KeyValue::new("service.name", service_name))
                .with_attribute(KeyValue::new(
                    "service.instance.id",
                    endpoint_id.fmt_short().to_string(),
                ))
                .build(),
        )
        .build();
    opentelemetry::global::set_tracer_provider(provider.clone());

    // (4) Bridge `tracing` into OpenTelemetry. Every `tracing` span flows
    //     through the `tracing-opentelemetry` layer (which forwards to OTLP
    //     and is filtered to `info` so noisy debug spans don't ship to
    //     Jaeger), in parallel with a plain fmt layer that prints to stderr
    //     under `RUST_LOG`.
    let tracer = opentelemetry::global::tracer(service_name);
    let telemetry = tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_filter(EnvFilter::new("info"));
    let fmt_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("span_propagation=info"));
    let subscriber = Registry::default().with(telemetry).with(
        tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_filter(fmt_filter),
    );
    tracing::subscriber::set_global_default(subscriber)?;
    Ok(provider)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Server => server(&cli.otlp).await,
        Command::Client { endpoint_id, count } => client(&cli.otlp, endpoint_id, count).await,
    }
}

async fn server(otlp_endpoint: &str) -> Result<()> {
    // The protocol opts into span propagation via the `span_propagation` flag
    // on `rpc_requests` above.
    let endpoint = Endpoint::bind(presets::N0).await?;
    let provider = init_tracing("example-server", endpoint.id(), otlp_endpoint)?;
    let protocol = IrohProtocol::<Proto>::new(Arc::new(|req, rx, tx| {
        Box::pin(async move {
            let msg: Message = <Proto as RemoteService>::with_remote_channels(req, rx, tx);
            match msg {
                Message::Get(msg) => {
                    // `WithChannels` carries a `span` field that the derive
                    // macro builds as `info_span!("Get")` with its parent set
                    // from the propagated remote context, so the handler's
                    // work shows up under the originating client trace.
                    let WithChannels {
                        inner, tx, span, ..
                    } = msg;
                    // Hand the response off to a separate task to show that
                    // `.instrument(span)` keeps work hooked into the same
                    // trace even after the handler future returns.
                    tokio::spawn(
                        async move {
                            info!(?inner, "handling request");
                            // Simulate async work.
                            tokio::time::sleep(Duration::from_millis(rand::random_range(20..200)))
                                .await;
                            let res = inner.0.to_uppercase();
                            info!(?res, "generated response");
                            tx.send(res).await.ok();
                            info!("response sent");
                        }
                        .instrument(span),
                    );
                }
            }
            Ok(())
        })
    }));
    let router = Router::builder(endpoint).accept(ALPN, protocol).spawn();

    println!("server endpoint id: {}", router.endpoint().id());
    println!("run the client with:");
    println!(
        "    cargo run -p irpc-iroh --features tracing-opentelemetry --example span_propagation -- client {}",
        router.endpoint().id()
    );
    println!("press ctrl+c to stop");
    tokio::signal::ctrl_c().await?;

    router.shutdown().await?;
    let _ = provider.force_flush();
    let _ = provider.shutdown();
    Ok(())
}

async fn client(otlp_endpoint: &str, endpoint_id: EndpointId, count: u32) -> Result<()> {
    // Each request lives inside a `req` span; the `Get` span the server
    // produces will hang off that one in Jaeger.
    let client_ep = Endpoint::bind(presets::N0).await?;
    let provider = init_tracing("example-client", client_ep.id(), otlp_endpoint)?;
    for req_id in 0..count {
        let client = irpc_iroh::client::<Proto>(client_ep.clone(), endpoint_id, ALPN);
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

    client_ep.close().await;
    let _ = provider.force_flush();
    let _ = provider.shutdown();
    Ok(())
}
