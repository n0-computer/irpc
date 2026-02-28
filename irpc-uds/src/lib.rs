//! Unix domain socket transport for irpc.
//!
//! Uses QUIC over Unix datagram sockets with plaintext crypto.
//! This gives you multiplexed bidirectional streams with flow control,
//! without TLS overhead, over a local Unix socket.

use std::{io, path::Path, sync::Arc};

use quinn::{Endpoint, EndpointConfig};

mod plaintext;
mod socket;

pub use socket::UdsSocket;

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
pub fn server_endpoint(path: impl AsRef<Path>) -> io::Result<Endpoint> {
    let socket = UdsSocket::bind(path)?;
    let server_config = plaintext::server_config();
    let runtime = Arc::new(quinn::TokioRuntime);
    Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
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
pub async fn connect(
    server_path: impl AsRef<Path>,
) -> io::Result<quinn::Connection> {
    let (socket, server_addr) = UdsSocket::connect(server_path)?;
    let client_config = plaintext::client_config();
    let runtime = Arc::new(quinn::TokioRuntime);
    let endpoint = Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        None,
        Box::new(socket),
        runtime,
    )?;
    endpoint.set_default_client_config(client_config);
    let conn = endpoint
        .connect(server_addr, "localhost")
        .map_err(|e| io::Error::other(e))?
        .await
        .map_err(|e| io::Error::other(e))?;
    Ok(conn)
}

/// Create a client for the given service over a Unix socket.
///
/// ```ignore
/// let client: irpc::Client<FooProtocol> = irpc_uds::client("/tmp/my.sock").await?;
/// let value = client.rpc(GetRequest("key".into())).await?;
/// ```
pub async fn client<S: irpc::Service>(
    server_path: impl AsRef<Path>,
) -> io::Result<irpc::Client<S>> {
    let conn = connect(server_path).await?;
    Ok(irpc::Client::boxed(conn))
}
