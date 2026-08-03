//! The ratified mark inventory — the console's symbol vocabulary.
//!
//! Eighteen marks over twenty-four codepoints. Seven name WHO an agent is,
//! eleven name WHAT it is doing, and the two cells sit side by side.
//! Cardinality is identity **plus** expression, never identity times
//! expression: a per-pair composite would need seventy-seven mutually
//! distinguishable single-cell codepoints and the admissible pool is roughly
//! two dozen.
//!
//! Eleven expression marks for eleven states, one each. That the two counts
//! match is the whole point — see `the_state_to_glyph_map_is_injective`.
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
    // `starting` had no mark of its own: it was the running spinner dimmed, so
    // the two states were separated by accent intensity and nothing else, and
    // at the bottom tier — where no colour is emitted — they became one cell.
    // The operator ruled it a distinct mark. Braille, because that family is
    // already here and adds no second font-coverage risk and no second width
    // class; STATIC, because "the engine has not come up yet" is honestly a
    // non-moving state, and a state deciding not to move at all is exactly what
    // the motion rules leave to it.
    //
    // U+2809 is dots 1+4 — the whole TOP ROW. ADR-0029 first picked U+2808
    // (dot 4 alone) for being the diagonal opposite of `idle`'s lower-left dot,
    // and the rendered probe showed why position alone was the wrong axis: two
    // single dots at the lightest available ink read as the same speck at a
    // glance, opposite corners or not. Two dots carry double the ink of `idle`,
    // so the pair now separates by WEIGHT as well as position — and a weight
    // difference survives the squint that a position difference does not.
    // ADR-0030 amends ADR-0029 on this one property; everything else it ruled
    // (braille, static, `hue_dim`) stands.
    Mark { name: "starting",         glyphs: &['⠉'],      ascii: '^', kind: MarkKind::Expression },
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
    // `starting` keeps `hue_dim` — it is still a live-but-not-yet-running
    // state and the dimmer accent reads correctly. What changed is that the
    // accent is no longer the ONLY thing separating it from `running`.
    StateBinding { name: "starting",        mark: "starting",         token: "hue_dim" },
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

/// The `--glyphs=` value. Exactly two, and **no detection**.
///
/// The admission rule has no per-glyph waiver path, so this is the single
/// documented escape for the whole inventory: a terminal or font that cannot
/// draw one mark cannot draw it, and the answer is the ASCII set rather than a
/// substitution nobody ruled.
///
/// Structurally symmetric with the colour flag — a closed value set, parsed
/// once at startup, an unknown value refused rather than absorbed. It is
/// deliberately NOT run through [`crate::TerminalEnv`], because that resolver's
/// whole job is reading the environment and this flag's whole ruling is that
/// nothing is detected. Two values is the entire vocabulary; a font-coverage
/// probe would be a third answer the ruling does not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GlyphSet {
    #[default]
    Unicode,
    Ascii,
}

impl GlyphSet {
    pub const FLAG: &'static str = "--glyphs";
    pub const VALUES: &'static [&'static str] = &["unicode", "ascii"];

    pub fn parse(value: &str) -> Result<Self, crate::FlagValueError> {
        match value {
            "unicode" => Ok(GlyphSet::Unicode),
            "ascii" => Ok(GlyphSet::Ascii),
            other => Err(crate::FlagValueError::new(Self::FLAG, other, Self::VALUES)),
        }
    }

    pub const fn is_ascii(self) -> bool {
        matches!(self, GlyphSet::Ascii)
    }
}

impl Mark {
    /// The codepoint this mark shows on `frame` under `glyphs`.
    ///
    /// A cycling mark advances; a static one ignores the counter. Under the
    /// escape every mark is its single ASCII fallback, cycles included: an
    /// animated ASCII spinner would need eight ASCII frames nobody ruled, and
    /// inventing them here would be minting inventory through the escape hatch.
    pub fn at(&self, frame: usize, glyphs: GlyphSet) -> char {
        if glyphs.is_ascii() || self.glyphs.is_empty() {
            return self.ascii;
        }
        self.glyphs[frame % self.glyphs.len()]
    }
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
    fn eighteen_marks_over_twenty_four_codepoints() {
        assert_eq!(MARKS.len(), 18, "the ruled inventory is 18 marks");
        let codepoints: BTreeSet<char> = MARKS
            .iter()
            .flat_map(|m| m.glyphs.iter().copied())
            .collect();
        assert_eq!(
            codepoints.len(),
            24,
            "the ruled inventory is 24 codepoints — 16 static plus one 8-frame cycle, \
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
            11
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
    fn the_escape_is_the_whole_inventory_and_has_exactly_two_values() {
        // The escape covers EVERY mark, because the admission rule has no
        // per-glyph waiver: a partial escape would leave the surface with one
        // unrenderable cell and no way to ask for a different one.
        for entry in MARKS {
            assert_eq!(entry.at(0, GlyphSet::Ascii), entry.ascii, "{}", entry.name);
            // Including the cycles, at every frame.
            for frame in 0..16 {
                assert_eq!(entry.at(frame, GlyphSet::Ascii), entry.ascii);
            }
        }
        // Unicode is the default and the cycles cycle.
        assert_eq!(GlyphSet::default(), GlyphSet::Unicode);
        let spinner = mark("spinner").expect("spinner");
        let frames: BTreeSet<char> = (0..8).map(|f| spinner.at(f, GlyphSet::Unicode)).collect();
        assert_eq!(frames.len(), 8);
        assert_eq!(
            spinner.at(8, GlyphSet::Unicode),
            spinner.at(0, GlyphSet::Unicode)
        );

        assert_eq!(GlyphSet::parse("ascii"), Ok(GlyphSet::Ascii));
        assert_eq!(GlyphSet::parse("unicode"), Ok(GlyphSet::Unicode));
        let err = GlyphSet::parse("auto").expect_err("there is no detection");
        let text = err.to_string();
        assert!(text.contains("--glyphs"), "{text}");
        for allowed in GlyphSet::VALUES {
            assert!(text.contains(allowed), "{text} omits {allowed}");
            assert!(GlyphSet::parse(allowed).is_ok(), "{allowed}");
        }
        assert_eq!(
            GlyphSet::VALUES.len(),
            2,
            "exactly two values, no detection"
        );
    }

    #[test]
    fn the_escape_never_collides_two_marks_into_one_cell() {
        // The property that makes the escape usable rather than merely
        // available: two states that were distinct under the inventory must
        // stay distinct under ASCII. Identity marks may repeat a letter with a
        // second-letter rule, so this binds the EXPRESSION set, which is where
        // the states live.
        let mut seen: Vec<(char, &str)> = MARKS
            .iter()
            .filter(|m| m.kind == MarkKind::Expression)
            .map(|m| (m.ascii, m.name))
            .collect();
        seen.sort_unstable();
        let collisions: Vec<_> = seen
            .windows(2)
            .filter(|pair| pair[0].0 == pair[1].0)
            .collect();
        assert!(
            collisions.is_empty(),
            "the ASCII escape collapses expression marks: {collisions:?}"
        );
    }

    #[test]
    fn the_state_to_glyph_map_is_injective() {
        // THE invariant: no two states in this system are distinguished by
        // colour alone, and the glyph alone is sufficient for every pair. It is
        // what makes a washed sixteen-colour terminal degrade to monochrome
        // SEMANTICS rather than to nothing — if colour carries zero bits
        // anywhere, removing all of it removes zero bits.
        //
        // It did not hold when this file was first written. `starting` was the
        // running spinner dimmed, so those two states were one glyph and two
        // accent intensities, and at the bottom tier they were the same cell.
        // Both readings were ratified text and neither could be quietly
        // preferred, so the collision was reported and pinned at exactly one
        // pair. The operator ruled: mint `starting` its own mark. This test is
        // the invariant it restored, asserted at full strength.
        let mut by_mark: Vec<(&str, &str)> = STATES.iter().map(|s| (s.mark, s.name)).collect();
        by_mark.sort_unstable();
        let shared: Vec<_> = by_mark
            .windows(2)
            .filter(|pair| pair[0].0 == pair[1].0)
            .map(|pair| (pair[0].0, pair[0].1, pair[1].1))
            .collect();
        assert!(
            shared.is_empty(),
            "these states share a mark, so the glyph alone no longer distinguishes \
             them and whatever separates them is colour: {shared:?}"
        );
        // The stronger consequence, stated so it cannot be lost: eleven states
        // over eleven expression marks, one each.
        assert_eq!(
            STATES.len(),
            MARKS
                .iter()
                .filter(|m| m.kind == MarkKind::Expression)
                .count()
        );
    }
}
