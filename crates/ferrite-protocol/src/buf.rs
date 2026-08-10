use crate::error::{ProtocolError, Result};

/// Bounds-checked cursor over one already-framed message body.
///
/// Every accessor returns `Err` rather than panicking when the buffer is
/// too short, which is what keeps a truncated or hostile frame from taking
/// the process down. There is deliberately no indexing API.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(ProtocolError::malformed(format!(
                "need {n} bytes, only {} left",
                self.remaining()
            )));
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn i16(&mut self) -> Result<i16> {
        let b = self.take(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }

    pub(crate) fn i32(&mut self) -> Result<i32> {
        let b = self.take(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// A NUL-terminated, UTF-8 string. PostgreSQL allows any encoding here
    /// in principle; Ferrite is UTF-8 only, so invalid sequences are a
    /// protocol error rather than a lossy conversion.
    pub(crate) fn cstr(&mut self) -> Result<String> {
        let end = self.buf[self.pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| ProtocolError::malformed("unterminated string"))?;
        let bytes = &self.buf[self.pos..self.pos + end];
        self.pos += end + 1;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| ProtocolError::malformed("string is not valid UTF-8"))
    }

    /// A length-prefixed byte run, where `-1` means SQL NULL.
    pub(crate) fn nullable_bytes(&mut self) -> Result<Option<&'a [u8]>> {
        let len = self.i32()?;
        if len == -1 {
            return Ok(None);
        }
        let len = usize::try_from(len)
            .map_err(|_| ProtocolError::malformed(format!("negative field length {len}")))?;
        Ok(Some(self.take(len)?))
    }

    /// A non-negative count read as `i16`, guarding against the negative
    /// values a hostile client can put in an array-length field.
    pub(crate) fn count(&mut self) -> Result<usize> {
        let n = self.i16()?;
        usize::try_from(n).map_err(|_| ProtocolError::malformed(format!("negative count {n}")))
    }

    /// Rejects trailing bytes: a well-formed message is fully consumed by
    /// its own decoder.
    pub(crate) fn finish(&self) -> Result<()> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(ProtocolError::malformed(format!(
                "{} trailing bytes",
                self.remaining()
            )))
        }
    }
}

/// Append-only message builder that back-fills the length prefix.
pub(crate) struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    /// Starts a tagged backend message; the four length bytes are reserved
    /// now and written by [`Writer::finish`].
    pub(crate) fn tagged(tag: u8) -> Self {
        let mut buf = Vec::with_capacity(32);
        buf.push(tag);
        buf.extend_from_slice(&[0; 4]);
        Self { buf }
    }

    pub(crate) fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    pub(crate) fn i16(&mut self, v: i16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub(crate) fn i32(&mut self, v: i32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub(crate) fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(v);
        self
    }

    pub(crate) fn cstr(&mut self, v: &str) -> &mut Self {
        self.buf.extend_from_slice(v.as_bytes());
        self.buf.push(0);
        self
    }

    /// Back-fills the length prefix and yields the finished frame. Takes
    /// `&mut self` so a builder chain can end on a temporary.
    pub(crate) fn finish(&mut self) -> Vec<u8> {
        let len = (self.buf.len() - 1) as i32;
        self.buf[1..5].copy_from_slice(&len.to_be_bytes());
        std::mem::take(&mut self.buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_rejects_truncation_instead_of_panicking() {
        let mut r = Reader::new(&[0, 1]);
        assert!(r.i32().is_err());
        assert!(r.i16().is_ok());
        assert!(r.u8().is_err());
    }

    #[test]
    fn reader_rejects_unterminated_and_non_utf8_strings() {
        assert!(Reader::new(b"abc").cstr().is_err());
        assert!(Reader::new(&[0xff, 0xfe, 0]).cstr().is_err());
        assert_eq!(Reader::new(b"ok\0").cstr().unwrap(), "ok");
    }

    #[test]
    fn reader_rejects_negative_lengths() {
        let negative_len = (-7i32).to_be_bytes();
        assert!(Reader::new(&negative_len).nullable_bytes().is_err());
        let null_len = (-1i32).to_be_bytes();
        assert_eq!(Reader::new(&null_len).nullable_bytes().unwrap(), None);
        let negative_count = (-3i16).to_be_bytes();
        assert!(Reader::new(&negative_count).count().is_err());
    }

    #[test]
    fn writer_backfills_length() {
        let out = Writer::tagged(b'Z').u8(b'I').finish();
        assert_eq!(out, vec![b'Z', 0, 0, 0, 5, b'I']);
    }
}
