//! Push ghostty's whole minimised fuzz corpus through the parser and require
//! that nothing dies.
//!
//! # What this replaces, and why
//!
//! The plan asked for miri over the pure-Rust paths plus a sanitizer across the
//! C boundary. Both were written before this crate had a shape, and the shape
//! it ended up with removes most of the target:
//!
//! **Miri is not worth a job here.** It would find nothing by construction.
//! Miri detects undefined behaviour, the crate's only pure-Rust logic is the
//! recording codec, and that module has no `unsafe`, no raw pointers, and no
//! FFI — the workspace forbids `unsafe_code` outright, so it could not have
//! any. Everything miri *would* have something to say about is inside
//! libghostty-vt-sys, a dependency this crate does not author, and miri cannot
//! execute the FFI calls that reach it anyway.
//!
//! **An ASan build is not available.** `libghostty-vt-sys`'s build script
//! offers no way to pass `-fsanitize=address` through to `zig build`, so the
//! Zig side would stay uninstrumented and ASan would see an opaque library with
//! no redzones — reporting cleanly on overflows it simply could not observe.
//! A green run from that would be worse than no run.
//!
//! **What IS available is Zig's own safety.** The same build script honours
//! `LIBGHOSTTY_VT_SYS_OPTIMIZE`, and Zig's `Debug` and `ReleaseSafe` modes
//! carry runtime checks for exactly the class that matters here: out-of-bounds
//! indexing, integer overflow, invalid enum casts, reaching `unreachable`. Those
//! checks live *inside* the VT implementation, and this test drives them with
//! several thousand adversarial inputs. That is the sanitizer this architecture
//! actually admits.
//!
//! # The corpus is not vendored
//!
//! It is read out of the pinned ghostty tree that `tools/pty-toolchain.sh`
//! already materialises — around 14 MB that has no business in this repository
//! when the build already has it on disk, pinned to the same revision the
//! parser was compiled from.

use std::path::PathBuf;

use gwk_pty::Grid;
use libghostty_vt::build_info::OptimizeMode;

/// Enough of the corpus that finding less means something is wrong.
///
/// `stream-cmin` held 3271 files and `stream-initial` 24 at `GHOSTTY_COMMIT`.
/// The floor is deliberately well under that: upstream re-minimising its corpus
/// is normal and should not fail our build, but the count collapsing to a
/// handful means the path is wrong and the run proved nothing.
const MINIMUM_VECTORS: usize = 500;

fn corpus_dirs() -> Option<Vec<PathBuf>> {
    let root = PathBuf::from(std::env::var_os("GHOSTTY_SOURCE_DIR")?);
    let base = root.join("test/fuzz-libghostty/corpus");
    Some(vec![base.join("stream-cmin"), base.join("stream-initial")])
}

#[test]
fn the_whole_fuzz_corpus_parses_without_taking_the_process_down() {
    let Some(dirs) = corpus_dirs() else {
        // Not a silent skip: without the tree there is no parser to test
        // either, so this cannot be reached from a run that built the crate.
        panic!(
            "GHOSTTY_SOURCE_DIR is unset. Run `eval \"$(./tools/pty-toolchain.sh --env)\"` first."
        );
    };

    // The whole value of this run is that Zig's checks are watching. They are
    // on in `Debug` and `ReleaseSafe` and off in the two fast modes, and the
    // default happens to be `Debug` only because cargo's dev profile sets
    // `DEBUG=true`. Asserting it means a release-profile run fails loudly
    // instead of passing in a fraction of the time having checked nothing —
    // the same accident the Zig-free job was written to close.
    let mode = libghostty_vt::build_info::optimize_mode().expect("build info");
    assert!(
        matches!(mode, OptimizeMode::Debug | OptimizeMode::ReleaseSafe),
        "libghostty-vt was built in {mode:?}, which has no runtime safety checks. \
         Set LIBGHOSTTY_VT_SYS_OPTIMIZE=ReleaseSafe."
    );

    let mut parsed = 0usize;
    let mut bytes = 0usize;

    for dir in dirs {
        let entries =
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if !path.is_file() {
                continue;
            }
            let raw = std::fs::read(&path).expect("reading a corpus file");
            // Derivation: CAP-001 — the harness that produced this corpus reads
            // byte 0 as a code-path selector rather than terminal input, and
            // builds its terminal at 80x24 with 100 lines of scrollback. Both
            // are matched here so a crash found in this run is one that could
            // happen in the run these inputs were minimised against.
            let input = raw.split_first().map_or(&[][..], |(_, rest)| rest);
            let mut grid = Grid::with_scrollback(80, 24, 100).expect("valid grid");
            grid.write(input);

            // Read the screen back rather than only writing to it. A parser
            // that corrupted its own state would otherwise go unnoticed until
            // something tried to render, and rendering is where a caller
            // actually meets the damage.
            let _ = grid.text();
            let _ = grid.cursor();

            parsed += 1;
            bytes += input.len();
        }
    }

    assert!(
        parsed >= MINIMUM_VECTORS,
        "only {parsed} corpus files found; expected at least {MINIMUM_VECTORS}. \
         The corpus path is probably wrong, and a run over nothing proves nothing."
    );
    eprintln!("parsed {parsed} corpus vectors, {bytes} bytes of terminal input");
}
