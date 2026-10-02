//! Serves two protocols on one ALPN.
//!
//! `AppProtocol` has one variant for `PingProtocol` and one for
//! `EchoProtocol`. The server reads `AppProtocol` requests. A
//! [`Handler::raw`] looks at the variant, and [`Handler::call`] passes the
//! inner request to the handler of that protocol.
//!
//! The client cannot use `Client::rpc`, because `AppProtocol` has no message
//! enum. It writes each request with [`RemoteSender::write`]. A crate like
//! irpc-schema can generate this code.
//!
//! [`RemoteSender::write`]: irpc::rpc::RemoteSender::write

use anyhow::Result;
use iroh::{Endpoint, endpoint::presets, protocol::Router};
use irpc::{
    Client, Request, RpcMessage, Service, WithChannels, channel::oneshot, iroh::IrohProtocol,
    rpc::Handler, rpc_requests,
};
use n0_future::task::AbortOnDropHandle;
use serde::{Deserialize, Serialize};

const ALPN: &[u8] = b"irpc-examples/combined/0";

#[rpc_requests(message = PingMessage)]
#[derive(Debug, Serialize, Deserialize)]
enum PingProtocol {
    /// Sends an empty reply.
    #[rpc(tx = oneshot::Sender<()>)]
    #[wrap(Ping)]
    Ping,
}

#[rpc_requests(message = EchoMessage)]
#[derive(Debug, Serialize, Deserialize)]
enum EchoProtocol {
    /// Sends the argument back.
    #[rpc(tx = oneshot::Sender<String>)]
    #[wrap(Echo)]
    Echo(String),
}

/// Holds a request of `PingProtocol` or of `EchoProtocol`.
#[derive(Debug, Serialize, Deserialize)]
enum AppProtocol {
    Ping(PingProtocol),
    Echo(EchoProtocol),
}

impl Service for AppProtocol {
    // The server passes the inner requests to other handlers, so it needs no message enum.
    type Message = ();
}

/// Returns a handler that passes each inner request to `ping` or `echo`.
fn app_handler(ping: Handler<PingProtocol>, echo: Handler<EchoProtocol>) -> Handler<AppProtocol> {
    Handler::raw(move |request, rx, tx| {
        let (ping, echo) = (ping.clone(), echo.clone());
        async move {
            match request {
                AppProtocol::Ping(request) => ping.call(request, rx, tx).await,
                AppProtocol::Echo(request) => echo.call(request, rx, tx).await,
            }
        }
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // A function handles the `PingProtocol` requests.
    let ping = Handler::concurrent(16, |msg: PingMessage| async move {
        let PingMessage::Ping(WithChannels { tx, .. }) = msg;
        tx.send(()).await.ok();
        Ok(())
    });
    // An actor handles the `EchoProtocol` requests.
    let (echo_tx, echo_rx) = tokio::sync::mpsc::channel(16);
    let _echo_actor = AbortOnDropHandle::new(tokio::spawn(echo_actor(echo_rx)));
    let echo = Handler::from_sender(echo_tx);

    let router = Router::builder(Endpoint::bind(presets::N0).await?)
        .accept(ALPN, IrohProtocol::new(app_handler(ping, echo)))
        .spawn();

    let client_endpoint = Endpoint::bind(presets::N0).await?;
    let client =
        Client::<AppProtocol>::iroh(client_endpoint.clone(), router.endpoint().addr(), ALPN);
    call::<()>(&client, AppProtocol::Ping(PingProtocol::Ping(Ping))).await?;
    println!("ping -> pong");
    let echo = AppProtocol::Echo(EchoProtocol::Echo(Echo("hello".into())));
    let reply: String = call(&client, echo).await?;
    println!("echo hello -> {reply}");

    client_endpoint.close().await;
    router.shutdown().await?;
    Ok(())
}

/// Sends `request` and returns its response.
async fn call<T: RpcMessage>(
    client: &Client<AppProtocol>,
    request: AppProtocol,
) -> irpc::Result<T> {
    let Request::Remote(sender) = client.request().await? else {
        unreachable!("the client is remote");
    };
    let (_send, recv) = sender.write(request).await?;
    let response: oneshot::Receiver<T> = recv.into();
    Ok(response.await?)
}

/// Answers each `EchoProtocol` request with its argument.
async fn echo_actor(mut rx: tokio::sync::mpsc::Receiver<EchoMessage>) {
    while let Some(msg) = rx.recv().await {
        let EchoMessage::Echo(WithChannels { inner, tx, .. }) = msg;
        tx.send(inner.0).await.ok();
    }
}
