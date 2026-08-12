//! Plain-text table twins for list lenses.
//!
//! A terminal list and its interactive lens share column priority and value
//! semantics. This module owns the non-ratatui half; callers keep wire JSON
//! untouched when stdout is not a terminal.

use gwk_domain::entity::{EngineSession, PtySession};
use gwk_domain::ids::{Seq, TaskId, Timestamp};

use crate::board::BoardState;
use crate::console::{
    Spend, attempt_state_word, dollars, elapsed, same_utc_date, short_role, spend_for,
};
use crate::theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageMeta {
    pub complete: bool,
    pub watermark: Option<Seq>,
    pub next_cursor: Option<String>,
}

struct Column {
    header: &'static str,
    priority: u8,
}

const fn column(header: &'static str, priority: u8) -> Column {
    Column { header, priority }
}

pub fn attempt_table(state: &BoardState, now: &Timestamp, meta: &PageMeta, width: usize) -> String {
    let columns = [
        column("ATTEMPT", 0),
        column("TASK", 3),
        column("ENGINE", 2),
        column("ROLE", 2),
        column("STATE", 0),
        column("SUB", 4),
        column("SES", 4),
        column("LEASE", 3),
        column("TOKENS", 3),
        column("SPEND", 1),
        column("AGE", 1),
    ];
    let rows = state
        .attempts
        .iter()
        .map(|attempt| {
            let spend = spend_for(state, &attempt.id);
            let lease = attempt
                .worktree_lease_id
                .as_ref()
                .and_then(|id| state.leases.iter().find(|candidate| candidate.id == *id));
            let lease = lease.map_or_else(
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
            vec![
                attempt.id.as_str().to_owned(),
                task_label(state, attempt.task_id.clone()),
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
                count(
                    state
                        .sessions
                        .iter()
                        .filter(|session| {
                            session.attempt_id == attempt.id && session.ended_at.is_none()
                        })
                        .count(),
                ),
                lease,
                spend.token_text(),
                spend.text(),
                elapsed(now, &attempt.created_at).unwrap_or_else(|| "-".to_owned()),
            ]
        })
        .collect::<Vec<_>>();
    let mut output = table(&columns, &rows, width);
    output.push_str(&trailer(rows.len(), meta));
    output
}

pub fn term_table(sessions: &[PtySession], meta: &PageMeta, width: usize) -> String {
    let columns = [
        column("SESSION", 0),
        column("GEN", 1),
        column("STATE", 0),
        column("ATT/DET", 3),
        column("ATTEMPT", 2),
        column("TITLE", 2),
        column("OPENED", 3),
    ];
    let rows = sessions
        .iter()
        .map(|session| {
            vec![
                session.id.as_str().to_owned(),
                session.generation.as_str().to_owned(),
                session.state.clone(),
                format!("{}/{}", session.attach_count, session.detach_count),
                "?".to_owned(),
                session.title.clone().unwrap_or_else(|| "-".to_owned()),
                clock(&session.opened_at),
            ]
        })
        .collect::<Vec<_>>();
    let mut output = table(&columns, &rows, width);
    output.push_str(&trailer(rows.len(), meta));
    output.push_str(
        "\nATTEMPT is ? for every row: pty_session carries no attempt or engine-session\n\
         reference, and engine_session carries no pty reference. Correlating `gw term`\n\
         with `gw session` needs a domain field, not a view change.\n",
    );
    output
}

pub fn session_table(sessions: &[EngineSession], meta: &PageMeta, width: usize) -> String {
    let columns = [
        column("SESSION", 0),
        column("ATTEMPT", 0),
        column("ENGINE", 1),
        column("STATE", 1),
        column("STARTED", 2),
        column("ENDED", 2),
    ];
    let rows = sessions
        .iter()
        .map(|session| {
            vec![
                session.id.as_str().to_owned(),
                session.attempt_id.as_str().to_owned(),
                session.engine.as_str().to_owned(),
                if session.ended_at.is_some() {
                    "ended".to_owned()
                } else {
                    "live".to_owned()
                },
                clock(&session.started_at),
                session
                    .ended_at
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), clock),
            ]
        })
        .collect::<Vec<_>>();
    let mut output = table(&columns, &rows, width);
    output.push_str(&trailer(rows.len(), meta));
    output
}

pub fn cost_table(state: &BoardState, now: &Timestamp, meta: &PageMeta, width: usize) -> String {
    let entries = state
        .costs
        .iter()
        .filter(|entry| same_utc_date(&entry.recorded_at, now))
        .collect::<Vec<_>>();
    let mut lanes: Vec<(String, String, Spend)> = Vec::new();
    for entry in &entries {
        let engine = entry.engine.as_str();
        let model = entry.model.as_deref().unwrap_or("-");
        match lanes
            .iter_mut()
            .find(|(lane, name, _)| lane == engine && name == model)
        {
            Some((_, _, spend)) => spend.add(entry),
            None => {
                let mut spend = Spend::default();
                spend.add(entry);
                lanes.push((engine.to_owned(), model.to_owned(), spend));
            }
        }
    }
    lanes.sort_by_key(|(_, _, spend)| std::cmp::Reverse(spend.micros()));
    let lane_columns = [
        column("ENGINE", 0),
        column("MODEL", 0),
        column("ENTRIES", 2),
        column("TOKENS", 1),
        column("SPEND", 0),
    ];
    let lane_rows = lanes
        .iter()
        .map(|(engine, model, spend)| {
            vec![
                engine.clone(),
                model.clone(),
                spend.entries().to_string(),
                spend.token_text(),
                spend.text(),
            ]
        })
        .collect::<Vec<_>>();

    let mut hours: Vec<(u32, u64)> = Vec::new();
    for entry in &entries {
        let Some(cost) = entry.cost_micros else {
            continue;
        };
        let Some(hour) = hour(&entry.recorded_at) else {
            continue;
        };
        match hours.iter_mut().find(|(candidate, _)| *candidate == hour) {
            Some((_, total)) => *total = total.saturating_add(cost.value()),
            None => hours.push((hour, cost.value())),
        }
    }
    hours.sort_by_key(|(hour, _)| *hour);
    let peak = hours
        .iter()
        .map(|(_, total)| *total)
        .max()
        .unwrap_or(1)
        .max(1);
    let hour_columns = [column("HOUR", 0), column("SPEND", 0), column("SHARE", 1)];
    let hour_rows = hours
        .iter()
        .map(|(hour, total)| {
            let cells = usize::try_from((u128::from(*total) * 24).div_ceil(u128::from(peak)))
                .unwrap_or(24)
                .clamp(1, 24);
            vec![format!("{hour:02}:00"), dollars(*total), "#".repeat(cells)]
        })
        .collect::<Vec<_>>();
    let total = entries
        .iter()
        .filter_map(|entry| entry.cost_micros)
        .fold(0u64, |sum, value| sum.saturating_add(value.value()));
    let priced = entries
        .iter()
        .filter(|entry| entry.cost_micros.is_some())
        .count();
    let unpriced = entries.len().saturating_sub(priced);
    let watermark = watermark(meta);

    let mut output = String::from("BY LANE\n");
    output.push_str(&table(&lane_columns, &lane_rows, width));
    output.push_str("\nBY HOUR\n");
    output.push_str(&table(&hour_columns, &hour_rows, width));
    output.push_str(&format!(
        "\n{} today · {priced} priced · {unpriced} unpriced · watermark {watermark}\n",
        dollars(total)
    ));
    output
}

fn table(columns: &[Column], rows: &[Vec<String>], width: usize) -> String {
    let rows = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|cell| safe_cell(cell, width))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let mut keep = (0..columns.len()).collect::<Vec<_>>();
    loop {
        let widths = keep
            .iter()
            .map(|column| {
                rows.iter()
                    .map(|row| row[*column].chars().count())
                    .chain(std::iter::once(columns[*column].header.len()))
                    .max()
                    .unwrap_or(0)
            })
            .collect::<Vec<_>>();
        let used = widths.iter().sum::<usize>() + keep.len().saturating_sub(1) * 2;
        if used <= width || keep.len() == 1 {
            let mut output = String::new();
            for (slot, column) in keep.iter().enumerate() {
                push_cell(
                    &mut output,
                    columns[*column].header,
                    widths[slot],
                    slot + 1 == keep.len(),
                );
            }
            output.push('\n');
            for row in &rows {
                for (slot, column) in keep.iter().enumerate() {
                    push_cell(
                        &mut output,
                        &row[*column],
                        widths[slot],
                        slot + 1 == keep.len(),
                    );
                }
                output.push('\n');
            }
            return output;
        }
        let victim = keep
            .iter()
            .enumerate()
            .max_by_key(|(slot, column)| (columns[**column].priority, *slot))
            .map(|(slot, _)| slot)
            .expect("table has a column");
        keep.remove(victim);
    }
}

fn push_cell(output: &mut String, text: &str, width: usize, last: bool) {
    output.push_str(text);
    if !last {
        output.extend(std::iter::repeat_n(' ', width + 2 - text.chars().count()));
    }
}

fn trailer(rows: usize, meta: &PageMeta) -> String {
    let count = if meta.complete {
        format!("{rows} rows")
    } else {
        format!("at least {rows} rows")
    };
    match &meta.next_cursor {
        Some(cursor) => {
            // The cursor is a machine token the operator retypes into the
            // next invocation: escape it, never width-truncate it — a cut
            // token pastes as a broken page and the '+' mark would read as
            // part of the value. The terminal may wrap the trailer line.
            // (4096 cells bounds a hostile value; real cursors are short.)
            let cursor = theme::safe_text(cursor, 4096);
            format!(
                "{count} · watermark {} · more: --cursor {cursor}\n",
                watermark(meta)
            )
        }
        None => format!("{count} · watermark {}\n", watermark(meta)),
    }
}

fn safe_cell(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let (mut lines, omitted) = theme::safe_text_lines(text, width, 1);
    let mut line = lines.pop().unwrap_or_default();
    if omitted {
        line.pop();
        line.push('+');
    }
    line
}

fn watermark(meta: &PageMeta) -> String {
    meta.watermark
        .map_or_else(|| "-".to_owned(), |value| value.to_string())
}

fn task_label(state: &BoardState, id: TaskId) -> String {
    state
        .tasks
        .iter()
        .find(|task| task.id == id)
        .map(|task| {
            task.id
                .as_str()
                .strip_prefix("t-")
                .unwrap_or_else(|| task.id.as_str())
                .to_owned()
        })
        .unwrap_or_else(|| "-".to_owned())
}

fn count(value: usize) -> String {
    if value == 0 {
        "-".to_owned()
    } else {
        value.to_string()
    }
}

fn clock(timestamp: &Timestamp) -> String {
    timestamp
        .as_str()
        .get(11..16)
        .unwrap_or(timestamp.as_str())
        .to_owned()
}

fn hour(timestamp: &Timestamp) -> Option<u32> {
    timestamp.as_str().get(11..13)?.parse().ok()
}
