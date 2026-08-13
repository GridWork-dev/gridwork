//! The Rung-2 mockup pipeline: `ratatui` `TestBackend` renders of every TUI
//! lens over the seeded workday estate (`tests/common/estate.rs`), committed
//! as plain-text goldens — the substrate for the design-mockup loop. See
//! `tests/common/README.md`.
//!
//! One `#[test]` per golden, deliberately: `BLESS=1` panics after writing a
//! golden (`common::assert_matches_golden`'s faithfully-copied bless-run
//! failure), so a loop of several `Variant::check` calls in one `#[test]`
//! would only ever bless its first iteration before the panic unwound the
//! rest away.

mod common;

use common::Variant;
use common::estate::{
    drilldown_attached, empty_frame_input, estate_board_state, estate_config_state,
    estate_frame_input, estate_queue_state,
};
use gwk_domain::entity::{WorkspaceNode, WorkspaceNodeKind};
use gwk_domain::frame::{CellStyle, PtyFrame, StyledCell};
use gwk_domain::ids::{
    PtyFrameSeq, PtySessionGeneration, PtySessionId, RequestId, Timestamp, WorkspaceNodeId,
};
use gwk_domain::protocol::{KernelResult, ServerControl};
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use gwk_tui::board::{self, BoardState, BoardView};
use gwk_tui::config;
use gwk_tui::drilldown;
use gwk_tui::hall;
use gwk_tui::input::HitMap;
use gwk_tui::queue;
use gwk_tui::shell::{self, ShellState, Surface};
use gwk_tui::workspace;
use gwk_tui::workspace::runtime::WorkspaceRuntime;

fn board_check(scenario: &'static str, width: u16, height: u16, state: &BoardState) {
    Variant::new("board", scenario, width, height).check(|area, buf, tier, glyphs| {
        let mut hits = HitMap::new();
        board::render(area, buf, state, None, tier, glyphs, &mut hits);
    });
}

// The design session's baseline: all ten Board panels — the nine views with
// no CLI equivalent included — over the same estate at 120x40.

#[test]
fn board_estate() {
    board_check("estate", 120, 40, &estate_board_state(BoardView::Estate));
}

#[test]
fn board_activity() {
    board_check("brief", 120, 40, &estate_board_state(BoardView::Activity));
}

#[test]
fn board_runs() {
    board_check("run", 120, 40, &estate_board_state(BoardView::Runs));
}

#[test]
fn board_dag() {
    board_check("dag", 120, 40, &estate_board_state(BoardView::Dag));
}

#[test]
fn board_flow() {
    board_check("flow", 120, 40, &estate_board_state(BoardView::Flow));
}

#[test]
fn board_events() {
    board_check("events", 120, 40, &estate_board_state(BoardView::Events));
}

#[test]
fn board_replay() {
    board_check("replay", 120, 40, &estate_board_state(BoardView::Replay));
}

#[test]
fn board_fleet() {
    board_check("fleet", 120, 40, &estate_board_state(BoardView::Fleet));
}

#[test]
fn board_cost_health() {
    board_check("cost", 120, 40, &estate_board_state(BoardView::CostHealth));
}

#[test]
fn board_audit() {
    board_check("audit", 120, 40, &estate_board_state(BoardView::Audit));
}

// The density floor: Fleet and Cost/Health are the two panels with the most
// columns, checked again at a small terminal size.

#[test]
fn board_fleet_at_80x24() {
    board_check("fleet", 80, 24, &estate_board_state(BoardView::Fleet));
}

#[test]
fn board_cost_health_at_80x24() {
    board_check("cost", 80, 24, &estate_board_state(BoardView::CostHealth));
}

#[test]
fn hall_estate_at_120x40_truecolor_unicode() {
    let input = estate_frame_input();
    Variant::new("hall", "estate", 120, 40).check(|area, buf, tier, glyphs| {
        let mut hits = HitMap::new();
        hall::render(area, buf, &input, tier, glyphs, &mut hits);
    });
}

#[test]
fn hall_estate_at_80x24() {
    let input = estate_frame_input();
    Variant::new("hall", "estate", 80, 24).check(|area, buf, tier, glyphs| {
        let mut hits = HitMap::new();
        hall::render(area, buf, &input, tier, glyphs, &mut hits);
    });
}

#[test]
fn hall_estate_ascii_mono() {
    let input = estate_frame_input();
    Variant::new("hall", "estate", 120, 40)
        .with_tier(ColorTier::Mono)
        .with_glyphs(GlyphSet::Ascii)
        .check(|area, buf, tier, glyphs| {
            let mut hits = HitMap::new();
            hall::render(area, buf, &input, tier, glyphs, &mut hits);
        });
}

#[test]
fn hall_estate_ansi16_unicode() {
    let input = estate_frame_input();
    Variant::new("hall", "estate", 120, 40)
        .with_tier(ColorTier::Ansi16)
        .check(|area, buf, tier, glyphs| {
            let mut hits = HitMap::new();
            hall::render(area, buf, &input, tier, glyphs, &mut hits);
        });
}

#[test]
fn hall_empty() {
    let input = empty_frame_input();
    Variant::new("hall", "empty", 80, 24).check(|area, buf, tier, glyphs| {
        let mut hits = HitMap::new();
        hall::render(area, buf, &input, tier, glyphs, &mut hits);
    });
}

#[test]
fn queue_estate_default() {
    let state = estate_queue_state();
    Variant::new("queue", "estate", 120, 40).check(|area, buf, tier, glyphs| {
        let mut hits = HitMap::new();
        queue::render(area, buf, &state, None, tier, glyphs, &mut hits);
    });
}

#[test]
fn queue_estate_ascii_mono() {
    let state = estate_queue_state();
    Variant::new("queue", "estate", 120, 40)
        .with_tier(ColorTier::Mono)
        .with_glyphs(GlyphSet::Ascii)
        .check(|area, buf, tier, glyphs| {
            let mut hits = HitMap::new();
            queue::render(area, buf, &state, None, tier, glyphs, &mut hits);
        });
}

#[test]
fn config_estate_default() {
    let state = estate_config_state();
    Variant::new("config", "estate", 120, 40).check(|area, buf, tier, glyphs| {
        let mut hits = HitMap::new();
        config::render(area, buf, &state, None, tier, glyphs, &mut hits);
    });
}

#[test]
fn drilldown_attached_default() {
    let state = drilldown_attached();
    Variant::new("drilldown", "attached", 100, 31).check(|area, buf, tier, _glyphs| {
        let mut hits = HitMap::new();
        drilldown::render(area, buf, &state, None, tier, &mut hits);
    });
}

fn workspace_row(
    id: &str,
    kind: WorkspaceNodeKind,
    parent: Option<&str>,
    session: Option<&str>,
) -> WorkspaceNode {
    WorkspaceNode {
        id: WorkspaceNodeId::new(id),
        version: 1,
        kind,
        parent_id: parent.map(WorkspaceNodeId::new),
        session_id: session.map(PtySessionId::new),
        created_at: Timestamp::new(id),
        updated_at: Timestamp::new(id),
    }
}

fn workspace_runtime(sessions: &[&str]) -> WorkspaceRuntime {
    if sessions.is_empty() {
        return WorkspaceRuntime::from_projection(&[]);
    }
    let mut rows = vec![
        workspace_row("ws-main", WorkspaceNodeKind::Workspace, None, None),
        workspace_row("tab-main", WorkspaceNodeKind::Tab, Some("ws-main"), None),
    ];
    for (index, session) in sessions.iter().enumerate() {
        let parent = if index == 0 {
            "tab-main".to_owned()
        } else {
            format!("pane-{}", index - 1)
        };
        rows.push(workspace_row(
            &format!("pane-{index}"),
            WorkspaceNodeKind::Pane,
            Some(&parent),
            Some(session),
        ));
    }
    WorkspaceRuntime::from_projection(&rows)
}

fn plain_style() -> CellStyle {
    CellStyle {
        bold: false,
        dim: false,
        italic: false,
        blink: false,
        inverse: false,
        invisible: false,
        strikethrough: false,
        overline: false,
        underline: None,
        fg: None,
        bg: None,
        underline_color: None,
    }
}

fn snapshot(runtime: &mut WorkspaceRuntime, index: usize, lines: &[&str]) {
    let (pane, session) = runtime.visible_bound_panes().expect("visible panes")[index].clone();
    runtime.ensure_attachment(pane, session.clone());
    let request_id = RequestId::new(format!("workspace-golden-{index}"));
    runtime
        .begin_attach(pane, request_id.clone())
        .expect("begin attach");
    let width = lines
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(1)
        .max(24);
    let rows: Vec<Vec<StyledCell>> = lines
        .iter()
        .map(|line| {
            let mut cells: Vec<_> = line
                .chars()
                .map(|glyph| StyledCell {
                    glyph: glyph.to_string(),
                    style: plain_style(),
                })
                .collect();
            cells.resize(
                width,
                StyledCell {
                    glyph: " ".to_owned(),
                    style: plain_style(),
                },
            );
            cells
        })
        .collect();
    let generation = PtySessionGeneration::new(format!("golden-generation-{index}"));
    runtime.ingest(&ServerControl::Response {
        request_id: request_id.clone(),
        result: KernelResult::PtyAttached {
            session_id: session.clone(),
            generation: generation.clone(),
            rows: rows.len() as u16,
            cols: width as u16,
            cursor: None,
        },
    });
    runtime
        .attachment_mut(pane)
        .expect("pane mirror")
        .ingest(&ServerControl::Response {
            request_id,
            result: KernelResult::PtySnapshot {
                session_id: session,
                generation,
                seq: PtyFrameSeq::new(7),
                frame: PtyFrame::from_cells(&rows, None),
            },
        });
}

fn paint_workspace(
    area: ratatui::layout::Rect,
    buffer: &mut ratatui::buffer::Buffer,
    runtime: &WorkspaceRuntime,
    tier: ColorTier,
    glyphs: GlyphSet,
    notice: Option<&str>,
) {
    let mut shell = ShellState::new(Surface::TermAttach);
    shell.set_attach_subject(runtime.focused_session().map_or_else(
        || "workspace  unbound".to_owned(),
        |session| format!("workspace  {session}"),
    ));
    if let Some(notice) = notice {
        shell.set_notice(notice);
    }
    let body = shell::body_area(area);
    let mut hits = HitMap::new();
    workspace::render::render(
        body,
        buffer,
        &runtime.state,
        tier,
        glyphs,
        &gwk_tui::chrome::ChromeTheme::signal(),
        &mut hits,
    );
    workspace::runtime::render_panes(body, buffer, runtime, tier);
    workspace::render::render_input(
        body,
        buffer,
        &runtime.input,
        tier,
        glyphs,
        &gwk_tui::chrome::ChromeTheme::signal(),
    );
    shell::render_chrome(area, buffer, &shell, tier, glyphs, "off");
}

#[test]
fn workspace_empty_120x40() {
    let runtime = workspace_runtime(&[]);
    Variant::new("workspace", "empty", 120, 40).check(|area, buffer, tier, glyphs| {
        paint_workspace(area, buffer, &runtime, tier, glyphs, None);
    });
}

#[test]
fn workspace_empty_80x24() {
    let runtime = workspace_runtime(&[]);
    Variant::new("workspace", "empty", 80, 24).check(|area, buffer, tier, glyphs| {
        paint_workspace(area, buffer, &runtime, tier, glyphs, None);
    });
}

fn light_runtime() -> WorkspaceRuntime {
    let mut runtime = workspace_runtime(&["pty-build"]);
    snapshot(
        &mut runtime,
        0,
        &[
            "gw@kernel:~/gridwork$ cargo test -p gwk-tui --lib workspace",
            "running 53 tests",
            "test result: ok. 53 passed; 0 failed",
            "gw@kernel:~/gridwork$ ",
        ],
    );
    runtime
}

#[test]
fn workspace_light_120x40() {
    let runtime = light_runtime();
    Variant::new("workspace", "light", 120, 40).check(|area, buffer, tier, glyphs| {
        paint_workspace(area, buffer, &runtime, tier, glyphs, None);
    });
}

#[test]
fn workspace_light_80x24() {
    let runtime = light_runtime();
    Variant::new("workspace", "light", 80, 24).check(|area, buffer, tier, glyphs| {
        paint_workspace(area, buffer, &runtime, tier, glyphs, None);
    });
}

fn dense_runtime() -> WorkspaceRuntime {
    let mut runtime = workspace_runtime(&["pty-build", "pty-review", "pty-tests", "pty-docs"]);
    for (index, lines) in [
        &["build", "Compiling gridwork", "Finished dev profile"][..],
        &["review", "checking workspace runtime", "no blockers"][..],
        &["tests", "53 passed", "0 failed"][..],
        &["docs", "derivation record", "digest pending"][..],
    ]
    .into_iter()
    .enumerate()
    {
        snapshot(&mut runtime, index, lines);
    }
    runtime
}

#[test]
fn workspace_dense_120x40() {
    let runtime = dense_runtime();
    Variant::new("workspace", "dense", 120, 40).check(|area, buffer, tier, glyphs| {
        paint_workspace(area, buffer, &runtime, tier, glyphs, None);
    });
}

#[test]
fn workspace_dense_80x24() {
    let runtime = dense_runtime();
    Variant::new("workspace", "dense", 80, 24).check(|area, buffer, tier, glyphs| {
        paint_workspace(area, buffer, &runtime, tier, glyphs, None);
    });
}

fn degraded_runtime() -> WorkspaceRuntime {
    let mut runtime = workspace_runtime(&["pty-live", "pty-wait"]);
    snapshot(
        &mut runtime,
        0,
        &["live pane", "transport output remains visible"],
    );
    runtime.clear_requests();
    runtime
}

#[test]
fn workspace_degraded_120x40_ascii_mono() {
    let runtime = degraded_runtime();
    Variant::new("workspace", "degraded", 120, 40)
        .with_tier(ColorTier::Mono)
        .with_glyphs(GlyphSet::Ascii)
        .check(|area, buffer, tier, glyphs| {
            paint_workspace(
                area,
                buffer,
                &runtime,
                tier,
                glyphs,
                Some("terminal transport closed; retrying from durable workspace truth"),
            );
        });
}

#[test]
fn workspace_degraded_80x24_ascii_mono() {
    let runtime = degraded_runtime();
    Variant::new("workspace", "degraded", 80, 24)
        .with_tier(ColorTier::Mono)
        .with_glyphs(GlyphSet::Ascii)
        .check(|area, buffer, tier, glyphs| {
            paint_workspace(
                area,
                buffer,
                &runtime,
                tier,
                glyphs,
                Some("terminal transport closed; retrying from durable workspace truth"),
            );
        });
}
