mod common;

use common::{assert_matches_golden, dump_frame};
use gwk_domain::ids::Timestamp;
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use gwk_tui::console::{HallContext, LoadState, render_hall, render_hall_at};
use gwk_tui::input::HitMap;

fn check(width: u16, height: u16, tier: ColorTier, glyphs: GlyphSet) {
    let input = common::estate::estate_frame_input();
    let context = HallContext {
        now: Timestamp::new("2026-08-11T17:30:00Z"),
        running: 4,
        attention: 2,
        cost_kernel_clocked: true,
        cost_micros: 5_300_000,
        load: LoadState::Ready,
    };
    let rendered = dump_frame(width, height, tier, glyphs, |area, buf, tier, glyphs| {
        let mut hits = HitMap::new();
        render_hall(area, buf, &input, &context, tier, glyphs, &mut hits);
    });
    let glyph_name = match glyphs {
        GlyphSet::Unicode => "unicode",
        GlyphSet::Ascii => "ascii",
    };
    assert_matches_golden(
        &format!(
            "mock-hall-inline-{width}x{height}-{}-{glyph_name}",
            tier.as_str()
        ),
        &rendered,
    );
}

#[test]
fn production_hall_matches_the_ruled_wide_artifact() {
    check(120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
}

#[test]
fn production_hall_matches_the_ruled_floor_artifact() {
    check(80, 24, ColorTier::Truecolor, GlyphSet::Unicode);
}

#[test]
fn production_hall_matches_the_ruled_degraded_artifact() {
    check(120, 40, ColorTier::Mono, GlyphSet::Ascii);
}

#[test]
fn production_hall_advances_ambient_state_marks_without_changing_static_goldens() {
    let input = common::estate::estate_frame_input();
    let context = HallContext {
        now: Timestamp::new("2026-08-11T17:30:00Z"),
        running: 4,
        attention: 2,
        cost_kernel_clocked: true,
        cost_micros: 5_300_000,
        load: LoadState::Ready,
    };
    let render = |phase| {
        dump_frame(
            120,
            40,
            ColorTier::Truecolor,
            GlyphSet::Unicode,
            |area, buf, tier, glyphs| {
                let mut hits = HitMap::new();
                render_hall_at(
                    area, &mut *buf, &input, &context, tier, glyphs, phase, &mut hits,
                );
            },
        )
    };
    assert_ne!(render(0), render(1));
}

/// Render just the header at one size, with the day-boundary provenance
/// under test.
fn header_with(kernel_clocked: bool) -> String {
    let input = common::estate::estate_frame_input();
    let context = HallContext {
        now: Timestamp::new("2026-08-11T17:30:00Z"),
        running: 4,
        attention: 2,
        cost_kernel_clocked: kernel_clocked,
        cost_micros: 5_300_000,
        load: LoadState::Ready,
    };
    let rendered = dump_frame(
        120,
        40,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
        |area, buf, tier, glyphs| {
            let mut hits = HitMap::new();
            render_hall(area, buf, &input, &context, tier, glyphs, &mut hits);
        },
    );
    rendered.lines().next().unwrap_or_default().to_owned()
}

#[test]
fn a_locally_clocked_total_says_whose_midnight_it_means() {
    // `$5.30 today` looks identical whichever clock drew the boundary, and the
    // one case where it is wrong is the one nobody can see. So the fallback
    // says so and the authoritative case stays clean.
    let kernel = header_with(true);
    let local = header_with(false);
    assert!(kernel.contains("$5.30 today"), "{kernel}");
    assert!(
        !kernel.contains("(local)"),
        "a kernel-supplied boundary hedged itself:\n{kernel}"
    );
    assert!(
        local.contains("$5.30 today (local)"),
        "a client-clocked total did not say so:\n{local}"
    );
}
