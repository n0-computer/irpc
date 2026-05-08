#[cfg(feature = "tracing-opentelemetry")]
mod span_propagation {
    use std::sync::Arc;

    use iroh::{Endpoint, endpoint::presets, protocol::Router};
    use irpc::{Service, WithChannels, channel::oneshot, rpc::RemoteService, rpc_requests};
    use n0_error::{Result, StdResultExt};
    use opentelemetry::trace::TraceId;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
    use serde::{Deserialize, Serialize};
    use tracing::{Instrument, info, info_span};
    use tracing_subscriber::{EnvFilter, Layer, Registry, layer::SubscriberExt};

    use crate::IrohProtocol;

    /// Find the trace ID of the first span named `name` that has a `req_id` attribute equal to `val`.
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

    /// Find a server-side "Get" span that shares the given trace ID.
    fn server_span_for_trace(spans: &[SpanData], trace_id: TraceId) -> Option<&SpanData> {
        spans
            .iter()
            .find(|s| s.name == "Get" && s.span_context.trace_id() == trace_id)
    }

    #[tokio::test]
    async fn span_propagation() -> n0_error::Result<()> {
        // Register W3C TraceContext propagator so inject/extract actually work.
        opentelemetry::global::set_text_map_propagator(
            opentelemetry_sdk::propagation::TraceContextPropagator::new(),
        );

        // Use an in-memory exporter so we can inspect exported spans after the test.
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        opentelemetry::global::set_tracer_provider(provider.clone());
        let tracer = opentelemetry::global::tracer("test");

        // Create a tracing layer with the configured tracer
        let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);
        // Use the tracing subscriber `Registry`, or any other subscriber
        // that impls `LookupSpan`
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

        const ALPN: &[u8] = b"test";

        async fn listen() -> Result<Router> {
            let endpoint = Endpoint::bind(presets::N0).await?;
            let protocol = IrohProtocol::<Proto>::new(Arc::new(|req, rx, tx| {
                Box::pin(
                    async move {
                        info!("handle request {req:?}");
                        let msg: Message =
                            <Proto as RemoteService>::with_remote_channels(req, rx, tx);
                        info!("handle request {msg:?}");
                        match msg {
                            Message::Get(msg) => {
                                let WithChannels {
                                    inner, tx, span, ..
                                } = msg;
                                info!("handle request {inner:?} with span {span:?}");
                                let _guard = span.enter();
                                info!("handle request {inner:?}, entered span");
                                tx.send(inner.0.to_uppercase()).await.ok();
                            }
                        }
                        Ok(())
                    }
                    .instrument(info_span!("server-handler")),
                )
            }));
            let router = Router::builder(endpoint).accept(ALPN, protocol).spawn();
            info!("endpoint id: {}", router.endpoint().id());
            Ok(router)
        }

        info!(
            "span propagation enabled: {}",
            <Proto as Service>::SPAN_PROPAGATION
        );

        let server = listen().instrument(info_span!("server")).await?;
        let client_ep = Endpoint::bind(presets::N0).await?;
        let client = crate::client::<Proto>(client_ep.clone(), server.endpoint().addr(), ALPN);

        async {
            async {
                info!("send request: hello");
                let res = client.rpc(GetRequest("hello".to_string())).await?;
                info!("got response: {res:?}");
                assert_eq!(res, "HELLO");
                n0_error::Ok(())
            }
            .instrument(info_span!("req", req_id = 23))
            .await?;

            async {
                let client =
                    crate::client::<Proto>(client_ep.clone(), server.endpoint().addr(), ALPN);
                info!("send request: bye");
                let res = client.rpc(GetRequest("bye".to_string())).await?;
                info!("got response: {res:?}");
                assert_eq!(res, "BYE");
                n0_error::Ok(())
            }
            .instrument(info_span!("req", req_id = 24))
            .await?;

            n0_error::Ok(())
        }
        .instrument(info_span!("client"))
        .await?;

        server.shutdown().await.anyerr()?;
        client_ep.close().await;

        // Flush to ensure all spans are exported, then inspect them.
        let _ = provider.force_flush();
        let spans = exporter.get_finished_spans().unwrap();

        // For each request, verify the server-side "Get" span shares the same
        // trace_id as the client-side "req" span — proving context was propagated
        // across the remote connection.
        for req_id in [23, 24] {
            let client_trace_id = trace_id_for_req(&spans, "req", req_id)
                .unwrap_or_else(|| panic!("no client span found with req_id={req_id}"));

            let server_span = server_span_for_trace(&spans, client_trace_id).unwrap_or_else(|| {
                panic!("no server 'Get' span with trace_id={client_trace_id} for req_id={req_id}")
            });

            assert!(
                server_span.parent_span_is_remote,
                "server span for req_id={req_id} should have a remote parent"
            );

            info!(
                "req_id={}: trace_id={}, server parent_span_id={} (remote={})",
                req_id,
                client_trace_id,
                server_span.parent_span_id,
                server_span.parent_span_is_remote,
            );
        }

        Ok(())
    }
}
