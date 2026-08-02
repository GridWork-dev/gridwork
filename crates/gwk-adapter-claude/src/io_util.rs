//! One bounded, newline-delimited read, in both the async flavor (the
//! stream-json child and the relay's server half both run on Tokio) and the
//! sync flavor (the relay's hook-side client is a plain blocking process, on
//! purpose — see `relay::relay_ask`).
//!
//! Neither `tokio::io::AsyncBufReadExt::read_until` nor
//! `std::io::BufRead::read_until` takes a size cap: both grow their buffer
//! until they see the delimiter or the peer hangs up, which turns a runaway
//! or malicious peer into unbounded memory growth. Scanning each fill of the
//! reader's own internal buffer for `\n` and erroring once the accumulated
//! line crosses `max_bytes` bounds that at `max_bytes` plus one buffer's
//! worth of overshoot, never further.
// Derivation: CLAUDE-HEADLESS — "stream-json: newline-delimited JSON for
// real-time streaming" is the framing this reader implements: one JSON
// value per `\n`-terminated line, for both the engine's own output and
// (`relay.rs`) this crate's internal relay wire, which is framed the same
// way on purpose so one reader serves both.

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

/// The blocking twin of [`read_bounded_line_async`], for the relay client
/// that deliberately never touches a Tokio runtime.
pub(crate) fn read_bounded_line_sync<R: io::BufRead>(
    reader: &mut R,
    max_bytes: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    loop {
        let buf = reader.fill_buf()?;
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

    #[test]
    fn sync_reader_matches_the_async_one() {
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(b"a\nbb\n".to_vec()));
        assert_eq!(
            read_bounded_line_sync(&mut reader, 1024).expect("read"),
            Some(b"a".to_vec())
        );
        assert_eq!(
            read_bounded_line_sync(&mut reader, 1024).expect("read"),
            Some(b"bb".to_vec())
        );
        assert_eq!(
            read_bounded_line_sync(&mut reader, 1024).expect("read"),
            None
        );
    }

    #[test]
    fn sync_reader_bounds_a_line_with_no_terminator() {
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(vec![b'y'; 5_000]));
        let error = read_bounded_line_sync(&mut reader, 64)
            .expect_err("oversized line with no terminator must be refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
