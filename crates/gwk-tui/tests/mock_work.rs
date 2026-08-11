//! Round 4 — the WORK lens (grill G1: `WORK = queue · tasks/dag · runs ·
//! config`; G6 ruled BOTH the Queue and the Config lens get wired).
//!
//! MOCKUP PAINTER ONLY. The queue and config bodies are painted by the REAL
//! `gwk_tui::queue::render` / `gwk_tui::config::render` over the harness's
//! seeded state — both are complete, unreachable lenses today, and this
//! round is the first time either has been seen inside a frame. Everything
//! around them (the lens header, sub-tab strip, keybar) and the two screens
//! that do not exist at all (gate decision, config form) are the mocked
//! chrome under design.
//!
//! ## Why this round exists
//!
//! `queue.rs` is 1,202 lines with 20 unit tests and two committed goldens;
//! `config.rs` is 1,262 lines of git-backed reconciler with ZERO tests. The
//! shipped `gw` binary imports neither. The Queue is also the ONLY reader
//! of `Gate` anywhere in the codebase — today a raised gate, including a
//! relayed permission prompt, is invisible on every surface that ships.
//!
//! ## The two screens that had to be invented
//!
//! - **Gate decision.** `queue.rs` deliberately refuses ack/mute/resolve on
//!   a gate target (`QueueTarget::Gate` returns `None` from all four verb
//!   builders) — a gate is DECIDED, never acknowledged. No decide verb
//!   exists anywhere in the crate, so the affordance is new.
//! - **Config form.** `ConfigFormSchema` validates a submitted form but
//!   nothing renders one; `ConfigState.contents` is loaded for all four
//!   files and never painted. Both screens here are the first drawing of
//!   either.
//!
//! ## Findings this round records
//!
//! - `Gate` carries no actor field on the wire, so a decided gate cannot
//!   say who decided it. Rendering "decided by" is a DOMAIN change.
//! - The queue's mail section admits only `Delivered|Acknowledged|Applied`;
//!   the seeded day's dead-lettered alert (`m-alert`, three delivery
//!   attempts, reason "nobody listening") is silently absent from a lens
//!   whose whole job is telling the operator what is owed.
//! - Config's dirty/divergent flags are estate-wide booleans with no
//!   per-file blame, and three structurally different reasons for
//!   `EditRoute::Editor` (authority-policy always, invalid TOML, content
//!   sources `~/.gridwork/env`) collapse into one label.

mod common;

#[path = "mockups/shared.rs"]
mod shared;

use common::{assert_matches_golden, dump_frame};
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use gwk_tui::config::{self, ConfigTarget};
use gwk_tui::input::HitMap;
use gwk_tui::queue;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use shared::{bold, put, put_keybar, put_right, put_rule, style, tier_badge};

const TABS: &[&str] = &["queue", "tasks", "runs", "config"];

/// Lens header plus the sub-tab strip the five-lens taxonomy needs. The
/// active tab is bracketed AND styled — at Mono the brackets are the only
/// signal left, so they are not decoration.
fn paint_header(
    buf: &mut Buffer,
    area: Rect,
    tier: ColorTier,
    glyphs: GlyphSet,
    active: &str,
    note: &str,
) {
    let compact = area.width < 100;
    put(buf, area, 1, 0, "WORK", bold("fg", tier));

    let mut x = 8;
    for tab in TABS {
        let is_active = *tab == active;
        let text = if is_active {
            format!("[{tab}]")
        } else {
            format!(" {tab} ")
        };
        let paint = if is_active {
            bold("hue", tier)
        } else {
            style("muted", tier)
        };
        put(buf, area, x, 0, &text, paint);
        x += text.chars().count() as u16 + 1;
    }
    put(buf, area, x + 2, 0, note, style("muted", tier));

    let badge = tier_badge(tier, glyphs);
    let right = if compact {
        format!("{badge}  17:30")
    } else {
        format!("tier {badge}  as-of 221  17:30")
    };
    put_right(buf, area, 0, &right, style("muted", tier));
}

fn body(area: Rect, rows: u16) -> Rect {
    Rect {
        x: area.x,
        y: area.y + 2,
        width: area.width,
        height: rows,
    }
}

// ---------------------------------------------------------------------------
// WORK > queue, over the real queue lens
// ---------------------------------------------------------------------------

fn paint_queue(area: Rect, buf: &mut Buffer, tier: ColorTier, glyphs: GlyphSet) {
    let state = common::estate::estate_queue_state();
    paint_header(
        buf,
        area,
        tier,
        glyphs,
        "queue",
        "attention, gates, and mail",
    );

    let mut hits = HitMap::default();
    let region = body(area, area.height - 4);
    queue::render(region, buf, &state, None, tier, glyphs, &mut hits);

    put_rule(buf, area, area.height - 2, tier);
    put_keybar(
        buf,
        area,
        tier,
        " enter open   a ack   m mute   r resolve   d decide gate   / filter   : go   q back",
        " enter open   a ack   m mute   r resolve   d decide   q back",
    );
}

/// The gate decision, which exists nowhere today. A gate's options are an
/// open `Vec<String>` off the wire, so the strip renders whatever the gate
/// carries — never a hardcoded allow/deny pair.
///
/// It is a modal confirm rather than a bare keypress because a gate is the
/// one queue row whose verb has an irreversible outside effect: the seeded
/// gate restarts the kernel.
fn paint_gate(area: Rect, buf: &mut Buffer, tier: ColorTier, glyphs: GlyphSet) {
    let state = common::estate::estate_queue_state();
    paint_header(buf, area, tier, glyphs, "queue", "deciding gate g-deploy");

    let mut hits = HitMap::default();
    let region = body(area, area.height - 12);
    queue::render(region, buf, &state, None, tier, glyphs, &mut hits);

    let top = area.height - 10;
    put_rule(buf, area, top, tier);

    let mut y = top + 1;
    put(buf, area, 2, y, "DECIDE", bold("warn", tier));
    put(buf, area, 11, y, "gate g-deploy", bold("fg", tier));
    put(buf, area, 27, y, "kind deploy", style("muted", tier));
    put(buf, area, 42, y, "raised 17:00", style("muted", tier));
    y += 1;

    put(
        buf,
        area,
        2,
        y,
        "restart the kernel service after the pty_session receipt fix?",
        style("fg", tier),
    );
    y += 2;

    // Options come off the gate; the selected one is prefixed, not merely
    // coloured, so the choice survives Mono.
    for (index, (key, option, note)) in [
        ("1", "allow", "restarts gridwork-kernel on gw-ms-a2"),
        (
            "2",
            "deny",
            "leaves the fix unshipped until the next window",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let selected = index == 0;
        put(
            buf,
            area,
            2,
            y,
            if selected { ">" } else { " " },
            bold("focus", tier),
        );
        put(buf, area, 4, y, key, style("muted", tier));
        put(
            buf,
            area,
            7,
            y,
            option,
            if selected {
                bold("hue", tier)
            } else {
                style("fg", tier)
            },
        );
        put(buf, area, 18, y, note, style("muted", tier));
        y += 1;
    }
    y += 1;

    // The honesty line: this is an outside effect, and the wire cannot say
    // who decided it.
    put(
        buf,
        area,
        2,
        y,
        "decides as operator -- receipted; the gate aggregate records no actor",
        style("warn", tier),
    );

    put_keybar(
        buf,
        area,
        tier,
        " 1/2 choose   enter decide   esc cancel   the chosen option is submitted as a kernel command",
        " 1/2 choose   enter decide   esc cancel",
    );
}

// ---------------------------------------------------------------------------
// WORK > config, over the real config lens
// ---------------------------------------------------------------------------

fn paint_config(area: Rect, buf: &mut Buffer, tier: ColorTier, glyphs: GlyphSet) {
    let state = common::estate::estate_config_state();
    paint_header(buf, area, tier, glyphs, "config", "the four governed files");

    let mut hits = HitMap::default();
    let region = body(area, area.height - 4);
    config::render(
        region,
        buf,
        &state,
        Some(&ConfigTarget::File(
            gwk_tui::config::ConfigPath::Capabilities,
        )),
        tier,
        glyphs,
        &mut hits,
    );

    put_rule(buf, area, area.height - 2, tier);
    put_keybar(
        buf,
        area,
        tier,
        " enter edit   j/k file   d diff   : go   q back      form-routed files open a validated form; $EDITOR otherwise",
        " enter edit   j/k file   d diff   q back",
    );
}

/// The generated form, which exists nowhere today: `ConfigFormSchema`
/// validates a submission (rejecting unknown, missing, or retyped fields)
/// but nothing has ever drawn one.
///
/// The shape follows the validator: every field the incumbent file carries,
/// its current value, and its type — because those three facts are exactly
/// what `validate_shape` enforces on submit. A form that let an operator
/// type a field the validator will reject is a form that lies.
fn paint_form(area: Rect, buf: &mut Buffer, tier: ColorTier, glyphs: GlyphSet) {
    paint_header(
        buf,
        area,
        tier,
        glyphs,
        "config",
        "editing identity/capabilities.toml",
    );

    let mut y = 2;
    put(buf, area, 1, y, "FORM", bold("fg", tier));
    put(
        buf,
        area,
        8,
        y,
        "identity/capabilities.toml   shape is fixed by the incumbent file",
        style("muted", tier),
    );
    y += 2;

    for (x, label) in [(2u16, "FIELD"), (34, "TYPE"), (44, "VALUE")] {
        put(buf, area, x, y, label, style("muted", tier));
    }
    y += 1;

    for (index, (field, kind, value, dirty)) in [
        ("code_write.default_agent", "string", "gw-rust-pro", false),
        ("code_write.lane", "string", "sonnet", true),
        (
            "code_review.default_agent",
            "string",
            "gw-code-reviewer",
            false,
        ),
        (
            "recon.default_agent",
            "string",
            "gw-phase-researcher",
            false,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let focused = index == 1;
        put(
            buf,
            area,
            0,
            y,
            if focused { ">" } else { " " },
            bold("focus", tier),
        );
        put(buf, area, 2, y, field, style("fg", tier));
        put(buf, area, 34, y, kind, style("muted", tier));
        let paint = if focused {
            bold("hue", tier)
        } else {
            style("fg", tier)
        };
        put(buf, area, 44, y, value, paint);
        if focused {
            put(
                buf,
                area,
                44 + value.chars().count() as u16,
                y,
                "_",
                bold("hue", tier),
            );
        }
        if dirty {
            put(buf, area, 74, y, "changed", style("warn", tier));
        }
        y += 1;
    }
    y += 1;

    put(
        buf,
        area,
        2,
        y,
        "no field can be added or removed here -- the validator rejects any shape",
        style("muted", tier),
    );
    y += 1;
    put(
        buf,
        area,
        2,
        y,
        "change against the incumbent file, so the form offers none",
        style("muted", tier),
    );
    y += 2;

    put(buf, area, 2, y, "COMMIT", bold("fg", tier));
    y += 1;
    put(
        buf,
        area,
        2,
        y,
        "routing: sonnet lane for bounded code_write_",
        style("fg", tier),
    );
    y += 1;
    put(
        buf,
        area,
        2,
        y,
        "one file, one commit, one config_change evidence record",
        style("muted", tier),
    );

    put_rule(buf, area, area.height - 2, tier);
    put_keybar(
        buf,
        area,
        tier,
        " tab field   enter commit   esc cancel      an exclusive lock is held; a concurrent edit is refused, never merged",
        " tab field   enter commit   esc cancel",
    );
}

// ---------------------------------------------------------------------------
// The golden matrix
// ---------------------------------------------------------------------------

type Painter = fn(Rect, &mut Buffer, ColorTier, GlyphSet);

fn check(scenario: &str, width: u16, height: u16, tier: ColorTier, glyphs: GlyphSet) {
    let painter: Painter = match scenario {
        "queue" => paint_queue,
        "gate" => paint_gate,
        "config" => paint_config,
        "form" => paint_form,
        other => panic!("unknown scenario {other}"),
    };
    let glyph_name = match glyphs {
        GlyphSet::Unicode => "unicode",
        GlyphSet::Ascii => "ascii",
    };
    let name = format!(
        "mock-work-{scenario}-{width}x{height}-{}-{glyph_name}",
        tier.as_str()
    );
    assert_matches_golden(&name, &dump_frame(width, height, tier, glyphs, painter));
}

#[test]
fn queue_at_120x40() {
    check("queue", 120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
}

#[test]
fn queue_at_80x24() {
    check("queue", 80, 24, ColorTier::Truecolor, GlyphSet::Unicode);
}

#[test]
fn queue_degraded_mono_ascii() {
    check("queue", 120, 40, ColorTier::Mono, GlyphSet::Ascii);
}

#[test]
fn gate_at_120x40() {
    check("gate", 120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
}

#[test]
fn config_at_120x40() {
    check("config", 120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
}

#[test]
fn form_at_120x40() {
    check("form", 120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
}
