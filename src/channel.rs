//! Channels that abstract over local or remote sending

use std::io;

use n0_error::stack_error;

pub mod mpsc;
pub mod none;
pub mod oneshot;

/// Error when sending a oneshot or mpsc message. For local communication,
/// the only thing that can go wrong is that the receiver has been dropped.
///
/// For rpc communication, there can be any number of errors, so this is a
/// generic io error.
#[stack_error(derive, add_meta, from_sources)]
pub enum SendError {
    /// The receiver has been closed. This is the only error that can occur
    /// for local communication.
    #[error("Receiver closed")]
    ReceiverClosed,
    /// The message exceeded the maximum allowed message size (see [`MAX_MESSAGE_SIZE`]).
    ///
    /// [`MAX_MESSAGE_SIZE`]: crate::rpc::MAX_MESSAGE_SIZE
    #[error("Maximum message size exceeded")]
    MaxMessageSizeExceeded,
    /// The underlying io error. This can occur for remote communication,
    /// due to a network error or serialization error.
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
            SendError::MaxMessageSizeExceeded { .. } => {
                io::Error::new(io::ErrorKind::InvalidData, e)
            }
            SendError::Io { source, .. } => source,
        }
    }
}
