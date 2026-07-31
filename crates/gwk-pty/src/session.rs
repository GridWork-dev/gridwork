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
    /// The size the PTY was last successfully set to.
    ///
    /// Deliberately NOT "the size both halves agree on", which is what this
    /// field was first called and is not true: the child can change the grid's
    /// width from the byte stream. `\x1b[?3h` under DECCOLM takes an 80-column
    /// grid to 132 without [`resize`](Self::resize) being called at all, and
    /// then the grid, the PTY and this field are three different numbers.
    ///
    /// It is still the right rollback target, because it is exactly the value
    /// the PTY is holding when a `tcsetwinsize()` call fails. What it is not is
    /// a record of what the grid was doing, so restoring it can discard a width
    /// the child chose — a real loss, but strictly smaller than the divergence
    /// it prevents, and the only bound available without asking a grid whose
    /// answer is fallible.
    ///
    /// `Grid::size()` would answer that question, but it answers with an
    /// `Option`, and a rollback skippable because the question failed is not a
    /// rollback.
    size: (u16, u16),
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
        Ok(Self {
            pty,
            child,
            grid,
            size: (cols, rows),
        })
    }

    /// Read whatever the child has written and feed it to the parser.
    ///
    /// Returns the number of bytes taken; `0` means the child hung up and no
    /// further call will return more.
    ///
    /// A chunk reaches the parser exactly as it arrived — see the derivation
    /// note on the write below.
    pub async fn pump(&mut self) -> io::Result<usize> {
        let mut buf = [0u8; READ_CHUNK];
        match self.pty.read(&mut buf).await {
            Ok(0) => Ok(0),
            Ok(n) => {
                // Derivation: ECMA-48 §5.4 — a control sequence is CSI, then
                // parameter bytes, then intermediate bytes, then a final byte.
                // A read boundary can fall anywhere inside that, so the chunk
                // goes to the parser exactly as it arrived. Splitting on what
                // looks like a sequence edge, or holding a partial one back to
                // "complete" it, is how a reader corrupts input the parser
                // would have handled correctly on its own.
                self.grid.write(&buf[..n]);
                Ok(n)
            }
            // Reading a PTY master whose slave has closed is EIO on Linux, not
            // end-of-file — the child exiting is normal, so reporting it as an
            // I/O error would make every clean exit look like a failure.
            //
            // This is a platform behavior and NOT a POSIX guarantee: macOS
            // returns 0 here instead, which the `Ok(0)` arm already covers.
            // Deleting this arm was tried: both PTY tests below fail on Linux,
            // so it is load-bearing here and the `Ok(0)` arm is what carries
            // the same case elsewhere.
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) => Ok(0),
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
    /// a PTY failure rolls it back — so on either single failure the two halves
    /// still agree.
    ///
    /// Not all-or-nothing in general, and the difference is worth stating
    /// rather than rounding off. Restoring the grid is a real reflow, not a
    /// saved buffer put back, so it can fail on its own; that needs a second
    /// independent failure, and if it happens the halves are left disagreeing
    /// and the PTY's error is what surfaces, because it is the cause. Claiming
    /// atomicity here would be the same defect this method has already carried
    /// once: a comment asserting a property the code does not have.
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
        if let Err(e) = self.pty.resize(Size::new(rows, cols)) {
            // Rolling back to `self.size` rather than to `grid.size()`: the
            // latter is an `Option`, and a rollback that gets skipped because
            // reading the old size failed is not a rollback.
            //
            // The result is discarded because there is nothing left to do with
            // it — the grid is already at the new size, the PTY is at the old,
            // and if the restore fails they stay that way whatever we return.
            // The PTY error is what propagates because it is the cause.
            //
            // ponytail: no test seeds this. `tcsetwinsize()` fails on EBADF,
            // ENOTTY, EIO (orphaned process group) or EINVAL, none of which is
            // reachable from safe code holding a live `Session` — the fd is
            // owned and known to be a terminal. Written anyway because EIO is
            // reachable in production even if not from here.
            let (cols, rows) = self.size;
            let _ = self.grid.resize(cols, rows);
            return Err(e.into());
        }
        self.size = (cols, rows);
        Ok(())
    }

    /// The authoritative grid.
    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    /// Wait for the child to exit.
    pub async fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        self.child.wait().await
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
}
