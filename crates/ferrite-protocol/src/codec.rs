//! Async framing: turns a byte stream into whole protocol messages.
//!
//! Frames are read length-first and the declared length is checked against
//! a configured ceiling *before* any allocation, so a client that claims a
//! two-gigabyte message gets a protocol error rather than a reserved
//! two-gigabyte buffer.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{ProtocolError, Result};
use crate::message::{Frontend, StartupRequest, MAX_STARTUP_PACKET_LEN};

/// Default ceiling on a single frame body. Generous enough for large
/// parameter values, small enough that a hostile length prefix cannot be
/// used as an allocation amplifier.
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

pub(crate) struct Framed<S> {
    stream: S,
    out: Vec<u8>,
    body: Vec<u8>,
    max_message_size: usize,
}

impl<S> Framed<S> {
    pub(crate) fn new(stream: S, max_message_size: usize) -> Self {
        Self {
            stream,
            out: Vec::with_capacity(8 * 1024),
            body: Vec::new(),
            max_message_size,
        }
    }

    pub(crate) fn into_inner(self) -> S {
        self.stream
    }

    /// Queues a frame. Nothing reaches the socket until [`Framed::flush`],
    /// which lets a result set go out as a few large writes.
    pub(crate) fn send(&mut self, frame: Vec<u8>) {
        self.out.extend_from_slice(&frame);
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Framed<S> {
    pub(crate) async fn flush(&mut self) -> Result<()> {
        if self.out.is_empty() {
            return Ok(());
        }
        self.stream.write_all(&self.out).await?;
        self.stream.flush().await?;
        self.out.clear();
        Ok(())
    }

    /// Reads the untagged packet that opens a connection.
    pub(crate) async fn read_startup(&mut self) -> Result<StartupRequest> {
        let len = self.read_len(MAX_STARTUP_PACKET_LEN).await?;
        self.fill_body(len).await?;
        StartupRequest::decode(&self.body)
    }

    /// Reads one tagged frontend message.
    pub(crate) async fn read_message(&mut self) -> Result<Frontend> {
        let tag = match self.stream.read_u8().await {
            Ok(tag) => tag,
            Err(e) if is_eof(&e) => return Err(ProtocolError::Closed),
            Err(e) => return Err(e.into()),
        };
        let len = self.read_len(self.max_message_size).await?;
        self.fill_body(len).await?;
        Frontend::decode(tag, &self.body)
    }

    /// Reads the four-byte length prefix and returns the body length,
    /// having already rejected anything outside `[4, max]`.
    async fn read_len(&mut self, max: usize) -> Result<usize> {
        let declared = match self.stream.read_i32().await {
            Ok(v) => v,
            Err(e) if is_eof(&e) => return Err(ProtocolError::Closed),
            Err(e) => return Err(e.into()),
        };
        if declared < 4 {
            return Err(ProtocolError::malformed(format!(
                "frame length {declared} is below the 4-byte minimum"
            )));
        }
        let len = declared as usize - 4;
        if len > max {
            return Err(ProtocolError::MessageTooLarge { len, max });
        }
        Ok(len)
    }

    async fn fill_body(&mut self, len: usize) -> Result<()> {
        self.body.clear();
        self.body.resize(len, 0);
        match self.stream.read_exact(&mut self.body).await {
            Ok(_) => Ok(()),
            Err(e) if is_eof(&e) => Err(ProtocolError::Closed),
            Err(e) => Err(e.into()),
        }
    }

    pub(crate) async fn write_raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.stream.write_all(bytes).await?;
        self.stream.flush().await?;
        Ok(())
    }
}

fn is_eof(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::UnexpectedEof
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn framed(bytes: &[u8]) -> Framed<Cursor<Vec<u8>>> {
        Framed::new(Cursor::new(bytes.to_vec()), DEFAULT_MAX_MESSAGE_SIZE)
    }

    #[tokio::test]
    async fn rejects_an_oversized_length_prefix_without_allocating() {
        let mut f = Framed::new(Cursor::new(vec![b'Q', 0x7f, 0xff, 0xff, 0xff]), 1024);
        assert!(matches!(
            f.read_message().await,
            Err(ProtocolError::MessageTooLarge { .. })
        ));
    }

    #[tokio::test]
    async fn rejects_a_length_prefix_below_the_minimum() {
        let mut f = framed(&[b'Q', 0, 0, 0, 3]);
        assert!(matches!(
            f.read_message().await,
            Err(ProtocolError::Malformed(_))
        ));
    }

    #[tokio::test]
    async fn reports_truncated_frames_as_a_clean_close() {
        let mut f = framed(&[b'Q', 0, 0, 0, 12, b'S', b'E']);
        assert!(matches!(f.read_message().await, Err(ProtocolError::Closed)));
    }

    #[tokio::test]
    async fn reads_a_whole_query_frame() {
        let mut f = framed(&[
            b'Q', 0, 0, 0, 13, b'S', b'E', b'L', b'E', b'C', b'T', b' ', b'1', 0,
        ]);
        assert_eq!(
            f.read_message().await.unwrap(),
            Frontend::Query("SELECT 1".into())
        );
    }

    #[tokio::test]
    async fn caps_the_startup_packet_independently() {
        let mut f = framed(&(MAX_STARTUP_PACKET_LEN as i32 + 100).to_be_bytes());
        assert!(matches!(
            f.read_startup().await,
            Err(ProtocolError::MessageTooLarge { .. })
        ));
    }
}
