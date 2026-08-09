//! A child process on a PTY, and the loop that keeps its output flowing into
//! a [`Grid`].
//!
//! # Why the PTY comes from a dependency
//!
//! Allocating a PTY and handing it to a child means `setsid` and an ioctl in
//! the window between `fork` and `exec` — in Rust, `CommandExt::pre_exec`,
//! which is `unsafe`. This workspace sets `unsafe_code = "forbid"`, a level
//! `#[allow]` cannot reach past, so that code can never live here. It is not a
//! preference: either the PTY comes from a dependency or the crate does not
//! compile.

use pty_process::{Pts, Pty, Size};
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::Grid;

/// How much child output one [`Session::pump`] can take.
///
/// ponytail: a fixed buffer, not a growable one. A terminal that has fallen
/// 8 KiB behind is already going to coalesce those writes into one repaint,
/// so a bigger read would save syscalls nobody is counting. Revisit if a
/// profile ever shows `read` dominating.
const READ_CHUNK: usize = 8192;

/// Why a [`Session`] could not be started.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    /// libghostty-vt refused the grid dimensions, which it does for a zero in
    /// either axis.
    #[error("a terminal cannot be {cols}x{rows}")]
    Dimensions { cols: u16, rows: u16 },
    /// The PTY could not be allocated, or the child could not be started on it.
    #[error("could not put a child on a pty: {0}")]
    Pty(#[from] pty_process::Error),
}

/// A child process attached to a PTY, and the grid its output paints.
///
/// The read loop is the caller's, not a task spawned behind your back: call
/// [`pump`](Self::pump) until it returns `0`. That keeps cancellation, back
/// pressure, and the choice of runtime where the caller can see them, and it
/// means the grid needs no lock — a `&mut self` read is the only writer.
pub struct Session {
    pty: Pty,
    child: tokio::process::Child,
    grid: Grid,
}

impl Session {
    /// Allocate a PTY of `cols` × `rows` and start `command` on it.
    ///
    /// The size is set on the PTY *before* the child starts, so a child that
    /// checks its window size on the first line — which is most full-screen
    /// programs — sees the real one rather than the 0×0 a fresh PTY reports.
    pub fn spawn(command: pty_process::Command, cols: u16, rows: u16) -> Result<Self, SpawnError> {
        let grid = Grid::new(cols, rows).ok_or(SpawnError::Dimensions { cols, rows })?;
        let (pty, pts): (Pty, Pts) = pty_process::open()?;
        pty.resize(Size::new(rows, cols))?;
        let child = command.spawn(pts)?;
        Ok(Self { pty, child, grid })
    }

    /// Read whatever the child has written and feed it to the parser.
    ///
    /// Returns the number of bytes taken; `0` means the child hung up and no
    /// further call will return more.
    pub async fn pump(&mut self) -> io::Result<usize> {
        self.pump_chunk().await.map(|chunk| chunk.len())
    }

    /// [`pump`](Self::pump), but returning the bytes that reached the parser.
    ///
    /// A caller that records the stream for later replay needs the chunk
    /// itself, not its length — [`crate::record::Event::Output`] stores the
    /// bytes exactly as read. An empty vector means the child hung up,
    /// matching `pump`'s `0`.
    ///
    /// A chunk reaches the parser exactly as it arrived — see the derivation
    /// note on the write below.
    pub async fn pump_chunk(&mut self) -> io::Result<Vec<u8>> {
        let mut buf = [0u8; READ_CHUNK];
        match self.pty.read(&mut buf).await {
            Ok(0) => Ok(Vec::new()),
            Ok(n) => {
                // Derivation: ECMA-48 §5.4 — a control sequence is CSI, then
                // parameter bytes, then intermediate bytes, then a final byte.
                // A read boundary can fall anywhere inside that, so the chunk
                // goes to the parser exactly as it arrived. Splitting on what
                // looks like a sequence edge, or holding a partial one back to
                // "complete" it, is how a reader corrupts input the parser
                // would have handled correctly on its own.
                self.grid.write(&buf[..n]);
                Ok(buf[..n].to_vec())
            }
            // Derivation: CAP-002 — reading a PTY master whose slave has closed
            // fails with EIO on Linux instead of reporting end-of-file. The
            // child exiting is the normal case, so surfacing that errno would
            // make every clean exit look like an I/O failure.
            //
            // A capture rather than a spec citation because there is no spec to
            // cite. POSIX gives `read` an EIO for a background process group
            // reading its controlling terminal — a different condition, and
            // citing it would be a false citation wearing a real one's clothes.
            // No man page installed here documents the slave-closed case at all.
            //
            // Deleting this arm was tried: every test below that drains a child
            // to EOF fails on Linux, three of them at the time of writing. The
            // mechanism rather than the count, because the count was written as
            // "both" and was stale within the same commit that added a third
            // such test two screens further down.
            //
            // So it is load-bearing here. Whether any other platform needs it is
            // NOT established by this repository, and an earlier version of this
            // comment asserted that macOS returns 0 instead — nothing checks
            // that. The macOS job builds the default members, which do not
            // include this crate. If a platform does report end-of-file, the
            // `Ok(0)` arm already carries it.
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) => {
                Ok(Vec::new())
            }
            Err(e) => Err(e),
        }
    }

    /// Send input to the child, as a terminal would.
    pub async fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.pty.write_all(bytes).await
    }

    /// Tell both the grid and the child about a new window size.
    ///
    /// The grid goes first because it is the half that can refuse cheaply, and
    /// a PTY failure rolls it back to the size the PTY is still holding — so on
    /// either single failure the two halves agree.
    ///
    /// Not all-or-nothing in general, and the difference is worth stating
    /// rather than rounding off. Rolling back is two fallible steps — a syscall
    /// to ask what to restore to, then a real reflow to get there — and either
    /// can fail on its own. That needs a second independent failure, and if it
    /// happens the halves are left disagreeing and the PTY's error is what
    /// surfaces, because it is the cause. Claiming atomicity here would be the
    /// same defect this method has already carried once: a comment asserting a
    /// property the code does not have.
    ///
    /// Note the scope: this is about the two halves *this method* moves. A
    /// child that resizes its own terminal desynchronizes them without going
    /// through here at all, and nothing in this type notices — see
    /// `a_child_can_move_the_pty_size_behind_the_grids_back`.
    // Derivation: POSIX-WINSIZE — `tcsetwinsize()` delivers SIGWINCH to the
    // foreground process group when the terminal size WAS CHANGED, and
    // explicitly delivers nothing when it is set to the value it already had.
    // Changed, not merely successful: a no-op resize returns success and wakes
    // nobody, so "the call returned Ok" is not the same claim as "the child has
    // been told" and this comment must not conflate them.
    //
    // That signal is the child's only cue to re-read the size and repaint, so a
    // size the two halves disagree about is not self-correcting — whichever one
    // is wrong stays wrong until something resizes again. Hence the rollback.
    //
    // It is NOT an interleaving hazard, which is what the first version of this
    // comment claimed. `pump` and `resize` both take `&mut self` and `resize`
    // has no await point, so a repaint cannot land mid-resize; the child's
    // bytes wait in the PTY buffer until a `pump` that cannot start until this
    // returns. Stating a mechanism that can't fire is worse than stating none:
    // the next reader checks it, finds it inapplicable, and reorders the lines.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), SpawnError> {
        self.grid
            .resize(cols, rows)
            .ok_or(SpawnError::Dimensions { cols, rows })?;
        // Get, then set, on the same fd. The read is what makes this a relay
        // rather than a blind write: when the PTY already holds the requested
        // size — because the child moved it there itself — there is nothing
        // to relay, and skipping the set leaves the child exactly as
        // undisturbed as the delivers-nothing rule above says a no-op set
        // would. The grid half has already reflowed either way, so this is
        // also the path that lets the two halves CONVERGE after a
        // child-initiated move, instead of the manager stomping it.
        //
        // Derivation: POSIX-WINSIZE §APPLICATION USAGE (informative) — the
        // two-sentence relay guidance: the manager relays a new size to the
        // child, and a size set from the subsidiary side is one the manager
        // "should attempt to change the screen to reflect". The skip below
        // is that reflection: a size already held because the child moved
        // it there is reflected, not overwritten. Informative text, cited
        // as such: it guides the shape of this method, it does not bind it.
        if let Ok(held) = rustix::termios::tcgetwinsize(&self.pty)
            && (held.ws_col, held.ws_row) == (cols, rows)
        {
            return Ok(());
        }
        if let Err(e) = self.pty.resize(Size::new(rows, cols)) {
            // Ask the kernel what to roll back to rather than remembering it.
            //
            // This restored a field updated after every successful resize, and
            // that field is the same number as the PTY's right up until it is
            // not: `tcsetwinsize()` is available to the child too, and `stty
            // rows 50 cols 100` is an ordinary line in a shell script. After
            // one of those, a remembered value names a size nothing is at, and
            // rolling back to it would move the grid away from the PTY rather
            // than onto it — turning the one case this branch exists to prevent
            // into the case it causes. The kernel is the only party here that
            // cannot be stale.
            //
            // Derivation: POSIX-WINSIZE — on failure `tcsetwinsize()` returns -1
            // and the terminal window size SHALL NOT be changed. That is what
            // makes `tcgetwinsize` the right question here rather than a guess:
            // the call above failed, so the size it reports is the one the PTY
            // was holding when it did. The fact is on the same page as the
            // SIGWINCH rule cited above the method, but it is a different
            // sentence doing different work, so it gets its own marker instead
            // of leaning on that one's proximity.
            //
            // Both results are discarded because nothing is left to do with
            // either: if the read or the reflow fails, the grid stays at the
            // new size against a PTY at the old one whatever we return, and the
            // PTY error propagates because it is the cause.
            //
            // ponytail: no test seeds this. `tcsetwinsize()` fails on EBADF,
            // ENOTTY, EIO (orphaned process group) or EINVAL, none of which is
            // reachable from safe code holding a live `Session` — the fd is
            // owned and known to be a terminal. Written anyway because EIO is
            // reachable in production even if not from here.
            if let Ok(held) = rustix::termios::tcgetwinsize(&self.pty) {
                let _ = self.grid.resize(held.ws_col, held.ws_row);
            }
            return Err(e.into());
        }
        Ok(())
    }

    /// Adopt the size the PTY actually holds — the child's half of the relay.
    ///
    /// `tcsetwinsize()` is not reserved to whoever opened the PTY: a child
    /// running `stty rows 50 cols 100` moves the kernel's size without this
    /// type hearing about it, and until this call the grid keeps reflowing
    /// output for a size nothing is at. Reading the fd and reflowing to what
    /// it holds is the other direction of [`resize`](Self::resize)'s relay —
    /// get on the same fd, no set, because the kernel copy is already the
    /// truth being adopted.
    ///
    /// Returns the size the PTY holds. If the grid refuses it (a zero in
    /// either axis is refusable — the child can put one there), the grid is
    /// left where it was and the returned pair still reports the kernel's
    /// answer, so the caller can see the two halves disagree rather than
    /// being told they converged.
    // Derivation: POSIX-WINSIZE §APPLICATION USAGE (informative) — the
    // manager-side relay guidance covers both directions of a size relay;
    // this is the read-and-adopt half. The delivers-nothing rule on the
    // marker above `resize` is why adopting a size never needs a set: the
    // kernel already holds it, and a set to the held value would tell the
    // child nothing anyway.
    pub fn pull_size(&mut self) -> io::Result<(u16, u16)> {
        let held = rustix::termios::tcgetwinsize(&self.pty)
            .map_err(|errno| io::Error::from_raw_os_error(errno.raw_os_error()))?;
        if self.grid.size() != Some((held.ws_col, held.ws_row)) {
            let _ = self.grid.resize(held.ws_col, held.ws_row);
        }
        Ok((held.ws_col, held.ws_row))
    }

    /// The authoritative grid.
    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    /// Wait for the child to exit.
    pub async fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    /// Kill the child and reap it.
    ///
    /// Dropping a `Session` closes the PTY master, which only *signals* the
    /// child (SIGHUP) — a child that ignores it lingers. A supervisor telling
    /// a session to stop needs the unconditional form, and it needs the reap,
    /// or the exit status is left for no one and the process table keeps a
    /// zombie until the supervisor itself exits.
    // Derivation: POSIX-TERM §11.1.10 — on a modem disconnect, and only with
    // CLOCAL unset, "the SIGHUP signal shall be sent to the controlling
    // process for which the terminal is the controlling terminal": one
    // process, not a process group and not the session, so even a child of
    // that child sees nothing from the hangup — which strengthens, not
    // weakens, this method's reason to exist. That closing the master
    // side presents a disconnect to the slave side is this crate's reading
    // of the PTY pairing, not a sentence the chapter contains (its
    // pseudo-terminal mentions — the §11.1.1 open path, §11.2.1's
    // O_TTY_INIT parenthetical, and §11.2.4's two control-mode mentions —
    // are silent on it); CAP-002 registers the mirror direction of the same
    // pairing for the same reason. `kill()` is
    // `child.kill()`, which does not ride the hangup path.
    pub async fn kill(&mut self) -> io::Result<()> {
        self.child.kill().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pump until the child hangs up, so a test asserts against a finished
    /// screen rather than whatever had arrived by the time it looked.
    async fn drain(session: &mut Session) {
        while session.pump().await.expect("pty read failed") > 0 {}
    }

    #[tokio::test]
    async fn a_childs_output_reaches_the_grid() {
        let mut session = Session::spawn(
            pty_process::Command::new("/bin/sh")
                .arg("-c")
                .arg("printf 'hello'"),
            80,
            24,
        )
        .expect("spawning /bin/sh on a pty");

        drain(&mut session).await;

        assert_eq!(
            session.grid().row_text(0).as_deref(),
            Some("hello"),
            "the child's stdout should be parsed into row 0"
        );
    }

    #[tokio::test]
    async fn the_child_is_driven_by_what_we_send_it() {
        // `cat` proves the loop end to end: nothing reaches the grid unless
        // our write got to the child AND its echo came back through the
        // parser. A child that only prints would pass with a dead write half.
        let mut session = Session::spawn(pty_process::Command::new("/bin/cat"), 80, 24)
            .expect("spawning /bin/cat on a pty");

        session.send(b"ping\n").await.expect("writing to the pty");
        // Read until the echo shows up rather than reading a fixed number of
        // times: the terminal echo and `cat`'s own copy can arrive in one
        // chunk or two, and asserting on a count would make this flaky.
        while session.grid().row_text(0).as_deref() != Some("ping") {
            assert!(
                session.pump().await.expect("pty read failed") > 0,
                "eof before echo"
            );
        }

        session.send(b"\x04").await.expect("sending EOT");
        drain(&mut session).await;
        assert!(session.wait().await.expect("waiting on cat").success());
    }

    #[tokio::test]
    async fn pump_chunk_returns_the_bytes_the_grid_was_fed() {
        let mut session = Session::spawn(
            pty_process::Command::new("/bin/sh")
                .arg("-c")
                .arg("printf 'hello'"),
            80,
            24,
        )
        .expect("spawning /bin/sh on a pty");

        // Collect until hang-up, so the assertion sees the whole stream no
        // matter how the reads happened to chunk it.
        let mut collected = Vec::new();
        loop {
            let chunk = session.pump_chunk().await.expect("pty read failed");
            if chunk.is_empty() {
                break;
            }
            collected.extend_from_slice(&chunk);
        }

        assert_eq!(
            collected, b"hello",
            "the returned chunks should be exactly what the child wrote"
        );
        assert_eq!(
            session.grid().row_text(0).as_deref(),
            Some("hello"),
            "and the same bytes should have reached the grid"
        );
    }

    #[tokio::test]
    async fn kill_stops_a_child_that_would_otherwise_run_forever() {
        // `cat` with no input never exits on its own; only the kill ends it.
        let mut session = Session::spawn(pty_process::Command::new("/bin/cat"), 80, 24)
            .expect("spawning /bin/cat on a pty");

        session.kill().await.expect("killing the child");
        let status = session.wait().await.expect("reaping the child");
        assert!(!status.success(), "a killed child did not exit cleanly");
    }

    #[tokio::test]
    async fn a_zero_dimension_is_refused_before_a_child_is_started() {
        // A `Session` holds a live child, so it is deliberately not `Debug` —
        // hence matching on the result rather than `expect_err`.
        assert!(matches!(
            Session::spawn(pty_process::Command::new("/bin/true"), 0, 24),
            Err(SpawnError::Dimensions { cols: 0, rows: 24 })
        ));
    }

    #[tokio::test]
    async fn a_refused_resize_does_not_reach_the_child() {
        // The invariant the ordering in `resize` exists for, and the one a
        // comment alone would not hold: when the grid rejects a size, the PTY
        // must still be carrying the old one. Swapping the two lines leaves
        // the child believing a size the grid never adopted, with nothing
        // downstream to reconcile them — so this test is what makes that
        // reorder fail rather than merely look wrong to a careful reader.
        let mut session = Session::spawn(pty_process::Command::new("/bin/cat"), 80, 24)
            .expect("spawning /bin/cat on a pty");

        assert!(matches!(
            session.resize(0, 24),
            Err(SpawnError::Dimensions { cols: 0, rows: 24 })
        ));

        // Asked of the kernel rather than of our own bookkeeping: a field we
        // set ourselves would agree with us whichever order the lines ran in.
        let size = rustix::termios::tcgetwinsize(&session.pty).expect("reading the pty size");
        assert_eq!(
            (size.ws_col, size.ws_row),
            (80, 24),
            "the child was told a size the grid refused"
        );

        assert_eq!(session.grid().size(), Some((80, 24)));
    }

    #[tokio::test]
    async fn the_child_is_told_the_size_on_the_axes_we_meant() {
        // `Session` takes (cols, rows) and `Size::new` takes (rows, cols), so
        // every call site flips the pair by hand. That conversion appears twice
        // — once in `spawn`, once in `resize` — and until this test nothing
        // checked either against the kernel. A transposition gives the child a
        // 24-column terminal while the grid is 80 wide: every full-screen
        // program wraps at 24, and the grid faithfully records the wrapped
        // output, so the screens still agree and the bug reads as the child's.
        //
        // The numbers are deliberately unequal and unequal AGAIN after the
        // resize; a square terminal would pass this test transposed.
        let mut session = Session::spawn(pty_process::Command::new("/bin/cat"), 80, 24)
            .expect("spawning /bin/cat on a pty");

        let spawned = rustix::termios::tcgetwinsize(&session.pty).expect("reading the pty size");
        assert_eq!(
            (spawned.ws_col, spawned.ws_row),
            (80, 24),
            "spawn put the axes on the pty the wrong way round"
        );

        session.resize(100, 30).expect("resizing to a valid size");

        let resized = rustix::termios::tcgetwinsize(&session.pty).expect("reading the pty size");
        assert_eq!(
            (resized.ws_col, resized.ws_row),
            (100, 30),
            "resize put the axes on the pty the wrong way round"
        );
        assert_eq!(session.grid().size(), Some((100, 30)));
    }

    /// Pump until some row contains `needle`, failing on hang-up. The trap
    /// tests below all follow one shape: install a handler, print a marker,
    /// block; the test waits for the marker instead of sleeping and hoping.
    async fn drain_until(session: &mut Session, needle: &str) {
        while !grid_contains(session, needle) {
            assert!(
                session.pump().await.expect("pty read failed") > 0,
                "eof before {needle:?} appeared"
            );
        }
    }

    fn grid_contains(session: &Session, needle: &str) -> bool {
        let Some((_, rows)) = session.grid().size() else {
            return false;
        };
        (0..rows).any(|y| {
            session
                .grid()
                .row_text(y)
                .is_some_and(|text| text.contains(needle))
        })
    }

    fn rows_containing(session: &Session, needle: &str) -> usize {
        let Some((_, rows)) = session.grid().size() else {
            return 0;
        };
        (0..rows)
            .filter(|y| {
                session
                    .grid()
                    .row_text(*y)
                    .is_some_and(|text| text.contains(needle))
            })
            .count()
    }

    /// The line discipline's special character for `index`, read from the
    /// same fd the session holds — asked of the terminal rather than assumed,
    /// because no standard fixes the byte values and a hardcoded ^C would be
    /// an assertion this repository has no row for.
    fn special_code(session: &Session, index: rustix::termios::SpecialCodeIndex) -> u8 {
        let attrs = rustix::termios::tcgetattr(&session.pty).expect("reading termios");
        attrs.special_codes[index]
    }

    #[tokio::test]
    async fn the_child_holds_a_controlling_terminal_and_reads_as_the_foreground_group() {
        // Derivation: POSIX-TERM §11.1.1 — opening /dev/tty is the named
        // way "to obtain a file descriptor for the controlling terminal".
        // That a process with no controlling terminal has nothing for that
        // open to resolve — so a successful open asserts the ctty exists —
        // is this crate's reading of that access path, not a sentence the
        // chapter contains. And
        // §11.1.4 — a process in the foreground process group may read from
        // the controlling terminal; only background process groups draw
        // SIGTTIN. The read completing proves the child sits in the
        // foreground group, not merely that its stdin is a terminal.
        let mut session = Session::spawn(
            pty_process::Command::new("/bin/sh")
                .arg("-c")
                .arg("exec 3</dev/tty && echo READY && read -r line <&3 && echo \"GOT:$line\""),
            80,
            24,
        )
        .expect("spawning /bin/sh on a pty");

        drain_until(&mut session, "READY").await;
        session.send(b"ping\n").await.expect("writing to the pty");
        drain_until(&mut session, "GOT:ping").await;
        assert!(session.wait().await.expect("reaping").success());
    }

    #[tokio::test]
    async fn the_intr_character_reaches_the_foreground_group_as_sigint() {
        // Derivation: POSIX-TERM §11.1.9 — the INTR special character
        // generates a SIGINT signal, sent to all processes in the foreground
        // process group for which the terminal is the controlling terminal.
        // The byte itself is read from this terminal's own c_cc array: the
        // standard names the character's JOB, not its value.
        let mut session = Session::spawn(
            pty_process::Command::new("/bin/sh")
                .arg("-c")
                .arg("trap 'echo CAUGHT-INT; exit 42' INT; echo READY; read -r line; exit 9"),
            80,
            24,
        )
        .expect("spawning /bin/sh on a pty");

        drain_until(&mut session, "READY").await;
        let intr = special_code(&session, rustix::termios::SpecialCodeIndex::VINTR);
        session.send(&[intr]).await.expect("sending INTR");
        drain_until(&mut session, "CAUGHT-INT").await;
        let status = session.wait().await.expect("reaping");
        assert_eq!(status.code(), Some(42), "the trap's exit code, not read's");
    }

    #[tokio::test]
    async fn the_quit_character_reaches_the_foreground_group_as_sigquit() {
        // Derivation: POSIX-TERM §11.1.9 — the QUIT special character
        // generates a SIGQUIT signal, sent to all processes in the
        // foreground process group. Trapped rather than defaulted: the
        // trap turns the delivery into a line the test can read and a
        // chosen exit code it can assert.
        let mut session = Session::spawn(
            pty_process::Command::new("/bin/sh")
                .arg("-c")
                .arg("trap 'echo CAUGHT-QUIT; exit 43' QUIT; echo READY; read -r line; exit 9"),
            80,
            24,
        )
        .expect("spawning /bin/sh on a pty");

        drain_until(&mut session, "READY").await;
        let quit = special_code(&session, rustix::termios::SpecialCodeIndex::VQUIT);
        session.send(&[quit]).await.expect("sending QUIT");
        drain_until(&mut session, "CAUGHT-QUIT").await;
        assert_eq!(session.wait().await.expect("reaping").code(), Some(43));
    }

    #[tokio::test]
    async fn the_susp_character_reaches_the_foreground_group_as_sigtstp() {
        // Derivation: POSIX-TERM §11.1.9 — the SUSP special character
        // generates a SIGTSTP signal, sent to all processes in the
        // foreground process group. Trapped so the child reports instead of
        // stopping: a stopped child never exits, and this test's subject is
        // the delivery, not the default disposition.
        let mut session = Session::spawn(
            pty_process::Command::new("/bin/sh")
                .arg("-c")
                .arg("trap 'echo CAUGHT-TSTP; exit 44' TSTP; echo READY; read -r line; exit 9"),
            80,
            24,
        )
        .expect("spawning /bin/sh on a pty");

        drain_until(&mut session, "READY").await;
        let susp = special_code(&session, rustix::termios::SpecialCodeIndex::VSUSP);
        session.send(&[susp]).await.expect("sending SUSP");
        drain_until(&mut session, "CAUGHT-TSTP").await;
        assert_eq!(session.wait().await.expect("reaping").code(), Some(44));
    }

    #[tokio::test]
    async fn a_background_read_draws_sigttin_and_its_write_passes_without_tostop() {
        // Derivation: POSIX-TERM §11.1.4 — a process in a background process
        // group reading from its controlling terminal has SIGTTIN sent to
        // its group; and, absent TOSTOP, a background write is ALLOWED. Both
        // rules ride one script: the background job's trap reports the
        // SIGTTIN by writing to the very terminal it may not read — the
        // report arriving is the write rule, the report firing is the read
        // rule.
        //
        // bash explicitly, not /bin/sh: the script needs `set -m` honored in
        // a non-interactive shell so the background job lands in its own
        // process group — without that there is no background group and
        // nothing to deliver SIGTTIN to.
        // The handler exits the job: a returning handler would let the shell
        // retry the read, draw SIGTTIN again, and report forever — a livelock
        // dressed as enthusiasm. One delivery is the fact being asserted.
        let mut session = Session::spawn(
            pty_process::Command::new("/bin/bash").arg("-c").arg(
                "set -m; echo READY; \
                 { trap 'echo BG-TTIN; exit 7' TTIN; read -r x < /dev/tty; } & \
                 wait; echo BYE",
            ),
            80,
            24,
        )
        .expect("spawning /bin/bash on a pty");

        drain_until(&mut session, "READY").await;
        drain_until(&mut session, "BG-TTIN").await;
        drain_until(&mut session, "BYE").await;
        assert!(session.wait().await.expect("reaping").success());
    }

    #[tokio::test]
    async fn a_background_write_with_tostop_set_draws_sigttou() {
        // Derivation: POSIX-TERM §11.1.4 — with TOSTOP set, a process in a
        // background process group writing to its controlling terminal has
        // SIGTTOU sent to its group. The trap's own report goes to a file,
        // not the terminal — under TOSTOP the terminal is exactly where a
        // background report may not go, so the foreground shell reads the
        // file and speaks for it.
        let mut session = Session::spawn(
            pty_process::Command::new("/bin/bash").arg("-c").arg(
                "set -m; m=$(mktemp); stty tostop; echo READY; \
                 { trap 'echo mark > \"$m\"' TTOU; echo attempt > /dev/tty; } & \
                 wait; stty -tostop; \
                 if [ -s \"$m\" ]; then echo CAUGHT-TTOU; fi; rm -f \"$m\"; echo BYE",
            ),
            80,
            24,
        )
        .expect("spawning /bin/bash on a pty");

        drain_until(&mut session, "CAUGHT-TTOU").await;
        drain_until(&mut session, "BYE").await;
        assert!(session.wait().await.expect("reaping").success());
    }

    #[tokio::test]
    async fn a_resize_delivers_sigwinch_and_an_identical_reset_is_observably_silent() {
        // Derivation: POSIX-WINSIZE — `tcsetwinsize()` delivers SIGWINCH to
        // the foreground process group when the size WAS CHANGED, and
        // delivers nothing when it is set to the value it already had. Both
        // halves of that sentence are asserted here by counting: two real
        // changes with an identical re-set between them must produce exactly
        // two reports — a third report is the delivers-nothing clause
        // failing, a missing one is the delivery clause failing.
        //
        // The ACK protocol is what makes the count safe to read without
        // trusting either signal timing or the shell's read-interruption
        // behavior. Each sent line comes back as an ACK, and the shell runs
        // pending traps at command boundaries — so by the time an ACK is on
        // the grid, every report the preceding resize was ever going to
        // produce is on it too. Counting between ACKs also defeats
        // coalescing: a wrongly-delivered re-set signal cannot hide inside
        // the next real one's delivery, because an ACK separates them.
        let mut session = Session::spawn(
            pty_process::Command::new("/bin/sh").arg("-c").arg(
                "trap 'echo GOT-WINCH' WINCH; echo READY; \
                 while read -r line; do \
                   echo \"ACK:$line\"; \
                   if [ \"$line\" = done ]; then break; fi; \
                 done; echo BYE",
            ),
            80,
            24,
        )
        .expect("spawning /bin/sh on a pty");

        drain_until(&mut session, "READY").await;

        session.resize(100, 30).expect("first real resize");
        session.send(b"t1\n").await.expect("first ack round-trip");
        drain_until(&mut session, "ACK:t1").await;
        assert_eq!(rows_containing(&session, "GOT-WINCH"), 1);

        // The identical re-set: same fd, same size, nothing to say — and its
        // own ACK round-trip, so a delivery here would be visible on its own
        // rather than coalescing into the next change's signal.
        session.resize(100, 30).expect("identical re-set");
        session.send(b"t2\n").await.expect("second ack round-trip");
        drain_until(&mut session, "ACK:t2").await;
        assert_eq!(
            rows_containing(&session, "GOT-WINCH"),
            1,
            "an identical re-set must be observably silent to the child"
        );

        session.resize(90, 28).expect("second real resize");
        session.send(b"done\n").await.expect("releasing the child");
        drain_until(&mut session, "BYE").await;
        assert_eq!(rows_containing(&session, "GOT-WINCH"), 2);
        assert!(session.wait().await.expect("reaping").success());
    }

    #[tokio::test]
    async fn pull_size_adopts_a_child_moved_size() {
        // The other direction of the relay: the child moves the kernel's
        // size, and the manager reads-and-adopts instead of stomping it. The
        // convergence `a_child_can_move_the_pty_size_behind_the_grids_back`
        // proves is missing is exactly what this call supplies.
        let mut session = Session::spawn(
            pty_process::Command::new("/bin/sh")
                .arg("-c")
                .arg("stty rows 50 cols 100"),
            80,
            24,
        )
        .expect("spawning /bin/sh on a pty");

        drain(&mut session).await;

        let adopted = session.pull_size().expect("reading the pty size");
        assert_eq!(adopted, (100, 50));
        assert_eq!(
            session.grid().size(),
            Some((100, 50)),
            "the grid follows the kernel, not the other way round"
        );

        // And a resize to the size the PTY already holds is the relay's
        // no-op: the halves agree and the child is not disturbed.
        session.resize(100, 50).expect("resizing to the held size");
        assert_eq!(session.grid().size(), Some((100, 50)));
    }

    #[tokio::test]
    async fn a_child_can_move_the_pty_size_behind_the_grids_back() {
        // Why `resize` asks the kernel what to roll back to instead of keeping
        // its own copy. `tcsetwinsize()` is not reserved to whoever opened the
        // pty, and `stty` is an unremarkable thing for a child to run, so a
        // remembered size can be stale through no fault of this type's.
        //
        // Asserted rather than left as a remark in a comment, because it is the
        // premise the rollback rests on: if the child ever stopped being able to
        // do this, the simpler bookkeeping would be correct again and this test
        // is what would say so.
        let mut session = Session::spawn(
            pty_process::Command::new("/bin/sh")
                .arg("-c")
                .arg("stty rows 50 cols 100"),
            80,
            24,
        )
        .expect("spawning /bin/sh on a pty");

        drain(&mut session).await;

        let size = rustix::termios::tcgetwinsize(&session.pty).expect("reading the pty size");
        assert_eq!(
            (size.ws_col, size.ws_row),
            (100, 50),
            "the child's own tcsetwinsize should have moved the pty"
        );

        // And the other half of the divergence: nothing propagated it. The grid
        // is still where `spawn` left it, which is exactly why a stale
        // remembered size would have been indistinguishable from a live one.
        assert_eq!(
            session.grid().size(),
            Some((80, 24)),
            "a child-initiated resize should not have reached the grid"
        );
    }
}
