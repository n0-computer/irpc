//! Utilities
//!
//! This module contains utilities to read and write varints.
#[cfg(any(feature = "rpc", feature = "varint-util"))]
mod varint_util {
    use std::{
        future::Future,
        io::{self, Error},
    };

    use serde::{Serialize, de::DeserializeOwned};
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
            if shift == 0
                && let Err(cause) = res
            {
                if cause.kind() == io::ErrorKind::UnexpectedEof {
                    return Ok(None);
                } else {
                    return Err(cause);
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
        /// Write a value with a varint encoded length prefix.
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
        /// Write a value with a varint encoded length prefix.
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

#[cfg(any(feature = "rpc", feature = "varint-util"))]
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
