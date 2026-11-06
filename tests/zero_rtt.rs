#![cfg(all(feature = "iroh", feature = "derive"))]
//! Checks that a client writes a request again when the server rejects 0-RTT.

use std::time::Duration;

use iroh::{
    Endpoint, EndpointAddr, RelayMode, SecretKey,
    endpoint::{Connection, presets},
    protocol::Router,
};
use irpc::{
    Client, WithChannels, channel::oneshot, iroh::Iroh0RttProtocol, rpc::MAX_MESSAGE_SIZE,
    rpc_requests,
};
use serde::{Deserialize, Serialize};
use testresult::TestResult;

const ALPN: &[u8] = b"irpc-test/zero-rtt/0";

#[rpc_requests(message = EchoMessage)]
#[derive(Debug, Serialize, Deserialize)]
enum EchoProtocol {
    #[rpc(tx = oneshot::Sender<Vec<u8>>)]
    #[wrap(Echo)]
    Echo(Vec<u8>),
}

fn spawn_echo() -> Client<EchoProtocol> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let EchoMessage::Echo(WithChannels { inner, tx, .. }) = msg;
            tx.send(inner.0).await.ok();
        }
    });
    Client::local(tx)
}

async fn endpoint(secret_key: SecretKey) -> TestResult<Endpoint> {
    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(secret_key)
        .relay_mode(RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr("127.0.0.1:0")?
        .bind()
        .await?;
    Ok(endpoint)
}

/// Starts a server that accepts 0-RTT, with fresh resumption state.
async fn server(secret_key: SecretKey) -> TestResult<(Router, EndpointAddr)> {
    let endpoint = endpoint(secret_key).await?;
    let addr = EndpointAddr::new(endpoint.id()).with_addrs(
        endpoint
            .bound_sockets()
            .into_iter()
            .map(iroh::TransportAddr::Ip),
    );
    let local = spawn_echo().as_local().expect("a local client");
    let router = Router::builder(endpoint)
        .accept(ALPN, Iroh0RttProtocol::new(local))
        .spawn();
    Ok((router, addr))
}

/// A server, and a client whose 0-RTT data the server rejects.
///
/// A server that restarted lost its resumption state, so it rejects the 0-RTT
/// data of a client that connected before.
struct Rejected {
    router: Router,
    // Keeps the connection of `api` alive.
    _client: Endpoint,
    api: Client<EchoProtocol>,
}

async fn rejected_zero_rtt_client() -> TestResult<Rejected> {
    let secret_key = SecretKey::generate();
    let client = endpoint(SecretKey::generate()).await?;

    // Connect once, so that the client gets a resumption ticket.
    let (router, addr) = server(secret_key.clone()).await?;
    let conn: Connection = client.connect(addr, ALPN).await?;
    let api = Client::<EchoProtocol>::boxed(conn.clone());
    assert_eq!(api.rpc(Echo(vec![1])).await?, vec![1]);
    tokio::time::sleep(Duration::from_millis(200)).await;
    conn.close(0u32.into(), b"");
    router.shutdown().await?;

    // The same server identity, but with fresh resumption state.
    let (router, addr) = server(secret_key).await?;
    let connecting = client
        .connect_with_opts(addr, ALPN, Default::default())
        .await?;
    let Ok(conn) = connecting.into_0rtt() else {
        panic!("the client has a resumption ticket, so it can try 0-RTT");
    };
    Ok(Rejected {
        router,
        _client: client,
        api: Client::boxed(conn),
    })
}

/// A small request is written before the rejection arrives.
#[tokio::test]
async fn small_request_is_written_again_after_zero_rtt_rejection() -> TestResult {
    let Rejected {
        router,
        _client,
        api,
    } = rejected_zero_rtt_client().await?;
    assert_eq!(api.rpc(Echo(vec![7])).await?, vec![7]);
    router.shutdown().await?;
    Ok(())
}

/// A large request is still writing when the rejection arrives, so the write fails with it.
#[tokio::test]
async fn large_request_is_written_again_after_zero_rtt_rejection() -> TestResult {
    let Rejected {
        router,
        _client,
        api,
    } = rejected_zero_rtt_client().await?;
    let large = vec![7u8; MAX_MESSAGE_SIZE as usize / 2];
    assert_eq!(api.rpc(Echo(large.clone())).await?, large);
    router.shutdown().await?;
    Ok(())
}
