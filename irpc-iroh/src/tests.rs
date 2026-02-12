#[cfg(feature = "use-tracing-opentelemetry")]
mod span_propagation {
    use std::sync::Arc;

    use iroh::{protocol::Router, Endpoint};
    use irpc::{channel::oneshot, rpc::RemoteService, rpc_requests, Service, WithChannels};
    use n0_error::{Result, StdResultExt};
    use serde::{Deserialize, Serialize};
    use tracing::{info, info_span, Instrument};
    use tracing_subscriber::{layer::SubscriberExt, EnvFilter, Layer, Registry};

    use crate::IrohProtocol;

    #[tokio::test]
    // #[n0_tracing::traced_test]
    async fn span_propagation() -> n0_error::Result<()> {
        // Create a tracing layer with the configured tracer
        let telemetry = tracing_opentelemetry::layer();
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
            let endpoint = Endpoint::bind().await?;
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
        let client_ep = Endpoint::bind().await?;
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

        Ok(())
    }
}
