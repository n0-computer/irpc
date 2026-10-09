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
///   ([`MaxMessageSizeExceeded`]), or it does not encode (`EncodeFailed`). This
///   sender finds this before it writes the message.
/// * Something other than this message ended the channel:
///   * The receiver closed it ([`ReceiverClosed`]).
///   * The receiver rejected an earlier message ([`MaxMessageSizeExceeded`] or
///     [`DecodeFailed`]).
///   * The server rejected the request ([`BadRequest`]).
///   * The connection failed ([`Io`]).
///
/// After an error, the channel is closed. Later sends get the same error, or
/// [`Io`] with [`io::ErrorKind::BrokenPipe`] after an io error or an encode
/// failure.
///
/// [`ReceiverClosed`]: Self::ReceiverClosed
/// [`MaxMessageSizeExceeded`]: Self::MaxMessageSizeExceeded
/// [`DecodeFailed`]: Self::DecodeFailed
/// [`BadRequest`]: Self::BadRequest
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
    /// The message exceeded the maximum allowed message size (see [`MAX_MESSAGE_SIZE`]).
    ///
    /// Either this sender found it, or the receiver reported it.
    ///
    /// [`MAX_MESSAGE_SIZE`]: crate::rpc::MAX_MESSAGE_SIZE
    #[error("Maximum message size exceeded")]
    MaxMessageSizeExceeded,
    /// The message does not encode.
    #[cfg(feature = "rpc")]
    #[error("Message does not encode")]
    EncodeFailed {
        #[error(std_err)]
        source: postcard::Error,
    },
    /// The receiver could not decode a message.
    #[error("Receiver could not decode the message")]
    DecodeFailed,
    /// The server rejected the request that this stream belongs to.
    ///
    /// This is about the request, not about a message on this channel. The
    /// sender of the updates of a request gets this.
    #[error("Bad request")]
    BadRequest,
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
