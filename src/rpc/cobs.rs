//! Aborting a COBS frame of an mpsc message.
//!
//! [COBS] encodes a message without zero bytes, and ends its frame with a zero
//! byte. The frame is a sequence of blocks. Each block starts with a code byte
//! `n`, followed by `n - 1` bytes that are not zero. Postcard encodes and
//! decodes the frames.
//!
//! A frame with a zero inside a block is invalid, which a sender uses to abort
//! a message that it already started: it ends the frame with a zero in the
//! middle of a block, or with `[2, 0]` at the start of a block. Postcard
//! decodes such a frame to [`postcard::Error::DeserializeBadEncoding`].
//!
//! [COBS]: https://en.wikipedia.org/wiki/Consistent_Overhead_Byte_Stuffing

/// Returns the bytes that abort a frame of which the first `written` bytes were sent.
///
/// They put a zero inside a block, which makes the frame invalid. `frame` may
/// end with the bytes of an earlier abort, of which a part was sent.
pub(super) fn abort_bytes(frame: &[u8], written: usize) -> &'static [u8] {
    let mut start = 0;
    while start < written {
        start += frame[start] as usize;
    }
    if start == written { &[2, 0] } else { &[0] }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize, de::DeserializeOwned};

    use super::*;

    fn samples() -> Vec<Vec<u8>> {
        let mut samples = vec![vec![], vec![0], vec![0, 0], vec![1, 0, 2]];
        for len in [253, 254, 255, 508, 509, 1000] {
            samples.push((0..len).map(|i| (i % 255 + 1) as u8).collect());
            samples.push((0..len).map(|i| (i % 7) as u8).collect());
        }
        samples
    }

    /// Checks that a frame aborted after any byte, with an abort cut after any byte as the
    /// sender replaces it, ends at the end of the abort, and decodes to `DeserializeBadEncoding`.
    #[test]
    fn abort_anywhere() {
        for data in samples() {
            let frame = postcard::to_stdvec_cobs(&data).unwrap();
            for written in 1..frame.len() {
                let mut buf = frame[..written].to_vec();
                buf.extend_from_slice(abort_bytes(&frame, written));
                for cut in written..buf.len() {
                    let mut stream = buf[..cut].to_vec();
                    stream.extend_from_slice(abort_bytes(&buf, cut));
                    let end = stream.iter().position(|&b| b == 0);
                    assert_eq!(end, Some(stream.len() - 1), "{data:?} {written} {cut}");
                    let res = postcard::from_bytes_cobs::<Vec<u8>>(&mut stream);
                    assert_eq!(
                        res,
                        Err(postcard::Error::DeserializeBadEncoding),
                        "{data:?} aborted after {written}, abort cut after {cut}"
                    );
                }
            }
        }
    }

    /// Serializes its bytes without a length, as a tuple of bytes.
    struct Raw<'a>(&'a [u8]);

    impl Serialize for Raw<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeTuple;
            let mut tuple = serializer.serialize_tuple(self.0.len())?;
            for byte in self.0 {
                tuple.serialize_element(byte)?;
            }
            tuple.end()
        }
    }

    #[derive(Debug, Deserialize)]
    enum TwoVariants {
        _A,
        _B,
    }

    /// Decodes the valid COBS frame of `bytes` as `T`, and returns the error.
    fn decode_error<T: DeserializeOwned + std::fmt::Debug>(bytes: &[u8]) -> postcard::Error {
        let mut frame = postcard::to_stdvec_cobs(&Raw(bytes)).unwrap();
        postcard::from_bytes_cobs::<T>(&mut frame).unwrap_err()
    }

    /// Checks that postcard does not report a message that does not decode, in a valid frame,
    /// as `DeserializeBadEncoding`, which the receiver takes as an aborted frame.
    #[test]
    fn decode_errors_are_not_bad_encoding() {
        let errors = [
            decode_error::<bool>(&[2]),
            decode_error::<u32>(&[0xFF; 6]),
            decode_error::<Vec<u8>>(&[5, 1]),
            decode_error::<String>(&[1, 0xFF]),
            decode_error::<Option<u8>>(&[2]),
            decode_error::<char>(&[1, 0xFF]),
            decode_error::<TwoVariants>(&[5]),
            decode_error::<u8>(&[]),
        ];
        for err in errors {
            assert_ne!(err, postcard::Error::DeserializeBadEncoding);
        }
    }

    /// Checks that a frame of just the end, which a sender never writes, is an empty message, not an abort.
    #[test]
    fn empty_frame_is_empty_message() {
        assert_eq!(postcard::from_bytes_cobs::<()>(&mut [0]), Ok(()));
        let err = postcard::from_bytes_cobs::<u8>(&mut [0]).unwrap_err();
        assert_ne!(err, postcard::Error::DeserializeBadEncoding);
    }
}
