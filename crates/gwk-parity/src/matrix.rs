//! The matrix model: three engines by four axes, twelve cells.
//!
//! Pure data — no process, no network, no adapter type. `docs/PARITY.md`:
//! "A matrix cell is green when its test passes against the live kernel; the
//! matrix is green when all twelve cells are." [`Verdict::Skipped`] is a
//! third state this crate adds beyond that pass/fail pair, for exactly the
//! case `docs/PARITY.md`'s own harness section names: "CLI versions asserted
//! against the pins ... before anything runs" — a missing or unpinned engine
//! is a skip, never a hang and never a false pass.

use std::fmt;

/// The three engines the matrix judges, in `docs/PARITY.md`'s own table
/// order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Engine {
    Opencode,
    Claude,
    Codex,
}

impl Engine {
    pub const ALL: [Engine; 3] = [Engine::Opencode, Engine::Claude, Engine::Codex];

    pub const fn name(self) -> &'static str {
        match self {
            Engine::Opencode => "opencode",
            Engine::Claude => "claude",
            Engine::Codex => "codex",
        }
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The four axes `docs/PARITY.md` defines, in its own section order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Axis {
    Lifecycle,
    StatusTruth,
    TranscriptIngestion,
    ApprovalRelay,
}

impl Axis {
    pub const ALL: [Axis; 4] = [
        Axis::Lifecycle,
        Axis::StatusTruth,
        Axis::TranscriptIngestion,
        Axis::ApprovalRelay,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Axis::Lifecycle => "lifecycle",
            Axis::StatusTruth => "status_truth",
            Axis::TranscriptIngestion => "transcript_ingestion",
            Axis::ApprovalRelay => "approval_relay",
        }
    }
}

impl fmt::Display for Axis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One cell's outcome. `Skipped` is distinct from `Pass`: a skip says
/// nothing ran (missing binary, unpinned version, no adapter-exposed way to
/// drive this interaction), where a pass says a real check ran and held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    Skipped,
}

impl Verdict {
    pub const fn name(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "fail",
            Verdict::Skipped => "skipped",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One filled matrix cell.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub engine: Engine,
    pub axis: Axis,
    pub verdict: Verdict,
    pub detail: String,
}

impl Cell {
    pub fn new(engine: Engine, axis: Axis, verdict: Verdict, detail: impl Into<String>) -> Self {
        Self {
            engine,
            axis,
            verdict,
            detail: detail.into(),
        }
    }

    pub fn pass(engine: Engine, axis: Axis, detail: impl Into<String>) -> Self {
        Self::new(engine, axis, Verdict::Pass, detail)
    }

    pub fn fail(engine: Engine, axis: Axis, detail: impl Into<String>) -> Self {
        Self::new(engine, axis, Verdict::Fail, detail)
    }

    pub fn skipped(engine: Engine, axis: Axis, detail: impl Into<String>) -> Self {
        Self::new(engine, axis, Verdict::Skipped, detail)
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "engine": self.engine.name(),
            "axis": self.axis.name(),
            "verdict": self.verdict.name(),
            "detail": self.detail,
        })
    }
}

/// The filled (or partially filled) matrix: every recorded [`Cell`], in
/// recording order.
#[derive(Debug, Clone, Default)]
pub struct Matrix {
    cells: Vec<Cell>,
}

impl Matrix {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, cell: Cell) {
        self.cells.push(cell);
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// `docs/PARITY.md`: "the matrix is green when all twelve cells are" —
    /// green here reads as pass-or-skip, since a skip records why nothing
    /// ran rather than asserting something false.
    pub fn all_green(&self) -> bool {
        self.cells.iter().all(|c| c.verdict != Verdict::Fail)
    }

    /// A human-readable engine-by-axis table, `docs/PARITY.md`'s own
    /// version-pin table style: one row per engine, one column per axis.
    pub fn render_table(&self) -> String {
        let mut out = String::new();
        let header: Vec<&str> = std::iter::once("engine")
            .chain(Axis::ALL.iter().map(|a| a.name()))
            .collect();
        out.push_str(&header.join(" | "));
        out.push('\n');
        for engine in Engine::ALL {
            let mut row = vec![engine.name().to_owned()];
            for axis in Axis::ALL {
                let cell = self
                    .cells
                    .iter()
                    .find(|c| c.engine == engine && c.axis == axis);
                row.push(match cell {
                    Some(c) => c.verdict.name().to_owned(),
                    None => "—".to_owned(),
                });
            }
            out.push_str(&row.join(" | "));
            out.push('\n');
        }
        out
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "cells": self.cells.iter().map(Cell::to_json).collect::<Vec<_>>(),
            "all_green": self.all_green(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_engine_and_axis_name_is_unique_and_lowercase() {
        let mut names: Vec<&str> = Engine::ALL.iter().map(|e| e.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Engine::ALL.len());

        let mut axis_names: Vec<&str> = Axis::ALL.iter().map(|a| a.name()).collect();
        axis_names.sort_unstable();
        axis_names.dedup();
        assert_eq!(axis_names.len(), Axis::ALL.len());

        for name in Engine::ALL
            .iter()
            .map(|e| e.name())
            .chain(Axis::ALL.iter().map(|a| a.name()))
        {
            assert!(name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        }
    }

    #[test]
    fn all_green_is_true_only_when_no_cell_failed() {
        let mut matrix = Matrix::new();
        assert!(matrix.all_green(), "an empty matrix has nothing failing");

        matrix.record(Cell::pass(Engine::Claude, Axis::Lifecycle, "ok"));
        matrix.record(Cell::skipped(Engine::Codex, Axis::Lifecycle, "no binary"));
        assert!(matrix.all_green());

        matrix.record(Cell::fail(
            Engine::Opencode,
            Axis::ApprovalRelay,
            "seeded failure",
        ));
        assert!(!matrix.all_green());
    }

    #[test]
    fn render_table_names_every_engine_and_axis() {
        let mut matrix = Matrix::new();
        matrix.record(Cell::pass(Engine::Claude, Axis::Lifecycle, "ok"));
        let table = matrix.render_table();
        for engine in Engine::ALL {
            assert!(table.contains(engine.name()), "table missing {engine}");
        }
        for axis in Axis::ALL {
            assert!(table.contains(axis.name()), "table missing {axis}");
        }
    }

    #[test]
    fn to_json_carries_every_recorded_cell_and_the_green_summary() {
        let mut matrix = Matrix::new();
        matrix.record(Cell::pass(Engine::Codex, Axis::TranscriptIngestion, "ok"));
        matrix.record(Cell::fail(Engine::Codex, Axis::ApprovalRelay, "seeded"));
        let json = matrix.to_json();
        assert_eq!(json["cells"].as_array().expect("array").len(), 2);
        assert_eq!(json["all_green"], false);
        assert_eq!(json["cells"][0]["engine"], "codex");
        assert_eq!(json["cells"][0]["axis"], "transcript_ingestion");
        assert_eq!(json["cells"][0]["verdict"], "pass");
    }
}
