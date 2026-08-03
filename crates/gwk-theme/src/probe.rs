//! Deciding whether a terminal can carry the unicode inventory.
//!
//! **This module measures nothing, and cannot.** `gwk-theme` emits and parses no
//! terminal byte — that is the classification keeping it outside the clean-room
//! gate, and a probe that wrote an escape sequence here would break it. The
//! measurement belongs one crate over, where terminal I/O is allowed. What lives
//! here is the total function from a set of measurements to a verdict, plus the
//! rule that decides what to do when the terminal does not answer.
//!
//! # What this can and cannot detect
//!
//! It detects **shear**: a mark the terminal lays out as two cells, which pushes
//! every column right of it out of alignment and is the failure the admission
//! rule exists to prevent. A cursor-position report measures that directly and
//! works over a remote attach, because it asks the terminal that is actually
//! drawing rather than inspecting the local machine's fonts.
//!
//! It does **not** detect tofu. A missing glyph drawn as a replacement box still
//! occupies one cell, so it is indistinguishable here from a glyph that rendered
//! correctly. No terminal protocol reports glyph coverage. Saying so is the
//! honest position; the escape for that case is the operator passing
//! `--glyphs=ascii`, which needs no detection.
//!
//! # The rule for an unanswered question
//!
//! A terminal that does not reply to a cursor-position query has not passed the
//! probe — it has declined to take it. [`ProbeOutcome::Inconclusive`] is
//! therefore a distinct outcome from a pass, and it degrades to ASCII exactly
//! like a failure does. Treating silence as consent is how a probe becomes
//! decoration.

use crate::marks::{GlyphSet, MARKS};

/// What a terminal reported for one inventory codepoint: how many cells its
/// cursor advanced after the glyph was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellReport {
    pub glyph: char,
    pub cells: u16,
}

/// The verdict over a whole set of reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Every inventory codepoint was reported, and every one measured one cell.
    UnicodeSafe,
    /// At least one codepoint measured other than one cell. Carries them.
    Sheared(Vec<char>),
    /// Every reported codepoint measured one cell, but some were never
    /// reported. Carries the unanswered ones.
    Inconclusive(Vec<char>),
}

impl ProbeOutcome {
    /// The glyph set to actually use. Only an unqualified pass earns unicode;
    /// shear and silence both degrade, because both mean the same thing to a
    /// reader looking at a broken column.
    pub fn glyph_set(&self) -> GlyphSet {
        match self {
            ProbeOutcome::UnicodeSafe => GlyphSet::Unicode,
            ProbeOutcome::Sheared(_) | ProbeOutcome::Inconclusive(_) => GlyphSet::Ascii,
        }
    }

    /// Whether the surface may use its unicode marks.
    pub fn is_safe(&self) -> bool {
        matches!(self, ProbeOutcome::UnicodeSafe)
    }
}

/// Every distinct codepoint the surface can draw, in inventory order.
///
/// Derived from [`MARKS`] rather than written down, so a mark added to the
/// inventory is automatically a mark the probe demands an answer about. A
/// hand-maintained list here would silently stop covering the surface the first
/// time the two drifted.
pub fn inventory_codepoints() -> Vec<char> {
    let mut out: Vec<char> = Vec::new();
    for mark in MARKS {
        for &g in mark.glyphs {
            if !out.contains(&g) {
                out.push(g);
            }
        }
    }
    out
}

/// Turn a set of measurements into a verdict.
///
/// Shear outranks silence: if anything measured wrong, that is the finding, and
/// the unanswered ones are not worth enumerating beside it.
pub fn evaluate(reports: &[CellReport]) -> ProbeOutcome {
    let mut sheared = Vec::new();
    let mut unanswered = Vec::new();

    for cp in inventory_codepoints() {
        match reports.iter().find(|r| r.glyph == cp) {
            Some(r) if r.cells == 1 => {}
            Some(_) => sheared.push(cp),
            None => unanswered.push(cp),
        }
    }

    if !sheared.is_empty() {
        ProbeOutcome::Sheared(sheared)
    } else if !unanswered.is_empty() {
        ProbeOutcome::Inconclusive(unanswered)
    } else {
        ProbeOutcome::UnicodeSafe
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_ok() -> Vec<CellReport> {
        inventory_codepoints()
            .into_iter()
            .map(|glyph| CellReport { glyph, cells: 1 })
            .collect()
    }

    #[test]
    fn probe_the_inventory_is_read_from_the_marks_not_written_down() {
        let cps = inventory_codepoints();
        // 18 marks over 24 distinct codepoints; the spinner and its reverse
        // share all eight frames, which is why this is not simply MARKS.len().
        assert_eq!(cps.len(), 24, "inventory codepoint count moved");
        let mut sorted = cps.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            cps.len(),
            "inventory_codepoints returned a duplicate"
        );
    }

    #[test]
    fn probe_a_full_clean_report_is_the_only_way_to_earn_unicode() {
        assert_eq!(evaluate(&all_ok()), ProbeOutcome::UnicodeSafe);
        assert_eq!(evaluate(&all_ok()).glyph_set(), GlyphSet::Unicode);
        assert!(evaluate(&all_ok()).is_safe());
    }

    #[test]
    fn probe_one_wide_cell_shears_and_degrades() {
        let mut reports = all_ok();
        let victim = reports[3].glyph;
        reports[3].cells = 2;

        match evaluate(&reports) {
            ProbeOutcome::Sheared(cs) => assert_eq!(cs, vec![victim]),
            other => panic!("expected Sheared, got {other:?}"),
        }
        assert_eq!(evaluate(&reports).glyph_set(), GlyphSet::Ascii);
    }

    #[test]
    fn probe_a_zero_width_report_is_shear_too_not_just_a_wide_one() {
        // A terminal that swallows a glyph entirely is as broken for layout as
        // one that doubles it, and an `> 1` test would have missed it.
        let mut reports = all_ok();
        let victim = reports[0].glyph;
        reports[0].cells = 0;

        match evaluate(&reports) {
            ProbeOutcome::Sheared(cs) => assert_eq!(cs, vec![victim]),
            other => panic!("expected Sheared, got {other:?}"),
        }
    }

    #[test]
    fn probe_silence_is_not_consent() {
        let mut reports = all_ok();
        let dropped = reports.pop().expect("inventory is not empty").glyph;

        match evaluate(&reports) {
            ProbeOutcome::Inconclusive(cs) => assert_eq!(cs, vec![dropped]),
            other => panic!("expected Inconclusive, got {other:?}"),
        }
        assert_eq!(evaluate(&reports).glyph_set(), GlyphSet::Ascii);
        assert!(!evaluate(&reports).is_safe());
    }

    #[test]
    fn probe_no_reports_at_all_names_the_whole_inventory_rather_than_passing() {
        match evaluate(&[]) {
            ProbeOutcome::Inconclusive(cs) => assert_eq!(cs.len(), 24),
            other => panic!("an empty report set must not pass, got {other:?}"),
        }
        assert_eq!(evaluate(&[]).glyph_set(), GlyphSet::Ascii);
    }

    #[test]
    fn probe_shear_outranks_silence() {
        let mut reports = all_ok();
        reports[2].cells = 2;
        reports.pop();

        assert!(
            matches!(evaluate(&reports), ProbeOutcome::Sheared(_)),
            "a measured failure is the finding; the unanswered ones are noise beside it"
        );
    }

    #[test]
    fn probe_a_report_for_a_codepoint_we_never_asked_about_changes_nothing() {
        let mut reports = all_ok();
        // '中' is not in the inventory and is genuinely two cells. A probe that
        // let stray input into the verdict could be failed by noise on the pipe.
        reports.push(CellReport {
            glyph: '中',
            cells: 2,
        });
        assert_eq!(evaluate(&reports), ProbeOutcome::UnicodeSafe);
    }
}
