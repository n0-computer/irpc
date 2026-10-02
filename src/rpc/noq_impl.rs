//! RPC over [`noq`] connections, with dial by socket address.
use std::sync::Arc;

use n0_future::{future::Boxed as BoxFuture, task::JoinSet};
use noq::{ConnectionError, PathId, RecvStream, SendStream, VarInt};
use tracing::{Instrument, debug, error_span, warn};

use crate::{
    RequestError, Service,
    rpc::{ConnectHook, Handler, IncomingRemoteConnection, RemoteConnection, handle_connection},
};

impl crate::sealed::Sealed for noq::Connection {}

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

/// A connection to a remote service.
///
/// Initially this does just have the endpoint and the address. Once a
/// connection is established, it will be stored.
#[derive(Debug, Clone)]
pub(crate) struct NoqLazyRemoteConnection(Arc<NoqLazyRemoteConnectionInner>);

#[derive(Debug)]
struct NoqLazyRemoteConnectionInner {
    endpoint: noq::Endpoint,
    addr: std::net::SocketAddr,
    connection: tokio::sync::Mutex<Option<noq::Connection>>,
    hook: Option<ConnectHook>,
}

impl NoqLazyRemoteConnection {
    pub fn new(endpoint: noq::Endpoint, addr: std::net::SocketAddr) -> Self {
        Self(Arc::new(NoqLazyRemoteConnectionInner {
            endpoint,
            addr,
            connection: Default::default(),
            hook: None,
        }))
    }
}

impl NoqLazyRemoteConnectionInner {
    /// Connects, and runs the hook on the new connection.
    async fn connect(&self) -> Result<noq::Connection, RequestError> {
        let conn = self.endpoint.connect(self.addr, "localhost")?.await?;
        if let Some(hook) = &self.hook
            && let Err(err) = hook.run(Box::new(conn.clone())).await
        {
            conn.close(0u32.into(), b"connect hook failed");
            return Err(err);
        }
        Ok(conn)
    }
}

impl crate::sealed::Sealed for NoqLazyRemoteConnection {}

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
            if let Some(conn) = guard.as_ref() {
                match conn.open_bi().await {
                    Ok(pair) => return Ok(pair),
                    // try with a new connection, just once
                    Err(_) => *guard = None,
                }
            }
            let conn = this.connect().await?;
            let pair = conn.open_bi().await?;
            *guard = Some(conn);
            Ok(pair)
        })
    }

    fn zero_rtt_rejected(&self) -> BoxFuture<bool> {
        Box::pin(async { false })
    }

    fn with_connect_hook(&self, hook: ConnectHook) -> Option<Box<dyn RemoteConnection>> {
        Some(Box::new(Self(Arc::new(NoqLazyRemoteConnectionInner {
            endpoint: self.0.endpoint.clone(),
            addr: self.0.addr,
            connection: Default::default(),
            hook: Some(hook),
        }))))
    }

    fn connect(&self) -> BoxFuture<std::result::Result<(), RequestError>> {
        let this = self.0.clone();
        Box::pin(async move {
            let mut guard = this.connection.lock().await;
            if guard
                .as_ref()
                .is_some_and(|conn| conn.close_reason().is_none())
            {
                return Ok(());
            }
            *guard = Some(this.connect().await?);
            Ok(())
        })
    }
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
                Ok(connection) => match handle_connection(&connection, handler).await {
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

impl IncomingRemoteConnection for noq::Connection {
    async fn accept_bi(&self) -> Result<(SendStream, RecvStream), ConnectionError> {
        self.accept_bi().await
    }

    fn close(&self, error_code: VarInt, reason: &[u8]) {
        self.close(error_code, reason)
    }

    fn remote_label(&self) -> Option<String> {
        let remote = self.path(PathId::ZERO)?.remote_address().ok()?;
        Some(remote.to_string())
    }
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
