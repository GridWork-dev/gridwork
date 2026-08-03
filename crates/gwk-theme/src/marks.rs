//! The ratified mark inventory — the console's symbol vocabulary.
//!
//! Seventeen marks over twenty-three codepoints. Seven name WHO an agent is,
//! ten name WHAT it is doing, and the two cells sit side by side. Cardinality
//! is identity **plus** expression, never identity times expression: a per-pair
//! composite would need seventy mutually distinguishable single-cell codepoints
//! and the admissible pool is roughly two dozen.
//!
//! # The admission rule
//!
//! A codepoint enters this inventory only if its `East_Asian_Width` is Neutral
//! or Narrow AND it is not an emoji character. Both clauses are independently
//! necessary: U+26A0 is `EAW=N` and still unusable, because terminals render it
//! as a two-cell colour emoji. There is **no per-glyph waiver path**. The
//! single documented escape is the ASCII set, which is why every mark carries
//! its fallback here rather than in a table somewhere else.
//!
//! Ambiguous width is the failure the rule exists for. A renderer measures
//! `EAW=A` as one cell through unicode-width's non-CJK path while a terminal
//! configured for double-width ambiguous characters draws two, shearing every
//! cell to the right of it. That is why the SMALL triangles are here and the
//! large ones are not: U+25B2 is Ambiguous, U+25B4 is Narrow, and they look
//! nearly identical in a specification.
//!
//! The rule is not prose here. It is enforced mechanically against UCD data by
//! `every_inventory_codepoint_passes_the_admission_rule`, with a positive
//! control that feeds the gate the two canonical bad codepoints and requires it
//! to reject them.

/// Which of the two cells a mark belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkKind {
    /// WHO — the left cell. Star spans, triangle is directional; solid produces
    /// a diff, hollow produces evidence.
    Identity,
    /// WHAT — the right cell.
    Expression,
}

/// One admitted mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    /// Machine name. For an identity mark this is the role it stands for.
    pub name: &'static str,
    /// The codepoints this mark cycles through: one for a static mark, eight
    /// for the two spinner directions. Every entry is subject to the admission
    /// rule.
    pub glyphs: &'static [char],
    /// The single documented escape. ASCII is Narrow by construction, so the
    /// escape can never itself fail admission.
    pub ascii: char,
    pub kind: MarkKind,
}

impl Mark {
    /// The mark's first codepoint — the one a static mark has, and the frame a
    /// cycling mark starts on.
    pub fn head(&self) -> char {
        // Total by construction: `every_mark_has_at_least_one_glyph` pins it,
        // and a zero-glyph mark is not a mark.
        self.glyphs.first().copied().unwrap_or(' ')
    }
}

/// The eight-frame cycle. Uniformly Braille, so the whole surface carries one
/// font-coverage risk and one width class.
pub const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⢰', '⣠', '⡄', '⠆'];

/// The same eight frames played backwards. A second mark, zero new codepoints —
/// which is how seventeen marks fit in twenty-three codepoints.
pub const SPINNER_REVERSED: &[char] = &['⠆', '⡄', '⣠', '⢰', '⠸', '⠹', '⠙', '⠋'];

/// The seventeen ruled marks. This is the single inventory constant the
/// admission rule is enforced over; a glyph that is not here does not reach the
/// cell buffer.
#[rustfmt::skip]
pub const MARKS: &[Mark] = &[
    // Identity — the systematic two-axis rule. Star = spans, triangle =
    // directional; solid = produces a diff, hollow = produces evidence. A role
    // with no mark takes a deterministic ASCII uppercase initial instead, which
    // is one cell guaranteed and extends forever.
    Mark { name: "orchestrator", glyphs: &['✦'], ascii: 'O', kind: MarkKind::Identity },
    Mark { name: "researcher",   glyphs: &['✧'], ascii: 'S', kind: MarkKind::Identity },
    Mark { name: "architect",    glyphs: &['▴'], ascii: 'A', kind: MarkKind::Identity },
    Mark { name: "implementer",  glyphs: &['▸'], ascii: 'I', kind: MarkKind::Identity },
    Mark { name: "reviewer",     glyphs: &['◃'], ascii: 'R', kind: MarkKind::Identity },
    Mark { name: "auditor",      glyphs: &['▿'], ascii: 'U', kind: MarkKind::Identity },
    // Absent renders distinctly from unmapped: the role is `Option<String>`,
    // and None is a fact rather than a gap.
    Mark { name: "role_absent",  glyphs: &['∙'], ascii: '.', kind: MarkKind::Identity },

    // Expression.
    Mark { name: "idle",             glyphs: &['⠄'],      ascii: '.', kind: MarkKind::Expression },
    Mark { name: "queued",           glyphs: &['⠒'],      ascii: ':', kind: MarkKind::Expression },
    Mark { name: "spinner",          glyphs: SPINNER,          ascii: '-', kind: MarkKind::Expression },
    Mark { name: "spinner_reversed", glyphs: SPINNER_REVERSED, ascii: '~', kind: MarkKind::Expression },
    // Plain `!`, not U+26A0: the warning sign is EAW=N and passes the first
    // clause, and is an emoji character, and fails the second.
    Mark { name: "attention",        glyphs: &['!'],      ascii: '!', kind: MarkKind::Expression },
    Mark { name: "blocked",          glyphs: &['⊘'],      ascii: '#', kind: MarkKind::Expression },
    // U+2717, not U+2718; U+2713, not U+2714. The heavy forms are emoji.
    Mark { name: "failed",           glyphs: &['✗'],      ascii: 'X', kind: MarkKind::Expression },
    Mark { name: "done",             glyphs: &['✓'],      ascii: 'v', kind: MarkKind::Expression },
    Mark { name: "canceled",         glyphs: &['⊖'],      ascii: 'o', kind: MarkKind::Expression },
    Mark { name: "unknown",          glyphs: &['?'],      ascii: '?', kind: MarkKind::Expression },
];

/// One agent state: which mark expresses it and which token colours it.
///
/// The bindings are ratified with the marks. Three states sit on `warn` and are
/// separated **100% by glyph and 0% by colour, by design** — colour encodes the
/// axis (is this attention-worthy), the glyph encodes the identity. `unknown`
/// is never `fail`, and `blocked` is never `hue`, because `hue` means running
/// and being blocked is not a human-wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateBinding {
    pub name: &'static str,
    /// A `Mark::name` in [`MARKS`].
    pub mark: &'static str,
    /// A [`crate::Token::name`] in [`crate::SIGNAL`].
    pub token: &'static str,
}

/// The eleven states over ten expression marks.
#[rustfmt::skip]
pub const STATES: &[StateBinding] = &[
    StateBinding { name: "idle",            mark: "idle",             token: "muted" },
    StateBinding { name: "queued",          mark: "queued",           token: "muted" },
    // `starting` and `running` share the spinner and differ only in accent
    // intensity. See `exactly_one_state_pair_is_separated_by_colour_alone`.
    StateBinding { name: "starting",        mark: "spinner",          token: "hue_dim" },
    StateBinding { name: "running",         mark: "spinner",          token: "hue" },
    StateBinding { name: "canceling",       mark: "spinner_reversed", token: "muted" },
    StateBinding { name: "needs_attention", mark: "attention",        token: "warn" },
    StateBinding { name: "blocked",         mark: "blocked",          token: "warn" },
    StateBinding { name: "failed",          mark: "failed",           token: "fail" },
    StateBinding { name: "done",            mark: "done",             token: "ok" },
    StateBinding { name: "canceled",        mark: "canceled",         token: "muted" },
    StateBinding { name: "unknown",         mark: "unknown",          token: "warn" },
];

/// The mark named `name`, or `None`.
pub fn mark(name: &str) -> Option<&'static Mark> {
    MARKS.iter().find(|mark| mark.name == name)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use unicode_properties::UnicodeEmoji;
    use unicode_width::UnicodeWidthChar;

    use super::*;
    use crate::SIGNAL;

    /// The admission rule as one total predicate, so the inventory sweep and
    /// its positive control run the SAME code. A gate proved only against the
    /// values it passes is not proved.
    fn admissible(c: char) -> Result<(), String> {
        // `EAW ∈ {N, Na}` expressed through the two width paths, which is the
        // property that actually bites: a Neutral or Narrow character is one
        // cell in BOTH, an Ambiguous one is one cell in the non-CJK path and
        // two in the CJK path. Wide and Fullwidth are two in both. There is no
        // EAW-class accessor in the crate graph, and this formulation is
        // stronger than one anyway — it tests the rendered consequence rather
        // than the classification the consequence is derived from.
        match (c.width(), c.width_cjk()) {
            (Some(1), Some(1)) => {}
            (narrow, cjk) => {
                return Err(format!(
                    "U+{:04X} is {narrow:?} cells wide normally and {cjk:?} under \
                     ambiguous-width=double; the admission rule takes only 1 and 1",
                    c as u32
                ));
            }
        }
        // Non-emoji, read strictly: neither an emoji character nor an emoji
        // component. A component is only emoji inside a sequence, but nothing
        // in this inventory needs one, so the inventory takes the strict
        // reading and leaves the loose one to whoever needs it.
        if c.is_emoji_char() || c.is_emoji_component() {
            return Err(format!(
                "U+{:04X} is an emoji character or component; terminals substitute a \
                 colour emoji font for it and the cell stops being one cell",
                c as u32
            ));
        }
        Ok(())
    }

    #[test]
    fn every_inventory_codepoint_passes_the_admission_rule() {
        for entry in MARKS {
            for &glyph in entry.glyphs {
                if let Err(why) = admissible(glyph) {
                    panic!("mark {:?}: {why}", entry.name);
                }
            }
            // The escape is ASCII by construction, and this is what makes that
            // claim mechanical rather than asserted.
            assert!(
                entry.ascii.is_ascii_graphic(),
                "mark {:?}: the escape must be printable ASCII",
                entry.name
            );
        }
    }

    #[test]
    fn the_admission_gate_rejects_the_codepoints_it_exists_for() {
        // The positive control. Without it, `every_inventory_codepoint_passes`
        // would still be green if `admissible` returned `Ok(())` for
        // everything — a gate nobody has watched reject is a gate that asserts
        // nothing. U+25C6 is the canonical ambiguous-width case: it is the
        // glyph an earlier draft printed, it measures one cell in the non-CJK
        // path, and it shears the row on a CJK-ambiguous-wide terminal. U+26A0
        // is the canonical second-clause case: Neutral width, still an emoji.
        for (bad, why) in [('◆', "ambiguous width"), ('⚠', "emoji")] {
            assert!(
                admissible(bad).is_err(),
                "U+{:04X} ({why}) was admitted; the gate is broken",
                bad as u32
            );
        }
        // And the control cuts both ways: a mark that IS admissible must pass,
        // or the gate could be a constant `Err`.
        assert!(admissible('▸').is_ok());
    }

    #[test]
    fn seventeen_marks_over_twenty_three_codepoints() {
        assert_eq!(MARKS.len(), 17, "the ruled inventory is 17 marks");
        let codepoints: BTreeSet<char> = MARKS
            .iter()
            .flat_map(|m| m.glyphs.iter().copied())
            .collect();
        assert_eq!(
            codepoints.len(),
            23,
            "the ruled inventory is 23 codepoints — 15 static plus one 8-frame cycle, \
             with the reversed cycle contributing no new ones"
        );
        assert_eq!(
            MARKS
                .iter()
                .filter(|m| m.kind == MarkKind::Identity)
                .count(),
            7
        );
        assert_eq!(
            MARKS
                .iter()
                .filter(|m| m.kind == MarkKind::Expression)
                .count(),
            10
        );
    }

    #[test]
    fn every_mark_has_at_least_one_glyph_and_a_unique_name() {
        let mut names: Vec<&str> = MARKS.iter().map(|m| m.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), MARKS.len(), "duplicate mark name");
        for entry in MARKS {
            assert!(
                !entry.glyphs.is_empty(),
                "mark {:?} has no glyph",
                entry.name
            );
        }
    }

    #[test]
    fn every_state_binding_resolves_to_a_real_mark_and_a_real_token() {
        for state in STATES {
            let bound = mark(state.mark)
                .unwrap_or_else(|| panic!("state {:?} names mark {:?}", state.name, state.mark));
            assert_eq!(
                bound.kind,
                MarkKind::Expression,
                "state {:?} is bound to an identity mark",
                state.name
            );
            assert!(
                SIGNAL.iter().any(|token| token.name == state.token),
                "state {:?} names token {:?}, which is not in SIGNAL",
                state.name,
                state.token
            );
        }
        let mut names: Vec<&str> = STATES.iter().map(|s| s.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), STATES.len(), "duplicate state name");
    }

    #[test]
    fn every_expression_mark_carries_a_state() {
        // The inverse of the binding test, and the one that catches a mark
        // admitted into the inventory that nothing ever renders.
        for entry in MARKS.iter().filter(|m| m.kind == MarkKind::Expression) {
            assert!(
                STATES.iter().any(|state| state.mark == entry.name),
                "expression mark {:?} is bound to no state",
                entry.name
            );
        }
    }

    #[test]
    fn exactly_one_state_pair_is_separated_by_colour_alone() {
        // A REPORTED collision between two rulings, pinned rather than
        // resolved. The mark set rules `starting` as a dimmed spinner and
        // `running` as the same spinner undimmed, which makes them one glyph
        // and two accent intensities. The injectivity invariant says no two
        // states are distinguished by colour alone and the glyph is sufficient
        // for every pair. Both cannot hold, and at the bottom tier — where no
        // colour is emitted at all — the two states become the same cell.
        //
        // The operator holds the call, so this test decides nothing: it pins
        // the collision at exactly one pair, so a second one cannot arrive
        // quietly while the first is still open.
        let mut colour_only: Vec<(&str, &str)> = Vec::new();
        for (i, a) in STATES.iter().enumerate() {
            for b in &STATES[i + 1..] {
                if a.mark == b.mark && a.token != b.token {
                    colour_only.push((a.name, b.name));
                }
            }
        }
        assert_eq!(
            colour_only,
            vec![("starting", "running")],
            "the state pairs separated by colour alone changed; the known one is \
             an open operator call, a new one is a defect"
        );
    }
}
