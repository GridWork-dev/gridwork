//! The control channel: the bidirectional `stream-json` process this crate
//! spawns and talks to over stdin/stdout. Rendering is a completely
//! separate channel ([`crate::spawn_tui`]) and nothing written here ever
//! reaches it — control never rides synthetic keystrokes.
// Derivation: CLAUDE-STREAM-JSON — [`crate::control_command`] already
// carries the citation for `--input-format stream-json --output-format
// stream-json` being bidirectional over stdin/stdout; this module is that
// pipe's other end, piping both and never touching [`crate::spawn_tui`]'s
// PTY. `MAX_STREAM_LINE_BYTES` frames it the way `io_util`'s own marker
// cites (`CLAUDE-HEADLESS`, newline-delimited JSON).

use std::process::Stdio;

use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::io_util::read_bounded_line_async;
use crate::message::{Message, MessageParseError, parse_line};

/// One `stream-json` line, either direction — bounded so a runaway line
/// from the child, or a runaway line queued to it, cannot grow an unbounded
/// buffer.
pub const MAX_STREAM_LINE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("spawning the control channel: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("the child was spawned without a piped {0}")]
    MissingPipe(&'static str),
    #[error("reading the control channel: {0}")]
    Read(#[source] std::io::Error),
    #[error("a stream-json line exceeded {MAX_STREAM_LINE_BYTES} bytes")]
    LineTooLong,
    #[error("writing to the control channel: {0}")]
    Write(#[source] std::io::Error),
    #[error(transparent)]
    Parse(#[from] MessageParseError),
}

/// A running control-channel child: [`crate::control_command`], piped, with
/// its lines parsed as they arrive.
#[derive(Debug)]
pub struct StreamClient {
    child: Child,
    stdin: ChildStdin,
    stdout: tokio::io::BufReader<ChildStdout>,
}

impl StreamClient {
    /// Spawn [`crate::control_command`] with stdin and stdout piped, and
    /// stderr left to inherit — the engine's own diagnostics are not this
    /// crate's protocol to parse.
    pub fn spawn() -> Result<Self, StreamError> {
        Self::spawn_command(crate::control_command())
    }

    /// [`Self::spawn`]'s actual work, taking the command so tests can spawn
    /// a deterministically-missing binary instead of racing whatever "claude"
    /// happens to resolve to on the box running the test — which, on a
    /// machine that also runs Claude Code itself, is a real engine binary,
    /// and this crate must never shell out to one from a test.
    fn spawn_command(command: std::process::Command) -> Result<Self, StreamError> {
        let mut command: tokio::process::Command = command.into();
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command.spawn().map_err(StreamError::Spawn)?;
        let stdin = child
            .stdin
            .take()
            .ok_or(StreamError::MissingPipe("stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(StreamError::MissingPipe("stdout"))?;
        Ok(Self {
            child,
            stdin,
            stdout: tokio::io::BufReader::new(stdout),
        })
    }

    /// Read and parse the next line. `Ok(None)` is a clean end of stream —
    /// the child closed stdout, ordinarily because it exited after its
    /// terminal `result` message.
    pub async fn next_message(&mut self) -> Result<Option<Message>, StreamError> {
        read_message(&mut self.stdout).await
    }

    /// Frame one already-shaped JSON value as an NDJSON line on stdin.
    ///
    /// The exact envelope a user turn needs under `--input-format
    /// stream-json` is not stated on any of the three permitted `CLAUDE-*`
    /// pages this adapter is scoped to (escalation — see the dispatch
    /// report), so this crate does not claim to build one. This is the
    /// honest primitive instead: it frames whatever the caller hands it,
    /// asserting nothing about its shape.
    pub async fn send_line(&mut self, value: &serde_json::Value) -> Result<(), StreamError> {
        write_line(&mut self.stdin, value).await
    }

    /// The spawned child, for the caller's own lifecycle handling (wait,
    /// kill, exit-status inspection).
    pub fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

/// The read half of [`StreamClient::next_message`], generic over the reader
/// so tests can drive it against an in-memory pipe instead of a real
/// `claude` child — public CI has neither the binary nor a login
/// (`docs/PARITY.md`'s own rule), so this is the only way the framing logic
/// itself gets exercised there.
async fn read_message<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Message>, StreamError> {
    match read_bounded_line_async(reader, MAX_STREAM_LINE_BYTES).await {
        Ok(Some(line)) => Ok(Some(parse_line(&line)?)),
        Ok(None) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => Err(StreamError::LineTooLong),
        Err(e) => Err(StreamError::Read(e)),
    }
}

/// The write half of [`StreamClient::send_line`], generic for the same
/// reason as [`read_message`].
async fn write_line<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &serde_json::Value,
) -> Result<(), StreamError> {
    let mut line = serde_json::to_vec(value).map_err(|e| {
        StreamError::Write(std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
    })?;
    line.push(b'\n');
    writer.write_all(&line).await.map_err(StreamError::Write)?;
    writer.flush().await.map_err(StreamError::Write)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises the parsing/framing halves directly against an in-memory
    /// pipe, rather than a real `claude` binary — public CI has neither the
    /// binary nor a login, by `docs/PARITY.md`'s own rule, and this crate's
    /// tests must hold inside that same no-network, no-engine gate.
    #[tokio::test]
    async fn next_message_parses_lines_and_reports_clean_eof() {
        let (mut writer, reader) = tokio::io::duplex(4096);
        let mut stdout = tokio::io::BufReader::new(reader);
        writer
            .write_all(b"{\"type\":\"system\",\"subtype\":\"init\"}\n{\"type\":\"result\",\"total_cost_usd\":0.1}\n")
            .await
            .expect("write fixture");
        drop(writer);

        let message = read_message(&mut stdout)
            .await
            .expect("read")
            .expect("a message");
        assert!(message.is_session_start());

        let message = read_message(&mut stdout)
            .await
            .expect("read")
            .expect("a message");
        assert!(message.is_result());

        assert_eq!(read_message(&mut stdout).await.expect("read"), None);
    }

    #[tokio::test]
    async fn a_malformed_line_from_the_child_is_a_typed_error_not_a_panic() {
        let (mut writer, reader) = tokio::io::duplex(4096);
        let mut stdout = tokio::io::BufReader::new(reader);
        writer
            .write_all(b"not json\n")
            .await
            .expect("write fixture");
        drop(writer);

        let error = read_message(&mut stdout)
            .await
            .expect_err("must be refused");
        assert!(matches!(error, StreamError::Parse(_)));
    }

    #[tokio::test]
    async fn an_oversized_line_from_the_child_is_bounded_not_a_panic() {
        let (mut writer, reader) = tokio::io::duplex(MAX_STREAM_LINE_BYTES + 4096);
        let mut stdout = tokio::io::BufReader::new(reader);
        let payload = vec![b'x'; MAX_STREAM_LINE_BYTES + 1];
        tokio::spawn(async move {
            let _ = writer.write_all(&payload).await;
            let _ = writer.write_all(b"\n").await;
        });

        let error = read_message(&mut stdout)
            .await
            .expect_err("must be refused");
        assert!(matches!(error, StreamError::LineTooLong));
    }

    #[tokio::test]
    async fn send_line_frames_exactly_one_ndjson_line() {
        let (mut client_side, mut server_side) = tokio::io::duplex(4096);
        let value = serde_json::json!({"type": "user", "text": "hi"});
        write_line(&mut client_side, &value).await.expect("write");
        drop(client_side);

        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut server_side, &mut buf)
            .await
            .expect("read back");

        let mut expected = serde_json::to_vec(&value).expect("serialize");
        expected.push(b'\n');
        assert_eq!(buf, expected);
        // Exactly one line: writing again must not have appended to it.
        assert_eq!(buf.iter().filter(|&&b| b == b'\n').count(), 1);
    }

    // `tokio::process::Command::spawn` needs a runtime bound to the process
    // reactor (it panics without one), so this is `#[tokio::test]` even
    // though `StreamClient::spawn_command` itself is synchronous.
    //
    // This deliberately does NOT go through `StreamClient::spawn` /
    // `control_command()`, which name the real "claude" binary — on a
    // machine that also runs Claude Code (this one included: an earlier
    // version of this test spawned a real, live `claude` process and leaked
    // its stderr into the test log), that binary is genuinely on `PATH`.
    // Spawning it from a unit test would violate this crate's own "no
    // engine binary" test contract, so this exercises the same error path
    // against a name nothing resolves to instead.
    #[tokio::test]
    async fn spawning_a_missing_binary_is_a_typed_error_not_a_panic() {
        let command = std::process::Command::new("gwk-adapter-claude-test-nonexistent-binary");
        let error = StreamClient::spawn_command(command)
            .expect_err("a nonexistent binary must fail to spawn");
        assert!(matches!(error, StreamError::Spawn(_)));
    }
}
