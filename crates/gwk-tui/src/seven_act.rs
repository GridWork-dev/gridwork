//! The seven-act phase lifecycle as a client-side template over generic
//! tasks and gates.
//!
//! This is a TEMPLATE, and template means data: seven rows naming the open
//! `kind` strings a choreographed phase writes through commands the kernel
//! already has — `create_task`, `open_gate`, `decide_gate`,
//! `record_evidence`. There is no `workflow_template` or `workflow_run`
//! domain object behind it and no kernel runtime enforcing act order; the
//! demonstrated phase is choreographed by convention, and that tradeoff is
//! on record. The kernel never learns what an "act" is.
//!
//! The gate model already speaks the lifecycle's verdict language:
//! `pass` / `fail` / `partial` is both the contract's
//! [`GateVerdict`](gwk_domain::fsm::GateVerdict) set and exactly how a
//! VERIFY note is allowed to conclude — so VERIFY's verdict travels as a
//! gate decision rather than as a convention invented beside one.
//!
//! Two rows are conditional. EVAL exists only for a phase whose declared
//! tags carry `ai`, and SHIP's audit gates are computed from the tags at
//! resolution time — which is why callers go through [`acts_for`] rather
//! than reading [`ACTS`] raw: the template is the same data for every
//! phase, and the tags are what a particular phase declares against it.

const REVIEW_GATE: &str = "review";

/// One act of the phase lifecycle, as the convention spells it.
#[derive(Debug, PartialEq, Eq)]
pub struct Act {
    /// The act's name, in the order [`ACTS`] holds them.
    pub name: &'static str,
    /// `create_task` `kind` for the act's task.
    pub task_kind: &'static str,
    /// `record_evidence` `kind` for the act's artifact — a SPEC document, a
    /// diff, a VERIFY note.
    pub evidence_kind: &'static str,
    /// `open_gate` `kind` for each gate the act always opens, in order.
    /// SHIP's tag-fired audit gates are not here — they depend on the
    /// phase's tags, which is [`acts_for`]'s business.
    pub gates: &'static [&'static str],
    /// The tag that admits the act. `None` means the act runs in every
    /// phase.
    pub requires_tag: Option<&'static str>,
}

/// The seven acts, in lifecycle order.
pub const ACTS: [Act; 7] = [
    Act {
        name: "spec",
        task_kind: "act:spec",
        evidence_kind: "spec",
        gates: &[],
        requires_tag: None,
    },
    Act {
        name: "plan",
        task_kind: "act:plan",
        evidence_kind: "plan",
        // The autonomy line: the operator owns SPEC and PLAN, and PLAN
        // approval is where the cycle goes unattended.
        gates: &["plan_approval"],
        requires_tag: None,
    },
    Act {
        name: "execute",
        task_kind: "act:execute",
        evidence_kind: "diff",
        // No template gate. An engine session's relayed permission prompts
        // arrive as gates too, but they are the engine's questions at run
        // time, not rows a template can know in advance.
        gates: &[],
        requires_tag: None,
    },
    Act {
        name: "verify",
        task_kind: "act:verify",
        evidence_kind: "verify",
        gates: &["verify"],
        requires_tag: None,
    },
    Act {
        name: "sweep",
        task_kind: "act:sweep",
        evidence_kind: "sweep",
        gates: &[],
        requires_tag: None,
    },
    Act {
        name: "eval",
        task_kind: "act:eval",
        evidence_kind: "eval",
        gates: &["eval"],
        requires_tag: Some("ai"),
    },
    Act {
        name: "ship",
        task_kind: "act:ship",
        evidence_kind: "review",
        gates: &[REVIEW_GATE],
        requires_tag: None,
    },
];

/// `(tag, gate kind)` — the audit gates SHIP fires per declared tag.
///
/// Row order is the canonical audit order: two tags naming the same audit
/// fire it once, where the table puts it, regardless of how the phase
/// ordered its tag list.
pub const SHIP_AUDITS: [(&str, &str); 9] = [
    ("security", "audit:security"),
    ("auth", "audit:security"),
    ("secrets", "audit:security"),
    ("external-system", "audit:security"),
    ("billing", "audit:security"),
    ("ui", "audit:ui"),
    ("frontend", "audit:ui"),
    ("infra", "audit:infra"),
    ("data-migration", "audit:migration"),
];

/// Every tag a phase may declare. `observability` is classification only —
/// it admits no act and fires no audit — and `ai` admits EVAL rather than
/// firing a SHIP audit, which is why both are here and neither is in
/// [`SHIP_AUDITS`].
pub const TAGS: [&str; 11] = [
    "ai",
    "auth",
    "billing",
    "data-migration",
    "external-system",
    "frontend",
    "infra",
    "observability",
    "secrets",
    "security",
    "ui",
];

/// A tag outside [`TAGS`]. Refused rather than absorbed, because a
/// misspelled tag would otherwise resolve to a phase that silently skips
/// the audit the caller believed was declared.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("no tag {0:?}; one of: {known}", known = TAGS.join(", "))]
pub struct UnknownTag(pub String);

/// One act as a particular phase runs it: the template row plus the gates
/// it resolved to under that phase's tags.
#[derive(Debug, PartialEq, Eq)]
pub struct PhaseAct {
    pub act: &'static Act,
    /// The act's fixed gates, and for SHIP the tag-fired audits after
    /// [`REVIEW_GATE`], in [`SHIP_AUDITS`] order, deduplicated.
    pub gates: Vec<&'static str>,
}

/// The acts a phase with these tags runs, in order.
pub fn acts_for(tags: &[&str]) -> Result<Vec<PhaseAct>, UnknownTag> {
    if let Some(unknown) = tags.iter().find(|tag| !TAGS.contains(tag)) {
        return Err(UnknownTag((*unknown).to_owned()));
    }
    Ok(ACTS
        .iter()
        .filter(|act| act.requires_tag.is_none_or(|tag| tags.contains(&tag)))
        .map(|act| {
            let mut gates: Vec<&'static str> = act.gates.to_vec();
            if act.gates.contains(&REVIEW_GATE) {
                for (tag, audit) in SHIP_AUDITS {
                    if tags.contains(&tag) && !gates.contains(&audit) {
                        gates.push(audit);
                    }
                }
            }
            PhaseAct { act, gates }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(acts: &[PhaseAct]) -> Vec<&'static str> {
        acts.iter().map(|resolved| resolved.act.name).collect()
    }

    #[test]
    fn the_seven_acts_hold_lifecycle_order() {
        let names: Vec<&str> = ACTS.iter().map(|act| act.name).collect();
        assert_eq!(
            names,
            ["spec", "plan", "execute", "verify", "sweep", "eval", "ship"]
        );
        for act in &ACTS {
            // The task kind is derived spelling, not a second name that can
            // drift from the first.
            assert_eq!(act.task_kind, format!("act:{}", act.name));
        }
    }

    #[test]
    fn every_kind_the_template_names_is_a_modest_open_string() {
        // The contract's `kind` fields are open bounded strings. Nothing
        // here may lean on the bound: every string the template writes is
        // short, ASCII-graphic, and unmistakable in a rendered row.
        let mut kinds: Vec<&str> = Vec::new();
        for act in &ACTS {
            kinds.extend([act.name, act.task_kind, act.evidence_kind]);
            kinds.extend(act.gates);
        }
        kinds.extend(SHIP_AUDITS.iter().map(|(_, audit)| *audit));
        kinds.extend(TAGS);
        for kind in kinds {
            assert!(!kind.is_empty() && kind.len() <= 32, "{kind:?}");
            assert!(
                kind.chars().all(|c| c.is_ascii_graphic()),
                "{kind:?} would not survive a rendered row"
            );
        }
        // Every audit-firing tag is a declarable tag.
        for (tag, _) in SHIP_AUDITS {
            assert!(TAGS.contains(&tag), "{tag:?} fires an audit nobody can declare");
        }
    }

    #[test]
    fn eval_rides_only_the_ai_tag() {
        let untagged = acts_for(&[]).expect("no tags is a legal declaration");
        assert_eq!(
            names(&untagged),
            ["spec", "plan", "execute", "verify", "sweep", "ship"]
        );
        let tagged = acts_for(&["ai"]).expect("known tag");
        assert_eq!(
            names(&tagged),
            ["spec", "plan", "execute", "verify", "sweep", "eval", "ship"]
        );
        assert_eq!(tagged[5].gates, ["eval"]);
    }

    #[test]
    fn ship_fires_the_audits_the_tags_declare_in_table_order() {
        // The demonstration phase's own declaration: ui, infra, security —
        // and the audits land in SHIP_AUDITS order, not declaration order.
        let acts = acts_for(&["ui", "infra", "security"]).expect("known tags");
        let ship = acts.last().expect("ship is always last");
        assert_eq!(
            ship.gates,
            ["review", "audit:security", "audit:ui", "audit:infra"]
        );
        // An untagged phase still reviews; it just audits nothing.
        let untagged = acts_for(&[]).expect("no tags");
        assert_eq!(untagged.last().expect("ship").gates, ["review"]);
    }

    #[test]
    fn two_tags_naming_one_audit_fire_it_once() {
        let acts = acts_for(&["secrets", "auth"]).expect("known tags");
        assert_eq!(
            acts.last().expect("ship").gates,
            ["review", "audit:security"]
        );
        // Classification-only: observability declares fine and fires nothing.
        let classified = acts_for(&["observability"]).expect("known tag");
        assert_eq!(classified.last().expect("ship").gates, ["review"]);
    }

    #[test]
    fn an_unknown_tag_is_refused_rather_than_silently_unaudited() {
        let refusal = acts_for(&["ui", "securty"]).expect_err("misspelled");
        assert_eq!(refusal, UnknownTag("securty".to_owned()));
        let message = refusal.to_string();
        assert!(message.contains("securty"), "{message}");
        assert!(message.contains("security"), "{message}");
    }
}
