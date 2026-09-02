//! Context lifecycle facts: the append path and the truth projection.
//!
//! The Context plane records ten immutable lifecycle facts, and four of them
//! project into truth tables — `gwk.context_manifest`, `_release`,
//! `_observation`, `_finalization`. This module is both halves: the entry point
//! that appends one fact, and the applier that writes its row.
//!
//! Not to be confused with [`crate::blob::context`], which is the classified
//! CAS half of Context storage. That module holds bytes; this one holds the
//! typed record layer above them.
//!
//! # Callable, and structurally not reachable from the wire
//!
//! [`PgEventStore::record_context_fact`] takes typed Rust values rather than a
//! `CommandEnvelope`, and that is not a convenience. There is no
//! `KernelRequest` variant and no `KernelCommand` variant carrying a
//! [`ContextFact`]: the serving path's only submit call decodes its payload
//! into the closed `KernelCommand` union, where a Context payload fails as a
//! validation refusal. The handshake refuses protocol major 2 at three further
//! sites. So this path exists for an in-process caller, reaches no socket, and
//! changes none of those five files.
//!
//! # Why the caller supplies the attribution
//!
//! [`ContextEventPayload`] requires a [`ContextAttribution`], and the kernel
//! cannot mint one. `compiler` names a compiler build; `derived_from` names the
//! manifest the compiler re-read to derive the other two. The only legitimate
//! producer of that value is the compiler, in a crate this one cannot depend
//! on. A kernel-invented attribution would be a fabricated provenance record,
//! which is worse than no record — so it is a parameter.
//!
//! That does not reopen CTX-12. What CTX-12 states is that a CLIENT cannot
//! assert attribution, and that stays structural: [`RecordContextFact`] carries
//! exactly one field, the wire cannot reach this function, and the only caller
//! that can is the party the attribution is about.
//!
//! # Six of the ten facts write no row
//!
//! `CompilationRequested`, `ManifestVerificationRecorded`, `RunOpened`,
//! `RunClosed` and the two optimization facts append and project nothing. That
//! is written as six empty match arms rather than a wildcard, because an empty
//! arm says "considered, writes nothing" where a wildcard says "forgotten", and
//! because an eleventh fact must fail to compile here. Their only integrity
//! guard is this module — no DDL constraint stands behind any of them.
//!
//! # These four tables sit outside the checkpoint
//!
//! None of the four is in `checkpoint::PROJECTIONS`, so their rows are not in
//! the projection digest and `recover`'s divergence check cannot see them.
//! Declaring them needs a `ProjectionKind` and a `ProjectionRecord` variant,
//! which is a domain-contract shape change ruled out of this task. Two things
//! follow, and both are invisible rather than loud. `checkpoint`'s guard that
//! the excluded set is exactly `[attention_item, receipt]` stays green only
//! because nothing was added — it cannot see a table that was never declared.
//! The second is subtler and turns out to be closed. `recover` computes `cold`
//! from the declared projections alone, so on the face of it a database whose
//! ordinary projections were wiped while these four survived would count as
//! cold and replay into non-empty tables. It cannot happen, and three
//! mechanisms have to hold for that to be true, so they are named rather than
//! summarised: `gwk.context_manifest.attempt_id` references `gwk.attempt`,
//! `attempt` is the first entry in `PROJECTIONS`, and the reference is plain
//! `NO ACTION`. So a Context row existing implies an attempt row existing,
//! which implies the declared projections are not empty, which is exactly the
//! test `cold` runs. The append-only trigger closes the other direction: the
//! Context rows cannot be deleted to break the implication, by anyone.
//!
//! Every insert below still carries `ON CONFLICT (id) DO NOTHING`, targeted at
//! the PRIMARY KEY and at nothing else, and the target is the whole of it. A
//! replay re-inserts the same row, which collides on its own id and is
//! absorbed; a collision on any OTHER unique constraint means a DIFFERENT row
//! is claiming that key, and that must reach the database as the violation it
//! is. An untargeted `ON CONFLICT DO NOTHING` swallows both, and the difference
//! is not theoretical: with no target, deleting the one-per-manifest pre-check
//! stopped producing a refusal at all — the second release appended its event
//! and silently projected nothing, leaving a log entry with no row behind it.
//! The mutation check found that, and it is why this clause names a column.
//!
//! What it does NOT carry is the read-back comparison the classified-CAS
//! adapter pairs its `ON CONFLICT` with. That would be a guard nothing can make
//! fail, and this repository's standard is that a guard nobody has watched fail
//! is a guard nobody has tested.

use gwk_domain::envelope::{Actor, ENVELOPE_SCHEMA_VERSION, EventEnvelope, Origin};
use gwk_domain::ids::{
    AggregateId, EventId, FenceToken, IdempotencyKey, ProjectId, Seq, Timestamp,
};
use gwk_domain::protocol::KernelErrorCode;
use gwk_domain::{
    ContextAggregate, ContextAttribution, ContextEventName, ContextEventPayload, ContextFact,
    ContextRunId, Digest, EvidenceRefs, ManifestId, RecordContextFact, VerificationVerdict,
};
use sqlx::{PgConnection, Row};

use crate::epoch::{self, Epoch};
use crate::numeric::to_numeric_text;
use crate::project::{Refusal, apply_event};
use crate::store::{MAX_INFLIGHT_APPENDS, PgEventStore, current_aggregate_version, events_for_key};

/// The kernel is the appender. The compiler's identity travels in the payload's
/// attribution, where a reader can hold it to the manifest it names.
const CONTEXT_ACTOR_KIND: &str = "kernel";
const CONTEXT_ORIGIN_SYSTEM: &str = "gwk-context";

/// What one recorded fact produced.
#[derive(Debug)]
pub struct ContextAppended {
    pub events: Vec<EventEnvelope>,
    /// True when the idempotency key already named this exact request, in which
    /// case `events` are the originals and nothing was written.
    pub replayed: bool,
}

/// Where one fact's event belongs in the log.
struct Route {
    aggregate_type: &'static str,
    aggregate_id: String,
    event_type: &'static str,
}

fn db(context: &str, error: sqlx::Error) -> Refusal {
    Refusal::storage(format!("{context}: {error}"))
}

fn conflict(message: impl Into<String>) -> Refusal {
    Refusal::new(KernelErrorCode::IdempotencyConflict, message)
}

/// `RecordCount` and `ObservationIndex` are bounded at 65,535 on construction
/// and both columns are `integer`, so this conversion is total. The saturating
/// arm is unreachable rather than lenient.
fn count32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn evidence_text(ids: &EvidenceRefs) -> Vec<String> {
    ids.as_slice()
        .iter()
        .map(|id| id.as_str().to_owned())
        .collect()
}

/// Is this event a Context lifecycle event rather than a kernel command?
///
/// Read off `aggregate_type` rather than by attempting a command decode and
/// falling back on failure: a malformed kernel command and a Context event must
/// not become indistinguishable. The three Context aggregate names collide with
/// no existing family.
pub(crate) fn is_context_aggregate(aggregate_type: &str) -> bool {
    ContextAggregate::ALL
        .iter()
        .any(|candidate| candidate.as_str() == aggregate_type)
}

impl PgEventStore {
    /// Append one Context lifecycle fact and project it.
    ///
    /// Mirrors `submit::try_submit`'s ordering without its FSM and authority
    /// machinery: admit, take the writer lock, read the epoch, resolve the
    /// route, answer the idempotency key, pre-check every cardinality the DDL
    /// enforces, append, project through the shared applier, checkpoint, commit.
    /// The lock is held to commit, so every read below decides against the same
    /// instant the write lands in.
    pub async fn record_context_fact(
        &self,
        project_id: &ProjectId,
        idempotency_key: &IdempotencyKey,
        attribution: &ContextAttribution,
        command: &RecordContextFact,
    ) -> Result<ContextAppended, Refusal> {
        let _permit = self.admit().map_err(|_| {
            Refusal::new(
                KernelErrorCode::Overloaded,
                format!("append queue is full ({MAX_INFLIGHT_APPENDS} in flight)"),
            )
        })?;
        let fact = &command.fact;
        let name = fact.event_name();

        let mut tx = self.pool().begin().await.map_err(|e| db("begin", e))?;
        // FIRST, and before every read below.
        let writer = self.lock_writer(&mut tx).await?;
        // Under the lock and before the key check: a sealed kernel refuses
        // whether or not this key has been seen, and answering "replay" out of
        // a log the caller may not read yet would be a leak dressed as a
        // convenience.
        let epoch = epoch::epoch_of(&mut tx).await?;
        if !matches!(epoch, Epoch::Active) {
            return Err(epoch::sealed_refusal(epoch, name.as_str()));
        }

        // Resolving the route is also the "no such manifest" refusal for the
        // two manifest-scoped facts that carry no attempt id: one read, two
        // jobs.
        let route = route_of(&mut tx, fact, name).await?;

        let payload = serde_json::to_value(ContextEventPayload {
            name,
            attribution: attribution.clone(),
            fact: fact.clone(),
        })
        .map_err(|e| Refusal::storage(format!("serialize context event: {e}")))?;
        let actor = Actor {
            kind: CONTEXT_ACTOR_KIND.to_owned(),
            id: None,
        };

        let stored = events_for_key(
            &mut tx,
            project_id.as_str(),
            route.aggregate_type,
            &route.aggregate_id,
            idempotency_key.as_str(),
        )
        .await?;
        if let Some(first) = stored.first() {
            // A replay must be the SAME request, on the same aggregate, in the
            // same project, from the same actor, with the same body. The
            // aggregate namespace is global while idempotency is per-project,
            // so the project comparison is what stops one project being handed
            // another's events.
            let same_request = stored.len() == 1
                && &first.project_id == project_id
                && first.aggregate_type == route.aggregate_type
                && first.aggregate_id.as_str() == route.aggregate_id
                && first.actor == actor
                && first.payload == payload;
            if !same_request {
                return Err(conflict(format!(
                    "idempotency key {:?} already names a different request on {}/{}",
                    idempotency_key.as_str(),
                    first.aggregate_type,
                    first.aggregate_id
                )));
            }
            // Nothing to write; the commit only releases the lock. Re-applying
            // the projection would collide with the row the original append
            // already wrote.
            tx.commit().await.map_err(|e| db("commit", e))?;
            return Ok(ContextAppended {
                events: stored,
                replayed: true,
            });
        }

        let expected_version =
            current_aggregate_version(&mut tx, route.aggregate_type, &route.aggregate_id).await?;
        precheck(&mut tx, fact, expected_version).await?;

        let aggregate_version = expected_version.checked_add(1).ok_or_else(|| {
            Refusal::new(
                KernelErrorCode::StaleVersion,
                format!(
                    "{}/{} is at the version ceiling",
                    route.aggregate_type, route.aggregate_id
                ),
            )
        })?;
        // The fact's own timestamp rather than a clock read: an event whose id
        // or time columns depend on when the append ran would replay into
        // different bytes than the write it replays.
        let at = fact_timestamp(fact).clone();
        let event = EventEnvelope {
            // Derived, not minted, for the reason `submit::build_event` gives:
            // `(aggregate_type, aggregate_id, aggregate_version)` is already
            // UNIQUE in the log, and a derived id is stable under replay where
            // a random one is not.
            event_id: EventId::new(format!(
                "{}:{}:{aggregate_version}",
                route.aggregate_type, route.aggregate_id
            )),
            project_id: project_id.clone(),
            aggregate_type: route.aggregate_type.to_owned(),
            aggregate_id: AggregateId::new(route.aggregate_id.clone()),
            aggregate_version,
            event_type: route.event_type.to_owned(),
            schema_version: ENVELOPE_SCHEMA_VERSION,
            global_sequence: Seq::new(0),
            occurred_at: at.clone(),
            appended_at: at,
            actor,
            origin: Origin {
                system: CONTEXT_ORIGIN_SYSTEM.to_owned(),
                r#ref: None,
            },
            causation_id: None,
            correlation_id: None,
            idempotency_key: Some(idempotency_key.clone()),
            payload,
            payload_ref: None,
        };

        // The kernel's own append path is authorized by the writer lock plus
        // the durable epoch, both stronger than a fence token; presenting the
        // current one satisfies the port without pretending this arrived
        // fenced.
        let fence = writer.current_fence.map(FenceToken::new);
        let appended = self
            .append_locked(&mut tx, &writer, expected_version, fence, &[event])
            .await?;
        if !appended.replayed {
            for event in &appended.events {
                // Through the shared applier, never straight into the arm
                // below: one applier is what makes a replay agree with the
                // write it replays.
                apply_event(&mut tx, event).await?;
            }
        }
        // After the projections, never before: a checkpoint names the sequence
        // it describes.
        if let Some(last) = appended.events.last() {
            self.checkpoint_if_due(&mut tx, &writer, last.global_sequence, &last.appended_at)
                .await?;
        }
        tx.commit().await.map_err(|e| db("commit", e))?;
        Ok(ContextAppended {
            events: appended.events,
            replayed: appended.replayed,
        })
    }
}

/// The timestamp the fact itself states.
fn fact_timestamp(fact: &ContextFact) -> &Timestamp {
    match fact {
        ContextFact::CompilationRequested { requested_at, .. } => requested_at,
        ContextFact::ManifestResolved { resolved_at, .. } => resolved_at,
        ContextFact::ManifestVerificationRecorded { verified_at, .. } => verified_at,
        ContextFact::ReleaseRecorded { released_at, .. } => released_at,
        ContextFact::RunOpened { opened_at, .. } => opened_at,
        ContextFact::ObservationAppended { observed_at, .. } => observed_at,
        ContextFact::RunClosed { closed_at, .. } => closed_at,
        ContextFact::AssuranceCertified { certified_at, .. } => certified_at,
        ContextFact::OptimizationCandidateProposed { proposed_at, .. } => proposed_at,
        ContextFact::OptimizationCandidateDispositioned {
            dispositioned_at, ..
        } => dispositioned_at,
    }
}

/// Where this fact's event belongs.
///
/// The `context_manifest` family keys on `attempt_id` rather than on the
/// manifest id, because `CompilationRequested` precedes the manifest and has no
/// manifest id to key on. The two facts carrying only a manifest id resolve the
/// attempt through `gwk.context_manifest`, whose `UNIQUE (attempt_id)` makes
/// that mapping one-to-one — and the same read is the "no such manifest"
/// refusal, so a supplement recorded before its manifest is refused here rather
/// than discovered as a foreign-key violation.
async fn route_of(
    conn: &mut PgConnection,
    fact: &ContextFact,
    name: ContextEventName,
) -> Result<Route, Refusal> {
    let aggregate_id = match fact {
        ContextFact::CompilationRequested { attempt_id, .. }
        | ContextFact::ManifestResolved { attempt_id, .. } => attempt_id.as_str().to_owned(),
        ContextFact::ManifestVerificationRecorded { manifest_id, .. }
        | ContextFact::ReleaseRecorded { manifest_id, .. } => {
            attempt_of_manifest(conn, manifest_id).await?
        }
        ContextFact::RunOpened { run_id, .. }
        | ContextFact::ObservationAppended { run_id, .. }
        | ContextFact::RunClosed { run_id, .. }
        | ContextFact::AssuranceCertified { run_id, .. } => run_id.as_str().to_owned(),
        ContextFact::OptimizationCandidateProposed { candidate_id, .. }
        | ContextFact::OptimizationCandidateDispositioned { candidate_id, .. } => {
            candidate_id.as_str().to_owned()
        }
    };
    Ok(Route {
        aggregate_type: name.aggregate().as_str(),
        aggregate_id,
        event_type: name.as_str(),
    })
}

async fn attempt_of_manifest(
    conn: &mut PgConnection,
    manifest_id: &ManifestId,
) -> Result<String, Refusal> {
    sqlx::query_scalar::<_, String>("SELECT attempt_id FROM gwk.context_manifest WHERE id = $1")
        .bind(manifest_id.as_str())
        .fetch_optional(conn)
        .await
        .map_err(|e| db("read context manifest", e))?
        .ok_or_else(|| Refusal::not_found(format!("no context manifest {manifest_id}")))
}

/// Every cardinality the four tables enforce, refused before the database says
/// it — plus the three the schema cannot say at all.
///
/// The DDL's single-column constraints are deliberately NOT re-checked here.
/// The id charset, every `sha256:` digest, every count bound, the assurance
/// token, and both validator functions are already closed by the newtypes at
/// construction and on the way in, so a runtime branch for them would be dead
/// code no mutation could turn red. What is proved instead is one round-trip
/// per table against a real database, in the integration suite.
async fn precheck(
    conn: &mut PgConnection,
    fact: &ContextFact,
    aggregate_version: u32,
) -> Result<(), Refusal> {
    match fact {
        ContextFact::CompilationRequested { .. } => {}

        ContextFact::ManifestResolved {
            manifest_id,
            attempt_id,
            participations,
            ..
        } => {
            let row = sqlx::query(
                "SELECT EXISTS(SELECT 1 FROM gwk.attempt WHERE id = $1) AS attempt_exists, \
                        EXISTS(SELECT 1 FROM gwk.context_manifest WHERE id = $2) AS id_taken, \
                        EXISTS(SELECT 1 FROM gwk.context_manifest WHERE attempt_id = $1) \
                          AS attempt_taken",
            )
            .bind(attempt_id.as_str())
            .bind(manifest_id.as_str())
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| db("precheck context manifest", e))?;
            if !flag(&row, "attempt_exists")? {
                return Err(Refusal::not_found(format!("no attempt {attempt_id}")));
            }
            if flag(&row, "id_taken")? {
                return Err(conflict(format!("context manifest {manifest_id} exists")));
            }
            if flag(&row, "attempt_taken")? {
                return Err(conflict(format!(
                    "attempt {attempt_id} already resolved a context manifest"
                )));
            }

            // Where the CAS already holds a candidate's bytes, the class this
            // fact states about that candidate must be the class those bytes
            // were sealed under. A participation carries its own class so a
            // candidate that was never sealed still has one; the price of that
            // is two recordings of a single seam, and this is what stops them
            // diverging. Divergence is silent otherwise: both rows stay valid,
            // both pass every CHECK, and only a scoped query notices, by
            // answering from the wrong side.
            //
            // On the APPEND path and deliberately not in the applier. The
            // applier also runs on replay, so a guard there would read a
            // mutable side table and make a projection rebuild's outcome
            // depend on when it was run rather than on the event log. Here the
            // writer lock is already held, so this decides against the same
            // instant the append lands in.
            let stated: Vec<(&str, &'static str)> = participations
                .as_slice()
                .iter()
                .map(|record| (record.digest.as_str(), record.class.as_str()))
                .collect();
            let sealed = sqlx::query(
                "SELECT digest, content_class FROM gwk.context_blob WHERE digest = ANY($1)",
            )
            .bind(
                stated
                    .iter()
                    .map(|(d, _)| (*d).to_owned())
                    .collect::<Vec<_>>(),
            )
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| db("precheck participation classes", e))?;
            for row in &sealed {
                let digest: String = row.try_get("digest").map_err(|e| db("blob digest", e))?;
                let sealed_class: String = row
                    .try_get("content_class")
                    .map_err(|e| db("blob content class", e))?;
                // ponytail: linear scan over both sides. Participations are
                // capped at 4096 and a manifest carries a handful; a map would
                // buy nothing here and hide the duplicate-digest case, which
                // this catches because it compares every stated record rather
                // than the first one matching.
                for (candidate, class) in &stated {
                    if *candidate == digest && *class != sealed_class {
                        return Err(Refusal::validation(format!(
                            "participation {digest} is stated {class} but the CAS \
                             sealed those bytes {sealed_class}"
                        )));
                    }
                }
            }
        }

        // The manifest was proved to exist while the route was resolved.
        ContextFact::ManifestVerificationRecorded { .. } => {}

        ContextFact::ReleaseRecorded {
            manifest_id,
            release_id,
            ..
        } => {
            let row = sqlx::query(
                "SELECT EXISTS(SELECT 1 FROM gwk.context_release WHERE id = $1) AS id_taken, \
                        EXISTS(SELECT 1 FROM gwk.context_release WHERE manifest_id = $2) \
                          AS manifest_taken",
            )
            .bind(release_id.as_str())
            .bind(manifest_id.as_str())
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| db("precheck context release", e))?;
            if flag(&row, "id_taken")? {
                return Err(conflict(format!("context release {release_id} exists")));
            }
            if flag(&row, "manifest_taken")? {
                return Err(conflict(format!(
                    "context manifest {manifest_id} is already released"
                )));
            }
        }

        // A run's aggregate version is the whole guard for its lifecycle: an
        // open must be the first event on the run, and every later run fact
        // must find one. There is no `gwk.context_run` table, so nothing in the
        // schema says either and this is the only place both are true.
        ContextFact::RunOpened {
            run_id,
            manifest_id,
            ..
        } => {
            if aggregate_version != 0 {
                return Err(conflict(format!("context run {run_id} is already open")));
            }
            require_manifest(&mut *conn, manifest_id).await?;
        }

        ContextFact::ObservationAppended {
            run_id,
            manifest_id,
            observation_id,
            observation_index,
            ..
        } => {
            require_run(run_id, aggregate_version)?;
            let row = sqlx::query(
                "SELECT EXISTS(SELECT 1 FROM gwk.context_manifest WHERE id = $1) \
                          AS manifest_exists, \
                        EXISTS(SELECT 1 FROM gwk.context_observation WHERE id = $2) AS id_taken, \
                        coalesce(max(observation_index), 0) AS highest \
                 FROM gwk.context_observation WHERE manifest_id = $1",
            )
            .bind(manifest_id.as_str())
            .bind(observation_id.as_str())
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| db("precheck context observation", e))?;
            if !flag(&row, "manifest_exists")? {
                return Err(Refusal::not_found(format!(
                    "no context manifest {manifest_id}"
                )));
            }
            if flag(&row, "id_taken")? {
                return Err(conflict(format!(
                    "context observation {observation_id} exists"
                )));
            }
            let highest: i32 = row
                .try_get("highest")
                .map_err(|e| db("column highest", e))?;
            // The DDL bounds this column and pins its uniqueness; it says
            // nothing about order. Indices 1, 5, 3 all insert cleanly, and the
            // read port promises index order with no gap check behind it. This
            // is the only guard, and it holds under the writer lock so two
            // concurrent appends cannot both read the same maximum and both
            // compute the same next index.
            let expected = u32::try_from(highest)
                .map_err(|_| Refusal::storage("negative observation index in the table"))?
                + 1;
            if observation_index.value() != expected {
                return Err(Refusal::validation(format!(
                    "context manifest {manifest_id} expects observation index {expected}, not {}",
                    observation_index.value()
                ))
                .with_detail(serde_json::json!({
                    "manifest_id": manifest_id.as_str(),
                    "expected": expected,
                    "got": observation_index.value(),
                })));
            }
        }

        ContextFact::RunClosed { run_id, .. } => {
            require_run(run_id, aggregate_version)?;
        }

        ContextFact::AssuranceCertified {
            run_id,
            manifest_id,
            finalization_id,
            verification_digest,
            ..
        } => {
            require_run(run_id, aggregate_version)?;
            let row = sqlx::query(
                "SELECT EXISTS(SELECT 1 FROM gwk.context_manifest WHERE id = $1) \
                          AS manifest_exists, \
                        EXISTS(SELECT 1 FROM gwk.context_finalization WHERE id = $2) AS id_taken, \
                        EXISTS(SELECT 1 FROM gwk.context_finalization WHERE manifest_id = $1) \
                          AS manifest_taken",
            )
            .bind(manifest_id.as_str())
            .bind(finalization_id.as_str())
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| db("precheck context finalization", e))?;
            if !flag(&row, "manifest_exists")? {
                return Err(Refusal::not_found(format!(
                    "no context manifest {manifest_id}"
                )));
            }
            if flag(&row, "id_taken")? {
                return Err(conflict(format!(
                    "context finalization {finalization_id} exists"
                )));
            }
            if flag(&row, "manifest_taken")? {
                return Err(conflict(format!(
                    "context manifest {manifest_id} is already finalized"
                )));
            }
            require_verified(&mut *conn, manifest_id, verification_digest).await?;
        }

        ContextFact::OptimizationCandidateProposed { .. }
        | ContextFact::OptimizationCandidateDispositioned { .. } => {}
    }
    Ok(())
}

fn require_run(run_id: &ContextRunId, aggregate_version: u32) -> Result<(), Refusal> {
    if aggregate_version == 0 {
        return Err(Refusal::not_found(format!("no context run {run_id}")));
    }
    Ok(())
}

async fn require_manifest(
    conn: &mut PgConnection,
    manifest_id: &ManifestId,
) -> Result<(), Refusal> {
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM gwk.context_manifest WHERE id = $1)",
    )
    .bind(manifest_id.as_str())
    .fetch_one(conn)
    .await
    .map_err(|e| db("read context manifest", e))?;
    if !exists {
        return Err(Refusal::not_found(format!(
            "no context manifest {manifest_id}"
        )));
    }
    Ok(())
}

/// A finalization row records a `verification_digest` and has no column for the
/// verdict behind it, so a REJECTED verification would land in a truth record
/// that reads exactly like an accepted one. Refusing here is the only place the
/// distinction can be made: no CHECK can separate the two, both digests being
/// well-formed sha-256.
///
/// The same read holds the certifying fact's own `verification_digest` to the
/// verification the log actually recorded, which is what stops a caller naming
/// a digest no verifier ever answered for.
async fn require_verified(
    conn: &mut PgConnection,
    manifest_id: &ManifestId,
    claimed: &Digest,
) -> Result<(), Refusal> {
    // Newest first. Re-verification is ordinary — the fact carries a verdict
    // rather than there being two event names for pass and fail — so the
    // standing answer is the last one recorded before this certification.
    let payload: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT payload FROM gwk.event \
         WHERE aggregate_type = $1 AND event_type = $2 \
           AND payload -> 'fact' ->> 'manifest_id' = $3 \
         ORDER BY seq DESC LIMIT 1",
    )
    .bind(ContextAggregate::ContextManifest.as_str())
    .bind(ContextEventName::ManifestVerificationRecorded.as_str())
    .bind(manifest_id.as_str())
    .fetch_optional(conn)
    .await
    .map_err(|e| db("read context verification", e))?;
    let Some(payload) = payload else {
        return Err(Refusal::not_found(format!(
            "context manifest {manifest_id} has no recorded verification"
        )));
    };
    let recorded: ContextEventPayload = serde_json::from_value(payload)
        .map_err(|e| Refusal::storage(format!("context verification payload: {e}")))?;
    let ContextFact::ManifestVerificationRecorded {
        verdict,
        verification_digest,
        ..
    } = recorded.fact
    else {
        return Err(Refusal::storage(
            "a context verification event carries another fact",
        ));
    };
    if verdict != VerificationVerdict::Verified {
        return Err(Refusal::validation(format!(
            "context manifest {manifest_id} was rejected by its verifier and cannot be certified"
        )));
    }
    if &verification_digest != claimed {
        return Err(Refusal::validation(format!(
            "context manifest {manifest_id} was verified under {verification_digest}, not {claimed}"
        )));
    }
    Ok(())
}

fn flag(row: &sqlx::postgres::PgRow, name: &str) -> Result<bool, Refusal> {
    row.try_get(name).map_err(|e| db(name, e))
}

/// Project one Context lifecycle event.
///
/// Reached from `project::apply_event`, which is the single applier both the
/// live write and the replay run through.
///
/// Every insert carries `ON CONFLICT (id) DO NOTHING`, targeted at the primary
/// key so a replay is absorbed while a different row claiming another unique
/// key still raises. See the module docs for why even the id collision is
/// unreachable today, and why the clause is there anyway.
pub(crate) async fn apply_context_event(
    conn: &mut PgConnection,
    event: &EventEnvelope,
) -> Result<(), Refusal> {
    let payload: ContextEventPayload =
        serde_json::from_value(event.payload.clone()).map_err(|e| {
            Refusal::storage(format!(
                "event {} payload is not a context event body: {e}",
                event.event_id
            ))
        })?;
    // The payload's closed name against the envelope's open string. The
    // envelope's copy is a string any writer could have set, and a row filed
    // under a name that does not describe it is worse than a refusal.
    if payload.name.as_str() != event.event_type {
        return Err(Refusal::storage(format!(
            "event {} is typed {} but carries {}",
            event.event_id,
            event.event_type,
            payload.name.as_str()
        )));
    }

    match &payload.fact {
        ContextFact::ManifestResolved {
            manifest_id,
            attempt_id,
            manifest_digest,
            route_digest,
            authority_digest,
            source_count,
            source_bytes,
            participations,
            evidence_ids,
            resolved_at,
        } => {
            let participations = serde_json::to_value(participations)
                .map_err(|e| Refusal::storage(format!("serialize participations: {e}")))?;
            sqlx::query(
                "INSERT INTO gwk.context_manifest \
                 (id, attempt_id, manifest_digest, route_digest, authority_digest, \
                  source_count, source_bytes, participations, evidence_ids, resolved_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7::numeric, $8::jsonb, $9, $10::timestamptz) \
                 ON CONFLICT (id) DO NOTHING",
            )
            .bind(manifest_id.as_str())
            .bind(attempt_id.as_str())
            .bind(manifest_digest.as_str())
            .bind(route_digest.as_str())
            .bind(authority_digest.as_str())
            .bind(count32(source_count.value()))
            .bind(to_numeric_text(source_bytes.value()))
            .bind(participations)
            .bind(evidence_text(evidence_ids))
            .bind(resolved_at.as_str())
            .execute(conn)
            .await
            .map_err(|e| db("insert context manifest", e))?;
        }

        ContextFact::ReleaseRecorded {
            manifest_id,
            release_id,
            rendered_digest,
            tool_schema_digest,
            rendered_bytes,
            tool_schema_count,
            evidence_ids,
            released_at,
        } => {
            sqlx::query(
                "INSERT INTO gwk.context_release \
                 (id, manifest_id, rendered_digest, tool_schema_digest, rendered_bytes, \
                  tool_schema_count, evidence_ids, released_at) \
                 VALUES ($1, $2, $3, $4, $5::numeric, $6, $7, $8::timestamptz) \
                 ON CONFLICT (id) DO NOTHING",
            )
            .bind(release_id.as_str())
            .bind(manifest_id.as_str())
            .bind(rendered_digest.as_str())
            .bind(tool_schema_digest.as_str())
            .bind(to_numeric_text(rendered_bytes.value()))
            .bind(count32(tool_schema_count.value()))
            .bind(evidence_text(evidence_ids))
            .bind(released_at.as_str())
            .execute(conn)
            .await
            .map_err(|e| db("insert context release", e))?;
        }

        ContextFact::ObservationAppended {
            manifest_id,
            observation_id,
            observation_index,
            fact_digest,
            observed_bytes,
            visible_source_count,
            truncated,
            evidence_ids,
            observed_at,
            ..
        } => {
            sqlx::query(
                "INSERT INTO gwk.context_observation \
                 (id, manifest_id, observation_index, fact_digest, observed_bytes, \
                  visible_source_count, truncated, evidence_ids, observed_at) \
                 VALUES ($1, $2, $3, $4, $5::numeric, $6, $7, $8, $9::timestamptz) \
                 ON CONFLICT (id) DO NOTHING",
            )
            .bind(observation_id.as_str())
            .bind(manifest_id.as_str())
            .bind(count32(observation_index.value()))
            .bind(fact_digest.as_str())
            .bind(to_numeric_text(observed_bytes.value()))
            .bind(count32(visible_source_count.value()))
            .bind(*truncated)
            .bind(evidence_text(evidence_ids))
            .bind(observed_at.as_str())
            .execute(conn)
            .await
            .map_err(|e| db("insert context observation", e))?;
        }

        ContextFact::AssuranceCertified {
            manifest_id,
            finalization_id,
            output_digest,
            verification_digest,
            approval_count,
            observation_count,
            final_event_root,
            lifecycle_complete,
            assurance,
            evidence_ids,
            closed_at,
            ..
        } => {
            // `finalized_at` takes `closed_at`, not `certified_at`. The sibling
            // tables each take the writing fact's own timestamp, so this is
            // deliberately the exception: every other field of this row that
            // means a time-of-fact is a run-close field, and `certified_at`
            // dates a later statement about a settled run.
            sqlx::query(
                "INSERT INTO gwk.context_finalization \
                 (id, manifest_id, output_digest, verification_digest, approval_count, \
                  observation_count, final_event_root, lifecycle_complete, assurance, \
                  evidence_ids, finalized_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::timestamptz) \
                 ON CONFLICT (id) DO NOTHING",
            )
            .bind(finalization_id.as_str())
            .bind(manifest_id.as_str())
            .bind(output_digest.as_str())
            .bind(verification_digest.as_str())
            .bind(count32(approval_count.value()))
            .bind(count32(observation_count.value()))
            .bind(final_event_root.as_str())
            .bind(*lifecycle_complete)
            .bind(assurance.as_str())
            .bind(evidence_text(evidence_ids))
            .bind(closed_at.as_str())
            .execute(conn)
            .await
            .map_err(|e| db("insert context finalization", e))?;
        }

        // The six that write no row. Empty arms rather than a wildcard, so an
        // eleventh fact fails to compile here and "considered, writes nothing"
        // never reads the same as "forgotten".
        ContextFact::CompilationRequested { .. } => {}
        ContextFact::ManifestVerificationRecorded { .. } => {}
        ContextFact::RunOpened { .. } => {}
        ContextFact::RunClosed { .. } => {}
        ContextFact::OptimizationCandidateProposed { .. } => {}
        ContextFact::OptimizationCandidateDispositioned { .. } => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_context_aggregate_is_recognised_and_nothing_else_is() {
        // The dispatch in `project::apply_event` turns on this predicate, so a
        // new aggregate that it does not recognise would send a Context event
        // into the command decoder and break every replay path at once.
        assert_eq!(ContextAggregate::ALL.len(), 3);
        for aggregate in ContextAggregate::ALL {
            assert!(is_context_aggregate(aggregate.as_str()));
        }
        // The negative control, and the collision check: these are the kernel
        // families a Context aggregate must never be confused with.
        for other in ["task", "attempt", "kernel", "message", "command", "gate"] {
            assert!(!is_context_aggregate(other), "{other} read as Context");
        }
    }

    #[test]
    fn the_finalization_row_is_dated_by_the_close_not_the_certification() {
        // Ruled, and pinned here because the value is a one-word choice inside
        // a bind list where a later reader would have no way to tell it was
        // chosen at all. The three sibling tables take the writing fact's own
        // timestamp; this one deliberately does not.
        let source = include_str!("context.rs");
        let insert = source
            .split("INSERT INTO gwk.context_finalization")
            .nth(1)
            .expect("the finalization insert");
        let binds = insert.split("landed(").next().expect("its bind list");
        assert!(
            binds.contains(".bind(closed_at.as_str())"),
            "finalized_at must be bound from closed_at"
        );
        assert!(
            !binds.contains(".bind(certified_at.as_str())"),
            "certified_at must not reach the finalization row"
        );
    }
}
