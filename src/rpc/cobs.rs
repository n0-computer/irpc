//! COBS framing for mpsc messages.
//!
//! [COBS] encodes a message without zero bytes, and ends its frame with a zero
//! byte. The frame is a sequence of blocks. Each block starts with a code byte
//! `n`, followed by `n - 1` bytes that are not zero. Between two blocks, the
//! message has a zero, unless the code byte is `0xFF`.
//!
//! A frame with a zero inside a block is invalid, which a sender uses to abort
//! a message that it already started: it ends the frame with a zero in the
//! middle of a block, or with `[2, 0]` at the start of a block.
//!
//! [COBS]: https://en.wikipedia.org/wiki/Consistent_Overhead_Byte_Stuffing

use std::{io, mem};

use smallvec::SmallVec;

/// Writes the COBS frame of the bytes written to it into a buffer.
pub(super) struct CobsEncoder<'a> {
    out: &'a mut SmallVec<[u8; 128]>,
    /// Where the code byte of the current block goes.
    code_pos: usize,
}

impl<'a> CobsEncoder<'a> {
    /// Starts a frame in `out`, which it clears.
    pub(super) fn new(out: &'a mut SmallVec<[u8; 128]>) -> Self {
        out.clear();
        out.push(0);
        Self { out, code_pos: 0 }
    }

    fn end_block(&mut self) {
        self.out[self.code_pos] = (self.out.len() - self.code_pos) as u8;
        self.code_pos = self.out.len();
        self.out.push(0);
    }

    /// Ends the frame.
    pub(super) fn finish(mut self) {
        self.end_block();
    }
}

impl io::Write for CobsEncoder<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        for &byte in buf {
            if byte == 0 {
                self.end_block();
                continue;
            }
            self.out.push(byte);
            if self.out.len() - self.code_pos == 0xFF {
                self.end_block();
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Returns the bytes that abort a frame of which the first `written` bytes were sent.
///
/// They put a zero inside a block, which makes the frame invalid.
pub(super) fn abort_bytes(frame: &[u8], written: usize) -> &'static [u8] {
    let mut start = 0;
    while start < written {
        start += frame[start] as usize;
    }
    if start == written { &[2, 0] } else { &[0] }
}

/// What [`CobsDecoder::decode`] found.
#[derive(Debug, PartialEq)]
pub(super) enum Frame {
    /// The input ended before the frame.
    Pending,
    /// A valid frame, with the message.
    Complete(Vec<u8>),
    /// An invalid frame, which the sender aborted.
    Aborted,
    /// The message is larger than the maximum size.
    TooLarge,
}

/// Decodes COBS frames from a stream, in parts as they arrive.
#[derive(Debug, Default)]
pub(super) struct CobsDecoder {
    /// The message of the frame so far.
    buf: Vec<u8>,
    /// Whether the frame has a block.
    started: bool,
    /// The bytes left in the current block.
    left: u8,
    /// Whether a zero follows the current block, if the frame does not end after it.
    zero: bool,
}

impl CobsDecoder {
    /// Whether a frame was started but did not end.
    pub(super) fn in_frame(&self) -> bool {
        self.started
    }

    /// Decodes `input` until a frame ends, and returns how many bytes it used.
    pub(super) fn decode(&mut self, input: &[u8], max_size: usize) -> (usize, Frame) {
        let mut i = 0;
        while i < input.len() {
            if self.buf.len() > max_size {
                *self = Self::default();
                return (i, Frame::TooLarge);
            }
            if self.left > 0 {
                let end = input.len().min(i + self.left as usize);
                let run = &input[i..end];
                if let Some(pos) = run.iter().position(|&b| b == 0) {
                    *self = Self::default();
                    return (i + pos + 1, Frame::Aborted);
                }
                self.buf.extend_from_slice(run);
                self.left -= run.len() as u8;
                i = end;
                continue;
            }
            let code = input[i];
            i += 1;
            if code == 0 {
                let frame = mem::take(self);
                if !frame.started {
                    return (i, Frame::Aborted);
                }
                return (i, Frame::Complete(frame.buf));
            }
            if self.started && self.zero {
                self.buf.push(0);
            }
            self.started = true;
            self.left = code - 1;
            self.zero = code != 0xFF;
        }
        (i, Frame::Pending)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn encode(data: &[u8]) -> Vec<u8> {
        let mut out = SmallVec::new();
        let mut encoder = CobsEncoder::new(&mut out);
        encoder.write_all(data).unwrap();
        encoder.finish();
        out.to_vec()
    }

    fn decode_all(mut input: &[u8]) -> Vec<Frame> {
        let mut decoder = CobsDecoder::default();
        let mut frames = Vec::new();
        while !input.is_empty() {
            let (n, frame) = decoder.decode(input, usize::MAX);
            input = &input[n..];
            frames.push(frame);
        }
        frames
    }

    fn samples() -> Vec<Vec<u8>> {
        let mut samples = vec![
            vec![],
            vec![0],
            vec![0, 0],
            vec![1],
            vec![1, 0, 2],
            vec![0, 1, 0],
        ];
        for len in [253, 254, 255, 508, 509, 1000] {
            samples.push((0..len).map(|i| (i % 255 + 1) as u8).collect());
            samples.push((0..len).map(|i| (i % 7) as u8).collect());
        }
        samples
    }

    /// Checks that a frame has no zero but the last byte, and decodes to its message.
    #[test]
    fn roundtrip() {
        for data in samples() {
            let frame = encode(&data);
            assert_eq!(frame.iter().position(|&b| b == 0), Some(frame.len() - 1));
            assert_eq!(decode_all(&frame), vec![Frame::Complete(data.clone())]);
        }
    }

    /// Checks that the encoding is the one of postcard.
    #[test]
    fn matches_postcard() {
        for data in samples() {
            let expected = postcard::to_stdvec_cobs(&Raw(&data)).unwrap();
            assert_eq!(encode(&data), expected, "{data:?}");
        }
    }

    /// Serializes its bytes without a length, as a tuple of bytes.
    struct Raw<'a>(&'a [u8]);

    impl serde::Serialize for Raw<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeTuple;
            let mut tuple = serializer.serialize_tuple(self.0.len())?;
            for byte in self.0 {
                tuple.serialize_element(byte)?;
            }
            tuple.end()
        }
    }

    /// Checks that a frame aborted after any number of bytes is aborted, and the next frame decodes.
    #[test]
    fn abort_anywhere() {
        for data in samples() {
            let frame = encode(&data);
            for written in 1..frame.len() {
                let mut stream = frame[..written].to_vec();
                stream.extend_from_slice(abort_bytes(&frame, written));
                stream.extend_from_slice(&encode(&[7]));
                assert_eq!(
                    decode_all(&stream),
                    vec![Frame::Aborted, Frame::Complete(vec![7])],
                    "{data:?} aborted after {written}"
                );
            }
        }
    }

    /// Checks that an abort that is itself cut, and replaced as the sender does, still aborts.
    #[test]
    fn abort_of_abort() {
        for data in samples() {
            let frame = encode(&data);
            for written in 1..frame.len() {
                let mut buf = frame[..written].to_vec();
                buf.extend_from_slice(abort_bytes(&frame, written));
                for cut in written..buf.len() {
                    let mut stream = buf[..cut].to_vec();
                    stream.extend_from_slice(abort_bytes(&buf, cut));
                    stream.extend_from_slice(&encode(&[7]));
                    assert_eq!(
                        decode_all(&stream),
                        vec![Frame::Aborted, Frame::Complete(vec![7])],
                        "{data:?} aborted after {written}, abort cut after {cut}"
                    );
                }
            }
        }
    }

    /// Checks that a frame that arrives a byte at a time decodes.
    #[test]
    fn byte_by_byte() {
        let data: Vec<u8> = (0..600).map(|i| (i % 5) as u8).collect();
        let frame = encode(&data);
        let mut decoder = CobsDecoder::default();
        for (i, byte) in frame.iter().enumerate() {
            let (n, res) = decoder.decode(std::slice::from_ref(byte), usize::MAX);
            assert_eq!(n, 1);
            if i + 1 < frame.len() {
                assert_eq!(res, Frame::Pending);
            } else {
                assert_eq!(res, Frame::Complete(data.clone()));
            }
        }
    }

    /// Checks that a message larger than the maximum size is `TooLarge`.
    #[test]
    fn too_large() {
        let mut decoder = CobsDecoder::default();
        assert_eq!(decoder.decode(&encode(&[1; 100]), 99).1, Frame::TooLarge);
        assert_eq!(decoder.decode(&encode(&[0; 100]), 99).1, Frame::TooLarge);
    }
}
