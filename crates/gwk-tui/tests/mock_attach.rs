//! Round 3 — TERM/attach and the send-mode surfaces (grill G2: estate rail
//! + pty main; F4: BOTH a modal INPUT mode and a `:send` one-shot).
//!
//! MOCKUP PAINTER ONLY. The hosted-session region is painted by the REAL
//! `gwk_tui::drilldown::render` over the harness's attached fixture, so the
//! pty content and its session line are genuine; everything around it —
//! the estate rail, the mode badge, the receipt row, the keybar — is the
//! mocked chrome under design.
//!
//! ## Scenarios
//!
//! - `view` — VIEW mode, live stream, rail visible. The at-rest attach.
//! - `input` — INPUT mode: mode badge, raw-passthrough warning in the bar,
//!   and the receipt row for a completed send.
//! - `refused` — a send refused for a stale generation, the loud path the
//!   input SPEC requires (`#99`'s `opened()` stale refusal posture).
//! - `send` — the `:send` one-shot verb composed from the TERM list with no
//!   attach at all, the second F4 surface.
//!
//! ## What this round fixes about attach
//!
//! - `{id}:{generation}` is printed. Today `generation` silently wipes the
//!   mirror on a flip and silently discounts stale batches while never
//!   appearing on screen; an operator cannot tell which life they are
//!   watching, let alone which life a send lands in.
//! - The rail costs the session columns. The fixture's session is 100 cols;
//!   a 28-column rail leaves 90, and a pty cannot reflow — the real console
//!   must RESIZE the hosted session to the region on attach, not crop it.
//!   The crop is visible in these frames deliberately, as the argument for
//!   the resize.
//! - Stream state is styled by severity rather than rendered as plain text
//!   among 21 possible close codes.
//!
//! ## The receipt contract shown here
//!
//! F1 ruled raw byte passthrough, F3 one receipt per flushed batch, F5 both
//! an in-lens receipt row and the pty's own echo. A receipt row therefore
//! reads `sent <bytes>B  rcpt <id>  <actor>  <clock>` — byte count, not
//! content: the bytes are raw and may be a password or a control sequence,
//! so the receipt proves delivery without transcribing what was sent.

mod common;

#[path = "mockups/shared.rs"]
mod shared;

use common::{assert_matches_golden, dump_frame};
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use gwk_tui::drilldown::{self, DrilldownState};
use gwk_tui::input::HitMap;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use shared::{bold, put, put_right, put_rule, style, tier_badge};

/// Width of the estate rail. Below this total width the rail is dropped
/// entirely rather than squeezed — a 12-column rail says nothing useful and
/// costs the session twelve columns.
const RAIL: u16 = 28;
const RAIL_FLOOR: u16 = 100;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Read-only attach.
    View,
    /// INPUT at rest: the toast from the last send has expired and the row
    /// carries nothing but the session's own status.
    Input,
    /// INPUT with a send receipt riding the status row. Clears after ~3s.
    Sent,
    /// A refused send. Unlike a receipt this is a STATE, not an event, so it
    /// persists until the next send or until INPUT is left.
    Refused,
}

// ---------------------------------------------------------------------------
// Chrome
// ---------------------------------------------------------------------------

/// `subject` is the attached session's `{id}:{generation}` when one is
/// attached, or the lens's own summary when the frame is a list. A header
/// never claims an attachment the frame does not have.
fn paint_header(
    buf: &mut Buffer,
    area: Rect,
    tier: ColorTier,
    glyphs: GlyphSet,
    subject: &str,
    note: &str,
) {
    let compact = area.width < RAIL_FLOOR;
    put(buf, area, 1, 0, "TERM", bold("fg", tier));
    put(buf, area, 8, 0, subject, bold("hue", tier));
    put(
        buf,
        area,
        8 + subject.chars().count() as u16 + 2,
        0,
        note,
        style("muted", tier),
    );

    let badge = tier_badge(tier, glyphs);
    let right = if compact {
        format!("{badge}  17:30")
    } else {
        format!("tier {badge}  as-of 221  17:30")
    };
    put_right(buf, area, 0, &right, style("muted", tier));
}

/// The identity a send is addressed to: session AND generation. A send to a
/// stale generation is refused, so the generation is not a detail.
fn subject(drill: &DrilldownState) -> String {
    format!(
        "{}:{}",
        drill.session_id().as_str(),
        drill
            .generation()
            .map_or("-", |generation| generation.as_str()),
    )
}

/// The estate rail: enough of the Hall to keep the estate glanceable while
/// attached, and no more. Every line here is a digest of a lens one `:`
/// away, not a second copy of it.
fn paint_rail(buf: &mut Buffer, area: Rect, tier: ColorTier) {
    let mut y = 2;
    put(buf, area, 1, y, "ESTATE", bold("fg", tier));
    y += 2;

    for (label, value, token) in [
        ("running", "4", "hue"),
        ("attention", "2", "warn"),
        ("blocked", "1", "warn"),
        ("today", "$5.30", "fg"),
    ] {
        put(buf, area, 1, y, label, style("muted", tier));
        put(buf, area, 14, y, value, style(token, tier));
        y += 1;
    }
    y += 1;

    put(buf, area, 1, y, "TERMS", bold("fg", tier));
    y += 1;
    for (id, note, focused) in [
        ("pty-1:gen-3", "attached", true),
        ("pty-2:gen-1", "closed", false),
    ] {
        put(
            buf,
            area,
            0,
            y,
            if focused { ">" } else { " " },
            bold("focus", tier),
        );
        put(buf, area, 2, y, id, style("fg", tier));
        put(buf, area, 15, y, note, style("muted", tier));
        y += 1;
    }
    y += 1;

    put(buf, area, 1, y, "QUEUE", bold("fg", tier));
    y += 1;
    put(buf, area, 1, y, "! gate deploy", style("warn", tier));
    y += 1;
    put(buf, area, 1, y, "! kek rotation", style("warn", tier));
}

/// The vertical rule between rail and session. Painted with `|`, which is
/// ASCII and admissible at every glyph tier — the theme's elevation tokens
/// are never-a-colour, so structure has to be a character.
fn paint_divider(buf: &mut Buffer, area: Rect, tier: ColorTier, top: u16, bottom: u16) {
    for y in top..bottom {
        put(buf, area, RAIL, y, "|", style("faint", tier));
    }
}

/// The answer to "did my keystrokes land" (F5 ruled this AND the pty's own
/// echo, because a frame can lag or suppress echo entirely).
///
/// It is painted OVER the right end of the session's own status row, after
/// `drilldown::render` has drawn it, rather than onto a row of its own. A
/// row that appeared and vanished with each send would change the session
/// region's height — and the console must resize the hosted pty to that
/// region, so a transient row means a resize storm, one per keystroke
/// batch. Riding the status row costs the session nothing and puts the
/// receipt beside the session identity it refers to.
///
/// The receipt states a BYTE COUNT, never the bytes: under ruled raw
/// passthrough a send may carry a password or a control sequence, so it
/// proves delivery without transcribing what was sent.
fn paint_toast(buf: &mut Buffer, area: Rect, session: Rect, tier: ColorTier, mode: Mode) {
    let y = session.y + session.height - 1;
    // Measure where `drilldown::render` actually left off on the status row
    // rather than assuming its width — the status text carries a variable
    // close code, and 21 of them exist.
    let mut end = session.x;
    for x in session.x..session.x + session.width {
        if buf[(x, y)].symbol() != " " {
            end = x + 1;
        }
    }

    // Longest variant that still leaves a two-column gap after the status
    // text. A receipt that shears the session identity is worse than a
    // terse receipt.
    let variants: [&str; 3] = match mode {
        Mode::View | Mode::Input => return,
        Mode::Sent => [
            "sent 14B  rcpt 01J9F2C4  operator  17:29:58",
            "sent 14B  rcpt 01J9F2C4",
            "sent 14B",
        ],
        Mode::Refused => [
            "REFUSED  stale generation gen-2 -- nothing was sent",
            "REFUSED  stale gen-2  nothing sent",
            "REFUSED",
        ],
    };
    let right = session.x + session.width;
    let Some(text) = variants.into_iter().find(|text| {
        let width = text.chars().count() as u16;
        right.saturating_sub(width) >= end + 2
    }) else {
        return;
    };

    let token = if mode == Mode::Refused { "fail" } else { "ok" };
    let x = right - text.chars().count() as u16 - area.x;
    put(buf, area, x, y - area.y, text, bold(token, tier));
}

/// Mode badge plus the keys legal in that mode. In INPUT every key is a
/// byte for the agent, so the leave key must be one the agent will never
/// want: `ctrl-]` (telnet's escape, and not a key any TUI binds).
fn paint_mode_bar(buf: &mut Buffer, area: Rect, y: u16, tier: ColorTier, mode: Mode) {
    let compact = area.width < RAIL_FLOOR;
    let (badge, badge_token, keys) = match mode {
        Mode::View => (
            " VIEW ",
            "muted",
            if compact {
                "i input   j/k scroll   q back"
            } else {
                "i input   j/k scroll   / filter   : go   q back"
            },
        ),
        Mode::Refused => (
            " INPUT ",
            "fail",
            if compact {
                "ctrl-] leave   refusal clears on the next send"
            } else {
                "ctrl-] leave input   the refusal above clears on the next send or on leaving"
            },
        ),
        Mode::Sent | Mode::Input => (
            " INPUT ",
            "warn",
            if compact {
                "ctrl-] leave   keys -> pty"
            } else {
                "ctrl-] leave input   every key (Esc, ^C, arrows) goes to the agent"
            },
        ),
    };
    put(buf, area, 1, y, badge, bold(badge_token, tier));
    put(
        buf,
        area,
        1 + badge.chars().count() as u16 + 2,
        y,
        keys,
        style("muted", tier),
    );
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

struct Drill {
    drill: DrilldownState,
    title: &'static str,
}

fn attached() -> Drill {
    Drill {
        drill: common::estate::drilldown_attached(),
        title: "kernel worktree shell",
    }
}

fn paint_attach(area: Rect, buf: &mut Buffer, tier: ColorTier, glyphs: GlyphSet, mode: Mode) {
    let state = attached();
    let rail = area.width >= RAIL_FLOOR;
    paint_header(buf, area, tier, glyphs, &subject(&state.drill), state.title);

    // Reserve: header (1) + blank (1) at the top; a rule (1) and the mode
    // bar (1) at the bottom. The receipt claims no row of its own — see
    // `paint_toast` for why that matters to a hosted pty.
    let bottom = area.height - 2;
    let session = Rect {
        x: area.x + if rail { RAIL + 2 } else { 0 },
        y: area.y + 2,
        width: area.width - if rail { RAIL + 2 } else { 0 },
        height: bottom - 2,
    };

    if rail {
        paint_rail(buf, area, tier);
        paint_divider(buf, area, tier, 2, bottom);
    }

    let mut hits = HitMap::default();
    drilldown::render(session, buf, &state.drill, None, tier, &mut hits);

    paint_toast(buf, area, session, tier, mode);
    put_rule(buf, area, bottom, tier);
    paint_mode_bar(buf, area, area.height - 1, tier, mode);
}

/// The second F4 surface: `:send` composed from the TERM list, no attach.
/// The command line names its target explicitly, so a one-shot can never
/// land in whichever session happened to be focused.
fn paint_send(area: Rect, buf: &mut Buffer, tier: ColorTier, glyphs: GlyphSet) {
    paint_header(
        buf,
        area,
        tier,
        glyphs,
        "2 sessions",
        "1 running   3 attaches   2 detaches today",
    );

    let mut y = 2;
    put(buf, area, 1, y, "TERMINALS", bold("fg", tier));
    y += 1;

    for (x, label) in [
        (2u16, "SESSION"),
        (18, "GEN"),
        (25, "STATE"),
        (35, "ATT/DET"),
        (45, "TITLE"),
        (72, "OPENED"),
    ] {
        put(buf, area, x, y, label, style("muted", tier));
    }
    y += 1;

    for (focused, id, generation, state_word, churn, title, opened) in [
        (
            true,
            "pty-1",
            "gen-3",
            "running",
            "2/1",
            "kernel worktree shell",
            "10:21",
        ),
        (
            false,
            "pty-2",
            "gen-1",
            "closed",
            "1/1",
            "release notes editor",
            "09:16",
        ),
    ] {
        let live = state_word == "running";
        put(
            buf,
            area,
            0,
            y,
            if focused { ">" } else { " " },
            bold("focus", tier),
        );
        put(buf, area, 2, y, id, style("fg", tier));
        put(buf, area, 18, y, generation, style("muted", tier));
        put(
            buf,
            area,
            25,
            y,
            state_word,
            style(if live { "hue" } else { "muted" }, tier),
        );
        put(buf, area, 35, y, churn, style("muted", tier));
        put(buf, area, 45, y, title, style("fg", tier));
        put(buf, area, 72, y, opened, style("muted", tier));
        y += 1;
    }

    y += 2;
    put(buf, area, 1, y, "COMMAND", bold("fg", tier));
    y += 1;
    // The composed one-shot. Escapes are shown literally so a control byte
    // is visible in the line the operator is about to fire.
    let composed = ":send pty-1 y\\n";
    put(buf, area, 2, y, composed, bold("hue", tier));
    put(
        buf,
        area,
        2 + composed.chars().count() as u16,
        y,
        "_",
        bold("hue", tier),
    );
    y += 1;
    put(
        buf,
        area,
        2,
        y,
        "14 bytes to pty-1:gen-3 as operator -- one receipt, refused if the generation moves",
        style("muted", tier),
    );

    put_rule(buf, area, area.height - 3, tier);
    put(
        buf,
        area,
        1,
        area.height - 2,
        "enter send   ctrl-c cancel",
        style("muted", tier),
    );
    put(
        buf,
        area,
        1,
        area.height - 1,
        " COMMAND ",
        bold("hue", tier),
    );
    put(
        buf,
        area,
        12,
        area.height - 1,
        "a one-shot needs no attach; the target is named, never inferred",
        style("muted", tier),
    );
}

// ---------------------------------------------------------------------------
// The golden matrix
// ---------------------------------------------------------------------------

fn paint_view(area: Rect, buf: &mut Buffer, tier: ColorTier, glyphs: GlyphSet) {
    paint_attach(area, buf, tier, glyphs, Mode::View);
}

fn paint_input(area: Rect, buf: &mut Buffer, tier: ColorTier, glyphs: GlyphSet) {
    paint_attach(area, buf, tier, glyphs, Mode::Input);
}

fn paint_sent(area: Rect, buf: &mut Buffer, tier: ColorTier, glyphs: GlyphSet) {
    paint_attach(area, buf, tier, glyphs, Mode::Sent);
}

fn paint_refused(area: Rect, buf: &mut Buffer, tier: ColorTier, glyphs: GlyphSet) {
    paint_attach(area, buf, tier, glyphs, Mode::Refused);
}

type Painter = fn(Rect, &mut Buffer, ColorTier, GlyphSet);

fn check(scenario: &str, width: u16, height: u16, tier: ColorTier, glyphs: GlyphSet) {
    let painter: Painter = match scenario {
        "view" => paint_view,
        "input" => paint_input,
        "sent" => paint_sent,
        "refused" => paint_refused,
        "send" => paint_send,
        other => panic!("unknown scenario {other}"),
    };
    let glyph_name = match glyphs {
        GlyphSet::Unicode => "unicode",
        GlyphSet::Ascii => "ascii",
    };
    let name = format!(
        "mock-attach-{scenario}-{width}x{height}-{}-{glyph_name}",
        tier.as_str()
    );
    assert_matches_golden(&name, &dump_frame(width, height, tier, glyphs, painter));
}

#[test]
fn view_at_120x40() {
    check("view", 120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
}

#[test]
fn view_at_80x24_rail_collapsed() {
    check("view", 80, 24, ColorTier::Truecolor, GlyphSet::Unicode);
}

#[test]
fn input_at_120x40() {
    check("input", 120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
}

#[test]
fn input_degraded_mono_ascii() {
    check("input", 120, 40, ColorTier::Mono, GlyphSet::Ascii);
}

#[test]
fn sent_at_120x40() {
    check("sent", 120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
}

#[test]
fn sent_at_80x24() {
    check("sent", 80, 24, ColorTier::Truecolor, GlyphSet::Unicode);
}

#[test]
fn refused_at_120x40() {
    check("refused", 120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
}

#[test]
fn send_at_120x40() {
    check("send", 120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
}
