#![cfg(feature = "derive")]

use irpc::{
    WithChannels,
    channel::{none::NoSender, oneshot},
    rpc_requests,
};
use serde::{Deserialize, Serialize};

#[test]
fn derive_simple() {
    #![allow(dead_code)]

    #[derive(Debug, Serialize, Deserialize)]
    struct RpcRequest;

    #[derive(Debug, Serialize, Deserialize)]
    struct ServerStreamingRequest;

    #[derive(Debug, Serialize, Deserialize)]
    struct ClientStreamingRequest;

    #[derive(Debug, Serialize, Deserialize)]
    struct BidiStreamingRequest;

    #[derive(Debug, Serialize, Deserialize)]
    struct Update1;

    #[derive(Debug, Serialize, Deserialize)]
    struct Update2;

    #[derive(Debug, Serialize, Deserialize)]
    struct Response1;

    #[derive(Debug, Serialize, Deserialize)]
    struct Response2;

    #[derive(Debug, Serialize, Deserialize)]
    struct Response3;

    #[derive(Debug, Serialize, Deserialize)]
    struct Response4;

    #[rpc_requests(message = RequestWithChannels, no_rpc)]
    #[derive(Debug, Serialize, Deserialize)]
    enum Request {
        #[rpc(tx=oneshot::Sender<()>)]
        Rpc(RpcRequest),
        #[rpc(tx=NoSender)]
        ServerStreaming(ServerStreamingRequest),
        #[rpc(tx=NoSender)]
        BidiStreaming(BidiStreamingRequest),
        #[rpc(tx=NoSender)]
        ClientStreaming(ClientStreamingRequest),
    }
}

/// The message enum returns the span in which the message was created.
#[test]
fn message_span() {
    #[rpc_requests(message = PingMessage, no_rpc)]
    #[derive(Debug, Serialize, Deserialize)]
    enum PingProtocol {
        #[rpc(tx = oneshot::Sender<()>)]
        #[wrap(Ping)]
        Ping,
    }

    let subscriber = tracing_subscriber::registry();
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("caller");
        let (tx, _rx) = oneshot::channel();
        let msg = span.in_scope(|| PingMessage::from(WithChannels::from((Ping, tx))));
        assert_eq!(msg.span().id(), span.id());
    });
}

/// Use
///
/// TRYBUILD=overwrite cargo test --test smoke
///
/// to update the snapshots
#[test]
#[ignore = "stupid diffs depending on rustc version"]
fn compile_fail() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/*.rs");
}
