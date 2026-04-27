//! Line-based framing over an async byte stream.
//!
//! Wraps a pair of `AsyncRead + AsyncWrite` halves with a
//! length-bounded line reader and a timeout-bounded writer. Every
//! operation has an explicit deadline; no call can block forever.
//!
//! Transport-agnostic: works on TCP, Unix domain sockets, and
//! `tokio::io::duplex` alike, because the underlying types are
//! reduced to the `AsyncRead`/`AsyncWrite` traits.
//!
//! The framing layer does not understand the MPD protocol. It
//! returns lines (stripped of their trailing `\n` and any preceding
//! `\r`) and accepts byte strings. Parsing is the protocol layer's
//! job; dispatch is the connection layer's.

use std::io;
use std::time::Duration;

use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::time;

use super::error::{MpdError, ProtocolError, TransportError};
use super::protocol::LINE_MAX;

/// Line reader / writer over a byte stream.
///
/// The reader and writer halves are typed so the caller can pass a
/// split socket (`TcpStream::into_split()` returns owned halves) or
/// an in-memory duplex pair.
pub(crate) struct Framing<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    reader: BufReader<R>,
    writer: W,
    /// Re-used allocation for read_line. Cleared at the start of
    /// every call; never grows unboundedly beyond what a single line
    /// requires (lines over `LINE_MAX` are rejected).
    scratch: String,
}

impl<R, W> Framing<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    /// Wrap a reader/writer pair.
    pub(crate) fn new(reader: R, writer: W) -> Self {
        Self {
            reader: BufReader::new(reader),
            writer,
            scratch: String::with_capacity(256),
        }
    }

    /// Read one line from the stream, enforcing a hard deadline.
    ///
    /// The returned string has its trailing `\n` and any preceding
    /// `\r` stripped. Fails cleanly on:
    ///
    /// - Immediate EOF before any byte is read: `TransportError::Closed`.
    /// - EOF mid-line (bytes read but no `\n` before close):
    ///   `TransportError::Closed`. Truncated lines are a protocol
    ///   violation; the connection cannot usefully continue.
    /// - Line exceeding [`LINE_MAX`]: `ProtocolError::LineTooLong`.
    /// - Non-UTF-8 bytes: `ProtocolError::NonUtf8`.
    /// - Deadline exceeded: `MpdError::Timeout`.
    /// - Other I/O errors: `TransportError::Io`.
    pub(crate) async fn read_line_with_timeout(
        &mut self,
        budget: Duration,
        operation: &'static str,
    ) -> Result<String, MpdError> {
        self.scratch.clear();
        let outcome =
            time::timeout(budget, self.reader.read_line(&mut self.scratch))
                .await;
        let n = match outcome {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(classify_read_error(e)),
            Err(_elapsed) => {
                return Err(MpdError::Timeout {
                    operation,
                    elapsed: budget,
                });
            }
        };
        if n == 0 {
            return Err(MpdError::Transport(TransportError::Closed));
        }
        if self.scratch.len() > LINE_MAX {
            return Err(MpdError::Protocol(ProtocolError::LineTooLong {
                len: self.scratch.len(),
                limit: LINE_MAX,
            }));
        }
        if !self.scratch.ends_with('\n') {
            // Peer sent bytes but closed before the line terminator.
            // MPD's protocol mandates `\n` on every line; a truncated
            // line means the connection is unusable.
            return Err(MpdError::Transport(TransportError::Closed));
        }
        // Strip the trailing `\n` and, if present, the preceding
        // `\r`. MPD servers emit `\n`; historical compatibility with
        // servers that emit `\r\n` is cheap to support.
        self.scratch.pop();
        if self.scratch.ends_with('\r') {
            self.scratch.pop();
        }
        Ok(std::mem::take(&mut self.scratch))
    }

    /// Write the given bytes to the stream and flush, enforcing a
    /// hard deadline on the combined operation.
    pub(crate) async fn write_all_with_timeout(
        &mut self,
        bytes: &[u8],
        budget: Duration,
        operation: &'static str,
    ) -> Result<(), MpdError> {
        let write_and_flush = async {
            self.writer.write_all(bytes).await?;
            self.writer.flush().await?;
            Ok::<(), io::Error>(())
        };
        match time::timeout(budget, write_and_flush).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                Err(MpdError::Transport(TransportError::Io { source: e }))
            }
            Err(_elapsed) => Err(MpdError::Timeout {
                operation,
                elapsed: budget,
            }),
        }
    }
}

fn classify_read_error(e: io::Error) -> MpdError {
    match e.kind() {
        io::ErrorKind::InvalidData => {
            // `BufReader::read_line` returns InvalidData when the
            // bytes it read were not valid UTF-8. We do not have the
            // raw bytes available (they were dropped by the String
            // conversion), so we report a count hint of zero.
            MpdError::Protocol(ProtocolError::NonUtf8(0))
        }
        io::ErrorKind::UnexpectedEof => {
            MpdError::Transport(TransportError::Closed)
        }
        _ => MpdError::Transport(TransportError::Io { source: e }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::io::duplex;
    use tokio::io::AsyncWriteExt;

    const SHORT: Duration = Duration::from_millis(500);
    const VERY_SHORT: Duration = Duration::from_millis(50);

    #[tokio::test]
    async fn read_line_parses_single_lf_terminated_line() {
        let (mut server, client) = duplex(256);
        let (r, w) = tokio::io::split(client);
        let mut f = Framing::new(r, w);

        tokio::spawn(async move {
            server.write_all(b"hello world\n").await.unwrap();
            server.shutdown().await.unwrap();
        });

        let line = f.read_line_with_timeout(SHORT, "read").await.unwrap();
        assert_eq!(line, "hello world");
    }

    #[tokio::test]
    async fn read_line_strips_crlf() {
        let (mut server, client) = duplex(256);
        let (r, w) = tokio::io::split(client);
        let mut f = Framing::new(r, w);

        tokio::spawn(async move {
            server.write_all(b"crlf content\r\n").await.unwrap();
            server.shutdown().await.unwrap();
        });

        let line = f.read_line_with_timeout(SHORT, "read").await.unwrap();
        assert_eq!(line, "crlf content");
    }

    #[tokio::test]
    async fn read_line_multiple_sequential_lines() {
        let (mut server, client) = duplex(256);
        let (r, w) = tokio::io::split(client);
        let mut f = Framing::new(r, w);

        tokio::spawn(async move {
            server.write_all(b"first\nsecond\nthird\n").await.unwrap();
            server.shutdown().await.unwrap();
        });

        assert_eq!(
            f.read_line_with_timeout(SHORT, "r").await.unwrap(),
            "first"
        );
        assert_eq!(
            f.read_line_with_timeout(SHORT, "r").await.unwrap(),
            "second"
        );
        assert_eq!(
            f.read_line_with_timeout(SHORT, "r").await.unwrap(),
            "third"
        );
    }

    #[tokio::test]
    async fn read_line_returns_closed_on_immediate_eof() {
        let (server, client) = duplex(256);
        let (r, w) = tokio::io::split(client);
        let mut f = Framing::new(r, w);
        drop(server);

        let err = f.read_line_with_timeout(SHORT, "read").await.unwrap_err();
        assert!(matches!(err, MpdError::Transport(TransportError::Closed)));
    }

    #[tokio::test]
    async fn read_line_returns_closed_on_eof_after_partial_line() {
        let (mut server, client) = duplex(256);
        let (r, w) = tokio::io::split(client);
        let mut f = Framing::new(r, w);

        tokio::spawn(async move {
            // No trailing newline; server closes mid-line.
            server.write_all(b"partial").await.unwrap();
            server.shutdown().await.unwrap();
        });

        let err = f.read_line_with_timeout(SHORT, "read").await.unwrap_err();
        assert!(
            matches!(err, MpdError::Transport(TransportError::Closed)),
            "expected Closed for truncated line, got {err:?}"
        );
    }

    #[tokio::test]
    async fn read_line_times_out_when_peer_silent() {
        let (server, client) = duplex(256);
        let (r, w) = tokio::io::split(client);
        let mut f = Framing::new(r, w);

        // Hold the server end open but send nothing.
        let _hold = tokio::spawn(async move {
            let _keep = server;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        let err = f
            .read_line_with_timeout(VERY_SHORT, "waited")
            .await
            .unwrap_err();
        match err {
            MpdError::Timeout { operation, elapsed } => {
                assert_eq!(operation, "waited");
                assert_eq!(elapsed, VERY_SHORT);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_all_delivers_bytes_and_flushes() {
        let (mut server, client) = duplex(256);
        let (r, w) = tokio::io::split(client);
        let mut f = Framing::new(r, w);

        let reader = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = vec![0u8; 5];
            server.read_exact(&mut buf).await.unwrap();
            buf
        });

        f.write_all_with_timeout(b"hello", SHORT, "write")
            .await
            .unwrap();
        assert_eq!(reader.await.unwrap(), b"hello");
    }

    #[tokio::test]
    async fn read_line_preserves_utf8_content() {
        let (mut server, client) = duplex(256);
        let (r, w) = tokio::io::split(client);
        let mut f = Framing::new(r, w);

        tokio::spawn(async move {
            server.write_all("Bj\u{00f6}rk\n".as_bytes()).await.unwrap();
            server.shutdown().await.unwrap();
        });

        let line = f.read_line_with_timeout(SHORT, "read").await.unwrap();
        assert_eq!(line, "Bj\u{00f6}rk");
    }

    #[tokio::test]
    async fn read_line_rejects_non_utf8() {
        let (mut server, client) = duplex(256);
        let (r, w) = tokio::io::split(client);
        let mut f = Framing::new(r, w);

        tokio::spawn(async move {
            // Invalid UTF-8 byte sequence followed by newline.
            server.write_all(&[0xFF, 0xFE, b'\n']).await.unwrap();
            server.shutdown().await.unwrap();
        });

        let err = f.read_line_with_timeout(SHORT, "read").await.unwrap_err();
        match err {
            MpdError::Protocol(ProtocolError::NonUtf8(_)) => {}
            other => panic!("expected NonUtf8, got {other:?}"),
        }
    }
}
