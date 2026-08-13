mod common;

use common::dump_frame;
use gwk_domain::ids::Timestamp;
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use gwk_tui::board::{BoardTarget, BoardView};
use gwk_tui::console::{FleetContext, LoadState, render_fleet};
use gwk_tui::input::HitMap;

fn render(width: u16, height: u16, tier: ColorTier, glyphs: GlyphSet) -> String {
    render_at(width, height, tier, glyphs, "2026-08-11T17:30:00Z")
}

fn render_at(width: u16, height: u16, tier: ColorTier, glyphs: GlyphSet, now: &str) -> String {
    let state = common::estate::estate_board_state(BoardView::Fleet);
    let context = FleetContext {
        now: Timestamp::new(now),
        load: LoadState::Ready,
    };
    dump_frame(width, height, tier, glyphs, |area, buf, tier, glyphs| {
        let mut hits = HitMap::<BoardTarget>::new();
        render_fleet(
            area,
            buf,
            &state,
            &context,
            state
                .attempts
                .get(1)
                .map(|attempt| BoardTarget::Attempt(attempt.id.clone()))
                .as_ref(),
            tier,
            glyphs,
            &mut hits,
        );
    })
}

#[test]
fn fleet_joins_cost_sessions_leases_and_unclaimed_resources() {
    let rendered = render(120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
    assert!(rendered.contains("at-tui-impl"), "{rendered}");
    assert!(
        rendered.contains("$2.10"),
        "joined session cost is absent:\n{rendered}"
    );
    assert!(rendered.contains("UNCLAIMED"), "{rendered}");
    assert!(rendered.contains("ls-release"), "{rendered}");
    assert!(rendered.contains("wt-release"), "{rendered}");
    assert!(rendered.contains("SPEND / HOUR"), "{rendered}");
}

#[test]
fn fleet_floor_drops_whole_low_priority_columns() {
    let rendered = render(80, 24, ColorTier::Truecolor, GlyphSet::Unicode);
    let header = rendered.lines().nth(2).unwrap_or_default();
    assert!(header.contains("ATTEMPT"), "{header}");
    assert!(header.contains("STATE"), "{header}");
    assert!(header.contains("SPEND"), "{header}");
    assert!(!header.contains("TOKENS"), "{header}");
    assert!(rendered.contains("> at-pty-impl"), "{rendered}");
}

#[test]
fn fleet_degrades_marks_without_losing_state_words() {
    let rendered = render(120, 40, ColorTier::Mono, GlyphSet::Ascii);
    assert!(rendered.contains("- running"), "{rendered}");
    assert!(rendered.contains("X failed"), "{rendered}");
}

#[test]
fn fleet_age_cell_never_cuts_mid_value_at_the_design_width() {
    // Twenty-one hours after the seeded 08:45 attempt the AGE value is five
    // cells ("21h00"), which the last column only has once it budgets to
    // the row edge — the phantom gutter budgeted four and painted "21h0",
    // a plausible smaller age.
    let rendered = render_at(
        120,
        40,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
        "2026-08-12T05:45:00Z",
    );
    assert!(rendered.contains("21h00"), "{rendered}");

    // Twelve days on, the six-cell "11d20h" cannot fit at all: it drops
    // whole behind the ruled '+' omission mark instead of painting the
    // five-cell lie "11d20" (or the four-cell "11d2").
    let rendered = render_at(
        120,
        40,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
        "2026-08-23T05:45:00Z",
    );
    assert!(!rendered.contains("11d2"), "{rendered}");
}

/// The empty-ledger frame, which is the live one: nothing writes `cost_entry`
/// yet, so every money figure on this lens folds over nothing. A fold cannot
/// tell "summed to zero" from "summed over nothing" once it has run, so the
/// check is that neither the header nor the chart is willing to print a
/// dollar amount when no entry was priced.
fn render_without_costs(width: u16, height: u16, tier: ColorTier, glyphs: GlyphSet) -> String {
    let mut state = common::estate::estate_board_state(BoardView::Fleet);
    state.costs.clear();
    let context = FleetContext {
        now: Timestamp::new("2026-08-11T17:30:00Z"),
        load: LoadState::Ready,
    };
    dump_frame(width, height, tier, glyphs, |area, buf, tier, glyphs| {
        let mut hits = HitMap::<BoardTarget>::new();
        render_fleet(area, buf, &state, &context, None, tier, glyphs, &mut hits);
    })
}

#[test]
fn fleet_states_an_empty_cost_ledger_rather_than_pricing_it_at_zero() {
    // Every rung the terminal offers, because a degraded tier is where an
    // honest rendering is most likely to collapse back into a bare number.
    for (width, height, tier, glyphs) in [
        (120u16, 40u16, ColorTier::Truecolor, GlyphSet::Unicode),
        (80, 24, ColorTier::Truecolor, GlyphSet::Unicode),
        (120, 40, ColorTier::Mono, GlyphSet::Ascii),
    ] {
        let rendered = render_without_costs(width, height, tier, glyphs);
        let at = format!("at {width}x{height} {}", tier.as_str());

        // The header. `$0.00 today` beside a live attempt count reads as "the
        // estate ran and cost nothing", which is a measurement nobody took.
        assert!(
            !rendered.contains("$0.00"),
            "a zero fold priced itself {at}:\n{rendered}"
        );
        assert!(
            rendered.contains("no spend recorded"),
            "the header does not say the ledger is empty {at}:\n{rendered}"
        );

        // The chart. `peak` floors to one micro on an empty ledger, so the
        // scale and its bottom rung are drawn over data that does not exist.
        assert!(
            !rendered.contains("> $0"),
            "the spend chart drew a scale over nothing {at}:\n{rendered}"
        );
        assert!(
            rendered.contains("no scale to draw"),
            "the chart is silent about why it is blank {at}:\n{rendered}"
        );

        // The row counts stay: zero rows is a fact, and it is the one figure
        // here that was actually measured.
        assert!(
            rendered.contains("0 priced"),
            "the honest row counts went missing {at}:\n{rendered}"
        );
    }
}

#[test]
fn fleet_keeps_pricing_a_ledger_that_has_entries() {
    // The guard above must not have bought its honesty by refusing to ever
    // print a total: the seeded ledger is priced, and still prices.
    let rendered = render(120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
    assert!(rendered.contains("$"), "{rendered}");
    assert!(!rendered.contains("no spend recorded"), "{rendered}");
    assert!(!rendered.contains("no scale to draw"), "{rendered}");
}

// ---------------------------------------------------------------------------
// Round 8 — the unknowns block (B5) and the NOEND column (B6)
// ---------------------------------------------------------------------------

/// Render an arbitrary board state through the lens.
fn render_state(
    state: &gwk_tui::board::BoardState,
    width: u16,
    height: u16,
    tier: ColorTier,
    glyphs: GlyphSet,
) -> String {
    let context = FleetContext {
        now: Timestamp::new("2026-08-11T17:30:00Z"),
        load: LoadState::Ready,
    };
    dump_frame(width, height, tier, glyphs, |area, buf, tier, glyphs| {
        let mut hits = HitMap::<BoardTarget>::new();
        render_fleet(area, buf, state, &context, None, tier, glyphs, &mut hits);
    })
}

/// The partial page with a running attempt whose engine session is off it, so
/// all three of the twin's unknowns are true at once.
fn partial_page() -> gwk_tui::board::BoardState {
    let mut state = common::estate::estate_board_state(BoardView::Fleet);
    state.complete = false;
    state
        .sessions
        .retain(|session| session.id.as_str() != "es-pty-impl");
    state
}

fn row<'a>(rendered: &'a str, id: &str) -> &'a str {
    rendered
        .lines()
        .find(|line| line.contains(id))
        .unwrap_or_else(|| panic!("no row for {id}:\n{rendered}"))
}

#[test]
fn fleet_column_does_not_claim_liveness_it_cannot_read() {
    // The cell folds `ended_at.is_none()`. `SES` reads as "the sessions this
    // attempt has" over a number that is not that, and any liveness word would
    // claim a reading nothing in the log supports -- nothing here heartbeats,
    // probes, or watches a process.
    let header = render(120, 40, ColorTier::Truecolor, GlyphSet::Unicode)
        .lines()
        .nth(2)
        .unwrap_or_default()
        .to_owned();
    assert!(header.contains("NOEND"), "{header}");
    assert!(
        !header.contains("SES"),
        "the column still names the noun rather than the field:\n{header}"
    );
    assert!(header.contains("SUB"), "a neighbouring column fell out");
    assert!(header.contains("LEASE"), "a neighbouring column fell out");
}

#[test]
fn a_running_attempt_with_no_session_reads_as_unknown_not_as_zero() {
    // The trap the whole round is about: by the time a cell formatter sees the
    // fold, "zero unended of three sessions" and "no session record at all"
    // are both `0usize`. The SESSION count has to decide, because only one of
    // those two is a measurement.
    for (tier, glyphs) in [
        (ColorTier::Truecolor, GlyphSet::Unicode),
        (ColorTier::Mono, GlyphSet::Ascii),
    ] {
        let rendered = render_state(&partial_page(), 120, 40, tier, glyphs);
        let at = tier.as_str();

        // Running, three spawns, and no engine session on the page.
        assert!(
            row(&rendered, "at-pty-impl").contains('?'),
            "a running attempt with no session record read as a measured zero at {at}:\n{rendered}"
        );
        // And the mark is not simply always on: this attempt HAS a live
        // session, so its cell is a count.
        assert!(
            !row(&rendered, "at-tui-impl").contains('?'),
            "the unknown mark leaked onto a bound attempt at {at}:\n{rendered}"
        );
        // Nor does a terminal attempt with no session get it -- no session is
        // the expected shape there, and `-` is the honest zero.
        assert!(
            !row(&rendered, "at-ship-done").contains('?'),
            "the unknown mark leaked onto a finished attempt at {at}:\n{rendered}"
        );

        // The discriminating case for WHICH count decides. This attempt is
        // Running and HAS an engine session; that session simply recorded an
        // end. Unended is zero and sessions is one, and only the second makes
        // the cell unknown -- a fold that branched on the unended count would
        // paint `?` here and be wrong about a page that answered the question.
        let mut settled = partial_page();
        for session in &mut settled.sessions {
            if session.attempt_id.as_str() == "at-tui-impl" {
                session.ended_at = Some(Timestamp::new("2026-08-11T17:00:00Z"));
            }
        }
        let rendered = render_state(&settled, 120, 40, tier, glyphs);
        assert!(
            !row(&rendered, "at-tui-impl").contains('?'),
            "a measured zero was reported as an absence at {at}:\n{rendered}"
        );
    }
}

#[test]
fn the_lens_states_the_three_facts_the_log_does_not_carry() {
    // B5: `agent_fleet` has always built these and `render_fleet` never asked
    // for them. Checked at Mono/Ascii as well, deliberately: the block leans on
    // no colour, because every token these rows use resolves to the terminal's
    // own foreground down there.
    for (tier, glyphs) in [
        (ColorTier::Truecolor, GlyphSet::Unicode),
        (ColorTier::Mono, GlyphSet::Ascii),
    ] {
        let rendered = render_state(&partial_page(), 120, 40, tier, glyphs);
        let at = tier.as_str();
        assert!(
            rendered.contains("UNKNOWN  3 facts not in the log"),
            "no unknowns block at {at}:\n{rendered}"
        );
        for note in [
            "liveness: no end stamp on 2 of 4 sessions -- unended is not alive",
            "engine binding: none on this page for 1 running attempt",
            "fleet size: read short of the last page -- counts are floors",
        ] {
            assert!(
                rendered.contains(note),
                "the lens is silent about {note:?} at {at}:\n{rendered}"
            );
        }
    }
}

#[test]
fn the_block_keeps_its_subjects_when_the_floor_takes_its_words() {
    // At 80x24 four rows of caveats is a fifth of the frame, so the `why`
    // clauses go. The SUBJECTS do not: a block degraded to a bare count would
    // be stating the number of things it was declining to name.
    let rendered = render_state(&partial_page(), 80, 24, ColorTier::Mono, GlyphSet::Ascii);
    assert!(
        rendered.contains("UNKNOWN  liveness, engine binding, fleet size"),
        "the floor dropped the subjects too:\n{rendered}"
    );
    assert!(
        !rendered.contains("unended is not alive"),
        "the floor kept the worded form and paid four rows for it:\n{rendered}"
    );
    // And the row it cost came out of the tail of the list, not the head: the
    // attempt the engine-binding note is ABOUT is still on screen.
    assert!(
        rendered.contains("at-pty-impl"),
        "the block displaced its own subject:\n{rendered}"
    );
}

#[test]
fn a_page_with_nothing_unknown_carries_no_block_at_all() {
    // The treatment is evidence-driven chrome, not a standing disclaimer. A
    // block on every frame is a block the eye learns to skip, and the note
    // COUNT is what decides -- never a flag, never a width.
    let mut state = common::estate::estate_board_state(BoardView::Fleet);
    for session in &mut state.sessions {
        if session.ended_at.is_none() {
            session.ended_at = Some(Timestamp::new("2026-08-11T17:00:00Z"));
        }
    }
    let rendered = render_state(&state, 120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
    assert!(
        !rendered.contains("UNKNOWN"),
        "a page carrying every fact still apologised for one:\n{rendered}"
    );
    // The rows the block would have taken went back to the estate.
    assert!(rendered.contains("UNCLAIMED"), "{rendered}");
}

#[test]
fn a_partial_read_folds_to_a_floor_and_says_which() {
    // `BoardState::complete` was never read by this lens. A fold over a read
    // that stopped short of the last projection page is a floor, and a header
    // printing it as a total is the same lie the empty ledger told.
    let partial = render_state(
        &partial_page(),
        120,
        40,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
    );
    assert!(
        partial.contains("at least 11 attempts"),
        "a partial page presented its counts as totals:\n{partial}"
    );

    let complete = render(120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
    assert!(
        complete.contains("11 attempts") && !complete.contains("at least"),
        "a complete read hedged a count it had actually finished:\n{complete}"
    );
}

#[test]
fn an_empty_ledger_and_an_empty_page_are_different_sentences() {
    // "no records at all" is a claim about the LEDGER, and only a read that
    // reached the last page can make it. A read that stopped short saw an
    // empty page and knows nothing about the rest.
    let mut complete = common::estate::estate_board_state(BoardView::Fleet);
    complete.costs.clear();
    let rendered = render_state(&complete, 120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
    assert!(rendered.contains("no spend recorded"), "{rendered}");

    let mut partial = complete.clone();
    partial.complete = false;
    let rendered = render_state(&partial, 120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
    assert!(
        rendered.contains("no spend on this page"),
        "a partial read claimed the whole ledger was empty:\n{rendered}"
    );
    assert!(!rendered.contains("no spend recorded"), "{rendered}");
}

#[test]
fn the_header_chrome_never_paints_over_the_summary_it_follows() {
    // The summary is painted first and the tier/watermark tail right-aligned
    // OVER it. At 100 columns the old `width < 100` threshold picked both long
    // forms and the collision ate the whole spend figure, leaving a header
    // that read as complete. The tail gives up its low-priority parts as one
    // column instead.
    let rendered = render_state(
        &partial_page(),
        100,
        30,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
    );
    let header = rendered.lines().next().unwrap_or_default();
    assert!(
        header.contains("$5.30 today"),
        "the chrome tail ate the spend figure:\n{header}"
    );
    assert!(header.contains("17:30"), "the clock fell off:\n{header}");
    assert!(
        !header.contains("as-of"),
        "the tail kept its low-priority parts and collided anyway:\n{header}"
    );
}

/// A page that contradicts itself: one engine session appearing twice. This is
/// what `agent_fleet` turns into a finding, and it is a different class from
/// every unknown the lens already names — those say the log is silent, this
/// says the page is wrong.
fn contradictory_page() -> gwk_tui::board::BoardState {
    let mut state = common::estate::estate_board_state(BoardView::Fleet);
    let duplicate = state
        .sessions
        .first()
        .cloned()
        .expect("the estate fixture carries at least one session");
    state.sessions.push(duplicate);
    state
}

#[test]
fn the_alarm_reads_before_the_rows_it_impeaches() {
    let rendered = render_state(
        &contradictory_page(),
        120,
        40,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
    );
    let alarm = rendered
        .lines()
        .position(|line| line.contains("INTEGRITY"))
        .unwrap_or_else(|| panic!("no integrity alarm on a contradictory page:\n{rendered}"));
    let columns = rendered
        .lines()
        .position(|line| line.contains("ATTEMPT") && line.contains("STATE"))
        .unwrap_or_else(|| panic!("no column heads:\n{rendered}"));
    // The whole ruling in one assertion. A caveat printed after the table has
    // already let the reader believe the rows.
    assert!(
        alarm < columns,
        "the alarm printed at line {alarm}, below the column heads at {columns}:\n{rendered}"
    );
    assert!(rendered.contains("duplicate"), "{rendered}");
}

#[test]
fn a_clean_page_paints_no_alarm_and_loses_no_rows() {
    let clean = common::estate::estate_board_state(BoardView::Fleet);
    let with_alarm = render_state(
        &contradictory_page(),
        120,
        40,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
    );
    let without = render_state(&clean, 120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
    assert!(
        !without.contains("INTEGRITY"),
        "a clean page raised an alarm:\n{without}"
    );
    // Evidence, not chrome: the block costs nothing when there is nothing to
    // report, so the clean frame keeps its column heads where they always were.
    let clean_columns = without
        .lines()
        .position(|line| line.contains("ATTEMPT") && line.contains("STATE"));
    let alarmed_columns = with_alarm
        .lines()
        .position(|line| line.contains("ATTEMPT") && line.contains("STATE"));
    assert_eq!(clean_columns, Some(2), "{without}");
    assert!(
        alarmed_columns > clean_columns,
        "the alarm did not push the table down at all — it is painting over it"
    );
}

#[test]
fn the_alarm_is_carried_by_words_not_colour() {
    // The specific way B4 was worse than a plain omission. Mono and ascii lose
    // every binding; an alarm that reads as an alarm only in truecolor is not
    // an alarm.
    let state = contradictory_page();
    let colour = render_state(&state, 120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
    let mono = render_state(&state, 120, 40, ColorTier::Mono, GlyphSet::Ascii);
    let words = |frame: &str| {
        frame
            .lines()
            .filter(|line| line.contains("INTEGRITY") || line.contains("duplicate"))
            .map(str::trim_end)
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert!(!words(&colour).is_empty(), "{colour}");
    assert_eq!(
        words(&colour),
        words(&mono),
        "the alarm said different things in mono:\n{mono}"
    );
}

#[test]
fn the_alarm_degrades_rather_than_eating_the_fleet() {
    // At 80x24 the block gives up its itemization and keeps the count, the
    // same trade the unknown block makes one rung lower.
    let rendered = render_state(
        &contradictory_page(),
        80,
        24,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
    );
    assert!(
        rendered.contains("INTEGRITY"),
        "the alarm vanished at the floor — the one thing it may never do:\n{rendered}"
    );
    let itemized = rendered.lines().filter(|l| l.contains("duplicate")).count();
    assert_eq!(itemized, 0, "the floor kept the itemization:\n{rendered}");
    // And the rows it exists to impeach are still on screen.
    assert!(rendered.contains("at-tui-impl"), "{rendered}");
}
