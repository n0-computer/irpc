//! Channels that abstract over local or remote sending

use std::io;

#[cfg(feature = "rpc")]
use n0_error::e;
use n0_error::stack_error;

#[cfg(feature = "rpc")]
use crate::rpc::{self, ErrorCode};

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
///   * The receiver stopped the stream with an irpc error code ([`Remote`]).
///     For example, the server rejected the request, or the receiver could not
///     decode an earlier message.
///   * The connection failed ([`Io`]).
///
/// If the server rejects the request and also closes the connection, the close
/// usually arrives first, and the sender gets [`Io`] instead of [`Remote`].
/// [`Self::remote_code`] returns the code in both cases.
///
/// After an error, the channel is closed. Later sends get the same error if
/// the receiver ended the channel, and [`Io`] with
/// [`io::ErrorKind::BrokenPipe`] otherwise.
///
/// [`ReceiverClosed`]: Self::ReceiverClosed
/// [`MaxSizeExceeded`]: Self::MaxSizeExceeded
/// [`Remote`]: Self::Remote
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
    /// The receiver stopped the stream with an irpc error code, and the channel is closed.
    ///
    /// The `code` says why, see [`ErrorCode`], and the [`rpc`] module for an overview.
    ///
    /// [`ErrorCode`]: crate::rpc::ErrorCode
    /// [`rpc`]: crate::rpc
    #[cfg(feature = "rpc")]
    #[error("Receiver closed the channel: {code}")]
    Remote { code: crate::rpc::ErrorCode },
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
    /// An io error, for example because the connection was lost.
    ///
    /// After a send was cancelled, later sends get an io error with
    /// [`io::ErrorKind::BrokenPipe`].
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
            _ => io::Error::new(io::ErrorKind::InvalidData, e),
        }
    }
}

#[cfg(feature = "rpc")]
impl SendError {
    /// Returns the irpc error code that the peer sent, if any.
    ///
    /// This is the code of a [`Remote`](Self::Remote) error, or the code that
    /// the peer closed the connection with.
    pub fn remote_code(&self) -> Option<ErrorCode> {
        match self {
            Self::Remote { code, .. } => Some(*code),
            Self::Io { source, .. } => rpc::io_close_code(source),
            _ => None,
        }
    }

    /// Returns the error for a receiver that stopped the stream with `code`.
    pub(crate) fn from_error_code(code: ErrorCode) -> Self {
        match code {
            // A receiver that is dropped stops the stream with code 0.
            ErrorCode::Closed => e!(SendError::ReceiverClosed),
            code => e!(SendError::Remote { code }),
        }
    }

    /// Returns the error for a send after an earlier error closed the sender.
    ///
    /// `code` is the code that the receiver stopped the stream with, if it did.
    pub(crate) fn closed(code: Option<ErrorCode>) -> Self {
        code.map(Self::from_error_code)
            .unwrap_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe).into())
    }
}

#[cfg(feature = "rpc")]
impl From<noq::WriteError> for SendError {
    fn from(err: noq::WriteError) -> Self {
        match rpc::stop_code(&err) {
            Some(code) => SendError::from_error_code(code),
            None => e!(SendError::Io, err.into()),
        }
    }
}
