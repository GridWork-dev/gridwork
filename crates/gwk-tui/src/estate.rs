//! Live projection/event joining for the estate frame.
//!
//! Projection pages describe current entity state but carry only a page
//! watermark. Every per-entity [`Seq`] in the Hall therefore comes from the
//! matching event version. A missing match is a typed refusal, never a reason
//! to reuse the page watermark as counterfeit provenance.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use gwk_domain::entity::{Attempt, AttentionItem, Message, Task};
use gwk_domain::envelope::EventEnvelope;
use gwk_domain::fsm::{AttemptState, TaskState};
use gwk_domain::ids::{MessageId, Seq, Timestamp};
use sha2::{Digest as _, Sha256};

use crate::hall::{
    Agent, AgentId, AgentState, Attention, AttentionId, District, DistrictId, FrameInput, Station,
    StationId, ViewIdError,
};

/// One projection row and the page watermark observed beside that row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamped<T> {
    pub value: T,
    pub watermark: Seq,
}

impl<T> Stamped<T> {
    pub fn new(value: T, watermark: Seq) -> Self {
        Self { value, watermark }
    }
}

/// The four projection families the live Hall consumes.
#[derive(Debug, Clone, Default)]
pub struct ProjectionSnapshot {
    pub tasks: Vec<Stamped<Task>>,
    pub attempts: Vec<Stamped<Attempt>>,
    pub messages: Vec<Stamped<Message>>,
    pub attention: Vec<Stamped<AttentionItem>>,
    /// Every page watermark, including empty pages.
    pub watermarks: Vec<Option<Seq>>,
}

/// A message retained beside the spatial frame for live arrival cues.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveMessage {
    pub id: MessageId,
    pub message: Message,
    pub changed_seq: Seq,
}

/// One normalized live read: paintable frame plus event-provenanced messages.
#[derive(Debug, Clone, PartialEq)]
pub struct EstateSnapshot {
    pub frame: FrameInput,
    pub messages: Vec<LiveMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EventStamp {
    sequence: Seq,
    project: String,
    event_type: String,
}

/// The event-side provenance index, updated from initial and incremental pages.
#[derive(Debug, Clone, Default)]
pub struct EventIndex {
    by_version: BTreeMap<(String, String, u32), EventStamp>,
    watermark: Option<Seq>,
}

/// Why projection state could not be represented truthfully.
#[derive(Debug, thiserror::Error)]
pub enum EstateError {
    #[error(
        "{aggregate_type}/{aggregate_id} version {aggregate_version} has no matching event sequence provenance"
    )]
    MissingProvenance {
        aggregate_type: &'static str,
        aggregate_id: String,
        aggregate_version: u32,
    },
    #[error(
        "{aggregate_type}/{aggregate_id} version {aggregate_version} maps to both sequence {first} and {second}"
    )]
    ConflictingProvenance {
        aggregate_type: String,
        aggregate_id: String,
        aggregate_version: u32,
        first: Seq,
        second: Seq,
    },
    #[error(
        "projection pages straddled an append: expected watermark {expected:?}, observed {observed:?}"
    )]
    SnapshotDrift {
        expected: Option<Seq>,
        observed: Option<Seq>,
    },
    #[error(
        "{aggregate_type}/{aggregate_id} version {aggregate_version} changed at sequence {sequence}, past projection watermark {watermark}"
    )]
    ProjectionAhead {
        aggregate_type: &'static str,
        aggregate_id: String,
        aggregate_version: u32,
        sequence: Seq,
        watermark: Seq,
    },
    #[error(
        "attention_item/{aggregate_id} projection unresolved={projection_unresolved} disagrees with event {event_type} at sequence {sequence}"
    )]
    AttentionStateDrift {
        aggregate_id: String,
        projection_unresolved: bool,
        event_type: String,
        sequence: Seq,
    },
    #[error("attempt {attempt_id} names absent task {task_id}")]
    OrphanAttempt { attempt_id: String, task_id: String },
    #[error(transparent)]
    ViewId(#[from] ViewIdError),
}

impl EventIndex {
    /// Add an event page. At-least-once repeats are accepted; conflicting
    /// provenance for one aggregate version is not.
    pub fn ingest(
        &mut self,
        events: impl IntoIterator<Item = EventEnvelope>,
    ) -> Result<(), EstateError> {
        for event in events {
            let key = (
                event.aggregate_type.clone(),
                event.aggregate_id.as_str().to_owned(),
                event.aggregate_version,
            );
            let stamp = EventStamp {
                sequence: event.global_sequence,
                project: event.project_id.as_str().to_owned(),
                event_type: event.event_type.clone(),
            };
            if let Some(first) = self.by_version.get(&key) {
                if first != &stamp {
                    return Err(EstateError::ConflictingProvenance {
                        aggregate_type: key.0,
                        aggregate_id: key.1,
                        aggregate_version: key.2,
                        first: first.sequence,
                        second: stamp.sequence,
                    });
                }
            } else {
                self.by_version.insert(key, stamp);
            }
            self.watermark = Some(self.watermark.map_or(event.global_sequence, |current| {
                current.max(event.global_sequence)
            }));
        }
        Ok(())
    }

    pub fn watermark(&self) -> Option<Seq> {
        self.watermark
    }

    pub fn is_retryable(error: &EstateError) -> bool {
        matches!(
            error,
            EstateError::SnapshotDrift { .. }
                | EstateError::ProjectionAhead { .. }
                | EstateError::AttentionStateDrift { .. }
        )
    }

    /// Join current projection rows to exact event versions and normalize them.
    ///
    /// `aged_before` is the caller-resolved aging boundary (the join reads no
    /// clock): a terminal-state attempt last updated strictly before it is
    /// collapsed into its district's `aged_done` count instead of occupying an
    /// agent slot. Attention-targeted attempts never age out.
    pub fn build(
        &self,
        projections: &ProjectionSnapshot,
        aged_before: Option<&Timestamp>,
    ) -> Result<EstateSnapshot, EstateError> {
        let frame_watermark = coherent_watermark(&projections.watermarks)?;
        let mut tasks = BTreeMap::new();
        for row in &projections.tasks {
            let stamp = self.exact_at(
                "task",
                row.value.id.as_str(),
                row.value.version,
                row.watermark,
            )?;
            tasks
                .entry(row.value.id.as_str().to_owned())
                .or_insert_with(|| TaskFact {
                    task: row.value.clone(),
                    sequence: stamp.sequence,
                    project: stamp.project.clone(),
                });
        }

        let mut attempts_by_task: BTreeMap<String, Vec<AttemptFact>> = BTreeMap::new();
        for row in &projections.attempts {
            let stamp = self.exact_at(
                "attempt",
                row.value.id.as_str(),
                row.value.version,
                row.watermark,
            )?;
            if !tasks.contains_key(row.value.task_id.as_str()) {
                return Err(EstateError::OrphanAttempt {
                    attempt_id: row.value.id.as_str().to_owned(),
                    task_id: row.value.task_id.as_str().to_owned(),
                });
            }
            attempts_by_task
                .entry(row.value.task_id.as_str().to_owned())
                .or_default()
                .push(AttemptFact {
                    attempt: row.value.clone(),
                    sequence: stamp.sequence,
                });
        }

        let mut targeted_attempts = BTreeSet::new();
        let mut normalized_attention = Vec::new();
        let mut attention_by_project: BTreeMap<String, Seq> = BTreeMap::new();
        for row in &projections.attention {
            let stamp = self.attention_pin_at(
                row.value.id.as_str(),
                row.watermark,
                row.value.resolved_at.is_none(),
            )?;
            let project = attention_project(&row.value, &tasks, &attempts_by_task)
                .unwrap_or_else(|| stamp.project.clone());
            if row.value.resolved_at.is_none()
                && let Some(attempt) = subject(&row.value, "attempt")
            {
                targeted_attempts.insert(attempt.to_owned());
            }
            let district = district_id(&project)?;
            normalized_attention.push(Attention {
                id: attention_id(row.value.id.as_str())?,
                district,
                unresolved: row.value.resolved_at.is_none(),
                changed_seq: stamp.sequence,
            });
            attention_by_project
                .entry(project)
                .and_modify(|current| *current = (*current).max(stamp.sequence))
                .or_insert(stamp.sequence);
        }
        normalized_attention.sort_by(|left, right| left.id.cmp(&right.id));

        let mut districts: BTreeMap<String, DistrictBuild> = BTreeMap::new();
        for (task_id, fact) in &tasks {
            let district = districts
                .entry(fact.project.clone())
                .or_insert_with(|| DistrictBuild::new(&fact.project, fact.sequence));
            district.changed_seq = district.changed_seq.max(fact.sequence);

            let (station_key, station_label, ordinal) = station_identity(&fact.task);
            let station_id = station_id(&station_key)?;
            let station = district
                .stations
                .entry(station_id.as_str().to_owned())
                .or_insert_with(|| Station {
                    id: station_id,
                    label: station_label,
                    template_ordinal: ordinal,
                    agents: Vec::new(),
                    changed_seq: fact.sequence,
                });
            station.changed_seq = station.changed_seq.max(fact.sequence);

            if let Some(attempts) = attempts_by_task.get(task_id) {
                for attempt in attempts {
                    let needs_attention = targeted_attempts.contains(attempt.attempt.id.as_str())
                        || fact.task.state == TaskState::InputRequired;
                    let state = agent_state(attempt.attempt.state, needs_attention);
                    let aged = matches!(
                        state,
                        AgentState::Done | AgentState::Failed | AgentState::Canceled
                    ) && aged_before
                        .is_some_and(|cutoff| attempt.attempt.updated_at.0 < cutoff.0);
                    if aged {
                        district.aged_done += 1;
                    } else {
                        station.agents.push(Agent {
                            id: agent_id(attempt.attempt.id.as_str())?,
                            role: attempt.attempt.role.clone(),
                            state,
                            duration: (attempt.attempt.runtime_started_at.is_some()
                                && matches!(
                                    state,
                                    AgentState::Starting
                                        | AgentState::Running
                                        | AgentState::Canceling
                                        | AgentState::NeedsAttention
                                ))
                            .then(|| "live".to_owned()),
                            changed_seq: attempt.sequence,
                        });
                    }
                    station.changed_seq = station.changed_seq.max(attempt.sequence);
                    district.changed_seq = district.changed_seq.max(attempt.sequence);
                }
            }
        }

        for (project, sequence) in attention_by_project {
            districts
                .entry(project.clone())
                .and_modify(|district| district.changed_seq = district.changed_seq.max(sequence))
                .or_insert_with(|| DistrictBuild::new(&project, sequence));
        }

        let districts = districts
            .into_values()
            .map(DistrictBuild::finish)
            .collect::<Result<Vec<_>, _>>()?;

        let mut messages = Vec::new();
        for row in &projections.messages {
            let stamp = self.exact_at(
                "message",
                row.value.id.as_str(),
                row.value.version,
                row.watermark,
            )?;
            messages.push(LiveMessage {
                id: row.value.id.clone(),
                message: row.value.clone(),
                changed_seq: stamp.sequence,
            });
        }
        messages.sort_by(|left, right| left.id.cmp(&right.id));

        Ok(EstateSnapshot {
            frame: FrameInput {
                districts,
                focus: None,
                attention: normalized_attention,
                watermark: frame_watermark,
            },
            messages,
        })
    }

    fn exact(
        &self,
        aggregate_type: &'static str,
        aggregate_id: &str,
        aggregate_version: u32,
    ) -> Result<&EventStamp, EstateError> {
        self.by_version
            .get(&(
                aggregate_type.to_owned(),
                aggregate_id.to_owned(),
                aggregate_version,
            ))
            .ok_or_else(|| EstateError::MissingProvenance {
                aggregate_type,
                aggregate_id: aggregate_id.to_owned(),
                aggregate_version,
            })
    }

    fn exact_at(
        &self,
        aggregate_type: &'static str,
        aggregate_id: &str,
        aggregate_version: u32,
        through: Seq,
    ) -> Result<&EventStamp, EstateError> {
        let stamp = self.exact(aggregate_type, aggregate_id, aggregate_version)?;
        if stamp.sequence > through {
            return Err(EstateError::ProjectionAhead {
                aggregate_type,
                aggregate_id: aggregate_id.to_owned(),
                aggregate_version,
                sequence: stamp.sequence,
                watermark: through,
            });
        }
        Ok(stamp)
    }

    fn attention_pin_at(
        &self,
        aggregate_id: &str,
        through: Seq,
        projection_unresolved: bool,
    ) -> Result<&EventStamp, EstateError> {
        let stamp = self
            .by_version
            .iter()
            .filter(|((kind, id, _), stamp)| {
                kind == "attention_item"
                    && id == aggregate_id
                    && stamp.sequence <= through
                    && matches!(
                        stamp.event_type.as_str(),
                        "attention_raised" | "attention_resolved"
                    )
            })
            .map(|(_, stamp)| stamp)
            .max_by_key(|stamp| stamp.sequence)
            .ok_or_else(|| EstateError::MissingProvenance {
                aggregate_type: "attention_item",
                aggregate_id: aggregate_id.to_owned(),
                aggregate_version: 0,
            })?;
        let event_unresolved = stamp.event_type == "attention_raised";
        if event_unresolved != projection_unresolved {
            return Err(EstateError::AttentionStateDrift {
                aggregate_id: aggregate_id.to_owned(),
                projection_unresolved,
                event_type: stamp.event_type.clone(),
                sequence: stamp.sequence,
            });
        }
        Ok(stamp)
    }
}

fn coherent_watermark(watermarks: &[Option<Seq>]) -> Result<Option<Seq>, EstateError> {
    let Some(expected) = watermarks.first().copied() else {
        return Ok(None);
    };
    for observed in watermarks.iter().copied().skip(1) {
        if observed != expected {
            return Err(EstateError::SnapshotDrift { expected, observed });
        }
    }
    Ok(expected)
}

#[derive(Debug, Clone)]
struct TaskFact {
    task: Task,
    sequence: Seq,
    project: String,
}

#[derive(Debug, Clone)]
struct AttemptFact {
    attempt: Attempt,
    sequence: Seq,
}

#[derive(Debug)]
struct DistrictBuild {
    label: String,
    changed_seq: Seq,
    aged_done: usize,
    stations: BTreeMap<String, Station>,
}

impl DistrictBuild {
    fn new(project: &str, changed_seq: Seq) -> Self {
        Self {
            label: project.to_owned(),
            changed_seq,
            aged_done: 0,
            stations: BTreeMap::new(),
        }
    }

    fn finish(self) -> Result<District, ViewIdError> {
        Ok(District {
            id: district_id(&self.label)?,
            label: self.label,
            stations: self.stations.into_values().collect(),
            aged_done: self.aged_done,
            changed_seq: self.changed_seq,
        })
    }
}

fn district_id(project: &str) -> Result<DistrictId, ViewIdError> {
    DistrictId::new(view_id("district", project))
}

fn station_id(key: &str) -> Result<StationId, ViewIdError> {
    StationId::new(view_id("station", key))
}

fn agent_id(attempt: &str) -> Result<AgentId, ViewIdError> {
    AgentId::new(view_id("agent", attempt))
}

fn attention_id(item: &str) -> Result<AttentionId, ViewIdError> {
    AttentionId::new(view_id("attention", item))
}

fn view_id(namespace: &str, opaque: &str) -> String {
    // The discriminator keeps a direct opaque token from aliasing another
    // token's digest representation in the same view namespace.
    let direct = format!("{namespace}-r-{opaque}");
    if direct.len() <= 64 && direct.chars().all(|character| character.is_ascii_graphic()) {
        return direct;
    }

    let digest = Sha256::digest(opaque.as_bytes());
    let mut token = String::with_capacity(48);
    for byte in &digest[..24] {
        let _ = write!(token, "{byte:02x}");
    }
    format!("{namespace}-h-{token}")
}

fn station_identity(task: &Task) -> (String, String, u16) {
    if let Some(kind) = task.kind.as_deref()
        && let Some((ordinal, act)) = crate::seven_act::ACTS
            .iter()
            .enumerate()
            .find(|(_, act)| act.task_kind == kind)
    {
        return (
            kind.to_owned(),
            act.name.to_owned(),
            u16::try_from(ordinal).unwrap_or(u16::MAX),
        );
    }
    // The label must describe the station's KEY, not one member: every task
    // sharing a kind merges into one station, so a first-inserted task title
    // would name the whole station after an arbitrary member.
    let key = task.kind.as_deref().unwrap_or(task.id.as_str()).to_owned();
    let label = task
        .kind
        .as_deref()
        .or(task.title.as_deref())
        .unwrap_or(task.id.as_str())
        .to_owned();
    (key, label, u16::MAX)
}

fn subject<'a>(item: &'a AttentionItem, kind: &str) -> Option<&'a str> {
    let reference = item.subject_ref.as_deref()?;
    reference
        .strip_prefix(kind)
        .and_then(|rest| rest.strip_prefix('/'))
}

fn attention_project(
    item: &AttentionItem,
    tasks: &BTreeMap<String, TaskFact>,
    attempts_by_task: &BTreeMap<String, Vec<AttemptFact>>,
) -> Option<String> {
    if let Some(task_id) = subject(item, "task") {
        return tasks.get(task_id).map(|task| task.project.clone());
    }
    let attempt_id = subject(item, "attempt")?;
    for (task_id, attempts) in attempts_by_task {
        if attempts
            .iter()
            .any(|attempt| attempt.attempt.id.as_str() == attempt_id)
        {
            return tasks.get(task_id).map(|task| task.project.clone());
        }
    }
    None
}

fn agent_state(state: AttemptState, needs_attention: bool) -> AgentState {
    if needs_attention {
        return AgentState::NeedsAttention;
    }
    match state {
        AttemptState::Queued | AttemptState::Leased => AgentState::Queued,
        AttemptState::Starting => AgentState::Starting,
        AttemptState::Running => AgentState::Running,
        AttemptState::Blocked => AgentState::Blocked,
        AttemptState::Canceling => AgentState::Canceling,
        AttemptState::Canceled => AgentState::Canceled,
        AttemptState::Failed => AgentState::Failed,
        AttemptState::Unknown => AgentState::Unknown,
        AttemptState::Succeeded => AgentState::Done,
    }
}
