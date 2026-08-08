//! The demonstration's two enablement pieces, bound to each other.
//!
//! The seven-act choreography is client-side template data (`gwk-tui`), and
//! the walk it enables must run **without leaving `gw`** — which holds only
//! if everything the template names is expressible through this crate: the
//! generic kernel commands for tasks, gates, and evidence, and the `pr` verb
//! for SHIP's PR and merge. These cases pin that seam through the same normal
//! `gwk-tui` dependency the installed interactive binary now carries.

use gridwork::pr::{Body, Gh, Merge, Open, Strategy};
use gwk_domain::command::KernelCommand;
use gwk_domain::ids::{EvidenceId, GateId, TaskId};
use gwk_tui::seven_act::acts_for;

#[test]
fn seven_act_choreography_resolves_over_the_generic_vocabulary() {
    // The demonstration phase's own declaration: ui, infra, security — and
    // no `ai`, so EVAL is honestly absent rather than skipped.
    let acts = acts_for(&["ui", "infra", "security"]).expect("the declared tags are known");
    let names: Vec<&str> = acts.iter().map(|resolved| resolved.act.name).collect();
    assert_eq!(
        names,
        ["spec", "plan", "execute", "verify", "sweep", "ship"]
    );

    // Every kind the template names rides a command the kernel already has —
    // a generic task, a generic gate, a generic evidence record. No act ever
    // becomes a domain object of its own.
    for resolved in &acts {
        let task = KernelCommand::CreateTask {
            task_id: TaskId::new(format!("t-{}", resolved.act.name)),
            kind: Some(resolved.act.task_kind.to_owned()),
            title: None,
            spec_ref: None,
            project: None,
            priority: None,
            tracker_ref: None,
        };
        assert_eq!(task.command_type(), "create_task");
        serde_json::to_value(&task).expect("a template task serializes");

        for gate in &resolved.gates {
            let gate = KernelCommand::OpenGate {
                gate_id: GateId::new(format!("g-{gate}")),
                attempt_id: None,
                phase_ref: Some(resolved.act.name.to_owned()),
                kind: Some((*gate).to_owned()),
                question: None,
                options: None,
            };
            assert_eq!(gate.command_type(), "open_gate");
            serde_json::to_value(&gate).expect("a template gate serializes");
        }

        let evidence = KernelCommand::RecordEvidence {
            evidence_id: EvidenceId::new(format!("e-{}", resolved.act.name)),
            kind: resolved.act.evidence_kind.to_owned(),
            r#ref: format!("outputs/{}.md", resolved.act.name),
            digest: None,
            byte_size: None,
        };
        assert_eq!(evidence.command_type(), "record_evidence");
        serde_json::to_value(&evidence).expect("a template evidence record serializes");
    }

    // SHIP's gate row carries the review plus the audits the tags fired —
    // the `ui` audit the demonstration owes is here, not bolted on later.
    let ship = acts.last().expect("ship is always last");
    assert_eq!(
        ship.gates,
        ["review", "audit:security", "audit:ui", "audit:infra"]
    );
}

#[test]
fn seven_act_ship_merges_without_leaving_gw() {
    // The operative clause, verbatim: a `gw` verb shelling `gh` for PR/merge
    // SATISFIES "without leaving `gw`". The help advertises the verb, and
    // what it runs is an argument array a test can read.
    assert!(gridwork::args::HELP.contains("gw pr open"));
    assert!(gridwork::args::HELP.contains("gw pr merge"));

    let open = Gh::Open(Open {
        title: "feat(tui): the walk's own tail".to_owned(),
        body: Body::File("-".to_owned()),
        repo: Some("GridWork-dev/gridwork".to_owned()),
        base: None,
        head: Some("feature/walk".to_owned()),
    });
    let argv = open.argv();
    assert_eq!(argv[..2], ["pr", "create"]);
    // One argument, spaces and all: the difference between an argument array
    // and a shell line, pinned where the walk will lean on it.
    assert!(argv.contains(&"feat(tui): the walk's own tail".to_owned()));

    let merge = Gh::Merge(Merge {
        subject: "61".to_owned(),
        strategy: Strategy::Squash,
        repo: Some("GridWork-dev/gridwork".to_owned()),
    });
    assert_eq!(
        merge.argv(),
        [
            "pr",
            "merge",
            "61",
            "--squash",
            "--repo",
            "GridWork-dev/gridwork"
        ]
    );
}
