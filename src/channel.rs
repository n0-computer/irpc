//! Channels that abstract over local or remote sending

use std::io;

#[cfg(feature = "rpc")]
use n0_error::e;
use n0_error::stack_error;

#[cfg(feature = "rpc")]
use crate::rpc::{ConnectionClose, RemoteCode};

pub mod mpsc;
pub mod none;
pub mod oneshot;

/// Error when sending a oneshot or mpsc message.
///
/// For a local channel, the only error is [`ReceiverClosed`].
///
/// For a remote channel, the receiver is on the other side of the connection:
/// the server for the updates of a request, and the client for a response. A
/// send fails for one of two reasons:
///
/// * This sender found the message bad: it is too large
///   ([`MaxSizeExceeded`]), or it does not encode (`EncodeFailed`).
/// * Something other than this message ended the channel:
///   * The receiver closed it ([`ReceiverClosed`]).
///   * The receiver stopped the stream with a code ([`Remote`]).
///     For example, the server rejected the request, or the receiver could not
///     decode an earlier message.
///   * The connection was closed on purpose ([`ConnectionClosed`]).
///   * The connection failed ([`Io`]).
///
/// If the server rejects the request and also closes the connection, the close
/// usually arrives first, and the sender gets [`ConnectionClosed`] instead of [`Remote`].
/// [`Self::remote_code`] returns the code in both cases.
///
/// After an error, the channel is closed. Later sends get the same error if
/// the peer ended the channel, and [`Closed`] otherwise.
///
/// [`ReceiverClosed`]: Self::ReceiverClosed
/// [`MaxSizeExceeded`]: Self::MaxSizeExceeded
/// [`Remote`]: Self::Remote
/// [`ConnectionClosed`]: Self::ConnectionClosed
/// [`Closed`]: Self::Closed
/// [`Io`]: Self::Io
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
    /// The receiver stopped the stream with a code, and the channel is closed.
    ///
    /// The `code` says why, see [`RemoteCode`], and the [`rpc`] module for an overview.
    ///
    /// [`RemoteCode`]: crate::rpc::RemoteCode
    /// [`rpc`]: crate::rpc
    #[cfg(feature = "rpc")]
    #[error("Receiver closed the channel: {code}")]
    Remote { code: crate::rpc::RemoteCode },
    /// This message is larger than [`MAX_MESSAGE_SIZE`], so this sender closed the channel.
    ///
    /// [`MAX_MESSAGE_SIZE`]: crate::rpc::MAX_MESSAGE_SIZE
    #[error("Message too large")]
    MaxSizeExceeded,
    /// This message does not encode, so this sender closed the channel.
    #[cfg(feature = "rpc")]
    #[error("Message does not encode")]
    EncodeFailed {
        #[error(std_err)]
        source: postcard::Error,
    },
    /// The connection was closed on purpose, by the peer or by this side.
    #[cfg(feature = "rpc")]
    #[error("Connection {close}")]
    ConnectionClosed { close: crate::rpc::ConnectionClose },
    /// This sender closed the channel after an earlier send failed or was cancelled.
    ///
    /// An earlier send on this sender or a clone of it found its message bad,
    /// failed with an io error, or was dropped before it was done. The stream
    /// can hold part of a message, so this sender sends no more messages.
    #[cfg(feature = "rpc")]
    #[error("Sender closed after an earlier error")]
    Closed,
    /// An io error, for example because the connection was lost.
    #[error("Io error")]
    Io {
        #[error(std_err)]
        source: io::Error,
    },
}

impl From<SendError> for io::Error {
    fn from(e: SendError) -> Self {
        match e {
            SendError::ReceiverClosed { .. } => io::Error::new(io::ErrorKind::BrokenPipe, e),
            SendError::Io { source, .. } => source,
            #[cfg(feature = "rpc")]
            SendError::Remote { .. } => io::Error::new(io::ErrorKind::ConnectionReset, e),
            #[cfg(feature = "rpc")]
            SendError::ConnectionClosed { .. } => io::Error::new(io::ErrorKind::NotConnected, e),
            #[cfg(feature = "rpc")]
            SendError::Closed { .. } => io::Error::new(io::ErrorKind::BrokenPipe, e),
            _ => io::Error::new(io::ErrorKind::InvalidData, e),
        }
    }
}

#[cfg(feature = "rpc")]
impl SendError {
    /// Returns the code that the peer sent, if any.
    ///
    /// This is the code of a [`Remote`](Self::Remote) error, or the code that
    /// the peer closed the connection with.
    pub fn remote_code(&self) -> Option<RemoteCode> {
        match self {
            Self::Remote { code, .. } => Some(*code),
            Self::ConnectionClosed { close, .. } => close.code(),
            _ => None,
        }
    }
}

#[cfg(feature = "rpc")]
impl From<noq::WriteError> for SendError {
    fn from(err: noq::WriteError) -> Self {
        match err {
            // A receiver that is dropped stops the stream with code 0.
            noq::WriteError::Stopped(code) => match RemoteCode::from_code(code.into_inner()) {
                Some(code) => e!(SendError::Remote { code }),
                None => e!(SendError::ReceiverClosed),
            },
            noq::WriteError::ConnectionLost(ref cause) => {
                match ConnectionClose::from_error(cause) {
                    Some(close) => e!(SendError::ConnectionClosed { close }),
                    None => e!(SendError::Io, err.into()),
                }
            }
            err => e!(SendError::Io, err.into()),
        }
    }
}
