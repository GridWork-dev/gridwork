//! Resolving the migration chain between the contract digest a database
//! records and the one this binary carries.
//!
//! `gwk_internal.schema_fingerprint` holds one digest,
//! [`CONTRACT_SQL_SHA256`](crate::CONTRACT_SQL_SHA256) is another, and when they
//! differ the question is whether this build knows a path from the first to the
//! second. That path is a LINE, not a graph: each
//! step names exactly the digest it may be applied to and exactly the digest it
//! produces, so at most one step can follow any other. Everything here exists
//! to keep it that way, because the alternative is a solver — and a solver is a
//! component whose bugs are silent, which is the last thing a schema migration
//! should be.
//!
//! Nothing in this module touches a database. It is handed a registry and two
//! digests and answers with a chain or a refusal; applying the chain is
//! somebody else's act, and that separation is what makes the decision table
//! testable without a server — the same split [`crate::admin::classify`] takes.
//!
//! The registry itself is generated: `schema/steps/*.sql` are the authored
//! files, `xtask/src/steps.rs` reads them, and it is that generator — not this
//! resolver — that holds a step's file name to the digests in its header.

use gwk_domain::contract_steps::Step;
use gwk_domain::is_sha256_hex;

/// Why a chain could not be produced.
///
/// Every variant carries its evidence as data rather than as a formatted
/// string. The rendering is one `Display` impl below, and a caller that wants
/// to count what a refusal found — how many bases a registry actually held, say
/// — can, instead of parsing the sentence back out of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainRefusal {
    /// The registry is empty. Distinct from [`ChainRefusal::NoChain`] on
    /// purpose: "no step bases on your digest" is what a populated registry
    /// says, and reporting it for a registry that was never populated is the
    /// wrong diagnosis of the right symptom.
    NoSteps {
        /// The digest the database records.
        from: String,
        /// The digest this binary carries.
        to: String,
    },

    /// A step carries something that is not a 64-character lowercase hex
    /// digest, so nothing downstream can compare it byte for byte.
    Malformed {
        /// The offending step's id.
        id: String,
        /// Which of the step's two digests it was.
        field: &'static str,
        /// What the field held.
        digest: String,
    },

    /// A step's base and result are the same digest, which is a one-element
    /// loop wearing a step's clothes.
    SelfStep {
        /// The offending step's id.
        id: String,
        /// The digest on both sides.
        digest: String,
    },

    /// Two steps base on one digest. Refused when the registry is read, not
    /// when a chain is walked: the chain being a line is a property of the
    /// registry, and a build that ships a fork should not depend on somebody
    /// asking for the route that crosses it.
    Branching {
        /// The digest both steps claim.
        base: String,
        /// The ids of the two steps that claim it, in registry order.
        ids: Vec<String>,
    },

    /// The walk arrived somewhere it had already been.
    Cycle {
        /// The digest reached for the second time.
        digest: String,
    },

    /// The walk ran out of steps before reaching the target.
    NoChain {
        /// The digest the database records.
        from: String,
        /// The digest this binary carries.
        to: String,
        /// Where the walk stopped. Equal to `from` when nothing bases on the
        /// recorded digest at all, and some later digest when a chain exists
        /// but arrives somewhere else.
        reached: String,
        /// Every base in the registry, in registry order. Carried rather than
        /// formatted so a caller can count them — a diagnostic that names one
        /// candidate is the diagnostic that sends a reader looking for the
        /// other twelve.
        bases: Vec<String>,
    },
}

impl std::fmt::Display for ChainRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSteps { from, to } => write!(
                f,
                "no steps are registered: this build carries an empty migration registry, so it \
                 knows of no way to carry a database from {from} to {to}"
            ),
            Self::Malformed { id, field, digest } => write!(
                f,
                "step {id}: {field} is {digest:?}, which is not a 64-character lowercase hex \
                 digest and cannot be compared against one"
            ),
            Self::SelfStep { id, digest } => write!(
                f,
                "step {id}: base and result are both {digest} — a step that arrives where it \
                 started can only be applied forever"
            ),
            Self::Branching { base, ids } => write!(
                f,
                "steps {} both base on {base}: the migration chain is a line, and two ways \
                 forward from one digest would make choosing between them a solver",
                ids.join(" and ")
            ),
            Self::Cycle { digest } => write!(
                f,
                "the chain revisits {digest}: migration steps form a line, not a loop"
            ),
            Self::NoChain {
                from,
                to,
                reached,
                bases,
            } if reached == from => write!(
                f,
                "no step bases on {from}, the contract digest this database records; this build \
                 carries {to}. Known bases: {}",
                bases.join(", ")
            ),
            Self::NoChain {
                from,
                to,
                reached,
                bases,
            } => write!(
                f,
                "the chain from {from} ends at {reached}, and this build carries {to} — the last \
                 step of the chain is not the one that arrives. Known bases: {}",
                bases.join(", ")
            ),
        }
    }
}

impl std::error::Error for ChainRefusal {}

/// The steps that carry a database from `from` to `to`, in application order.
///
/// `from` is the digest a database records and `to` the digest this binary
/// carries; the caller has already established that they differ (an equal pair
/// is [`crate::admin::InitOutcome::AlreadyInitialized`], answered before
/// anything reaches here).
///
/// The registry is validated as a whole before any of it is walked, so a
/// refusal is a statement about this build rather than about the route the
/// caller happened to ask for.
pub fn resolve<'a>(
    steps: &'a [Step],
    from: &str,
    to: &str,
) -> std::result::Result<Vec<&'a Step>, ChainRefusal> {
    // The count decides, before anything folds over the registry. A walk across
    // zero steps terminates immediately and reports that nothing bases on the
    // recorded digest, which is true and useless: the registry it inspected was
    // never populated. `all()` over an empty set is true and a search over an
    // empty set finds nothing, and neither observation distinguishes the two
    // states this has to tell apart.
    let registered = steps.len();
    if registered == 0 {
        return Err(ChainRefusal::NoSteps {
            from: from.to_owned(),
            to: to.to_owned(),
        });
    }

    validate(steps)?;
    walk(steps, from, to)
}

/// Check the registry as a whole, independent of any route through it.
///
/// Everything here is a property of what this build ships, so it is settled
/// once, up front — a fork or a malformed digest in a corner of the registry
/// nobody asked about is still a build that should not migrate anything.
fn validate(steps: &[Step]) -> std::result::Result<(), ChainRefusal> {
    // (base, id) for every step already inspected. A `Vec` scan rather than a
    // map: the registry is one file per schema change, so it is a handful of
    // entries, and the linear scan keeps the FIRST claimant of a base — which
    // is what the refusal names.
    let mut claimed: Vec<(&str, &str)> = Vec::with_capacity(steps.len());

    for step in steps {
        for (field, digest) in [("base", step.base), ("result", step.result)] {
            if !is_sha256_hex(digest) {
                return Err(ChainRefusal::Malformed {
                    id: step.id.to_owned(),
                    field,
                    digest: digest.to_owned(),
                });
            }
        }
        if step.base == step.result {
            return Err(ChainRefusal::SelfStep {
                id: step.id.to_owned(),
                digest: step.base.to_owned(),
            });
        }
        if let Some((_, first)) = claimed.iter().find(|(base, _)| *base == step.base) {
            return Err(ChainRefusal::Branching {
                base: step.base.to_owned(),
                ids: vec![(*first).to_owned(), step.id.to_owned()],
            });
        }
        claimed.push((step.base, step.id));
    }

    Ok(())
}

/// Follow the one step out of each digest until `to` is reached, or until it is
/// clear that it will not be.
///
/// [`validate`] has already established that no digest has two steps out of it,
/// so there is never a choice to make here — which is the whole reason this is a
/// walk and not a search.
fn walk<'a>(
    steps: &'a [Step],
    from: &str,
    to: &str,
) -> std::result::Result<Vec<&'a Step>, ChainRefusal> {
    let mut chain: Vec<&Step> = Vec::new();
    let mut visited: Vec<&str> = vec![from];
    let mut cursor = from;

    while cursor != to {
        let Some(step) = steps.iter().find(|step| step.base == cursor) else {
            return Err(ChainRefusal::NoChain {
                from: from.to_owned(),
                to: to.to_owned(),
                reached: cursor.to_owned(),
                bases: steps.iter().map(|step| step.base.to_owned()).collect(),
            });
        };
        // Unique bases make the graph a function, so a revisit is a loop and a
        // loop never ends. Bounding the walk by the step count would catch it
        // too, but only as "this took too long" — and the operator needs the
        // digest to know which pair of files to look at.
        if visited.contains(&step.result) {
            return Err(ChainRefusal::Cycle {
                digest: step.result.to_owned(),
            });
        }
        visited.push(step.result);
        chain.push(step);
        cursor = step.result;
    }

    Ok(chain)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Four digests that are distinguishable at a glance and still the exact
    // shape a real one has — the resolver rejects anything else, so a
    // convenient short stand-in would only test the rejection path.
    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const D: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    const Z: &str = "zzzzzzzz00000000000000000000000000000000000000000000000000000000";

    /// A step from `base` to `result`, named the way the generator names one.
    const fn step(id: &'static str, base: &'static str, result: &'static str) -> Step {
        Step {
            id,
            base,
            result,
            sql: "SELECT 1;\n",
        }
    }

    const A_TO_B: Step = step("aaaaaaaa-bbbbbbbb.sql", A, B);
    const B_TO_C: Step = step("bbbbbbbb-cccccccc.sql", B, C);
    const C_TO_D: Step = step("cccccccc-dddddddd.sql", C, D);

    #[test]
    fn a_digest_nothing_bases_on_names_every_known_base() {
        let steps = [A_TO_B, B_TO_C, C_TO_D];
        let refusal = resolve(&steps, Z, D).expect_err("Z is not in the chain");

        let ChainRefusal::NoChain {
            from,
            to,
            reached,
            bases,
        } = &refusal
        else {
            panic!("expected NoChain, got {refusal:?}");
        };
        assert_eq!(
            from, Z,
            "the refusal carries the digest the database records"
        );
        assert_eq!(to, D, "and the one this binary carries");
        assert_eq!(reached, Z, "the walk never left the starting digest");

        // COUNT, not presence. A diagnostic that offers one candidate reads
        // exactly like a diagnostic that found one candidate, and the reader
        // who trusts it goes looking for a step that does not exist.
        assert_eq!(bases.len(), 3, "every base in the registry: {bases:?}");
        let rendered = refusal.to_string();
        assert_eq!(
            [A, B, C].iter().filter(|b| rendered.contains(*b)).count(),
            3,
            "all three bases reach the rendered message: {rendered}"
        );
    }

    #[test]
    fn two_steps_on_one_base_are_refused_before_any_walk() {
        // The requested route — C to D — never touches the fork at A. A
        // registry that branches is refused anyway, because the refusal is
        // about what this build ships, not about where this caller was going.
        let steps = [A_TO_B, step("aaaaaaaa-cccccccc.sql", A, C), C_TO_D];
        let refusal = resolve(&steps, C, D).expect_err("the registry forks at A");

        let ChainRefusal::Branching { base, ids } = &refusal else {
            panic!("expected Branching, got {refusal:?}");
        };
        assert_eq!(base, A);
        assert_eq!(ids.len(), 2, "both claimants are named: {ids:?}");
        assert_eq!(ids[0], A_TO_B.id);
        assert_eq!(ids[1], "aaaaaaaa-cccccccc.sql");
    }

    #[test]
    fn a_cycle_names_the_repeated_digest() {
        let steps = [A_TO_B, step("bbbbbbbb-aaaaaaaa.sql", B, A)];
        let refusal = resolve(&steps, A, D).expect_err("A and B loop");
        assert_eq!(
            refusal,
            ChainRefusal::Cycle {
                digest: A.to_owned()
            },
            "the refusal names the digest reached twice"
        );
    }

    #[test]
    fn a_chain_that_stops_short_of_the_target_is_refused() {
        // The registry is a perfectly good chain — it just does not end where
        // this binary is. That is the case where a resolver is most tempted to
        // apply what it has and hope.
        let steps = [A_TO_B, B_TO_C];
        let refusal = resolve(&steps, A, D).expect_err("the chain ends at C, not D");

        let ChainRefusal::NoChain { reached, from, .. } = &refusal else {
            panic!("expected NoChain, got {refusal:?}");
        };
        assert_eq!(reached, C, "the refusal points at where the chain ended");
        assert_ne!(reached, from, "not at where it began");
    }

    #[test]
    fn an_empty_registry_is_its_own_refusal() {
        let refusal = resolve(&[], A, B).expect_err("nothing is registered");
        assert_eq!(
            refusal,
            ChainRefusal::NoSteps {
                from: A.to_owned(),
                to: B.to_owned()
            }
        );

        let rendered = refusal.to_string();
        assert!(rendered.contains("no steps are registered"), "{rendered}");
        // Zero steps walked and zero steps found are the same observation, and
        // only one of them is the truth here.
        assert!(!rendered.contains("no step bases on"), "{rendered}");
    }

    #[test]
    fn the_happy_path_walks_each_step_once_in_order() {
        let steps = [A_TO_B, B_TO_C, C_TO_D];
        let chain = resolve(&steps, A, D).expect("A to D is a chain");

        assert_eq!(chain.len(), 3, "one step per hop, no more");
        let ids: Vec<&str> = chain.iter().map(|step| step.id).collect();
        assert_eq!(ids, [A_TO_B.id, B_TO_C.id, C_TO_D.id]);
    }

    #[test]
    fn the_chain_comes_out_of_the_digests_not_out_of_the_slice_order() {
        // Same three steps, shuffled. The generator emits file-name order,
        // which is not application order and is not meant to be.
        let steps = [C_TO_D, A_TO_B, B_TO_C];
        let chain = resolve(&steps, A, D).expect("A to D is still a chain");

        assert_eq!(chain.len(), 3);
        let ids: Vec<&str> = chain.iter().map(|step| step.id).collect();
        assert_eq!(ids, [A_TO_B.id, B_TO_C.id, C_TO_D.id]);
    }

    #[test]
    fn a_step_that_arrives_where_it_started_is_refused() {
        let steps = [A_TO_B, step("bbbbbbbb-bbbbbbbb.sql", B, B)];
        let refusal = resolve(&steps, A, D).expect_err("B to B is not a step");
        assert_eq!(
            refusal,
            ChainRefusal::SelfStep {
                id: "bbbbbbbb-bbbbbbbb.sql".to_owned(),
                digest: B.to_owned()
            }
        );
    }

    #[test]
    fn a_digest_that_is_not_lowercase_hex_is_refused() {
        let steps = [step("zzzzzzzz-bbbbbbbb.sql", Z, B)];
        let refusal = resolve(&steps, Z, B).expect_err("Z is not hex");
        assert_eq!(
            refusal,
            ChainRefusal::Malformed {
                id: "zzzzzzzz-bbbbbbbb.sql".to_owned(),
                field: "base",
                digest: Z.to_owned()
            }
        );
    }
}
