//! Channels that abstract over local or remote sending

use std::io;

#[cfg(feature = "rpc")]
use n0_error::e;
use n0_error::stack_error;

#[cfg(feature = "rpc")]
use crate::rpc::{ConnectionClose, ErrorCode, RpcSendError};

pub mod mpsc;
pub mod none;
pub mod oneshot;

/// Error when sending a request, or a oneshot or mpsc message.
///
/// For a local channel, the only error is [`ReceiverClosed`]. A remote channel
/// can also fail for reasons that a local channel does not have, see
/// [`RpcSendError`](crate::rpc::RpcSendError).
///
/// For a remote channel, the receiver is on the other side of the connection:
/// the server for a request and its updates, and the client for a response.
///
/// After an error, the channel is closed. Later sends get the same error if
/// the peer ended the channel, and [`RpcSendError::Closed`] otherwise.
///
/// [`ReceiverClosed`]: Self::ReceiverClosed
/// [`RpcSendError::Closed`]: crate::rpc::RpcSendError::Closed
#[stack_error(derive, add_meta, from_sources)]
#[non_exhaustive]
pub enum SendError {
    /// The receiver has been closed.
    ///
    /// This is the only error that can occur for local communication. For
    /// remote communication, this means that the receiver stopped the stream
    /// with code 0, for example because it was dropped.
    #[error("Receiver closed")]
    ReceiverClosed,
    /// The remote channel failed for a reason that a local channel does not have.
    #[cfg(feature = "rpc")]
    #[error("Remote channel failed")]
    Rpc { source: RpcSendError },
}

impl From<SendError> for io::Error {
    fn from(e: SendError) -> Self {
        match e {
            SendError::ReceiverClosed { .. } => io::Error::new(io::ErrorKind::BrokenPipe, e),
            #[cfg(feature = "rpc")]
            SendError::Rpc { source, .. } => source.into(),
        }
    }
}

#[cfg(feature = "rpc")]
impl SendError {
    /// Returns the code that the peer sent, if any.
    ///
    /// See [`RpcSendError::remote_code`].
    pub fn remote_code(&self) -> Option<ErrorCode> {
        match self {
            Self::Rpc { source, .. } => source.remote_code(),
            _ => None,
        }
    }
}

/// Converts an error from writing the stream of a remote channel.
///
/// A stop with code 0 becomes [`SendError::ReceiverClosed`], a stop with any
/// other code [`RpcSendError::Remote`], the close of the connection
/// [`RpcSendError::ConnectionClosed`], and any other error [`RpcSendError::Io`].
#[cfg(feature = "rpc")]
impl From<noq::WriteError> for SendError {
    fn from(err: noq::WriteError) -> Self {
        match err {
            noq::WriteError::Stopped(code) => match ErrorCode::from_varint(code) {
                Some(code) => e!(RpcSendError::Remote { code }).into(),
                // A receiver that is dropped stops the stream with code 0.
                None => e!(SendError::ReceiverClosed),
            },
            noq::WriteError::ConnectionLost(ref cause) => {
                match ConnectionClose::from_error(cause) {
                    Some(close) => e!(RpcSendError::ConnectionClosed { close }).into(),
                    None => e!(RpcSendError::Io, err.into()).into(),
                }
            }
            err => e!(RpcSendError::Io, err.into()).into(),
        }
    }
}
