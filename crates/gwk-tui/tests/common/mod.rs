//! Shared snapshot-golden harness for `gwk-tui`'s ratatui `TestBackend`
//! renders — the Rung-2 mockup pipeline's substrate.
//!
//! The buffer-flatten convention (each row `trim_end()`'d, newline-joined)
//! and the `BLESS=1` golden-rewrite semantics are lifted verbatim from the
//! lens modules' own private test kits (`hall.rs`, `board.rs`, `queue.rs`).
//! This is the ONE shared copy for integration tests; the four private
//! copies stay as they are — migrating them is a follow-up, not this
//! harness.
//!
//! A test-helper module is compiled into EVERY test binary that declares
//! it, so a binary that uses a subset of `estate`'s scenario builders would
//! otherwise fail `-D warnings` on the rest — the same reason
//! `gwk-kernel/tests/common/mod.rs` carries this attribute.
#![allow(dead_code)]

pub mod estate;

use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

/// One point in the render matrix. Its [`Variant::golden_name`] is what a
/// golden file encodes: `seed-<lens>-<scenario>-<WxH>-<tier>-<glyphs>`, e.g.
/// `seed-board-fleet-120x40-truecolor-unicode.txt`.
#[derive(Debug, Clone, Copy)]
pub struct Variant {
    pub lens: &'static str,
    pub scenario: &'static str,
    pub width: u16,
    pub height: u16,
    pub tier: ColorTier,
    pub glyphs: GlyphSet,
}

impl Variant {
    /// A variant at the default tier (`Truecolor`) and glyph set (`Unicode`).
    pub const fn new(lens: &'static str, scenario: &'static str, width: u16, height: u16) -> Self {
        Self {
            lens,
            scenario,
            width,
            height,
            tier: ColorTier::Truecolor,
            glyphs: GlyphSet::Unicode,
        }
    }

    pub const fn with_tier(mut self, tier: ColorTier) -> Self {
        self.tier = tier;
        self
    }

    pub const fn with_glyphs(mut self, glyphs: GlyphSet) -> Self {
        self.glyphs = glyphs;
        self
    }

    pub fn golden_name(&self) -> String {
        format!(
            "seed-{}-{}-{}x{}-{}-{}",
            self.lens,
            self.scenario,
            self.width,
            self.height,
            self.tier.as_str(),
            glyph_name(self.glyphs),
        )
    }

    /// Render `paint` into this variant's `TestBackend`, flatten it to text,
    /// and check it against this variant's golden.
    pub fn check(&self, paint: impl FnOnce(Rect, &mut Buffer, ColorTier, GlyphSet)) {
        let rendered = dump_frame(self.width, self.height, self.tier, self.glyphs, paint);
        assert_matches_golden(&self.golden_name(), &rendered);
    }
}

fn glyph_name(glyphs: GlyphSet) -> &'static str {
    match glyphs {
        GlyphSet::Unicode => "unicode",
        GlyphSet::Ascii => "ascii",
    }
}

/// Render one `TestBackend` frame and flatten it to the repo's golden-text
/// convention.
pub fn dump_frame(
    width: u16,
    height: u16,
    tier: ColorTier,
    glyphs: GlyphSet,
    paint: impl FnOnce(Rect, &mut Buffer, ColorTier, GlyphSet),
) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| paint(frame.area(), frame.buffer_mut(), tier, glyphs))
        .expect("draw");
    flatten(terminal.backend().buffer())
}

fn flatten(buffer: &Buffer) -> String {
    let mut rendered = String::new();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        rendered.push_str(line.trim_end());
        rendered.push('\n');
    }
    rendered
}

/// Check `rendered` against `crates/gwk-tui/goldens/<name>.txt`. `BLESS=1`
/// rewrites the golden and then deliberately fails the run — it is not a
/// passing run.
pub fn assert_matches_golden(name: &str, rendered: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("goldens")
        .join(format!("{name}.txt"));
    let bless = std::env::var_os("BLESS").is_some();
    if bless {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("create goldens dir");
        }
        std::fs::write(&path, rendered).expect("write golden");
    } else {
        let committed = std::fs::read_to_string(&path)
            .unwrap_or_else(|why| panic!("{}: {why} (BLESS=1 to create)", path.display()));
        if committed != rendered {
            let at = committed
                .lines()
                .zip(rendered.lines())
                .position(|(a, b)| a != b);
            panic!(
                "{} drifted at line {:?}.\n  golden:   {:?}\n  rendered: {:?}\nIf the frame is \
                 right, re-run with BLESS=1.",
                path.display(),
                at.map(|line| line + 1),
                at.and_then(|line| committed.lines().nth(line)),
                at.and_then(|line| rendered.lines().nth(line)),
            );
        }
    }
    assert!(
        !bless,
        "BLESS=1 rewrites the goldens; it is not a passing run"
    );
}
