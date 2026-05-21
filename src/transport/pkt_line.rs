//! pkt-line framing for git's wire protocol.
//!
//! A pkt-line is `pkt-len pkt-payload` where `pkt-len` is 4 ASCII hex digits
//! and the length INCLUDES the 4 header bytes themselves. The protocol
//! reserves three short lengths as control packets, none of which carry
//! payload bytes:
//!
//! ```text
//!   0000  flush-pkt          end of a logical message / section
//!   0001  delim-pkt          section separator (protocol v2)
//!   0002  response-end-pkt   end of stateless-RPC response (protocol v2)
//! ```
//!
//! A normal data packet has length in the range `0x0004..=0xFFFF`. Length
//! `0x0004` is technically legal (an empty payload); git itself recommends
//! against sending it but we accept it on read — the spec is the law on
//! the wire.
//!
//! Max payload is 65516 bytes (since the total packet, including the
//! 4-byte header, must fit in 0xFFFF). [`PktLineWriter::write_data`]
//! enforces that.
//!
//! See `gitprotocol-common(5)` for the framing spec and `gitprotocol-v2(5)`
//! for the meanings of the special length-1/length-2 packets.

use std::io::{Read, Write};

use super::TransportError;

/// Largest legal pkt-line on the wire, header included.
pub const MAX_PKT_LINE: usize = 0xFFFF;
/// Largest legal pkt-line payload (header excluded).
pub const MAX_PKT_PAYLOAD: usize = MAX_PKT_LINE - 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PktLine {
    /// Regular data packet (length `0x4..=0xFFFF`, header inclusive).
    /// The payload here is the bytes AFTER the 4-byte length header.
    Data(Vec<u8>),
    /// `0000` — end of one logical "section" or full response.
    Flush,
    /// `0001` — section delimiter (v2 only).
    Delim,
    /// `0002` — response-end marker (v2 stateless RPC).
    ResponseEnd,
}

/// Streaming pkt-line decoder over any [`Read`]er.
pub struct PktLineReader<R: Read> {
    inner: R,
}

impl<R: Read> PktLineReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Read the next packet. Returns `Ok(None)` on clean EOF (the underlying
    /// reader had no bytes left when we tried to read the length header).
    /// EOF in the MIDDLE of a packet is `Err(Io(UnexpectedEof))`.
    ///
    /// Note: a flush-pkt is its OWN variant, not EOF — callers that want
    /// to detect "end of message" should look for [`PktLine::Flush`].
    pub fn next_pkt(&mut self) -> Result<Option<PktLine>, TransportError> {
        let mut hdr = [0u8; 4];
        match read_exact_or_eof(&mut self.inner, &mut hdr)? {
            ReadHdr::Eof => return Ok(None),
            ReadHdr::Got => {}
        }

        let len = parse_hex4(&hdr).ok_or(TransportError::BadPktLength(hdr))?;

        match len {
            0 => Ok(Some(PktLine::Flush)),
            1 => Ok(Some(PktLine::Delim)),
            2 => Ok(Some(PktLine::ResponseEnd)),
            // Length 3 is reserved and invalid on the wire.
            3 => Err(TransportError::BadPktLength(hdr)),
            n => {
                // Total packet length is n, header is 4 bytes, so payload is n-4.
                let payload_len = usize::from(n - 4);
                let mut buf = vec![0u8; payload_len];
                self.inner.read_exact(&mut buf)?;
                Ok(Some(PktLine::Data(buf)))
            }
        }
    }
}

impl<R: Read> Iterator for PktLineReader<R> {
    type Item = Result<PktLine, TransportError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_pkt() {
            Ok(Some(p)) => Some(Ok(p)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

/// Streaming pkt-line encoder over any [`Write`]r.
pub struct PktLineWriter<W: Write> {
    inner: W,
}

impl<W: Write> PktLineWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    /// Write one data pkt-line. The header is computed automatically.
    /// Returns an error if `data.len() > MAX_PKT_PAYLOAD`.
    pub fn write_data(&mut self, data: &[u8]) -> Result<(), TransportError> {
        if data.len() > MAX_PKT_PAYLOAD {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "pkt-line payload too large: {} > {}",
                    data.len(),
                    MAX_PKT_PAYLOAD
                ),
            )));
        }
        let total = data.len() + 4;
        let hdr = hex4(total as u16);
        self.inner.write_all(&hdr)?;
        self.inner.write_all(data)?;
        Ok(())
    }

    pub fn write_flush(&mut self) -> Result<(), TransportError> {
        self.inner.write_all(&flush_pkt())?;
        Ok(())
    }

    pub fn write_delim(&mut self) -> Result<(), TransportError> {
        self.inner.write_all(&delim_pkt())?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), TransportError> {
        self.inner.flush()?;
        Ok(())
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

/// Encode a single data pkt-line as a fresh `Vec<u8>`. Convenience for
/// when you'd rather build the bytes than thread a writer through.
///
/// Panics if `data.len() > MAX_PKT_PAYLOAD`. Callers that may push large
/// payloads should use [`PktLineWriter::write_data`], which returns a
/// proper error.
pub fn encode_data_pkt(data: &[u8]) -> Vec<u8> {
    assert!(
        data.len() <= MAX_PKT_PAYLOAD,
        "pkt-line payload too large: {} > {}",
        data.len(),
        MAX_PKT_PAYLOAD
    );
    let total = data.len() + 4;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&hex4(total as u16));
    out.extend_from_slice(data);
    out
}

/// The flush-pkt bytes (`b"0000"`).
pub fn flush_pkt() -> [u8; 4] {
    *b"0000"
}

/// The delim-pkt bytes (`b"0001"`, v2 only).
pub fn delim_pkt() -> [u8; 4] {
    *b"0001"
}

/// The response-end-pkt bytes (`b"0002"`, v2 only).
pub fn response_end_pkt() -> [u8; 4] {
    *b"0002"
}

// ---- internals ----

enum ReadHdr {
    Got,
    Eof,
}

/// Read exactly `buf.len()` bytes, but distinguish clean EOF (0 bytes
/// before the first byte) from premature EOF (partial read).
fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<ReadHdr> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 if filled == 0 => return Ok(ReadHdr::Eof),
            0 => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "EOF in the middle of pkt-line length header",
                ));
            }
            n => filled += n,
        }
    }
    Ok(ReadHdr::Got)
}

/// Parse 4 ASCII hex digits into a u16. Returns `None` if any byte is
/// not a hex digit.
fn parse_hex4(hdr: &[u8; 4]) -> Option<u16> {
    let mut v: u16 = 0;
    for &b in hdr.iter() {
        let n = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return None,
        };
        v = (v << 4) | u16::from(n);
    }
    Some(v)
}

/// Encode a u16 as 4 lowercase ASCII hex digits.
fn hex4(v: u16) -> [u8; 4] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    [
        HEX[((v >> 12) & 0xF) as usize],
        HEX[((v >> 8) & 0xF) as usize],
        HEX[((v >> 4) & 0xF) as usize],
        HEX[(v & 0xF) as usize],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn decode_flush() {
        let mut r = PktLineReader::new(Cursor::new(b"0000".as_slice()));
        assert_eq!(r.next_pkt().unwrap(), Some(PktLine::Flush));
        assert_eq!(r.next_pkt().unwrap(), None);
    }

    #[test]
    fn decode_delim() {
        let mut r = PktLineReader::new(Cursor::new(b"0001".as_slice()));
        assert_eq!(r.next_pkt().unwrap(), Some(PktLine::Delim));
        assert_eq!(r.next_pkt().unwrap(), None);
    }

    #[test]
    fn decode_response_end() {
        let mut r = PktLineReader::new(Cursor::new(b"0002".as_slice()));
        assert_eq!(r.next_pkt().unwrap(), Some(PktLine::ResponseEnd));
    }

    #[test]
    fn decode_data() {
        // Spec example: pkt-line "000bfoobar\n" decodes to "foobar\n"
        // (0x000b = 11 total = 4 header + 7 byte payload).
        let mut r = PktLineReader::new(Cursor::new(b"000bfoobar\n".as_slice()));
        assert_eq!(
            r.next_pkt().unwrap(),
            Some(PktLine::Data(b"foobar\n".to_vec()))
        );
        assert_eq!(r.next_pkt().unwrap(), None);

        // And the smaller "0006a\n" example.
        let mut r2 = PktLineReader::new(Cursor::new(b"0006a\n".as_slice()));
        assert_eq!(r2.next_pkt().unwrap(), Some(PktLine::Data(b"a\n".to_vec())));
    }

    #[test]
    fn decode_sequence() {
        // "hello\n" is 6 bytes → 0x000a total; "world\n" same. Then
        // each is followed by a flush-pkt.
        let bytes: &[u8] = b"000ahello\n0000000aworld\n0000";
        let mut r = PktLineReader::new(Cursor::new(bytes));
        assert_eq!(
            r.next_pkt().unwrap(),
            Some(PktLine::Data(b"hello\n".to_vec()))
        );
        assert_eq!(r.next_pkt().unwrap(), Some(PktLine::Flush));
        assert_eq!(
            r.next_pkt().unwrap(),
            Some(PktLine::Data(b"world\n".to_vec()))
        );
        assert_eq!(r.next_pkt().unwrap(), Some(PktLine::Flush));
        assert_eq!(r.next_pkt().unwrap(), None);
    }

    #[test]
    fn encode_data() {
        // 0x000a = 10 total = 4 header + 6 byte payload ("hello\n").
        assert_eq!(encode_data_pkt(b"hello\n"), b"000ahello\n".to_vec());
        // Spec example from gitprotocol-common(5): "0006a\n" for "a\n".
        assert_eq!(encode_data_pkt(b"a\n"), b"0006a\n".to_vec());
        // And "0004" for empty payload.
        assert_eq!(encode_data_pkt(b""), b"0004".to_vec());
    }

    #[test]
    fn round_trip() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut w = PktLineWriter::new(&mut buf);
            w.write_data(b"foo\n").unwrap();
            w.write_data(b"a much longer payload to force a different length")
                .unwrap();
            w.write_delim().unwrap();
            w.write_data(&[]).unwrap(); // empty data pkt (length 0x0004)
            w.write_flush().unwrap();
        }
        let mut r = PktLineReader::new(Cursor::new(&buf));
        let v1 = r.next_pkt().unwrap().unwrap();
        let v2 = r.next_pkt().unwrap().unwrap();
        let v3 = r.next_pkt().unwrap().unwrap();
        let v4 = r.next_pkt().unwrap().unwrap();
        let v5 = r.next_pkt().unwrap().unwrap();
        assert_eq!(v1, PktLine::Data(b"foo\n".to_vec()));
        assert_eq!(
            v2,
            PktLine::Data(b"a much longer payload to force a different length".to_vec())
        );
        assert_eq!(v3, PktLine::Delim);
        assert_eq!(v4, PktLine::Data(vec![]));
        assert_eq!(v5, PktLine::Flush);
        assert!(r.next_pkt().unwrap().is_none());
    }

    #[test]
    fn bad_header() {
        let mut r = PktLineReader::new(Cursor::new(b"xyzz".as_slice()));
        match r.next_pkt() {
            Err(TransportError::BadPktLength(h)) => assert_eq!(&h, b"xyzz"),
            other => panic!("expected BadPktLength, got {other:?}"),
        }
    }

    #[test]
    fn empty_data_pkt() {
        // 0x0004 = total length 4 = 0 bytes of payload.
        let mut r = PktLineReader::new(Cursor::new(b"0004".as_slice()));
        assert_eq!(r.next_pkt().unwrap(), Some(PktLine::Data(vec![])));
    }

    #[test]
    fn length_one_is_delim_not_data() {
        // 0x0001 must NOT decode as a data packet with 0 payload bytes —
        // it's the v2 delim marker.
        let mut r = PktLineReader::new(Cursor::new(b"0001".as_slice()));
        let p = r.next_pkt().unwrap();
        assert_eq!(p, Some(PktLine::Delim));
        assert_ne!(p, Some(PktLine::Data(vec![])));
    }

    #[test]
    fn length_three_is_reserved_error() {
        // 0x0003 is not assigned; it's not flush/delim/response-end and
        // can't be a data packet (header alone is 4 bytes).
        let mut r = PktLineReader::new(Cursor::new(b"0003".as_slice()));
        assert!(matches!(r.next_pkt(), Err(TransportError::BadPktLength(_))));
    }

    #[test]
    fn partial_header_is_unexpected_eof() {
        let mut r = PktLineReader::new(Cursor::new(b"00".as_slice()));
        match r.next_pkt() {
            Err(TransportError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }

    #[test]
    fn truncated_payload_is_unexpected_eof() {
        // Header says 0x000b (11 total = 4+7) but we only supply 3 payload bytes.
        let mut r = PktLineReader::new(Cursor::new(b"000bhel".as_slice()));
        match r.next_pkt() {
            Err(TransportError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }

    #[test]
    fn hex4_encodes_all_widths() {
        assert_eq!(&hex4(0), b"0000");
        assert_eq!(&hex4(1), b"0001");
        assert_eq!(&hex4(0x000b), b"000b");
        assert_eq!(&hex4(0x00ff), b"00ff");
        assert_eq!(&hex4(0xffff), b"ffff");
    }

    #[test]
    fn parse_hex4_round_trips() {
        for v in [0u16, 1, 4, 0x000b, 0x00ff, 0x1234, 0xfedc, 0xffff] {
            assert_eq!(parse_hex4(&hex4(v)), Some(v));
        }
    }

    #[test]
    fn flush_delim_response_end_constants() {
        assert_eq!(&flush_pkt(), b"0000");
        assert_eq!(&delim_pkt(), b"0001");
        assert_eq!(&response_end_pkt(), b"0002");
    }
}
