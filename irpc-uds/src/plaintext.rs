//! Plaintext QUIC crypto for local IPC.
//!
//! Adapted from [quinn-plaintext](https://github.com/jeromegn/quinn-plaintext)
//! for iroh-quinn-proto 0.15.0.
//!
//! This provides no encryption — packets are sent in plaintext with a seahash
//! checksum for integrity. Only suitable for local Unix socket transport where
//! encryption is unnecessary.

use std::sync::Arc;

use bytes::BytesMut;
use quinn_proto::{
    crypto::{self, CryptoError, HeaderKey},
    transport_parameters::TransportParameters,
    ConnectError, ConnectionId, PathId, Side, TransportError, TransportErrorCode,
};
use tracing::trace;

/// Create a plaintext [`quinn::ServerConfig`] for use with Unix socket transport.
pub fn server_config() -> quinn_proto::ServerConfig {
    quinn_proto::ServerConfig::with_crypto(Arc::new(PlaintextServerConfig))
}

/// Create a plaintext [`quinn::ClientConfig`] for use with Unix socket transport.
pub fn client_config() -> quinn_proto::ClientConfig {
    quinn_proto::ClientConfig::new(Arc::new(PlaintextClientConfig))
}

pub struct PlaintextHeaderKey {
    side: Side,
}

impl PlaintextHeaderKey {
    fn new(side: Side) -> Self {
        Self { side }
    }
}

impl HeaderKey for PlaintextHeaderKey {
    fn decrypt(&self, _pn_offset: usize, _packet: &mut [u8]) {
        trace!(side = ?self.side, "HeaderKey::decrypt (no-op)");
    }

    fn encrypt(&self, _pn_offset: usize, _packet: &mut [u8]) {
        trace!(side = ?self.side, "HeaderKey::encrypt (no-op)");
    }

    fn sample_size(&self) -> usize {
        0
    }
}

pub struct PlaintextPacketKey;

impl PlaintextPacketKey {
    fn new(_side: Side) -> Self {
        Self
    }
}

impl crypto::PacketKey for PlaintextPacketKey {
    fn encrypt(&self, _path_id: PathId, _packet: u64, _buf: &mut [u8], _header_len: usize) {
        // No-op: UDS is reliable, no checksum needed
    }

    fn decrypt(
        &self,
        _path_id: PathId,
        _packet: u64,
        _header: &[u8],
        _payload: &mut BytesMut,
    ) -> Result<(), CryptoError> {
        // No-op: UDS is reliable, no checksum needed
        Ok(())
    }

    fn tag_len(&self) -> usize {
        0
    }

    fn confidentiality_limit(&self) -> u64 {
        u64::MAX
    }

    fn integrity_limit(&self) -> u64 {
        1 << 36
    }
}

#[derive(Default)]
pub struct PlaintextClientConfig;

impl crypto::ClientConfig for PlaintextClientConfig {
    fn start_session(
        self: Arc<Self>,
        _version: u32,
        _server_name: &str,
        params: &TransportParameters,
    ) -> Result<Box<dyn crypto::Session>, ConnectError> {
        Ok(Box::new(PlaintextSession::new(Side::Client, *params)))
    }
}

#[derive(Default)]
pub struct PlaintextServerConfig;

impl crypto::ServerConfig for PlaintextServerConfig {
    fn initial_keys(
        &self,
        _version: u32,
        _dst_cid: ConnectionId,
    ) -> Result<crypto::Keys, crypto::UnsupportedVersion> {
        Ok(crypto_keys(Side::Server))
    }

    fn retry_tag(&self, _version: u32, _orig_dst_cid: ConnectionId, _packet: &[u8]) -> [u8; 16] {
        [0u8; 16]
    }

    fn start_session(
        self: Arc<Self>,
        _version: u32,
        params: &TransportParameters,
    ) -> Box<dyn crypto::Session> {
        Box::new(PlaintextSession::new(Side::Server, *params))
    }
}

struct PlaintextSession {
    side: Side,
    params: TransportParameters,
    peer_params: Option<TransportParameters>,
    wrote_transport_params: bool,
    initial_keys: Option<crypto::Keys>,
    handshake_keys: Option<crypto::Keys>,
}

impl PlaintextSession {
    fn new(side: Side, params: TransportParameters) -> Self {
        Self {
            side,
            params,
            peer_params: None,
            wrote_transport_params: false,
            initial_keys: Some(crypto_keys(side)),
            handshake_keys: Some(crypto_keys(side)),
        }
    }
}

impl crypto::Session for PlaintextSession {
    fn initial_keys(&self, _dst_cid: ConnectionId, _side: Side) -> crypto::Keys {
        crypto_keys(self.side)
    }

    fn handshake_data(&self) -> Option<Box<dyn std::any::Any>> {
        self.peer_params
            .map(|tp| Box::new(tp) as Box<dyn std::any::Any>)
    }

    fn peer_identity(&self) -> Option<Box<dyn std::any::Any>> {
        None
    }

    fn early_crypto(&self) -> Option<(Box<dyn crypto::HeaderKey>, Box<dyn crypto::PacketKey>)> {
        None
    }

    fn early_data_accepted(&self) -> Option<bool> {
        Some(false)
    }

    fn is_handshaking(&self) -> bool {
        self.peer_params.is_none()
            || !self.wrote_transport_params
                && (self.initial_keys.is_some() || self.handshake_keys.is_some())
    }

    fn read_handshake(&mut self, mut buf: &[u8]) -> Result<bool, TransportError> {
        if self.peer_params.is_none() {
            self.peer_params =
                Some(TransportParameters::read(self.side, &mut buf).map_err(|e| {
                    TransportError::new(
                        TransportErrorCode::TRANSPORT_PARAMETER_ERROR,
                        format!("failed to read transport parameters: {e}"),
                    )
                })?);
        }
        Ok(true)
    }

    fn transport_parameters(&self) -> Result<Option<TransportParameters>, TransportError> {
        Ok(self.peer_params)
    }

    fn write_handshake(&mut self, buf: &mut Vec<u8>) -> Option<crypto::Keys> {
        if self.side.is_client() && !self.wrote_transport_params {
            self.params.write(buf);
            self.wrote_transport_params = true;
        }

        self.initial_keys.take().or_else(|| {
            self.handshake_keys.take().inspect(|_| {
                if self.side.is_server() && !self.wrote_transport_params {
                    self.params.write(buf);
                    self.wrote_transport_params = true;
                }
            })
        })
    }

    fn next_1rtt_keys(&mut self) -> Option<crypto::KeyPair<Box<dyn crypto::PacketKey>>> {
        Some(crypto_packet_keypair(self.side))
    }

    fn is_valid_retry(&self, _orig_dst_cid: ConnectionId, _header: &[u8], _payload: &[u8]) -> bool {
        true
    }

    fn export_keying_material(
        &self,
        _output: &mut [u8],
        _label: &[u8],
        _context: &[u8],
    ) -> Result<(), crypto::ExportKeyingMaterialError> {
        Ok(())
    }
}

fn crypto_keys(side: Side) -> crypto::Keys {
    crypto::Keys {
        header: crypto_header_keypair(side),
        packet: crypto_packet_keypair(side),
    }
}

fn crypto_header_keypair(side: Side) -> crypto::KeyPair<Box<dyn crypto::HeaderKey>> {
    crypto::KeyPair {
        local: Box::new(PlaintextHeaderKey::new(side)),
        remote: Box::new(PlaintextHeaderKey::new(side)),
    }
}

fn crypto_packet_keypair(side: Side) -> crypto::KeyPair<Box<dyn crypto::PacketKey>> {
    crypto::KeyPair {
        local: Box::new(PlaintextPacketKey::new(side)),
        remote: Box::new(PlaintextPacketKey::new(side)),
    }
}
