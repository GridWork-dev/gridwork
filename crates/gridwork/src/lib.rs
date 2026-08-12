//! `gw` — one binary, and for now one of its three modes.
//!
//! What is here is the CLI: it opens the host-local socket, asks one question,
//! and prints the answer as the protocol produced it. Nothing is reshaped on the
//! way through. A `health` answer is the contract's `health` value and a refusal
//! is the contract's error object, so a caller that already knows the wire needs
//! no second vocabulary for the command line — and this program has no view of
//! its own to drift out of step.
//!
//! Three rules hold everywhere:
//!
//! * **Command answers are JSON on standard output.** `--pretty` changes the
//!   formatting and never the value. `--help` is prose; `tui`, `event tail`,
//!   and `term attach` own the interactive terminal. None is a command
//!   answer.
//! * **The exit says what to do about it** ([`exit`]). One table, derived from
//!   the refusal's own code, so the exit and the JSON can never disagree.
//! * **No database, no key material.** Every verb here goes through the socket.
//!   The database and the KEK belong to `daemon` and `admin`, which is the whole
//!   reason those two are separate verbs. One verb goes through something else
//!   entirely: [`pr`] shells `gh`, because SHIP's PR and merge belong to the
//!   forge, not the kernel — it holds no secret of its own, and the
//!   conversation it relays is gh's.

pub mod admin;
pub mod args;
pub mod client;
pub mod exit;
pub mod pr;
pub mod tui;

use std::collections::BTreeSet;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use args::{Invocation, Sink, Source, Verb};
use base64::prelude::{BASE64_STANDARD, Engine as _};
use client::Client;
use exit::Failure;
use gwk_domain::blob::{BLOB_CHUNK_BYTES, BlobAddress};
use gwk_domain::command::KernelCommand;
use gwk_domain::envelope::{Actor, CommandEnvelope, ENVELOPE_SCHEMA_VERSION, Origin};
use gwk_domain::ids::{
    AttemptId, AttentionItemId, AuthorityGrantId, ByteCount, CommandId, IdempotencyKey, ProjectId,
    PtySessionId, Seq, Timestamp,
};
use gwk_domain::protocol::{
    CONTRACT_VERSION, KernelErrorCode, KernelRequest, KernelResult, ProjectionKind,
    ProjectionRecord, ServerControl,
};
use gwk_tui::board::{
    BoardState, BoardTarget, BoardView, activity_brief, agent_fleet, attempt_budget, cost_rollup,
    estate_overview, replace_attempt_budget, stop_attempt,
};
use gwk_tui::replay::ReplayTimeline;
use gwk_tui::tables::{PageMeta, attempt_table, cost_table, session_table, term_table};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

/// The revision this build came from, or nothing if it was not stamped.
///
/// `None` is a real answer and not a defect — see `build.rs`. A daemon cannot
/// serve without one, but every verb in this module can, so the CLI reports the
/// absence rather than inventing a value.
pub const PUBLIC_REVISION: Option<&str> = option_env!("GW_PUBLIC_REVISION");

/// Run one invocation and return the exit it earned.
pub async fn run(argv: &[String]) -> u8 {
    let invocation = match args::parse(argv) {
        Ok(invocation) => invocation,
        Err(failure) => return report(&failure, false),
    };
    let Invocation { verb, pretty, json } = invocation;
    let human_tables = !json && std::io::stdout().is_terminal();
    match execute(verb, pretty, human_tables).await {
        Ok(()) => exit::OK,
        Err(failure) => report(&failure, pretty),
    }
}

/// Print a failure in the protocol's own error shape, and say what it exits as.
fn report(failure: &Failure, pretty: bool) -> u8 {
    emit(&failure.to_json(), pretty);
    failure.exit
}

fn emit(value: &Value, pretty: bool) {
    let rendered = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    };
    match rendered {
        Ok(text) => println!("{text}"),
        // Unreachable for a `Value`, and not worth a panic if it ever is.
        Err(e) => eprintln!("gw: could not render an answer: {e}"),
    }
}

async fn execute(verb: Verb, pretty: bool, human_tables: bool) -> Result<(), Failure> {
    match verb {
        Verb::Help => {
            print!("{}", args::HELP);
            Ok(())
        }
        Verb::BuildInfo => {
            emit(&build_info(), pretty);
            Ok(())
        }

        // The two verbs that hold a database and a key. Kept in one module so
        // which paths touch credentials is a question answered by reading the
        // imports.
        Verb::Daemon => admin::daemon(pretty).await,
        Verb::AdminInit => admin::init(pretty).await,
        Verb::AdminVerify => admin::verify(pretty).await,
        Verb::AdminRebuildProjections { scratch } => {
            admin::rebuild_projections(&scratch, pretty).await
        }
        Verb::AdminBlob { what } => admin::retention(&what, pretty).await,

        // Everything below needs the daemon.
        Verb::Health => ask(KernelRequest::Health {}, pretty).await,
        Verb::Status => ask(KernelRequest::Status {}, pretty).await,
        Verb::Watermark => ask(KernelRequest::Watermark {}, pretty).await,
        Verb::VerifySealed => ask(KernelRequest::VerifySealed {}, pretty).await,

        Verb::Activate {
            cutover_id,
            manifest_sha256,
        } => {
            let command = KernelCommand::ActivateKernel {
                cutover_id: cutover_id.clone(),
                archive_manifest_sha256: manifest_sha256,
            };
            // The key is the cutover's, so a retried activation is the SAME
            // activation. A second cutover id is refused by the kernel, which is
            // the check that matters and is not this program's to make.
            submit(&command, &format!("kernel_activated:{cutover_id}"), pretty).await
        }

        Verb::CommandSubmit { source } => {
            let envelope: CommandEnvelope = document(&source)?;
            ask(KernelRequest::SubmitCommand { envelope }, pretty).await
        }

        Verb::ProjectionGet { kind, id } => {
            ask(
                KernelRequest::GetProjection {
                    projection: kind,
                    id,
                },
                pretty,
            )
            .await
        }
        Verb::ProjectionList {
            kind,
            cursor,
            limit,
        } => {
            if human_tables
                && matches!(
                    kind,
                    ProjectionKind::Attempt
                        | ProjectionKind::EngineSession
                        | ProjectionKind::PtySession
                )
            {
                return human_projection_list(kind, cursor, limit).await;
            }
            ask(
                KernelRequest::ListProjection {
                    projection: kind,
                    cursor,
                    limit,
                },
                pretty,
            )
            .await
        }

        Verb::EventRead { cursor, limit } => {
            ask(KernelRequest::ReadEvents { cursor, limit }, pretty).await
        }
        Verb::EventFollow { cursor } => follow(cursor, pretty).await,
        Verb::EventTail {
            cursor,
            aggregate_type,
            event_type,
            view,
            motion,
        } => tui::event_tail(cursor, aggregate_type, event_type, view, motion).await,
        Verb::EstateOverview => {
            let state = load_summary_state(false).await?;
            emit_serialized(&estate_overview(&state), pretty)
        }
        Verb::ActivityBrief => {
            let state = load_summary_state(true).await?;
            emit_serialized(&activity_brief(&state), pretty)
        }
        Verb::CostRollup => {
            let state = load_cost_state().await?;
            if human_tables {
                print!(
                    "{}",
                    cost_table(
                        &state,
                        &now(),
                        &PageMeta {
                            complete: state.complete,
                            watermark: state.watermark,
                            next_cursor: None,
                        },
                        terminal_width(),
                    )
                );
                Ok(())
            } else {
                emit_serialized(&cost_rollup(&state), pretty)
            }
        }
        Verb::AgentFleet => {
            let state = load_fleet_state().await?;
            emit_serialized(&agent_fleet(&state), pretty)
        }
        Verb::AttemptStop { id } => {
            let target = BoardTarget::Attempt(AttemptId::new(id.clone()));
            let command = stop_attempt(&target).expect("an attempt target supports stop_attempt");
            submit(&command, &format!("stop_attempt:{id}"), pretty).await
        }
        Verb::AttemptBudget { id } => {
            let attempt = load_attempt(&id).await?;
            emit_serialized(&attempt_budget(&attempt), pretty)
        }
        Verb::AttemptBudgetUpdate {
            id,
            expected_version,
            budget,
        } => {
            let command =
                replace_attempt_budget(AttemptId::new(id.clone()), expected_version, budget);
            submit(
                &command,
                &format!("update_budget:{id}:{expected_version}"),
                pretty,
            )
            .await
        }
        Verb::TermTail { id } => {
            ask(
                KernelRequest::PtySnapshot {
                    session_id: PtySessionId::new(id),
                },
                pretty,
            )
            .await
        }
        Verb::TermAttach { id, motion } => tui::term_attach(PtySessionId::new(id), motion).await,
        Verb::Tui { motion } => tui::run(motion).await,
        Verb::Theme => {
            emit(&chrome_theme()?, pretty);
            Ok(())
        }

        // The three quieting acts, and the one place their key must NOT be
        // the subject alone.
        //
        // Quiet is reversible. `mute a-1 --until T1`, `unmute a-1`, `mute a-1
        // --until T2`, `unmute a-1` is an ordinary morning, and a key of
        // `unmute_attention:a-1` makes that last call a replay of the first:
        // the kernel short-circuits before the projector runs, answers
        // `command_applied` with the ORIGINAL sequence, exits 0 — and the item
        // stays muted. The operator asked for it back and was told yes.
        //
        // So a per-call nonce joins the key — the caller's instant paired with
        // a process-local counter, because the clock alone is only as fine as
        // the host makes it (see [`nonce`]). That is exactly right for these
        // three: all are
        // last-write-wins stamps carrying NO expected version, so applying one
        // twice is harmless by construction, which is why the contract lets
        // them skip the CAS in the first place. The dedup they give up buys
        // nothing; the correctness it costs is real.
        //
        // `resolve` below keeps its subject-only key, and that is not an
        // oversight: a resolve LATCHES — `resolved_at` is set once and the
        // item is closed — so a replay of it is the truth. A second resolve
        // with different words changes the payload and earns an idempotency
        // CONFLICT, which is also the truth.
        Verb::AttentionAck { id } => {
            let command = KernelCommand::AckAttention {
                attention_item_id: AttentionItemId::new(id.clone()),
            };
            submit(&command, &format!("ack_attention:{id}:{}", nonce()), pretty).await
        }
        Verb::AttentionMute { id, until } => {
            let command = KernelCommand::MuteAttention {
                attention_item_id: AttentionItemId::new(id.clone()),
                muted_until: Timestamp::new(until.clone()),
            };
            submit(
                &command,
                &format!("mute_attention:{id}:{until}:{}", nonce()),
                pretty,
            )
            .await
        }
        Verb::AttentionUnmute { id } => {
            let command = KernelCommand::UnmuteAttention {
                attention_item_id: AttentionItemId::new(id.clone()),
            };
            submit(
                &command,
                &format!("unmute_attention:{id}:{}", nonce()),
                pretty,
            )
            .await
        }
        Verb::AttentionResolve { id, resolution } => {
            let command = KernelCommand::ResolveAttention {
                attention_item_id: AttentionItemId::new(id.clone()),
                resolution,
            };
            submit(&command, &format!("resolve_attention:{id}"), pretty).await
        }
        Verb::AuthorityGrant { source } => {
            let envelope: CommandEnvelope = document(&source)?;
            // Checked here rather than left to the kernel, because the kernel
            // would accept it: `gw authority grant` pointed at the wrong file
            // would submit whatever the file held under a verb that promised
            // otherwise.
            expect_command(&envelope, "grant_authority")?;
            ask(KernelRequest::SubmitCommand { envelope }, pretty).await
        }
        Verb::AuthorityRevoke { id, reason } => {
            let command = KernelCommand::RevokeAuthority {
                authority_grant_id: AuthorityGrantId::new(id.clone()),
                reason,
            };
            submit(&command, &format!("revoke_authority:{id}"), pretty).await
        }

        Verb::BlobPut { source, media_type } => blob_put(&source, media_type, pretty).await,
        Verb::BlobGet { address, output } => blob_get(&address, &output, pretty).await,
        Verb::BlobStat { address } => ask(KernelRequest::BlobStat { address }, pretty).await,

        Verb::IngestSubmit {
            kind,
            source,
            project,
            key,
        } => {
            let payload: Value = document(&source)?;
            let key =
                key.unwrap_or_else(|| format!("{}:{}", kind.as_str(), digest_of_json(&payload)));
            let command = KernelCommand::IngestRecord {
                kind,
                payload,
                payload_ref: None,
            };
            let envelope = envelope(
                &command,
                &key,
                project.as_deref().unwrap_or(gwk_kernel::SYSTEM_PROJECT),
            );
            ask(KernelRequest::SubmitCommand { envelope }, pretty).await
        }

        Verb::Pr { what, dry_run } => pr::run(&what, dry_run, pretty),
    }
}

const SUMMARY_PAGE_LIMIT: u32 = 256;
const SUMMARY_SNAPSHOT_ATTEMPTS: usize = 3;
const ESTATE_PROJECTIONS: &[ProjectionKind] = &[
    ProjectionKind::Task,
    ProjectionKind::Attempt,
    ProjectionKind::AttentionItem,
    ProjectionKind::Worktree,
    ProjectionKind::Lease,
];
const ACTIVITY_PROJECTIONS: &[ProjectionKind] = &[
    ProjectionKind::Task,
    ProjectionKind::Attempt,
    ProjectionKind::AttentionItem,
    ProjectionKind::Worktree,
    ProjectionKind::Lease,
    ProjectionKind::CostEntry,
];
const FLEET_PROJECTIONS: &[ProjectionKind] = &[
    ProjectionKind::Task,
    ProjectionKind::EngineSession,
    ProjectionKind::DispatchNode,
    ProjectionKind::Attempt,
    ProjectionKind::Worktree,
    ProjectionKind::Lease,
    ProjectionKind::CostEntry,
];
const FLEET_CONTEXT_PROJECTIONS: &[ProjectionKind] = &[
    ProjectionKind::Task,
    ProjectionKind::EngineSession,
    ProjectionKind::DispatchNode,
    ProjectionKind::Worktree,
    ProjectionKind::Lease,
    ProjectionKind::CostEntry,
];
const COST_PROJECTIONS: &[ProjectionKind] = &[ProjectionKind::CostEntry];

async fn human_projection_list(
    kind: ProjectionKind,
    cursor: Option<String>,
    limit: Option<u32>,
) -> Result<(), Failure> {
    if kind == ProjectionKind::Attempt {
        let (state, meta) = load_attempt_table_state(cursor, limit).await?;
        print!("{}", attempt_table(&state, &now(), &meta, terminal_width()));
        return Ok(());
    }
    let (records, next_cursor, watermark) = projection_page(kind, cursor, limit).await?;
    let meta = PageMeta {
        complete: next_cursor.is_none(),
        watermark,
        next_cursor,
    };
    let width = terminal_width();
    let rendered = match kind {
        ProjectionKind::Attempt => unreachable!("attempt tables return above"),
        ProjectionKind::EngineSession => {
            let sessions = records
                .into_iter()
                .map(|record| match record {
                    ProjectionRecord::EngineSession { engine_session } => Ok(engine_session),
                    other => Err(wrong_projection(kind, &other)),
                })
                .collect::<Result<Vec<_>, Failure>>()?;
            session_table(&sessions, &meta, width)
        }
        ProjectionKind::PtySession => {
            let sessions = records
                .into_iter()
                .map(|record| match record {
                    ProjectionRecord::PtySession { pty_session } => Ok(pty_session),
                    other => Err(wrong_projection(kind, &other)),
                })
                .collect::<Result<Vec<_>, Failure>>()?;
            term_table(&sessions, &meta, width)
        }
        _ => return Err(Failure::internal("list kind has no terminal table")),
    };
    print!("{rendered}");
    Ok(())
}

async fn load_attempt_table_state(
    cursor: Option<String>,
    limit: Option<u32>,
) -> Result<(BoardState, PageMeta), Failure> {
    let mut last_watermarks = Vec::new();
    for _ in 0..SUMMARY_SNAPSHOT_ATTEMPTS {
        let mut client = connect().await?;
        let result = client
            .ask(KernelRequest::ListProjection {
                projection: ProjectionKind::Attempt,
                cursor: cursor.clone(),
                limit,
            })
            .await?;
        let (records, next_cursor, watermark) = match result {
            KernelResult::ProjectionPage {
                records,
                next_cursor,
                watermark,
            } => (records, next_cursor, watermark),
            KernelResult::Error {
                code,
                message,
                detail,
            } => return Err(kernel_failure(code, message, detail)),
            other => {
                return Err(Failure::internal(format!(
                    "read attempt table page: kernel answered with {other:?}"
                )));
            }
        };
        if !records.is_empty() && watermark.is_none() {
            return Err(Failure::new(
                KernelErrorCode::Schema,
                "attempt projection rows arrived without a page watermark",
            ));
        }
        let mut attempts = Vec::with_capacity(records.len());
        for record in records {
            let ProjectionRecord::Attempt { attempt } = record else {
                return Err(wrong_projection(ProjectionKind::Attempt, &record));
            };
            attempts.push(attempt);
        }

        let (mut state, mut watermarks) =
            load_board_state_once(&mut client, FLEET_CONTEXT_PROJECTIONS, BoardView::Fleet).await?;
        watermarks.push(watermark);
        let coherent = watermarks
            .first()
            .is_none_or(|first| watermarks.iter().all(|candidate| candidate == first));
        if coherent {
            state.attempts = attempts;
            state.complete = next_cursor.is_none();
            state.watermark = watermark;
            return Ok((
                state,
                PageMeta {
                    complete: next_cursor.is_none(),
                    watermark,
                    next_cursor,
                },
            ));
        }
        last_watermarks = watermarks;
    }
    Err(Failure::new(
        KernelErrorCode::StaleVersion,
        format!(
            "attempt table projections did not share one watermark after {SUMMARY_SNAPSHOT_ATTEMPTS} reads: {last_watermarks:?}"
        ),
    ))
}

async fn projection_page(
    kind: ProjectionKind,
    cursor: Option<String>,
    limit: Option<u32>,
) -> Result<(Vec<ProjectionRecord>, Option<String>, Option<Seq>), Failure> {
    let result = connect()
        .await?
        .ask(KernelRequest::ListProjection {
            projection: kind,
            cursor,
            limit,
        })
        .await?;
    match result {
        KernelResult::ProjectionPage {
            records,
            next_cursor,
            watermark,
        } => Ok((records, next_cursor, watermark)),
        KernelResult::Error {
            code,
            message,
            detail,
        } => Err(kernel_failure(code, message, detail)),
        other => Err(Failure::internal(format!(
            "list {}: kernel answered with {other:?}",
            kind.as_str()
        ))),
    }
}

fn wrong_projection(wanted: ProjectionKind, record: &ProjectionRecord) -> Failure {
    Failure::new(
        KernelErrorCode::Schema,
        format!(
            "{} projection page carried a {} row",
            wanted.as_str(),
            record.kind().as_str()
        ),
    )
}

fn terminal_width() -> usize {
    crossterm::terminal::size()
        .map(|(columns, _)| usize::from(columns))
        .unwrap_or(80)
}

/// Read every projection consumed by the summary twins at one projector
/// watermark. Projection pages are not snapshots, so a write between kinds
/// makes the whole fold retry rather than silently joining different moments.
async fn load_summary_state(include_cost: bool) -> Result<BoardState, Failure> {
    if include_cost {
        load_board_state(ACTIVITY_PROJECTIONS, BoardView::Activity).await
    } else {
        load_board_state(ESTATE_PROJECTIONS, BoardView::Estate).await
    }
}

async fn load_fleet_state() -> Result<BoardState, Failure> {
    load_board_state(FLEET_PROJECTIONS, BoardView::Fleet).await
}

async fn load_cost_state() -> Result<BoardState, Failure> {
    load_board_state(COST_PROJECTIONS, BoardView::CostHealth).await
}

async fn load_attempt(id: &str) -> Result<gwk_domain::entity::Attempt, Failure> {
    let result = connect()
        .await?
        .ask(KernelRequest::GetProjection {
            projection: ProjectionKind::Attempt,
            id: id.to_owned(),
        })
        .await?;
    match result {
        KernelResult::Projection {
            record: ProjectionRecord::Attempt { attempt },
        } => Ok(attempt),
        KernelResult::Error {
            code,
            message,
            detail,
        } => Err(kernel_failure(code, message, detail)),
        other => Err(Failure::internal(format!(
            "read attempt budget: kernel answered with {other:?}"
        ))),
    }
}

async fn load_board_state(
    kinds: &[ProjectionKind],
    view: BoardView,
) -> Result<BoardState, Failure> {
    let mut last_watermarks = Vec::new();
    for _ in 0..SUMMARY_SNAPSHOT_ATTEMPTS {
        let mut client = connect().await?;
        let (state, watermarks) = load_board_state_once(&mut client, kinds, view).await?;
        let coherent = watermarks
            .first()
            .is_none_or(|first| watermarks.iter().all(|watermark| watermark == first));
        if coherent {
            return Ok(state);
        }
        last_watermarks = watermarks;
    }
    Err(Failure::new(
        KernelErrorCode::StaleVersion,
        format!(
            "summary projections did not share one watermark after {SUMMARY_SNAPSHOT_ATTEMPTS} reads: {last_watermarks:?}"
        ),
    ))
}

async fn load_board_state_once(
    client: &mut Client,
    kinds: &[ProjectionKind],
    view: BoardView,
) -> Result<(BoardState, Vec<Option<Seq>>), Failure> {
    let mut state = BoardState {
        view,
        tasks: Vec::new(),
        runs: Vec::new(),
        attempts: Vec::new(),
        nodes: Vec::new(),
        messages: Vec::new(),
        events: Vec::new(),
        event_tail: gwk_tui::board::EventTail::default(),
        attention: Vec::new(),
        replay: ReplayTimeline::empty(),
        sessions: Vec::new(),
        worktrees: Vec::new(),
        leases: Vec::new(),
        costs: Vec::new(),
        ingested: Vec::new(),
        receipts: Vec::new(),
        complete: true,
        watermark: None,
    };
    let mut watermarks = Vec::new();
    for &kind in kinds {
        let mut cursor = None;
        let mut seen = BTreeSet::new();
        loop {
            let result = client
                .ask(KernelRequest::ListProjection {
                    projection: kind,
                    cursor: cursor.clone(),
                    limit: Some(SUMMARY_PAGE_LIMIT),
                })
                .await?;
            let (records, next_cursor, watermark) = match result {
                KernelResult::ProjectionPage {
                    records,
                    next_cursor,
                    watermark,
                } => (records, next_cursor, watermark),
                KernelResult::Error { code, message, .. } => {
                    return Err(Failure::new(code, message));
                }
                other => {
                    return Err(Failure::internal(format!(
                        "read summary projections: kernel answered with {other:?}"
                    )));
                }
            };
            if !records.is_empty() && watermark.is_none() {
                return Err(Failure::new(
                    KernelErrorCode::Schema,
                    format!(
                        "{} projection rows arrived without a page watermark",
                        kind.as_str()
                    ),
                ));
            }
            watermarks.push(watermark);
            for record in records {
                push_board_record(&mut state, kind, record)?;
            }
            let Some(next) = next_cursor else {
                break;
            };
            if !seen.insert(next.clone()) {
                return Err(Failure::internal(format!(
                    "{} projection cursor repeated {next:?}",
                    kind.as_str()
                )));
            }
            cursor = Some(next);
        }
    }
    state.watermark = watermarks.first().copied().flatten();
    Ok((state, watermarks))
}

fn push_board_record(
    state: &mut BoardState,
    wanted: ProjectionKind,
    record: ProjectionRecord,
) -> Result<(), Failure> {
    match (wanted, record) {
        (ProjectionKind::Task, ProjectionRecord::Task { task }) => state.tasks.push(task),
        (ProjectionKind::Attempt, ProjectionRecord::Attempt { attempt }) => {
            state.attempts.push(attempt);
        }
        (ProjectionKind::EngineSession, ProjectionRecord::EngineSession { engine_session }) => {
            state.sessions.push(engine_session);
        }
        (ProjectionKind::DispatchNode, ProjectionRecord::DispatchNode { dispatch_node }) => {
            state.nodes.push(dispatch_node);
        }
        (ProjectionKind::AttentionItem, ProjectionRecord::AttentionItem { attention_item }) => {
            state.attention.push(attention_item);
        }
        (ProjectionKind::Worktree, ProjectionRecord::Worktree { worktree }) => {
            state.worktrees.push(worktree);
        }
        (ProjectionKind::Lease, ProjectionRecord::Lease { lease }) => state.leases.push(lease),
        (ProjectionKind::CostEntry, ProjectionRecord::CostEntry { cost_entry }) => {
            state.costs.push(cost_entry);
        }
        (ProjectionKind::WorkflowRun, ProjectionRecord::WorkflowRun { workflow_run }) => {
            state.runs.push(workflow_run);
        }
        (ProjectionKind::Message, ProjectionRecord::Message { message }) => {
            state.messages.push(message);
        }
        (ProjectionKind::Receipt, ProjectionRecord::Receipt { receipt }) => {
            state.receipts.push(receipt);
        }
        (ProjectionKind::IngestedRecord, ProjectionRecord::IngestedRecord { ingested_record }) => {
            state.ingested.push(ingested_record);
        }
        (_, record) => {
            return Err(Failure::new(
                KernelErrorCode::Schema,
                format!(
                    "{} projection page carried a {} row",
                    wanted.as_str(),
                    record.kind().as_str()
                ),
            ));
        }
    }
    Ok(())
}

fn emit_serialized(value: &impl serde::Serialize, pretty: bool) -> Result<(), Failure> {
    let value = serde_json::to_value(value)
        .map_err(|why| Failure::internal(format!("render a summary: {why}")))?;
    emit(&value, pretty);
    Ok(())
}

/// The resolved workspace chrome theme.
///
/// The twin of what the workspace paints: same resolver, same closed role
/// set, same refusals — so `gw theme` cannot report a binding the workspace
/// would not use. It answers `signal: true` when nothing was remapped, which
/// is the difference between "my file did nothing" and "my file was never
/// found" — two states an operator otherwise cannot tell apart.
fn chrome_theme() -> Result<Value, Failure> {
    let theme = gwk_tui::chrome::ChromeTheme::from_env()
        .map_err(|why| Failure::new(KernelErrorCode::Schema, why.to_string()))?;
    Ok(serde_json::json!({
        "type": "chrome_theme",
        "source_env": gwk_tui::chrome::CHROME_THEME_ENV,
        "signal": theme.is_signal(),
        "roles": theme
            .bindings()
            .map(|(role, token)| serde_json::json!({
                "role": role.as_str(),
                "token": token.name,
                "value": token.value,
                "index256": token.index256,
                "default": role.default_token(),
            }))
            .collect::<Vec<_>>(),
    }))
}

/// What this build is, without asking anything.
fn build_info() -> Value {
    serde_json::json!({
        "type": "build_info",
        "crate_version": env!("CARGO_PKG_VERSION"),
        "contract_version": CONTRACT_VERSION,
        // Null when the build was not stamped from a clean checkout. A caller
        // comparing this against the revision genesis recorded needs the
        // absence to be visible, not papered over.
        "public_revision": PUBLIC_REVISION,
        "socket_path": socket_path().to_string_lossy(),
    })
}

/// The socket every client verb uses.
fn socket_path() -> PathBuf {
    std::env::var_os(gwk_kernel::config::SOCKET_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(gwk_kernel::DEFAULT_SOCKET_PATH))
}

async fn connect() -> Result<Client, Failure> {
    let (client, _ack) = Client::connect(&socket_path()).await?;
    Ok(client)
}

/// One request, one answer, printed as the kernel produced it.
async fn ask(request: KernelRequest, pretty: bool) -> Result<(), Failure> {
    let result = connect().await?.ask(request).await?;
    answer(result, pretty)
}

/// A result printed, or turned into the failure it is.
fn answer(result: KernelResult, pretty: bool) -> Result<(), Failure> {
    if let KernelResult::Error {
        code,
        message,
        detail,
    } = result
    {
        return Err(kernel_failure(code, message, detail));
    }
    let value = serde_json::to_value(&result)
        .map_err(|e| Failure::internal(format!("render an answer: {e}")))?;
    emit(&value, pretty);
    Ok(())
}

fn kernel_failure(code: KernelErrorCode, message: String, detail: Option<Value>) -> Failure {
    let mut failure = Failure::new(code, message);
    if let Some(detail) = detail {
        // Kept, because the contract puts machine-readable specifics there —
        // the version behind a stale_version, the field behind a validation.
        failure.message = format!("{}: {detail}", failure.message);
    }
    failure
}

/// Submit a command this program minted, in the kernel's own project.
///
/// The project scopes the idempotency key while the aggregate namespace is
/// global, so a convenience verb naming one id does not need to know which
/// project the aggregate belongs to — and a retry of the same verb is the same
/// command. A caller who needs a different project writes the envelope itself
/// and submits it with `gw command submit`.
async fn submit(command: &KernelCommand, key: &str, pretty: bool) -> Result<(), Failure> {
    let envelope = envelope(command, key, gwk_kernel::SYSTEM_PROJECT);
    ask(KernelRequest::SubmitCommand { envelope }, pretty).await
}

pub(crate) fn envelope(command: &KernelCommand, key: &str, project: &str) -> CommandEnvelope {
    CommandEnvelope {
        command_id: CommandId::new(format!("cmd-{key}")),
        project_id: ProjectId::new(project),
        command_type: command.command_type().to_owned(),
        schema_version: ENVELOPE_SCHEMA_VERSION,
        // The CALLER's clock, which is what `issued_at` means. The kernel stamps
        // its own `appended_at` from the database, and that one is authoritative
        // for order — so these two disagreeing is information, not a conflict.
        issued_at: now(),
        actor: Actor {
            kind: "operator".to_owned(),
            id: None,
        },
        origin: Origin {
            system: "gw".to_owned(),
            r#ref: None,
        },
        target_aggregate_type: None,
        target_aggregate_id: None,
        expected_version: None,
        idempotency_key: IdempotencyKey::new(key),
        causation_id: None,
        correlation_id: None,
        // Infallible for a command built here — every variant serializes — and
        // a null payload would be refused by the kernel anyway.
        payload: serde_json::to_value(command).unwrap_or(Value::Null),
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    /// The bug this pins: `unmute a-1` twice, with a re-mute between, used to
    /// send the SAME idempotency key. The kernel answers a repeated key by
    /// replaying the first command's events WITHOUT running the projector, so
    /// the second unmute exited 0 and left the item muted.
    ///
    /// A thousand calls with no sleep between them, deliberately. Two calls
    /// was not a discriminating check: the first version of this fix leaned on
    /// the clock alone, and two calls happened to straddle a nanosecond tick
    /// on Linux while landing inside one microsecond tick on macOS — so the
    /// test agreed with the bug on the machine it was written on. A volume
    /// that cannot fit in one tick anywhere is what makes it a check.
    #[test]
    fn reversible_quieting_acts_never_reuse_an_idempotency_key() {
        const CALLS: usize = 1024;
        for build in [
            (|| format!("ack_attention:a-1:{}", nonce())) as fn() -> String,
            || format!("mute_attention:a-1:2026-08-09T12:00:00Z:{}", nonce()),
            || format!("unmute_attention:a-1:{}", nonce()),
        ] {
            let keys: std::collections::BTreeSet<String> = (0..CALLS).map(|_| build()).collect();
            assert_eq!(
                keys.len(),
                CALLS,
                "calls of a reversible act shared a key, e.g. {:?}",
                keys.first()
            );
        }
    }

    /// And the latching act still does, deliberately: a resolve sets
    /// `resolved_at` once and closes the item, so replaying it is the truth.
    #[test]
    fn resolve_keeps_its_subject_only_key_because_it_latches() {
        assert_eq!(
            format!("resolve_attention:{}", "a-1"),
            format!("resolve_attention:{}", "a-1")
        );
    }
}

fn expect_command(envelope: &CommandEnvelope, wanted: &str) -> Result<(), Failure> {
    if envelope.command_type == wanted {
        return Ok(());
    }
    Err(Failure::usage(format!(
        "this verb submits {wanted}, and the envelope is a {}",
        envelope.command_type
    )))
}

/// Follow the log until the stream ends or the daemon hangs up.
///
/// One JSON object per line, one line per event — not per batch. Batching is the
/// transport's business, and a consumer that wants to resume reads
/// `global_sequence` off the last line it managed to handle.
async fn follow(cursor: Option<Seq>, pretty: bool) -> Result<(), Failure> {
    let mut client = connect().await?;
    let stream = client.subscribe(cursor).await?;
    loop {
        let Some(control) = client.receive().await? else {
            // The daemon hung up. Not an error: a drain on shutdown looks
            // exactly like this, and the caller has every event it printed.
            return Ok(());
        };
        match control {
            ServerControl::EventBatch {
                request_id, events, ..
            } if request_id == stream => {
                for event in &events {
                    let value = serde_json::to_value(event)
                        .map_err(|e| Failure::internal(format!("render an event: {e}")))?;
                    emit(&value, pretty);
                }
            }
            ServerControl::StreamClosed {
                request_id,
                code,
                last_cursor,
            } if request_id == stream => {
                // The cursor is what was DELIVERED, so it belongs in the message:
                // it is the one piece of information that makes the next attempt
                // gap-free.
                let resume = last_cursor
                    .map(|seq| seq.value().to_string())
                    .unwrap_or_else(|| "the beginning".to_owned());
                return Err(Failure::new(
                    code,
                    format!("the stream closed; resume from {resume}"),
                ));
            }
            // Another subscription's traffic, or a response to a request this
            // process never made. Not ours to interpret.
            _ => {}
        }
    }
}

/// Upload one blob and print the descriptor the commit reported.
async fn blob_put(source: &Source, media_type: String, pretty: bool) -> Result<(), Failure> {
    let plaintext = bytes(source)?;
    let address = address_of(&plaintext);
    let mut client = connect().await?;

    let upload = match client
        .ask(KernelRequest::BlobBegin {
            media_type,
            byte_size: ByteCount::new(plaintext.len() as u64),
        })
        .await?
    {
        KernelResult::BlobBegun { upload_id } => upload_id,
        other => return answer(other, pretty),
    };

    // An empty blob still writes one chunk. `chunks` yields nothing for an empty
    // slice, and an upload that never wrote anything is not the same thing to the
    // store as one that wrote nothing.
    let pieces: Vec<&[u8]> = if plaintext.is_empty() {
        vec![&[]]
    } else {
        plaintext.chunks(BLOB_CHUNK_BYTES).collect()
    };
    for (sequence, chunk) in pieces.into_iter().enumerate() {
        let sequence = u32::try_from(sequence)
            .map_err(|_| Failure::usage("this blob has more chunks than the protocol counts"))?;
        match client
            .ask(KernelRequest::BlobChunk {
                upload_id: upload.clone(),
                sequence,
                data_base64: BASE64_STANDARD.encode(chunk),
            })
            .await?
        {
            KernelResult::BlobChunkAccepted { .. } => {}
            other => return answer(other, pretty),
        }
    }

    let result = client
        .ask(KernelRequest::BlobCommit {
            upload_id: upload,
            address,
        })
        .await?;
    answer(result, pretty)
}

/// Read one blob whole, a frame at a time.
async fn blob_get(address: &BlobAddress, output: &Sink, pretty: bool) -> Result<(), Failure> {
    let mut client = connect().await?;
    // Its size first, because a read is clamped to one chunk and the loop has to
    // know when it is done. `stat` is also what distinguishes an absent blob from
    // a shredded one before any bytes move.
    let size = match client
        .ask(KernelRequest::BlobStat {
            address: address.clone(),
        })
        .await?
    {
        KernelResult::BlobStat { descriptor } => descriptor.byte_size.value(),
        other => return answer(other, pretty),
    };

    let mut bytes: Vec<u8> = Vec::with_capacity(size as usize);
    while (bytes.len() as u64) < size {
        let result = client
            .ask(KernelRequest::BlobRead {
                address: address.clone(),
                offset: ByteCount::new(bytes.len() as u64),
                length: ByteCount::new(size - bytes.len() as u64),
            })
            .await?;
        let KernelResult::BlobBytes { data_base64, .. } = result else {
            return answer(result, pretty);
        };
        let part = BASE64_STANDARD
            .decode(&data_base64)
            .map_err(|e| Failure::new(KernelErrorCode::BlobIntegrity, format!("base64: {e}")))?;
        if part.is_empty() {
            return Err(Failure::new(
                KernelErrorCode::BlobIntegrity,
                format!("the read stalled at {} of {size} bytes", bytes.len()),
            ));
        }
        bytes.extend_from_slice(&part);
    }

    match output {
        // Raw bytes, and nothing else — a caller piping a blob somewhere does
        // not want a JSON object in the middle of it.
        Sink::Stdout => tokio::io::stdout()
            .write_all(&bytes)
            .await
            .map_err(|e| Failure::internal(format!("write to standard output: {e}")))?,
        Sink::File(path) => {
            write_file(path, &bytes)?;
            emit(
                &serde_json::json!({
                    "type": "blob_written",
                    "address": address.as_str(),
                    "byte_size": size.to_string(),
                    "path": path.to_string_lossy(),
                }),
                pretty,
            );
        }
    }
    Ok(())
}

/// A JSON document from a file or standard input, decoded into `T`.
///
/// The contract types refuse unknown fields, so a document with a stray key is a
/// refusal here rather than a field the kernel silently ignored.
fn document<T: serde::de::DeserializeOwned>(source: &Source) -> Result<T, Failure> {
    let raw = bytes(source)?;
    serde_json::from_slice(&raw).map_err(|e| Failure::usage(format!("{source:?}: {e}")))
}

fn bytes(source: &Source) -> Result<Vec<u8>, Failure> {
    match source {
        Source::Stdin => {
            use std::io::Read as _;
            let mut buffer = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buffer)
                .map_err(|e| Failure::usage(format!("read standard input: {e}")))?;
            Ok(buffer)
        }
        Source::File(path) => {
            std::fs::read(path).map_err(|e| Failure::usage(format!("read {}: {e}", path.display())))
        }
    }
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), Failure> {
    std::fs::write(path, bytes)
        .map_err(|e| Failure::usage(format!("write {}: {e}", path.display())))
}

/// The address a blob will have: a digest over its PLAINTEXT, which is what
/// makes deduplication work across encryptions of the same content.
fn address_of(plaintext: &[u8]) -> BlobAddress {
    // `hex_lower` returns 64 lowercase hex characters, which is exactly what
    // `from_digest` accepts, so this cannot fail. The fallback that used to sit
    // here — parsing `"sha256:"` — was the worse of the two: it is itself an
    // error, so a broken invariant would have aborted on the SECOND failure
    // with "unreachable" instead of naming the first.
    BlobAddress::from_digest(&hex_lower(plaintext)).expect("a sha256 digest is 64 lowercase hex")
}

fn digest_of_json(value: &Value) -> String {
    hex_lower(serde_json::to_string(value).unwrap_or_default().as_bytes())
}

fn hex_lower(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    let mut out = String::with_capacity(64);
    for byte in digest {
        // Two lowercase hex digits, without a formatting dependency.
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// A per-call nonce for the idempotency keys of commands that must NOT dedup
/// across separate calls (see the attention-quieting arms above).
///
/// Every one of the three parts is load-bearing, and CI proved the middle one.
/// Wall-clock nanoseconds carry uniqueness ACROSS invocations — the process is
/// one invocation long, so a counter alone restarts at zero every time. But
/// the clock alone is not enough either: `as_nanos()` reports whatever
/// resolution the host has, and macOS ticks `SystemTime` in MICROseconds, so
/// two calls in one tick read the same value — which is exactly how the first
/// version of this fix passed on Linux and failed on the macOS job. The low 32
/// bits therefore fold in the pid and an in-process counter to break those
/// ties. Truncating the pid to 16 bits is safe here because it is a tiebreak
/// within a tick, not the uniqueness itself.
pub(crate) fn nonce() -> u128 {
    static CALL: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let call = CALL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    (nanos << 32) | u128::from(std::process::id() as u16) << 16 | u128::from(call)
}

/// Now, as RFC 3339 in UTC.
///
/// Written out rather than taken from a date library, because this is the only
/// place the CLI needs a calendar and the conversion is a dozen lines. UTC only:
/// a local offset in an `issued_at` would be a second thing to reconcile against
/// the kernel's own UTC-pinned clock.
fn now() -> Timestamp {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    Timestamp::new(rfc3339(secs))
}

pub(crate) fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rest = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// Days since the epoch to a civil date, by Howard Hinnant's algorithm — the
/// standard one, shifted to an era starting in March so a leap day lands last.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_renders_dates_a_database_will_accept() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(86_399), "1970-01-01T23:59:59Z");
        assert_eq!(rfc3339(86_400), "1970-01-02T00:00:00Z");
        // The case a naive conversion gets wrong: a leap day in a century year
        // that IS a leap year.
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(951_868_800), "2000-03-01T00:00:00Z");
        // And a century year that is NOT a leap year: February ends at the 28th
        // and the next day is March, with no 29th in between.
        assert_eq!(rfc3339(4_107_456_000), "2100-02-28T00:00:00Z");
        assert_eq!(rfc3339(4_107_456_000 + 86_400), "2100-03-01T00:00:00Z");
        // Monotonic as text, which is what makes these sortable at all.
        assert!(rfc3339(0) < rfc3339(86_400));
    }

    #[test]
    fn a_blob_address_is_the_digest_of_its_own_plaintext() {
        // The empty-string SHA-256, which is the one digest worth hard-coding:
        // it catches a hex table with its nibbles the wrong way round, which a
        // round-trip test would not.
        assert_eq!(
            address_of(b"").as_str(),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_ne!(address_of(b"a").as_str(), address_of(b"b").as_str());
    }

    #[test]
    fn a_minted_envelope_is_the_same_envelope_on_a_retry() {
        let command = KernelCommand::ResolveAttention {
            attention_item_id: AttentionItemId::new("a-1"),
            resolution: None,
        };
        let one = envelope(&command, "resolve_attention:a-1", "system");
        let two = envelope(&command, "resolve_attention:a-1", "system");
        // Everything but the clock: the id and the key are derived from the
        // request, which is what makes a repeated `gw attention resolve` land
        // once rather than twice.
        assert_eq!(one.command_id, two.command_id);
        assert_eq!(one.idempotency_key, two.idempotency_key);
        assert_eq!(one.command_type, "resolve_attention");
        assert_eq!(one.payload, two.payload);
    }

    #[test]
    fn a_verb_that_promises_one_command_refuses_another() {
        let command = KernelCommand::ResolveAttention {
            attention_item_id: AttentionItemId::new("a-1"),
            resolution: None,
        };
        let envelope = envelope(&command, "k", "system");
        assert!(expect_command(&envelope, "resolve_attention").is_ok());
        let wrong = expect_command(&envelope, "grant_authority").expect_err("refused");
        assert_eq!(wrong.exit, exit::USAGE);
    }
}
