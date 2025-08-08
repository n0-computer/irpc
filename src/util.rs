//! Utilities
//!
//! This module contains utilities to read and write varints, as well as
//! functions to set up quinn endpoints for local rpc and testing.
#[cfg(feature = "quinn_endpoint_setup")]
#[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "quinn_endpoint_setup")))]
mod quinn_setup_utils {
    use std::sync::Arc;

    use anyhow::Result;
    use quinn::{crypto::rustls::QuicClientConfig, ClientConfig, ServerConfig};

    /// Create a quinn client config and trusts given certificates.
    ///
    /// ## Args
    ///
    /// - server_certs: a list of trusted certificates in DER format.
    pub fn configure_client(server_certs: &[&[u8]]) -> Result<ClientConfig> {
        let mut certs = rustls::RootCertStore::empty();
        for cert in server_certs {
            let cert = rustls::pki_types::CertificateDer::from(cert.to_vec());
            certs.add(cert)?;
        }

        let crypto_client_config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("valid versions")
        .with_root_certificates(certs)
        .with_no_client_auth();
        let quic_client_config =
            quinn::crypto::rustls::QuicClientConfig::try_from(crypto_client_config)?;

        Ok(ClientConfig::new(Arc::new(quic_client_config)))
    }

    /// Create a quinn server config with a self-signed certificate
    ///
    /// Returns the server config and the certificate in DER format
    pub fn configure_server() -> anyhow::Result<(ServerConfig, Vec<u8>)> {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
        let cert_der = cert.cert.der();
        let priv_key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
        let cert_chain = vec![cert_der.clone()];

        let mut server_config = ServerConfig::with_single_cert(cert_chain, priv_key.into())?;
        Arc::get_mut(&mut server_config.transport)
            .unwrap()
            .max_concurrent_uni_streams(0_u8.into());

        Ok((server_config, cert_der.to_vec()))
    }

    /// Create a quinn client config and trust all certificates.
    pub fn configure_client_insecure() -> Result<ClientConfig> {
        let crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
            .with_no_client_auth();
        let client_cfg = QuicClientConfig::try_from(crypto)?;
        let client_cfg = ClientConfig::new(Arc::new(client_cfg));
        Ok(client_cfg)
    }

    #[cfg(not(target_arch = "wasm32"))]
    mod non_wasm {
        use std::net::SocketAddr;

        use quinn::Endpoint;

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
            let mut endpoint = Endpoint::client(bind_addr)?;
            endpoint.set_default_client_config(client_cfg);
            Ok(endpoint)
        }

        /// Constructs a QUIC endpoint configured for use a client only that trusts all certificates.
        ///
        /// This is useful for testing and local connections, but should be used with care.
        pub fn make_insecure_client_endpoint(bind_addr: SocketAddr) -> Result<Endpoint> {
            let client_cfg = configure_client_insecure()?;
            let mut endpoint = Endpoint::client(bind_addr)?;
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
#[cfg(feature = "quinn_endpoint_setup")]
#[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "quinn_endpoint_setup")))]
pub use quinn_setup_utils::*;

#[cfg(feature = "rpc")]
mod varint_util {
    use std::{
        future::Future,
        io::{self, Error},
    };

    use serde::{de::DeserializeOwned, Serialize};
    use smallvec::SmallVec;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    /// Reads a u64 varint from an AsyncRead source, using the Postcard/LEB128 format.
    ///
    /// In Postcard's varint format (LEB128):
    /// - Each byte uses 7 bits for the value
    /// - The MSB (most significant bit) of each byte indicates if there are more bytes (1) or not (0)
    /// - Values are stored in little-endian order (least significant group first)
    ///
    /// Returns the decoded u64 value.
    pub async fn read_varint_u64<R>(reader: &mut R) -> io::Result<Option<u64>>
    where
        R: AsyncRead + Unpin,
    {
        let mut result: u64 = 0;
        let mut shift: u32 = 0;

        loop {
            // We can only shift up to 63 bits (for a u64)
            if shift >= 64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Varint is too large for u64",
                ));
            }

            // Read a single byte
            let res = reader.read_u8().await;
            if shift == 0 {
                if let Err(cause) = res {
                    if cause.kind() == io::ErrorKind::UnexpectedEof {
                        return Ok(None);
                    } else {
                        return Err(cause);
                    }
                }
            }

            let byte = res?;

            // Extract the 7 value bits (bits 0-6, excluding the MSB which is the continuation bit)
            let value = (byte & 0x7F) as u64;

            // Add the bits to our result at the current shift position
            result |= value << shift;

            // If the high bit is not set (0), this is the last byte
            if byte & 0x80 == 0 {
                break;
            }

            // Move to the next 7 bits
            shift += 7;
        }

        Ok(Some(result))
    }

    /// Writes a u64 varint to any object that implements the `std::io::Write` trait.
    ///
    /// This encodes the value using LEB128 encoding.
    ///
    /// # Arguments
    /// * `writer` - Any object implementing `std::io::Write`
    /// * `value` - The u64 value to encode as a varint
    ///
    /// # Returns
    /// The number of bytes written or an IO error
    pub fn write_varint_u64_sync<W: std::io::Write>(
        writer: &mut W,
        value: u64,
    ) -> std::io::Result<usize> {
        // Handle zero as a special case
        if value == 0 {
            writer.write_all(&[0])?;
            return Ok(1);
        }

        let mut bytes_written = 0;
        let mut remaining = value;

        while remaining > 0 {
            // Extract the 7 least significant bits
            let mut byte = (remaining & 0x7F) as u8;
            remaining >>= 7;

            // Set the continuation bit if there's more data
            if remaining > 0 {
                byte |= 0x80;
            }

            writer.write_all(&[byte])?;
            bytes_written += 1;
        }

        Ok(bytes_written)
    }

    pub fn write_length_prefixed<T: Serialize>(
        mut write: impl std::io::Write,
        value: T,
    ) -> io::Result<()> {
        let size = postcard::experimental::serialized_size(&value)
            .map_err(|e| Error::new(io::ErrorKind::InvalidData, e))? as u64;
        write_varint_u64_sync(&mut write, size)?;
        postcard::to_io(&value, &mut write)
            .map_err(|e| Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(())
    }

    /// Provides a fn to read a varint from an AsyncRead source.
    pub trait AsyncReadVarintExt: AsyncRead + Unpin {
        /// Reads a u64 varint from an AsyncRead source, using the Postcard/LEB128 format.
        ///
        /// If the stream is at the end, this returns `Ok(None)`.
        fn read_varint_u64(&mut self) -> impl Future<Output = io::Result<Option<u64>>>;

        fn read_length_prefixed<T: DeserializeOwned>(
            &mut self,
            max_size: usize,
        ) -> impl Future<Output = io::Result<T>>;
    }

    impl<T: AsyncRead + Unpin> AsyncReadVarintExt for T {
        fn read_varint_u64(&mut self) -> impl Future<Output = io::Result<Option<u64>>> {
            read_varint_u64(self)
        }

        async fn read_length_prefixed<I: DeserializeOwned>(
            &mut self,
            max_size: usize,
        ) -> io::Result<I> {
            let size = match self.read_varint_u64().await? {
                Some(size) => size,
                None => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF reached")),
            };

            if size > max_size as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Length-prefixed value too large",
                ));
            }

            let mut buf = vec![0; size as usize];
            self.read_exact(&mut buf).await?;
            postcard::from_bytes(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        }
    }

    /// Provides a fn to write a varint to an [`io::Write`] target, as well as a
    /// helper to write a length-prefixed value.
    pub trait WriteVarintExt: std::io::Write {
        /// Write a varint
        #[allow(dead_code)]
        fn write_varint_u64(&mut self, value: u64) -> io::Result<usize>;
        /// Write a value with a varint enoded length prefix.
        fn write_length_prefixed<T: Serialize>(&mut self, value: T) -> io::Result<()>;
    }

    impl<T: std::io::Write> WriteVarintExt for T {
        fn write_varint_u64(&mut self, value: u64) -> io::Result<usize> {
            write_varint_u64_sync(self, value)
        }

        fn write_length_prefixed<V: Serialize>(&mut self, value: V) -> io::Result<()> {
            write_length_prefixed(self, value)
        }
    }

    /// Provides a fn to write a varint to an [`io::Write`] target, as well as a
    /// helper to write a length-prefixed value.
    pub trait AsyncWriteVarintExt: AsyncWrite + Unpin {
        /// Write a varint
        fn write_varint_u64(&mut self, value: u64) -> impl Future<Output = io::Result<usize>>;
        /// Write a value with a varint enoded length prefix.
        fn write_length_prefixed<T: Serialize>(
            &mut self,
            value: T,
        ) -> impl Future<Output = io::Result<usize>>;
    }

    impl<T: AsyncWrite + Unpin> AsyncWriteVarintExt for T {
        async fn write_varint_u64(&mut self, value: u64) -> io::Result<usize> {
            let mut buf: SmallVec<[u8; 10]> = Default::default();
            write_varint_u64_sync(&mut buf, value).unwrap();
            self.write_all(&buf[..]).await?;
            Ok(buf.len())
        }

        async fn write_length_prefixed<V: Serialize>(&mut self, value: V) -> io::Result<usize> {
            let mut buf = Vec::new();
            write_length_prefixed(&mut buf, value)?;
            let size = buf.len();
            self.write_all(&buf).await?;
            Ok(size)
        }
    }
}
#[cfg(feature = "rpc")]
pub use varint_util::{AsyncReadVarintExt, AsyncWriteVarintExt, WriteVarintExt};

mod fuse_wrapper {
    use std::{
        future::Future,
        pin::Pin,
        result::Result,
        task::{Context, Poll},
    };

    pub struct FusedOneshotReceiver<T>(pub tokio::sync::oneshot::Receiver<T>);

    impl<T> Future for FusedOneshotReceiver<T> {
        type Output = Result<T, tokio::sync::oneshot::error::RecvError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            if self.0.is_terminated() {
                // don't panic when polling a terminated receiver
                Poll::Pending
            } else {
                Future::poll(Pin::new(&mut self.0), cx)
            }
        }
    }
}
pub(crate) use fuse_wrapper::FusedOneshotReceiver;

#[cfg(feature = "rpc")]
mod now_or_never {
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
    };

    // Simple pin_mut! macro implementation
    macro_rules! pin_mut {
    ($($x:ident),* $(,)?) => {
        $(
            let mut $x = $x;
            #[allow(unused_mut)]
            let mut $x = unsafe { Pin::new_unchecked(&mut $x) };
        )*
    }
}

    // Minimal implementation of a no-op waker
    fn noop_waker() -> Waker {
        fn noop(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            let vtable = &RawWakerVTable::new(clone, noop, noop, noop);
            RawWaker::new(std::ptr::null(), vtable)
        }

        unsafe { Waker::from_raw(clone(std::ptr::null())) }
    }

    /// Attempts to complete a future immediately, returning None if it would block
    pub(crate) fn now_or_never<F: Future>(future: F) -> Option<F::Output> {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        pin_mut!(future);

        match future.poll(&mut cx) {
            Poll::Ready(x) => Some(x),
            Poll::Pending => None,
        }
    }
}
#[cfg(feature = "rpc")]
pub(crate) use now_or_never::now_or_never;

mod stream_item {
    use std::{
        future::{Future, IntoFuture},
        io,
        marker::PhantomData,
    };

    use futures_util::future::BoxFuture;
    use n0_future::{stream, Stream, StreamExt};
    use serde::{Deserialize, Serialize};

    use crate::{
        channel::{mpsc, RecvError, SendError},
        RpcMessage,
    };

    pub type StreamSender<T, E> = mpsc::Sender<Item<T, E>>;
    pub type StreamReceiver<T, E> = mpsc::Receiver<Item<T, E>>;

    #[derive(thiserror::Error, Debug)]
    pub enum StreamError<E: std::error::Error> {
        #[error(transparent)]
        Transport(#[from] crate::Error),
        #[error(transparent)]
        Remote(E),
    }

    impl<E: std::error::Error> From<crate::channel::RecvError> for StreamError<E> {
        fn from(value: crate::channel::RecvError) -> Self {
            Self::Transport(value.into())
        }
    }

    pub type StreamResult<T, E> = std::result::Result<T, StreamError<E>>;

    pub struct Progress<
        T: RpcMessage,
        E: RpcMessage + std::error::Error,
        C: Extend<T> + Default = T,
    > {
        fut: BoxFuture<'static, crate::Result<mpsc::Receiver<Item<T, E>>>>,
        _collection_type: PhantomData<C>,
    }

    impl<T, E, C> Progress<T, E, C>
    where
        T: RpcMessage,
        E: RpcMessage + std::error::Error,
        C: Extend<T> + Default + Send,
    {
        pub fn new(
            fut: impl Future<Output = crate::Result<mpsc::Receiver<Item<T, E>>>> + Send + 'static,
        ) -> Self {
            Self {
                fut: Box::pin(fut),
                _collection_type: PhantomData,
            }
        }

        pub fn stream(self) -> impl Stream<Item = StreamResult<T, E>> {
            self.fut.into_stream()
        }
    }

    impl<T, E, C> IntoFuture for Progress<T, E, C>
    where
        T: RpcMessage,
        E: RpcMessage + std::error::Error,
        C: Default + Extend<T> + Send + 'static,
    {
        type Output = StreamResult<C, E>;
        type IntoFuture = BoxFuture<'static, Self::Output>;

        fn into_future(self) -> Self::IntoFuture {
            Box::pin(self.fut.try_collect())
        }
    }

    #[derive(Debug, Serialize, Deserialize, Clone)]
    pub enum Item<T, E> {
        Ok(T),
        Err(E),
        Done,
    }

    impl<T: RpcMessage, E: RpcMessage + std::error::Error> StreamItem for Item<T, E> {
        type Item = T;
        type Error = E;
        fn into_result_opt(self) -> Option<Result<Self::Item, Self::Error>> {
            match self {
                Item::Ok(item) => Some(Ok(item)),
                Item::Err(error) => Some(Err(error)),
                Item::Done => None,
            }
        }

        fn from_result(item: std::result::Result<Self::Item, Self::Error>) -> Self {
            match item {
                Ok(item) => Self::Ok(item),
                Err(err) => Self::Err(err),
            }
        }

        fn done() -> Self {
            Self::Done
        }
    }

    /// Trait for an enum that has three variants, item, error, and done.
    ///
    /// This is very common for irpc stream items if you want to provide an explicit
    /// end of stream marker to make sure unsuccessful termination is not mistaken
    /// for successful end of stream.
    pub trait StreamItem: crate::RpcMessage {
        /// The error case of the item enum.
        type Error: crate::RpcMessage + std::error::Error;
        /// The item case of the item enum.
        type Item: crate::RpcMessage;
        /// Converts the stream item into either None for end of stream, or a Result
        /// containing the item or an error. Error is assumed as a termination, so
        /// if you get error you won't get an additional end of stream marker.
        fn into_result_opt(self) -> Option<Result<Self::Item, Self::Error>>;
        /// Converts a result into the item enum.
        fn from_result(item: std::result::Result<Self::Item, Self::Error>) -> Self;
        /// Produces a done marker for the item enum.
        fn done() -> Self;
    }

    pub trait MpscSenderExt<T: StreamItem>: Sized {
        /// Forward a stream of items to the sender.
        ///
        /// This will convert items and errors into the item enum type, and add
        /// a done marker if the stream ends without an error.
        fn forward_stream(
            self,
            stream: impl Stream<Item = std::result::Result<T::Item, T::Error>>,
        ) -> impl Future<Output = std::result::Result<(), SendError>>;

        /// Forward an iterator of items to the sender.
        ///
        /// This will convert items and errors into the item enum type, and add
        /// a done marker if the iterator ends without an error.
        fn forward_iter(
            self,
            iter: impl Iterator<Item = std::result::Result<T::Item, T::Error>>,
        ) -> impl Future<Output = std::result::Result<(), SendError>>;
    }

    impl<T: StreamItem> MpscSenderExt<T> for mpsc::Sender<T> {
        async fn forward_stream(
            self,
            stream: impl Stream<Item = std::result::Result<T::Item, T::Error>>,
        ) -> std::result::Result<(), SendError> {
            tokio::pin!(stream);
            while let Some(item) = stream.next().await {
                let done = item.is_err();
                self.send(T::from_result(item)).await?;
                if done {
                    return Ok(());
                };
            }
            self.send(T::done()).await
        }

        async fn forward_iter(
            self,
            iter: impl Iterator<Item = std::result::Result<T::Item, T::Error>>,
        ) -> std::result::Result<(), SendError> {
            for item in iter {
                let done = item.is_err();
                self.send(T::from_result(item)).await?;
                if done {
                    return Ok(());
                };
            }
            self.send(T::done()).await
        }
    }

    pub trait IrpcReceiverFutExt<T: StreamItem> {
        /// Collects the receiver returned by this future into a collection,
        /// provided that we get a receiver and draining the receiver does not
        /// produce any error items.
        ///
        /// The collection must implement Default and Extend<T::Item>.
        /// Note that using this with a very large stream might use a lot of memory.
        fn try_collect<C, E>(self) -> impl Future<Output = std::result::Result<C, E>>
        where
            C: Default + Extend<T::Item>,
            E: From<StreamError<T::Error>>;

        /// Converts the receiver returned by this future into a stream of items,
        /// where each item is either a successful item or an error.
        ///
        /// There will be at most one error item, which will terminate the stream.
        /// If the future returns an error, the stream will yield that error as the
        /// first item and then terminate.
        fn into_stream<E>(self) -> impl Stream<Item = std::result::Result<T::Item, E>>
        where
            E: From<StreamError<T::Error>>;
    }

    impl<T, F> IrpcReceiverFutExt<T> for F
    where
        T: StreamItem,
        F: Future<Output = std::result::Result<mpsc::Receiver<T>, crate::Error>>,
    {
        async fn try_collect<C, E>(self) -> std::result::Result<C, E>
        where
            C: Default + Extend<T::Item>,
            E: From<StreamError<T::Error>>,
        {
            let mut items = C::default();
            let mut stream = self.into_stream::<E>();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(i) => items.extend(Some(i)),
                    Err(e) => return Err(e),
                }
            }
            Ok(items)
        }

        fn into_stream<E>(self) -> impl Stream<Item = std::result::Result<T::Item, E>>
        where
            E: From<StreamError<T::Error>>,
        {
            enum State<S, T> {
                Init(S),
                Receiving(mpsc::Receiver<T>),
                Done,
            }
            fn eof() -> RecvError {
                io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected end of stream").into()
            }
            async fn process_recv<S, T, E>(
                mut rx: mpsc::Receiver<T>,
            ) -> Option<(std::result::Result<T::Item, E>, State<S, T>)>
            where
                T: StreamItem,
                E: From<StreamError<T::Error>>,
            {
                match rx.recv().await {
                    Ok(Some(item)) => match item.into_result_opt()? {
                        Ok(i) => Some((Ok(i), State::Receiving(rx))),
                        Err(e) => Some((Err(E::from(StreamError::Remote(e))), State::Done)),
                    },
                    Ok(None) => Some((Err(E::from(StreamError::from(eof()))), State::Done)),
                    Err(e) => Some((Err(E::from(StreamError::from(e))), State::Done)),
                }
            }
            Box::pin(stream::unfold(State::Init(self), |state| async move {
                match state {
                    State::Init(fut) => match fut.await {
                        Ok(rx) => process_recv(rx).await,
                        Err(e) => Some((Err(E::from(StreamError::from(e))), State::Done)),
                    },
                    State::Receiving(rx) => process_recv(rx).await,
                    State::Done => None,
                }
            }))
        }
    }
}

#[cfg(all(feature = "derive", feature = "stream"))]
#[cfg_attr(quicrpc_docsrs, doc(cfg(all(feature = "derive", feature = "stream"))))]
pub use irpc_derive::StreamItem;
#[cfg(feature = "stream")]
#[cfg_attr(quicrpc_docsrs, doc(cfg(feature = "stream")))]
pub use stream_item::{
    IrpcReceiverFutExt, Item, MpscSenderExt, Progress, StreamItem, StreamReceiver, StreamSender,
};
