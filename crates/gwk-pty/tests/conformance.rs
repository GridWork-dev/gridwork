//! Golden-frame conformance: parse third-party VT vectors, compare the screen
//! against frames pinned when the toolchain was pinned.
//!
//! This asserts *stability*, which is a smaller and more honest claim than
//! correctness. Nobody here is qualified to declare what a terminal should do
//! with `12-malformed-utf8`; what this catches is the parser quietly deciding
//! something different than it did yesterday, at the moment a pin moves rather
//! than months later through a rendering bug nobody can reproduce.
//!
//! Where the vectors and the geometry come from, and why byte 0 is dropped:
//! `fixtures/PROVENANCE.md`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use gwk_pty::Grid;
use sha2::{Digest, Sha256};

// Derivation: CAP-001 — the harness these vectors were written for builds its
// terminal at 80x24 with 100 lines of scrollback, and reads the first input
// byte as a code-path selector rather than as terminal input. Both are honoured
// here: matching the geometry is what makes a frame captured here describe the
// same terminal upstream drives, and dropping byte 0 is what keeps a stray NUL
// or SOH out of every vector.
/// The geometry the corpus was written against.
const COLS: u16 = 80;
const ROWS: u16 = 24;
const SCROLLBACK: usize = 100;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn hex(bytes: &[u8]) -> String {
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Every vector, in the order the manifest lists them.
///
/// The vectors are stored hex-encoded because they are full of ESC and NUL and
/// `tools/leak-scan.sh` refuses to pass a repository holding tracked binaries
/// it cannot read. Each line carries the digest of its own decoded bytes, and
/// this checks it — an encoding nobody verifies is just a slower way to lose
/// the provenance the corpus exists for.
fn vectors() -> Vec<(String, Vec<u8>)> {
    let path = fixtures_dir().join("stream-initial.hex");
    let manifest = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));

    let mut out = Vec::new();
    for line in manifest.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (name, digest, payload) = (
            parts.next().expect("name"),
            parts.next().expect("digest"),
            parts.next().expect("hex"),
        );
        assert!(
            parts.next().is_none(),
            "{name}: trailing junk after the hex payload"
        );

        let raw: Vec<u8> = (0..payload.len())
            .step_by(2)
            .map(|i| {
                u8::from_str_radix(&payload[i..i + 2], 16)
                    .unwrap_or_else(|e| panic!("{name}: bad hex at byte {}: {e}", i / 2))
            })
            .collect();
        assert_eq!(
            hex(&raw),
            digest,
            "{name}: decoded bytes do not match the digest on its own line"
        );
        out.push((name.to_owned(), raw));
    }

    assert!(!out.is_empty(), "the corpus is empty; nothing is tested");
    out
}

/// Parse one vector and render the frame block that goes in the golden file.
fn frame_for(name: &str, raw: &[u8]) -> String {
    // Byte 0 selects a code path in upstream's harness and is not terminal
    // input. See fixtures/PROVENANCE.md.
    let input = raw.split_first().map_or(&[][..], |(_, rest)| rest);

    let mut grid = Grid::with_scrollback(COLS, ROWS, SCROLLBACK).expect("valid grid");
    grid.write(input);
    let text = grid.text().expect("screen text");
    let (x, y) = grid.cursor().expect("cursor");

    let mut out = String::new();
    let _ = writeln!(out, "== {name}");
    let _ = writeln!(out, "input-sha256 {}", hex(raw));
    let _ = writeln!(out, "cursor {x},{y}");
    // Rows are fenced so trailing spaces survive both git and an editor that
    // strips them. A row of spaces and an untouched row are different screen
    // states, and a format that could not tell them apart would let a real
    // divergence through as a no-op diff.
    for line in text.lines() {
        let _ = writeln!(out, "|{line}|");
    }
    let _ = writeln!(out, "== end {name}");
    out
}

fn render_all() -> String {
    let mut out = String::from(
        "# Golden frames. Generated; do not hand-edit.\n\
         # Provenance and how to regenerate: fixtures/PROVENANCE.md\n\
         #\n\
         # A diff here means the parser's behaviour moved. That is information,\n\
         # not automatically a bug — read it before regenerating.\n\n",
    );
    for (name, raw) in vectors() {
        out.push_str(&frame_for(&name, &raw));
        out.push('\n');
    }
    out
}

#[test]
fn frames_match_what_was_pinned() {
    let golden_path = fixtures_dir().join("golden-frames.txt");
    let rendered = render_all();

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&golden_path, &rendered).expect("writing golden frames");
        eprintln!("wrote {}", golden_path.display());
        return;
    }

    let pinned = std::fs::read_to_string(&golden_path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}\nRun with UPDATE_GOLDEN=1 to create it.",
            golden_path.display()
        )
    });

    if pinned == rendered {
        return;
    }

    // A whole-file assert_eq on 24 screens prints two walls of text and leaves
    // the reader to find the difference. Name the vectors that moved instead.
    let all = vectors();
    let moved: Vec<String> = all
        .iter()
        .filter(|(name, raw)| !pinned.contains(&frame_for(name, raw)))
        .map(|(name, _)| name.clone())
        .collect();

    panic!(
        "golden frames changed for {} of {} vectors: {}\n\
         The parser now does something different with these inputs.\n\
         Read the diff before regenerating with UPDATE_GOLDEN=1.",
        moved.len(),
        all.len(),
        moved.join(", ")
    );
}

#[test]
fn the_corpus_is_the_one_that_was_vendored() {
    // Guards the other direction: a fixture edited to make a frame agree would
    // otherwise pass silently, and the corpus's whole value is that it was not
    // written here. The digests are in the golden file beside each frame, so
    // this cannot drift from what the frames were captured against.
    let pinned =
        std::fs::read_to_string(fixtures_dir().join("golden-frames.txt")).expect("golden frames");
    for (name, raw) in vectors() {
        let expected = format!("== {name}\ninput-sha256 {}\n", hex(&raw));
        assert!(
            pinned.contains(&expected),
            "fixture {name} does not match the digest recorded when its frame was captured"
        );
    }
}

#[test]
fn every_vector_is_actually_parsed() {
    // Cheap guard against the corpus silently emptying out — a runner that
    // finds no files would otherwise report success, which is the failure mode
    // a conformance gate can least afford.
    assert_eq!(
        vectors().len(),
        24,
        "expected the 24 vendored vectors; see fixtures/PROVENANCE.md"
    );
}
