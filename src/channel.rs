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
/// * The message of this send is bad. It is too large
///   ([`MessageMaxSizeExceeded`]), or it does not encode (`MessageEncodeFailed`).
///   This sender finds this before it writes the message.
/// * Something other than this message ended the channel:
///   * The receiver closed it ([`ReceiverClosed`]).
///   * The server rejected the request ([`RequestMaxSizeExceeded`] or
///     [`RequestDecodeFailed`]).
///   * The receiver rejected an earlier message ([`MessageMaxSizeExceeded`] or
///     [`MessageDecodeFailed`]).
///   * The connection failed ([`Io`]).
///
/// After an error, the channel is closed. Later sends get the same error, or
/// [`Io`] with [`io::ErrorKind::BrokenPipe`] after an io error or an encode
/// failure.
///
/// [`ReceiverClosed`]: Self::ReceiverClosed
/// [`RequestMaxSizeExceeded`]: Self::RequestMaxSizeExceeded
/// [`RequestDecodeFailed`]: Self::RequestDecodeFailed
/// [`MessageMaxSizeExceeded`]: Self::MessageMaxSizeExceeded
/// [`MessageDecodeFailed`]: Self::MessageDecodeFailed
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
    /// The server rejected the request of this channel because it is too large.
    ///
    /// The channel is closed. Only the sender of updates gets this, see [Bad requests].
    ///
    /// [Bad requests]: crate::rpc#bad-requests
    #[error("Server rejected the request: too large")]
    RequestMaxSizeExceeded,
    /// The server rejected the request of this channel because it does not decode.
    ///
    /// The channel is closed. Only the sender of updates gets this, see [Bad requests].
    ///
    /// [Bad requests]: crate::rpc#bad-requests
    #[error("Server rejected the request: does not decode")]
    RequestDecodeFailed,
    /// This message, or an earlier one, is larger than [`MAX_MESSAGE_SIZE`].
    ///
    /// This sender or the receiver found it, and the channel is closed.
    ///
    /// [`MAX_MESSAGE_SIZE`]: crate::rpc::MAX_MESSAGE_SIZE
    #[error("Message too large")]
    MessageMaxSizeExceeded,
    /// This message does not encode, so this sender closed the channel.
    #[cfg(feature = "rpc")]
    #[error("Message does not encode")]
    MessageEncodeFailed {
        #[error(std_err)]
        source: postcard::Error,
    },
    /// The receiver could not decode an earlier message, and closed the channel.
    #[error("Receiver could not decode the message")]
    MessageDecodeFailed,
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
            _ => io::Error::new(io::ErrorKind::InvalidData, e),
        }
    }
}
