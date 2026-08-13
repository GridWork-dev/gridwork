//! Round 5 — CLI human output (grill G4: auto-table on a TTY, identical
//! JSON when piped, `--json` forces the wire shape anywhere).
//!
//! MOCKUP ONLY — no CLI code. These goldens are plain-text transcripts, not
//! `TestBackend` frames: the subject is stdout, so rendering it through a
//! terminal buffer would be a lie about the medium. They still ride the
//! harness's `assert_matches_golden`, which compares strings.
//!
//! Every table here is folded from the SAME seeded estate the lens mockups
//! render, which is what makes "one design system" checkable rather than
//! asserted: `gw attempt list`'s columns ARE the FLEET lens's ruled columns,
//! and `gw term list`'s ARE the TERM lens's.
//!
//! ## The rule this round adds — one drop order, two surfaces
//!
//! Every column carries a priority. When the width will not take them all,
//! the lowest-priority column goes first, on BOTH surfaces — so a narrow
//! terminal and a narrow lens shed the same columns in the same order, and
//! an operator who learns one has learned the other. A column is dropped
//! whole; nothing is ever silently truncated mid-value.
//!
//! ## Honesty carried over from the lens rounds
//!
//! - Spend keeps its three distinct readings: `-` (no cost entry), `+Nu`
//!   (entries exist, none priced — unknown, not zero), `$X +Nu` (priced
//!   total with an unpriced floor).
//! - The trailer states the row count, the projector watermark it was read
//!   at, and how to get the next page. A count that could be a floor says
//!   "at least".
//! - Colour is never load-bearing: these tables read identically piped to a
//!   file, which is the whole point of the shape being ruled once.

mod common;

#[path = "mockups/shared.rs"]
mod shared;

use common::assert_matches_golden;
use gwk_domain::entity::CostEntry;
use gwk_domain::fsm::AttemptState;
use gwk_domain::ids::AttemptId;
use gwk_tui::board::{BoardState, BoardView, NO_END_RECORDED};
use gwk_tui::console::unended_cell;
use shared::{dollars, short_role, tokens};

const NOW_MINUTES: u32 = 17 * 60 + 30;
const WATERMARK: u64 = 221;

// ---------------------------------------------------------------------------
// Table shape
// ---------------------------------------------------------------------------

/// A column and how hard it fights to stay. Priority 0 never drops; the
/// highest priority drops first. The identity column and the one fact the
/// verb exists to report are always 0.
struct Column {
    header: &'static str,
    priority: u8,
}

const fn col(header: &'static str, priority: u8) -> Column {
    Column { header, priority }
}

/// Render a table, dropping whole columns by descending priority until it
/// fits. Two-space gutters; every column left-aligned, because a mixed
/// alignment costs more than it buys when values are ids and words.
fn table(columns: &[Column], rows: &[Vec<String>], width: usize) -> String {
    let mut keep: Vec<usize> = (0..columns.len()).collect();
    loop {
        let widths: Vec<usize> = keep
            .iter()
            .map(|index| {
                rows.iter()
                    .map(|row| row[*index].chars().count())
                    .chain(std::iter::once(columns[*index].header.len()))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let total: usize = widths.iter().sum::<usize>() + 2 * keep.len().saturating_sub(1);
        if total <= width || keep.len() == 1 {
            let mut out = String::new();
            for (slot, index) in keep.iter().enumerate() {
                push_cell(
                    &mut out,
                    columns[*index].header,
                    widths[slot],
                    slot + 1 == keep.len(),
                );
            }
            out.push('\n');
            for row in rows {
                for (slot, index) in keep.iter().enumerate() {
                    push_cell(&mut out, &row[*index], widths[slot], slot + 1 == keep.len());
                }
                out.push('\n');
            }
            return out;
        }
        // Drop the current least-important column; ties break rightmost-first.
        let victim = keep
            .iter()
            .enumerate()
            .max_by_key(|(slot, index)| (columns[**index].priority, *slot))
            .map(|(slot, _)| slot)
            .expect("a column to drop");
        keep.remove(victim);
    }
}

fn push_cell(out: &mut String, text: &str, width: usize, last: bool) {
    out.push_str(text);
    if !last {
        for _ in text.chars().count()..width + 2 {
            out.push(' ');
        }
    }
}

/// The trailer every list verb prints. `complete` false renders the count
/// as a floor — the wording the summary structs already carry and no
/// shipped verb can currently reach.
fn trailer(rows: usize, complete: bool, cursor: Option<&str>) -> String {
    let count = if complete {
        format!("{rows} rows")
    } else {
        format!("at least {rows} rows")
    };
    match cursor {
        Some(cursor) => format!("{count} · watermark {WATERMARK} · more: --cursor {cursor}\n"),
        None => format!("{count} · watermark {WATERMARK}\n"),
    }
}

// ---------------------------------------------------------------------------
// Derivations (the same folds the FLEET lens makes)
// ---------------------------------------------------------------------------

fn minutes(stamp: &str) -> u32 {
    let hour: u32 = stamp[11..13].parse().expect("hour");
    let minute: u32 = stamp[14..16].parse().expect("minute");
    hour * 60 + minute
}

fn age(stamp: &str) -> String {
    let elapsed = NOW_MINUTES.saturating_sub(minutes(stamp));
    if elapsed < 60 {
        format!("{elapsed}m")
    } else {
        format!("{}h{:02}", elapsed / 60, elapsed % 60)
    }
}

fn clock(stamp: &str) -> String {
    stamp[11..16].to_owned()
}

fn attempt_state_word(state: AttemptState) -> &'static str {
    match state {
        AttemptState::Queued => "queued",
        AttemptState::Leased => "leased",
        AttemptState::Starting => "starting",
        AttemptState::Running => "running",
        AttemptState::Blocked => "blocked",
        AttemptState::Canceling => "canceling",
        AttemptState::Canceled => "canceled",
        AttemptState::Failed => "failed",
        AttemptState::Unknown => "unknown",
        AttemptState::Succeeded => "done",
    }
}

#[derive(Default, Clone, Copy)]
struct Spend {
    micros: u64,
    priced: usize,
    unpriced: usize,
    input: u64,
    output: u64,
}

impl Spend {
    fn add(&mut self, entry: &CostEntry) {
        match entry.cost_micros {
            Some(micros) => {
                self.micros += micros.value();
                self.priced += 1;
            }
            None => self.unpriced += 1,
        }
        self.input += entry.input_tokens.map_or(0, |count| count.value());
        self.output += entry.output_tokens.map_or(0, |count| count.value());
    }

    fn text(&self) -> String {
        match (self.priced, self.unpriced) {
            (0, 0) => "-".to_owned(),
            (0, unpriced) => format!("+{unpriced}u"),
            (_, 0) => dollars(self.micros),
            (_, unpriced) => format!("{} +{unpriced}u", dollars(self.micros)),
        }
    }

    fn token_text(&self) -> String {
        if self.priced + self.unpriced == 0 {
            return "-".to_owned();
        }
        format!("{}/{}", tokens(self.input), tokens(self.output))
    }
}

/// Attribution precedence, identical to the FLEET lens: attempt, else the
/// entry's engine session, else its dispatch node. Counted exactly once.
fn cost_attempt(entry: &CostEntry, state: &BoardState) -> Option<AttemptId> {
    if let Some(attempt) = &entry.attempt_id {
        return Some(attempt.clone());
    }
    if let Some(session_id) = &entry.engine_session_id
        && let Some(session) = state.sessions.iter().find(|s| &s.id == session_id)
    {
        return Some(session.attempt_id.clone());
    }
    if let Some(node_id) = &entry.dispatch_node_id
        && let Some(node) = state.nodes.iter().find(|n| &n.id == node_id)
    {
        return node.attempt_id.clone();
    }
    None
}

fn spend_for(state: &BoardState, attempt: &AttemptId) -> Spend {
    let mut spend = Spend::default();
    for entry in &state.costs {
        if cost_attempt(entry, state).as_ref() == Some(attempt) {
            spend.add(entry);
        }
    }
    spend
}

// ---------------------------------------------------------------------------
// The verbs
// ---------------------------------------------------------------------------

/// `gw attempt list` — the FLEET lens's work-grain columns as a table. The
/// lens's state glyph has no CLI equivalent, so STATE carries the word
/// alone: the same reason the Board prints state as a word.
fn attempt_list(width: usize) -> String {
    let state = common::estate::estate_board_state(BoardView::Fleet);
    let columns = &[
        col("ATTEMPT", 0),
        col("TASK", 3),
        col("ENGINE", 2),
        col("ROLE", 2),
        col("STATE", 0),
        col("SUB", 4),
        // Round 8: `SES` claimed a liveness the fold does not support. The
        // shipped `tables::attempt_table` renders this same column and this
        // golden is the contract between the two, so the round-5 painter
        // moves with it.
        col("NOEND", 4),
        col("LEASE", 3),
        col("TOKENS", 3),
        col("SPEND", 1),
        col("AGE", 1),
    ];
    let rows: Vec<Vec<String>> = state
        .attempts
        .iter()
        .map(|attempt| {
            let spend = spend_for(&state, &attempt.id);
            let lease = attempt
                .worktree_lease_id
                .as_ref()
                .and_then(|id| state.leases.iter().find(|lease| &lease.id == id));
            let lease_text = lease.map_or_else(
                || "-".to_owned(),
                |lease| {
                    let mut flags = String::new();
                    if lease.dirty {
                        flags.push('d');
                    }
                    if lease.unpushed {
                        flags.push('u');
                    }
                    if flags.is_empty() {
                        lease.id.as_str().to_owned()
                    } else {
                        format!("{} {flags}", lease.id.as_str())
                    }
                },
            );
            let count = |value: usize| {
                if value == 0 {
                    "-".to_owned()
                } else {
                    value.to_string()
                }
            };
            vec![
                attempt.id.as_str().to_owned(),
                state
                    .tasks
                    .iter()
                    .find(|task| task.id == attempt.task_id)
                    .map_or_else(
                        || "-".to_owned(),
                        |task| {
                            task.id
                                .as_str()
                                .strip_prefix("t-")
                                .unwrap_or_else(|| task.id.as_str())
                                .to_owned()
                        },
                    ),
                attempt.engine.as_str().to_owned(),
                attempt.role.as_deref().map_or("-", short_role).to_owned(),
                attempt_state_word(attempt.state).to_owned(),
                count(
                    state
                        .nodes
                        .iter()
                        .filter(|node| node.attempt_id.as_ref() == Some(&attempt.id))
                        .count(),
                ),
                unended_cell(&state, attempt),
                lease_text,
                spend.token_text(),
                spend.text(),
                age(attempt.created_at.as_str()),
            ]
        })
        .collect();

    let mut out = String::from("$ gw attempt list\n");
    out.push_str(&table(columns, &rows, width));
    out.push_str(&trailer(rows.len(), true, None));
    out
}

/// `gw term list` — the ruled `gw term` noun (kickoff ruling 7). Terminal
/// lifetimes, not engine sessions.
///
/// ATTEMPT is the column that CANNOT be filled today: `PtySession` carries
/// no attempt or engine-session reference and `EngineSession` carries no
/// pty reference, so the two nouns cannot be correlated even in principle.
/// It is rendered as an explicit `?` rather than omitted, because a missing
/// column hides the gap while a `?` states it.
fn term_list(width: usize) -> String {
    let columns = &[
        col("SESSION", 0),
        col("GEN", 1),
        col("STATE", 0),
        col("ATT/DET", 3),
        col("ATTEMPT", 2),
        col("TITLE", 2),
        col("OPENED", 3),
    ];
    let (running, closed) = common::estate::estate_pty_sessions();
    let rows: Vec<Vec<String>> = [running, closed]
        .iter()
        .map(|session| {
            vec![
                session.id.as_str().to_owned(),
                session.generation.as_str().to_owned(),
                session.state.clone(),
                format!("{}/{}", session.attach_count, session.detach_count),
                "?".to_owned(),
                session.title.clone().unwrap_or_else(|| "-".to_owned()),
                clock(session.opened_at.as_str()),
            ]
        })
        .collect();

    let mut out = String::from("$ gw term list\n");
    out.push_str(&table(columns, &rows, width));
    out.push_str(&trailer(rows.len(), true, None));
    out.push_str(
        "\nATTEMPT is ? for every row: pty_session carries no attempt or engine-session\n\
         reference, and engine_session carries no pty reference. Correlating `gw term`\n\
         with `gw session` needs a domain field, not a view change.\n",
    );
    out
}

/// `gw session list` — engine sessions, the noun `gw session` keeps.
fn session_list(width: usize) -> String {
    let state = common::estate::estate_board_state(BoardView::Fleet);
    let columns = &[
        col("SESSION", 0),
        col("ATTEMPT", 0),
        col("ENGINE", 1),
        col("STATE", 1),
        col("STARTED", 2),
        col("ENDED", 2),
    ];
    let rows: Vec<Vec<String>> = state
        .sessions
        .iter()
        .map(|session| {
            vec![
                session.id.as_str().to_owned(),
                session.attempt_id.as_str().to_owned(),
                session.engine.as_str().to_owned(),
                // Round 8: one field, one word, shared with the Board panel
                // that always refused to call this alive.
                if session.ended_at.is_some() {
                    "ended".to_owned()
                } else {
                    NO_END_RECORDED.to_owned()
                },
                clock(session.started_at.as_str()),
                session
                    .ended_at
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), |stamp| clock(stamp.as_str())),
            ]
        })
        .collect();

    let mut out = String::from("$ gw session list\n");
    out.push_str(&table(columns, &rows, width));
    // The seeded page is complete, but the floor wording is what a partial
    // read must print — shown here because no shipped verb can reach it.
    out.push_str(&trailer(rows.len(), false, Some("c2VxOjIyMQ")));
    out
}

/// `gw cost rollup` — two folds of one ledger: by lane (what the shipped
/// rollup already groups) and by hour (the time axis it has never had).
fn cost_rollup(width: usize) -> String {
    let state = common::estate::estate_board_state(BoardView::CostHealth);

    let mut lanes: Vec<(String, String, Spend)> = Vec::new();
    for entry in &state.costs {
        let engine = entry.engine.as_str().to_owned();
        let model = entry.model.clone().unwrap_or_else(|| "-".to_owned());
        match lanes
            .iter_mut()
            .find(|(lane, name, _)| lane == &engine && name == &model)
        {
            Some((_, _, spend)) => spend.add(entry),
            None => {
                let mut spend = Spend::default();
                spend.add(entry);
                lanes.push((engine, model, spend));
            }
        }
    }
    lanes.sort_by_key(|(_, _, spend)| std::cmp::Reverse(spend.micros));

    let lane_columns = &[
        col("ENGINE", 0),
        col("MODEL", 0),
        col("ENTRIES", 2),
        col("TOKENS", 1),
        col("SPEND", 0),
    ];
    let lane_rows: Vec<Vec<String>> = lanes
        .iter()
        .map(|(engine, model, spend)| {
            vec![
                engine.clone(),
                model.clone(),
                (spend.priced + spend.unpriced).to_string(),
                spend.token_text(),
                spend.text(),
            ]
        })
        .collect();

    let mut hours: Vec<(u32, u64)> = Vec::new();
    for entry in &state.costs {
        let Some(micros) = entry.cost_micros else {
            continue;
        };
        let hour = minutes(entry.recorded_at.as_str()) / 60;
        match hours.iter_mut().find(|(bucket, _)| *bucket == hour) {
            Some((_, total)) => *total += micros.value(),
            None => hours.push((hour, micros.value())),
        }
    }
    hours.sort_by_key(|(hour, _)| *hour);
    let peak = hours.iter().map(|(_, total)| *total).max().unwrap_or(1);

    let hour_columns = &[col("HOUR", 0), col("SPEND", 0), col("SHARE", 1)];
    let hour_rows: Vec<Vec<String>> = hours
        .iter()
        .map(|(hour, total)| {
            // Same min-one-mark rule the lens chart uses: an hour that spent
            // anything must be visible, however small.
            let cells = ((total * 24).div_ceil(peak) as usize).clamp(1, 24);
            vec![format!("{hour:02}:00"), dollars(*total), "#".repeat(cells)]
        })
        .collect();

    let total: u64 = state
        .costs
        .iter()
        .filter_map(|entry| entry.cost_micros.map(|micros| micros.value()))
        .sum();
    let unpriced = state
        .costs
        .iter()
        .filter(|entry| entry.cost_micros.is_none())
        .count();

    let mut out = String::from("$ gw cost rollup\n");
    out.push_str("BY LANE\n");
    out.push_str(&table(lane_columns, &lane_rows, width));
    out.push_str("\nBY HOUR\n");
    out.push_str(&table(hour_columns, &hour_rows, width));
    out.push_str(&format!(
        "\n{} today · {} priced · {unpriced} unpriced · watermark {WATERMARK}\n",
        dollars(total),
        state.costs.len() - unpriced,
    ));
    out
}

/// The G4 contract itself: same verb, two destinations. The table is what a
/// terminal gets; the wire shape is what a pipe gets, byte-identical to
/// today's output so no script changes.
fn piping() -> String {
    let mut out = String::new();
    out.push_str("$ gw term list | jq -c '.records[0]'\n");
    out.push_str(
        "{\"id\":\"pty-1\",\"version\":4,\"state\":\"running\",\"generation\":\"gen-3\",\
         \"attach_count\":2,\"detach_count\":1,\"title\":\"kernel worktree shell\",\
         \"opened_at\":\"2026-08-11T10:21:00Z\",\"updated_at\":\"2026-08-11T17:28:00Z\"}\n",
    );
    out.push_str("\n$ gw term list --json | head -c 64\n");
    out.push_str("{\"type\":\"projection_page\",\"records\":[{\"id\":\"pty-1\",\"version\":4\n");
    out.push_str(
        "\nA pipe gets the wire shape it gets today -- counters as canonical decimal\n\
         strings, absent fields omitted rather than null. --json forces that shape on a\n\
         terminal too. Only the tty default changed.\n",
    );
    out
}

// ---------------------------------------------------------------------------
// The golden matrix
// ---------------------------------------------------------------------------

fn check(verb: &str, width: usize) {
    let rendered = match verb {
        "attempt-list" => attempt_list(width),
        "term-list" => term_list(width),
        "session-list" => session_list(width),
        "cost-rollup" => cost_rollup(width),
        "piping" => piping(),
        other => panic!("unknown verb {other}"),
    };
    assert_matches_golden(&format!("mock-cli-{verb}-{width}col"), &rendered);
}

#[test]
fn attempt_list_at_120() {
    check("attempt-list", 120);
}

#[test]
fn attempt_list_at_80() {
    check("attempt-list", 80);
}

#[test]
fn term_list_at_120() {
    check("term-list", 120);
}

#[test]
fn session_list_at_120() {
    check("session-list", 120);
}

#[test]
fn cost_rollup_at_120() {
    check("cost-rollup", 120);
}

#[test]
fn piping_contract() {
    check("piping", 120);
}
