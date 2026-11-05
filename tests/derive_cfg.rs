#![cfg(feature = "derive")]
#![deny(private_interfaces)]
#![allow(dead_code)]

use irpc::{Service, WithChannels, rpc_requests};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct EnabledRequest;

#[derive(Debug, Serialize, Deserialize)]
struct ChoiceRequest;

#[rpc_requests(message = Message, alias = "Msg", rpc_feature = "rpc")]
#[derive(Debug, Serialize, Deserialize)]
#[non_exhaustive]
enum Protocol {
    #[rpc()]
    #[wrap(Ping)]
    Ping,
    #[cfg(any())]
    #[rpc(tx = UndefinedSender, rx = UndefinedReceiver)]
    Plain(UndefinedRequest),
    #[cfg(any())]
    #[rpc()]
    #[wrap(Wrapped)]
    Wrapped(UndefinedField),
    #[cfg_attr(all(), cfg(any()), doc = "disabled")]
    #[rpc()]
    #[wrap(Conditional)]
    Conditional(UndefinedField),
    #[cfg_attr(all(), cfg_attr(all(), cfg(any()), doc = "disabled"))]
    #[rpc()]
    #[wrap(Nested)]
    Nested(UndefinedField),
    #[cfg_attr(all(), cfg(all()), serde(rename = "enabled"))]
    #[cfg_attr(any(), cfg(any()))]
    #[rpc()]
    Enabled(EnabledRequest),
    #[cfg(feature = "stream")]
    #[rpc()]
    WithStream(ChoiceRequest),
    #[cfg(not(feature = "stream"))]
    #[rpc()]
    WithoutStream(ChoiceRequest),
}

// These names must be available when the corresponding variant is disabled.
struct Wrapped;
struct Conditional;
struct Nested;
type PlainMsg = ();
type WrappedMsg = ();
type ConditionalMsg = ();
type NestedMsg = ();
#[cfg(feature = "stream")]
type WithoutStreamMsg = ();
#[cfg(not(feature = "stream"))]
type WithStreamMsg = ();

// A condition on the protocol also applies to every generated item.
#[rpc_requests(message = DisabledMessage, alias = "Msg", rpc_feature = "rpc")]
#[cfg(any())]
#[derive(Debug, Serialize, Deserialize)]
enum DisabledProtocol {
    #[rpc()]
    #[wrap(DisabledRequest)]
    Request(UndefinedField),
}

struct DisabledProtocol;
struct DisabledMessage;
struct DisabledRequest;
type RequestMsg = ();

#[rpc_requests(message = EmptyMessage, rpc_feature = "rpc")]
#[derive(Debug, Serialize, Deserialize)]
enum EmptyProtocol {
    #[cfg(any())]
    #[rpc()]
    Disabled(UndefinedRequest),
}

#[test]
fn enabled_variants_have_channels_conversions_and_spans() {
    let protocol: Protocol = Ping.into();
    assert!(matches!(protocol, Protocol::Ping(_)));
    let _ = protocol.new_request_span(0);
    let msg: PingMsg = WithChannels::from((Ping,));
    let message: Message = msg.into();
    assert!(matches!(message, Message::Ping(_)));
    let _ = message.span();

    let protocol: Protocol = EnabledRequest.into();
    assert!(matches!(protocol, Protocol::Enabled(_)));
    let _ = protocol.new_request_span(1);
    let msg: EnabledMsg = WithChannels::from((EnabledRequest,));
    let message: Message = msg.into();
    assert!(matches!(message, Message::Enabled(_)));
    let _ = message.span();

    let protocol: Protocol = ChoiceRequest.into();
    #[cfg(feature = "stream")]
    assert!(matches!(protocol, Protocol::WithStream(_)));
    #[cfg(not(feature = "stream"))]
    assert!(matches!(protocol, Protocol::WithoutStream(_)));
    let _ = protocol.new_request_span(2);
    let message: Message = WithChannels::from((ChoiceRequest,)).into();
    #[cfg(feature = "stream")]
    assert!(matches!(message, Message::WithStream(_)));
    #[cfg(not(feature = "stream"))]
    assert!(matches!(message, Message::WithoutStream(_)));
    let _ = message.span();
}

#[cfg(feature = "rpc")]
#[test]
fn conditional_protocol_implements_remote_service() {
    fn assert_remote<S: irpc::rpc::RemoteService>() {}
    assert_remote::<Protocol>();
}
