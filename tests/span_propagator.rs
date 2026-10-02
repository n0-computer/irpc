//! Checks that a custom [`Propagator`] carries headers from client to server.
//!
//! Lives in its own test binary, because the propagator is global.

use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use irpc::{
    Client, WithChannels,
    channel::oneshot,
    noq::{listen, make_client_endpoint, make_server_endpoint},
    rpc::{Handler, RemoteService},
    rpc_requests,
    span_propagation::{Propagator, SpanContextCarrier, set_propagator},
};
use n0_future::task::AbortOnDropHandle;
use noq::Endpoint;
use serde::{Deserialize, Serialize};
use testresult::TestResult;

fn create_connected_endpoints() -> TestResult<(Endpoint, Endpoint, SocketAddr)> {
    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into();
    let (server, cert) = make_server_endpoint(addr)?;
    let client = make_client_endpoint(addr, &[cert.as_slice()])?;
    let port = server.local_addr()?.port();
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into();
    Ok((server, client, server_addr))
}

#[rpc_requests(message = EchoMessage, span_propagation)]
#[derive(Debug, Serialize, Deserialize)]
enum EchoProtocol {
    #[rpc(tx = oneshot::Sender<String>)]
    #[wrap(Echo)]
    Echo(String),
}

/// Writes a counter into the headers, and records what the server reads.
#[derive(Debug, Clone, Default)]
struct TestPropagator {
    sent: Arc<AtomicUsize>,
    received: Arc<Mutex<Vec<String>>>,
}

impl Propagator for TestPropagator {
    fn inject(&self, _span: &tracing::Span, carrier: &mut SpanContextCarrier) {
        let n = self.sent.fetch_add(1, Ordering::SeqCst);
        carrier.set("test-request", n.to_string());
    }

    fn set_parent(&self, _span: &tracing::Span, carrier: &SpanContextCarrier) {
        let value = carrier.get("test-request").unwrap_or_default().to_string();
        self.received.lock().expect("poisoned").push(value);
    }
}

#[tokio::test]
async fn propagator_carries_headers_to_the_server() -> TestResult<()> {
    let propagator = TestPropagator::default();
    set_propagator(propagator.clone())?;

    let (server, client, server_addr) = create_connected_endpoints()?;
    let handler: Handler<EchoProtocol> = Arc::new(|request, rx, tx| {
        Box::pin(async move {
            let EchoMessage::Echo(WithChannels { inner, tx, .. }) =
                request.with_remote_channels(rx, tx);
            tx.send(inner.0).await.ok();
            Ok(())
        })
    });
    let _server = AbortOnDropHandle::new(tokio::spawn(listen::<EchoProtocol>(server, handler)));
    let client = Client::<EchoProtocol>::noq(client, server_addr);
    for text in ["a", "b"] {
        assert_eq!(client.rpc(Echo(text.into())).await?, text);
    }
    let received = propagator.received.lock().expect("poisoned").clone();
    assert_eq!(received, ["0", "1"]);
    Ok(())
}
