//! Adds request types to a protocol without breaking old servers.
//!
//! `v2::Protocol` is `v1::Protocol` with one more request type, `Reverse`, at the
//! end. postcard encodes a request type as its index, so the requests of v1 keep
//! their encoding, and a v1 server reads them from a v2 client.
//!
//! The example runs a v1 server and a v2 server, and sends the same requests
//! from a v2 client to each. The v1 server cannot decode `Reverse`. irpc resets
//! the streams of that request, so the client gets an error for which
//! [`irpc::Error::is_invalid_request`] returns true. The v1 handler uses
//! [`Handler::skip_bad_requests`], so it reads the next request on the same
//! connection. Without it, the bad request closes the connection.

use anyhow::Result;
use iroh::{Endpoint, EndpointAddr, endpoint::presets, protocol::Router};
use irpc::{Client, WithChannels, iroh::IrohProtocol, rpc::Handler};

const ALPN: &[u8] = b"irpc-examples/versioning/1";

mod v1 {
    use irpc::{channel::oneshot, rpc_requests};
    use serde::{Deserialize, Serialize};

    #[rpc_requests(message = Message)]
    #[derive(Debug, Serialize, Deserialize)]
    pub enum Protocol {
        /// Sends the text back.
        #[rpc(tx = oneshot::Sender<String>)]
        #[wrap(Echo)]
        Echo(String),
    }
}

mod v2 {
    use irpc::{channel::oneshot, rpc_requests};
    use serde::{Deserialize, Serialize};

    #[rpc_requests(message = Message)]
    #[derive(Debug, Serialize, Deserialize)]
    pub enum Protocol {
        /// Sends the text back.
        #[rpc(tx = oneshot::Sender<String>)]
        #[wrap(Echo)]
        Echo(String),
        /// Sends the text back, reversed. New in v2, so it comes last.
        #[rpc(tx = oneshot::Sender<String>)]
        #[wrap(Reverse)]
        Reverse(String),
    }
}

/// Returns a handler that knows only v1.
fn v1_handler() -> Handler<v1::Protocol> {
    Handler::sequential(|msg| async move {
        match msg {
            v1::Message::Echo(WithChannels { inner, tx, .. }) => tx.send(inner.0).await.ok(),
        };
        Ok(())
    })
}

/// Returns a handler that knows v2.
fn v2_handler() -> Handler<v2::Protocol> {
    Handler::sequential(|msg| async move {
        match msg {
            v2::Message::Echo(WithChannels { inner, tx, .. }) => tx.send(inner.0).await.ok(),
            v2::Message::Reverse(WithChannels { inner, tx, .. }) => {
                tx.send(inner.0.chars().rev().collect()).await.ok()
            }
        };
        Ok(())
    })
}

/// Sends `Echo`, `Reverse`, and `Echo` again to `server` from a v2 client.
async fn run_client(name: &str, endpoint: &Endpoint, server: EndpointAddr) -> Result<()> {
    // The client has this one connection and does not connect again, so the
    // last request shows that the connection still works.
    let conn = endpoint.connect(server, ALPN).await?;
    let client = Client::<v2::Protocol>::boxed(conn);

    let echoed = client.rpc(v2::Echo("hello".into())).await?;
    println!("{name}: echo hello -> {echoed}");
    match client.rpc(v2::Reverse("hello".into())).await {
        Ok(reversed) => println!("{name}: reverse hello -> {reversed}"),
        Err(err) => println!(
            "{name}: reverse hello failed: {err:#} (invalid request: {})",
            err.is_invalid_request()
        ),
    }
    let echoed = client.rpc(v2::Echo("again".into())).await?;
    println!("{name}: echo again -> {echoed}");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let v1_router = Router::builder(Endpoint::bind(presets::N0).await?)
        // Skip the requests of newer versions, instead of closing the connection.
        .accept(
            ALPN,
            IrohProtocol::new(v1_handler().skip_bad_requests(true)),
        )
        .spawn();
    let v2_router = Router::builder(Endpoint::bind(presets::N0).await?)
        .accept(ALPN, IrohProtocol::new(v2_handler()))
        .spawn();

    let endpoint = Endpoint::bind(presets::N0).await?;
    run_client("v1 server", &endpoint, v1_router.endpoint().addr()).await?;
    run_client("v2 server", &endpoint, v2_router.endpoint().addr()).await?;

    endpoint.close().await;
    v1_router.shutdown().await?;
    v2_router.shutdown().await?;
    Ok(())
}
