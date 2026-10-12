//! Channels that abstract over local or remote sending

use std::io;

use n0_error::stack_error;

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
    /// The code says why, see [`ErrorCode`].
    ///
    /// [`ErrorCode`]: crate::rpc::ErrorCode
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
