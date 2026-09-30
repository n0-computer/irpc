//! RPC over [`noq`] connections, with dial by socket address.
use std::{io, sync::Arc};

use n0_error::e;
use n0_future::{future::Boxed as BoxFuture, task::JoinSet};
use noq::{ConnectionError, PathId};
use tracing::{Instrument, debug, error_span, trace, warn};

use crate::{
    RequestError, Service,
    channel::mpsc,
    rpc::{
        ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED, Handler, MAX_MESSAGE_SIZE, RemoteConnection,
        RemoteService,
    },
    util::AsyncReadVarintExt,
};

/// A connection to a remote service.
///
/// Initially this does just have the endpoint and the address. Once a
/// connection is established, it will be stored.
#[derive(Debug, Clone)]
pub(crate) struct NoqLazyRemoteConnection(Arc<NoqLazyRemoteConnectionInner>);

#[derive(Debug)]
struct NoqLazyRemoteConnectionInner {
    pub endpoint: noq::Endpoint,
    pub addr: std::net::SocketAddr,
    pub connection: tokio::sync::Mutex<Option<noq::Connection>>,
}

impl RemoteConnection for noq::Connection {
    fn clone_boxed(&self) -> Box<dyn RemoteConnection> {
        Box::new(self.clone())
    }

    fn open_bi(
        &self,
    ) -> BoxFuture<std::result::Result<(noq::SendStream, noq::RecvStream), RequestError>> {
        let conn = self.clone();
        Box::pin(async move {
            let pair = conn.open_bi().await?;
            Ok(pair)
        })
    }

    fn zero_rtt_rejected(&self) -> BoxFuture<bool> {
        Box::pin(async { false })
    }
}

impl NoqLazyRemoteConnection {
    pub fn new(endpoint: noq::Endpoint, addr: std::net::SocketAddr) -> Self {
        Self(Arc::new(NoqLazyRemoteConnectionInner {
            endpoint,
            addr,
            connection: Default::default(),
        }))
    }
}

impl RemoteConnection for NoqLazyRemoteConnection {
    fn clone_boxed(&self) -> Box<dyn RemoteConnection> {
        Box::new(self.clone())
    }

    fn open_bi(
        &self,
    ) -> BoxFuture<std::result::Result<(noq::SendStream, noq::RecvStream), RequestError>> {
        let this = self.0.clone();
        Box::pin(async move {
            let mut guard = this.connection.lock().await;
            let pair = match guard.as_mut() {
                Some(conn) => {
                    // try to reuse the connection
                    match conn.open_bi().await {
                        Ok(pair) => pair,
                        Err(_) => {
                            // try with a new connection, just once
                            *guard = None;
                            connect_and_open_bi(&this.endpoint, &this.addr, guard).await?
                        }
                    }
                }
                None => connect_and_open_bi(&this.endpoint, &this.addr, guard).await?,
            };
            Ok(pair)
        })
    }

    fn zero_rtt_rejected(&self) -> BoxFuture<bool> {
        Box::pin(async { false })
    }
}

async fn connect_and_open_bi(
    endpoint: &noq::Endpoint,
    addr: &std::net::SocketAddr,
    mut guard: tokio::sync::MutexGuard<'_, Option<noq::Connection>>,
) -> Result<(noq::SendStream, noq::RecvStream), RequestError> {
    let conn = endpoint.connect(*addr, "localhost")?.await?;
    let (send, recv) = conn.open_bi().await?;
    *guard = Some(conn);
    Ok((send, recv))
}

/// Utility function to listen for incoming connections and handle them with the provided handler.
///
/// The wire format used depends on `S::SPAN_PROPAGATION` - if true, span context is expected.
pub async fn listen<S: Service>(endpoint: noq::Endpoint, handler: Handler<S>) {
    let mut request_id = 0u64;
    let mut tasks = JoinSet::new();
    loop {
        let incoming = tokio::select! {
            Some(res) = tasks.join_next(), if !tasks.is_empty() => {
                res.expect("irpc connection task panicked");
                continue;
            }
            incoming = endpoint.accept() => {
                match incoming {
                    None => break,
                    Some(incoming) => incoming
                }
            }
        };
        let handler = handler.clone();
        let fut = async move {
            match incoming.await {
                Ok(connection) => match handle_connection(connection, handler).await {
                    Err(err) => warn!("connection closed with error: {err:?}"),
                    Ok(()) => debug!("connection closed"),
                },
                Err(cause) => {
                    warn!("failed to accept connection: {cause:?}");
                }
            };
        };
        let span = error_span!("rpc", id = request_id, remote = tracing::field::Empty);
        tasks.spawn(fut.instrument(span));
        request_id += 1;
    }
}

/// Handles a quic connection with the provided `handler`.
///
/// This function handles requests for a service `S`. The wire format used depends on
/// `S::SPAN_PROPAGATION` - if true, span context is expected in the wire format.
pub async fn handle_connection<S: Service>(
    connection: noq::Connection,
    handler: Handler<S>,
) -> io::Result<()> {
    let remote = connection
        .path(PathId::ZERO)
        .and_then(|p| p.remote_address().ok());
    if let Some(remote) = remote {
        tracing::Span::current().record("remote", tracing::field::display(remote));
    }
    debug!("connection accepted");
    loop {
        let Some((msg, carrier, rx, tx)) = read_request_inner::<S>(&connection).await? else {
            return Ok(());
        };
        crate::span_propagation::scope_remote(carrier, handler(msg, rx, tx)).await?;
    }
}

/// Reads a request from a connection and converts it to a message enum.
///
/// This combines `read_request_raw` with `RemoteService::with_remote_channels`.
pub async fn read_request<S: RemoteService>(
    connection: &noq::Connection,
) -> std::io::Result<Option<S::Message>> {
    let Some((msg, carrier, rx, tx)) = read_request_inner::<S>(connection).await? else {
        return Ok(None);
    };
    Ok(Some(
        crate::span_propagation::scope_remote(carrier, async move {
            S::with_remote_channels(msg, rx, tx)
        })
        .await,
    ))
}

/// Reads a single request from the connection.
///
/// This accepts a bi-directional stream from the connection and reads and parses the request.
///
/// When `S::SPAN_PROPAGATION` is true, any propagated span context on the wire is
/// silently dropped. Use [`handle_connection`] (or [`read_request`]) if you need
/// the propagated context to reach the generated handler spans.
///
/// Returns the parsed request and the stream pair if reading and parsing the request succeeded.
/// Returns None if the remote closed the connection with error code `0`.
/// Returns an error for all other failure cases.
pub async fn read_request_raw<S: Service>(
    connection: &noq::Connection,
) -> std::io::Result<Option<(S, noq::RecvStream, noq::SendStream)>> {
    Ok(read_request_inner::<S>(connection)
        .await?
        .map(|(msg, _carrier, rx, tx)| (msg, rx, tx)))
}

/// Internal: read a request and also return the propagated span context carrier.
///
/// The carrier is `Some` iff `S::SPAN_PROPAGATION` is true and the remote sent one.
async fn read_request_inner<S: Service>(
    connection: &noq::Connection,
) -> std::io::Result<
    Option<(
        S,
        Option<crate::span_propagation::SpanContextCarrier>,
        noq::RecvStream,
        noq::SendStream,
    )>,
> {
    let (send, mut recv) = match connection.accept_bi().await {
        Ok((s, r)) => (s, r),
        Err(ConnectionError::ApplicationClosed(cause)) if cause.error_code.into_inner() == 0 => {
            trace!("remote side closed connection {cause:?}");
            return Ok(None);
        }
        Err(cause) => {
            warn!("failed to accept bi stream {cause:?}");
            return Err(cause.into());
        }
    };
    let size = recv
        .read_varint_u64()
        .await?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "failed to read size"))?;
    if size > MAX_MESSAGE_SIZE {
        connection.close(
            ERROR_CODE_MAX_MESSAGE_SIZE_EXCEEDED.into(),
            b"request exceeded max message size",
        );
        return Err(e!(mpsc::RecvError::MaxMessageSizeExceeded).into());
    }
    let mut buf = vec![0; size as usize];
    recv.read_exact(&mut buf)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::UnexpectedEof, e))?;

    let (carrier, msg): (Option<crate::span_propagation::SpanContextCarrier>, S) =
        if S::SPAN_PROPAGATION {
            postcard::from_bytes(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        } else {
            let msg = postcard::from_bytes(&buf)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            (None, msg)
        };

    Ok(Some((msg, carrier, recv, send)))
}

#[cfg(feature = "noq_endpoint_setup")]
mod noq_setup_utils {
    use std::{sync::Arc, time::Duration};

    use n0_error::{Result, StdResultExt};
    use noq::{ClientConfig, ServerConfig, crypto::rustls::QuicClientConfig};

    /// Create a noq client config and trusts given certificates.
    ///
    /// ## Args
    ///
    /// - server_certs: a list of trusted certificates in DER format.
    pub fn configure_client(server_certs: &[&[u8]]) -> Result<ClientConfig> {
        let mut certs = rustls::RootCertStore::empty();
        for cert in server_certs {
            let cert = rustls::pki_types::CertificateDer::from(cert.to_vec());
            certs.add(cert).std_context("Error configuring certs")?;
        }

        let provider = rustls::crypto::ring::default_provider();
        let crypto_client_config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("valid versions")
            .with_root_certificates(certs)
            .with_no_client_auth();
        let quic_client_config =
            noq::crypto::rustls::QuicClientConfig::try_from(crypto_client_config)
                .std_context("Error creating QUIC client config")?;

        let mut transport_config = noq::TransportConfig::default();
        transport_config.keep_alive_interval(Some(Duration::from_secs(1)));
        let mut client_config = ClientConfig::new(Arc::new(quic_client_config));
        client_config.transport_config(Arc::new(transport_config));
        Ok(client_config)
    }

    /// Create a noq server config with a self-signed certificate
    ///
    /// Returns the server config and the certificate in DER format
    pub fn configure_server() -> Result<(ServerConfig, Vec<u8>)> {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .std_context("Error generating self-signed cert")?;
        let cert_der = cert.cert.der();
        let priv_key =
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
        let cert_chain = vec![cert_der.clone()];

        let mut server_config = ServerConfig::with_single_cert(cert_chain, priv_key.into())
            .std_context("Error creating server config")?;
        Arc::get_mut(&mut server_config.transport)
            .unwrap()
            .max_concurrent_uni_streams(0_u8.into());

        Ok((server_config, cert_der.to_vec()))
    }

    /// Create a noq client config and trust all certificates.
    pub fn configure_client_insecure() -> Result<ClientConfig> {
        let provider = rustls::crypto::ring::default_provider();
        let crypto = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(rustls::DEFAULT_VERSIONS)
            .expect("valid versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
            .with_no_client_auth();
        let client_cfg =
            QuicClientConfig::try_from(crypto).std_context("Error creating QUIC client config")?;
        let client_cfg = ClientConfig::new(Arc::new(client_cfg));
        Ok(client_cfg)
    }

    #[cfg(not(target_arch = "wasm32"))]
    mod non_wasm {
        use std::net::SocketAddr;

        use noq::Endpoint;

        use super::*;

        /// Constructs a QUIC endpoint configured for use a client only.
        ///
        /// ## Args
        ///
        /// - server_certs: list of trusted certificates.
        pub fn make_client_endpoint(
            bind_addr: SocketAddr,
            server_certs: &[&[u8]],
        ) -> Result<Endpoint> {
            let client_cfg = configure_client(server_certs)?;
            let endpoint = Endpoint::client(bind_addr)?;
            endpoint.set_default_client_config(client_cfg);
            Ok(endpoint)
        }

        /// Constructs a QUIC endpoint configured for use a client only that trusts all certificates.
        ///
        /// This is useful for testing and local connections, but should be used with care.
        pub fn make_insecure_client_endpoint(bind_addr: SocketAddr) -> Result<Endpoint> {
            let client_cfg = configure_client_insecure()?;
            let endpoint = Endpoint::client(bind_addr)?;
            endpoint.set_default_client_config(client_cfg);
            Ok(endpoint)
        }

        /// Constructs a QUIC server endpoint with a self-signed certificate
        ///
        /// Returns the server endpoint and the certificate in DER format
        pub fn make_server_endpoint(bind_addr: SocketAddr) -> Result<(Endpoint, Vec<u8>)> {
            let (server_config, server_cert) = configure_server()?;
            let endpoint = Endpoint::server(server_config, bind_addr)?;
            Ok((endpoint, server_cert))
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub use non_wasm::{make_client_endpoint, make_insecure_client_endpoint, make_server_endpoint};

    #[derive(Debug)]
    struct SkipServerVerification;

    impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            use rustls::SignatureScheme::*;
            // list them all, we don't care.
            vec![
                RSA_PKCS1_SHA1,
                ECDSA_SHA1_Legacy,
                RSA_PKCS1_SHA256,
                ECDSA_NISTP256_SHA256,
                RSA_PKCS1_SHA384,
                ECDSA_NISTP384_SHA384,
                RSA_PKCS1_SHA512,
                ECDSA_NISTP521_SHA512,
                RSA_PSS_SHA256,
                RSA_PSS_SHA384,
                RSA_PSS_SHA512,
                ED25519,
                ED448,
            ]
        }
    }
}
#[cfg(feature = "noq_endpoint_setup")]
pub use noq_setup_utils::*;
