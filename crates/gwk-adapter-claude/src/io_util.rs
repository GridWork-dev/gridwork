//! One bounded, newline-delimited read for the async stream-json framing
//! this crate's control channel uses (`stream.rs`). A sync twin joins this
//! module once the `PreToolUse` relay's blocking hook-side client needs one.
//!
//! `tokio::io::AsyncBufReadExt::read_until` takes no size cap: it grows its
//! buffer until it sees the delimiter or the peer hangs up, which turns a
//! runaway or malicious peer into unbounded memory growth. Scanning each
//! fill of the reader's own internal buffer for `\n` and erroring once the
//! accumulated line crosses `max_bytes` bounds that at `max_bytes` plus one
//! buffer's worth of overshoot, never further.
// Derivation: CLAUDE-HEADLESS — "stream-json: newline-delimited JSON for
// real-time streaming" is the framing this reader implements: one JSON
// value per `\n`-terminated line.

use std::io;

/// Read one `\n`-terminated line, without its terminator. `Ok(None)` is a
/// clean end of stream with nothing left to return; a line longer than
/// `max_bytes` is `Err` rather than a silently truncated `Ok`.
pub(crate) async fn read_bounded_line_async<R>(
    reader: &mut R,
    max_bytes: usize,
) -> io::Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;

    let mut out = Vec::new();
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return Ok(if out.is_empty() { None } else { Some(out) });
        }
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            out.extend_from_slice(&buf[..pos]);
            let consumed = pos + 1;
            reader.consume(consumed);
            return if out.len() > max_bytes {
                Err(too_long(max_bytes))
            } else {
                Ok(Some(out))
            };
        }
        let n = buf.len();
        out.extend_from_slice(buf);
        reader.consume(n);
        if out.len() > max_bytes {
            return Err(too_long(max_bytes));
        }
    }
}

fn too_long(max_bytes: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("line exceeded {max_bytes} bytes with no terminator"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn async_reader_splits_on_newline_and_reports_clean_eof() {
        let mut reader = std::io::Cursor::new(b"one\ntwo\nthree".to_vec());
        assert_eq!(
            read_bounded_line_async(&mut reader, 1024)
                .await
                .expect("read"),
            Some(b"one".to_vec())
        );
        assert_eq!(
            read_bounded_line_async(&mut reader, 1024)
                .await
                .expect("read"),
            Some(b"two".to_vec())
        );
        // No trailing newline: the partial line at EOF is still returned.
        assert_eq!(
            read_bounded_line_async(&mut reader, 1024)
                .await
                .expect("read"),
            Some(b"three".to_vec())
        );
        assert_eq!(
            read_bounded_line_async(&mut reader, 1024)
                .await
                .expect("read"),
            None
        );
    }

    #[tokio::test]
    async fn async_reader_bounds_a_line_with_no_terminator() {
        let payload = vec![b'x'; 10_000];
        let mut reader = std::io::Cursor::new(payload);
        let error = read_bounded_line_async(&mut reader, 100)
            .await
            .expect_err("oversized line with no terminator must be refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn async_reader_bounds_a_line_that_does_terminate() {
        let mut payload = vec![b'x'; 10_000];
        payload.push(b'\n');
        let mut reader = std::io::Cursor::new(payload);
        let error = read_bounded_line_async(&mut reader, 100)
            .await
            .expect_err("oversized terminated line must still be refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
