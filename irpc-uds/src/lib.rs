//! Unix domain socket transport for irpc.
//!
//! Uses QUIC over Unix datagram sockets with plaintext crypto.
//! This gives you multiplexed bidirectional streams with flow control,
//! without TLS overhead, over a local Unix socket.
//!
//! This crate only provides functionality on Unix platforms.

#[cfg(unix)]
use std::{io, path::Path, sync::Arc, time::Duration};

#[cfg(unix)]
use quinn::{Endpoint, EndpointConfig, TransportConfig, VarInt};

#[cfg(unix)]
mod plaintext;
#[cfg(unix)]
mod socket;

#[cfg(unix)]
pub use socket::UdsSocket;

/// Max UDP payload for Unix datagram sockets.
///
/// UDS can handle much larger datagrams than internet UDP.
/// Quinn hangs above ~12000 for unknown reasons, so we use 8K as a safe max.
#[cfg(unix)]
const UDS_MTU: u16 = 8_192;

#[cfg(unix)]
fn local_endpoint_config() -> EndpointConfig {
    let mut config = EndpointConfig::default();
    config.max_udp_payload_size(UDS_MTU).unwrap();
    config
}

/// Create a [`TransportConfig`] optimized for local Unix socket IPC.
///
/// Compared to defaults (tuned for 100ms RTT internet):
/// - MTU: 8192 (vs 1200) — UDS doesn't fragment
/// - Initial RTT: 1ms (vs 333ms) — local IPC is sub-microsecond
/// - Stream receive window: 16MB (vs ~1.25MB) — no bandwidth-delay product concern
/// - Receive window: max — no memory concern for local IPC
/// - No MTU discovery — not needed for UDS
/// - No keep-alive — local sockets don't have NAT timeouts
#[cfg(unix)]
fn local_transport_config() -> TransportConfig {
    let mut config = TransportConfig::default();
    config.initial_mtu(UDS_MTU);
    config.min_mtu(UDS_MTU);
    config.mtu_discovery_config(None);
    config.initial_rtt(Duration::from_millis(1));
    config.stream_receive_window(VarInt::from_u32(16 * 1024 * 1024));
    config.receive_window(VarInt::MAX);
    config.send_window(64 * 1024 * 1024);
    config.max_concurrent_bidi_streams(VarInt::from_u32(1024));
    config
}

/// Create a server [`Endpoint`] bound to the given Unix socket path.
///
/// The returned endpoint can accept incoming QUIC connections over the
/// Unix datagram socket. Use with standard irpc patterns:
///
/// ```ignore
/// let endpoint = irpc_uds::server_endpoint("/tmp/my.sock")?;
/// let handler = FooProtocol::remote_handler(local_sender);
/// irpc::rpc::listen(endpoint, handler).await;
/// ```
#[cfg(unix)]
pub fn server_endpoint(path: impl AsRef<Path>) -> io::Result<Endpoint> {
    let socket = UdsSocket::bind(path)?;
    let mut server_config = plaintext::server_config();
    server_config.transport_config(Arc::new(local_transport_config()));
    let runtime = Arc::new(quinn::TokioRuntime);
    Endpoint::new_with_abstract_socket(
        local_endpoint_config(),
        Some(server_config),
        Box::new(socket),
        runtime,
    )
}

/// Create a client [`quinn::Connection`] to a server at the given Unix socket path.
///
/// Returns a connection that can be used with `irpc::Client::boxed()`.
///
/// ```ignore
/// let conn = irpc_uds::connect("/tmp/my.sock").await?;
/// let client = irpc::Client::<FooProtocol>::boxed(conn);
/// let value = client.rpc(GetRequest("key".into())).await?;
/// ```
#[cfg(unix)]
pub async fn connect(server_path: impl AsRef<Path>) -> io::Result<quinn::Connection> {
    let (socket, server_addr) = UdsSocket::connect(server_path)?;
    let mut client_config = plaintext::client_config();
    client_config.transport_config(Arc::new(local_transport_config()));
    let runtime = Arc::new(quinn::TokioRuntime);
    let endpoint = Endpoint::new_with_abstract_socket(
        local_endpoint_config(),
        None,
        Box::new(socket),
        runtime,
    )?;
    endpoint.set_default_client_config(client_config);
    let conn = endpoint
        .connect(server_addr, "localhost")
        .map_err(io::Error::other)?
        .await
        .map_err(io::Error::other)?;
    Ok(conn)
}

/// Create a client for the given service over a Unix socket.
///
/// ```ignore
/// let client: irpc::Client<FooProtocol> = irpc_uds::client("/tmp/my.sock").await?;
/// let value = client.rpc(GetRequest("key".into())).await?;
/// ```
#[cfg(unix)]
pub async fn client<S: irpc::Service>(
    server_path: impl AsRef<Path>,
) -> io::Result<irpc::Client<S>> {
    let conn = connect(server_path).await?;
    Ok(irpc::Client::boxed(conn))
}
