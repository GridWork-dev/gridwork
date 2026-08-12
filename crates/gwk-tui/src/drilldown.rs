//! The session drill-down lens: a hosted engine's styled-cell wire frames.
//!
//! This module never interprets terminal bytes. The host owns the engine and
//! converts its grid into [`PtyFrame`](gwk_domain::frame::PtyFrame) and
//! [`PtyDelta`](gwk_domain::frame::PtyDelta); this lens validates those typed
//! values, expands the wire's interned runs into a per-cell client-side
//! mirror at the decode boundary, and paints it into Ratatui's buffer — the
//! render path never sees a run or a style index.
//!
//! # Geometry
//!
//! A session's dimensions belong to the hosted process. A smaller lens crops
//! and scrolls the client-side mirror; it never resizes the session (which
//! would SIGWINCH the engine for every attached consumer). A wire `resized`
//! delta invalidates the mirror and asks for a new snapshot, exactly as the
//! contract requires.
//!
//! # Fidelity
//!
//! Engine foreground and background colors pass through unchanged. An absent
//! color becomes Ratatui's [`Color::Reset`](ratatui::style::Color::Reset), not
//! an unpainted `None`: the child asked for the terminal default. Ratatui 0.30
//! has one underline modifier rather than the wire's five shapes and has no
//! overline modifier. Those two losses are counted in the frame instead of
//! disappearing silently; every other carried style attribute paints directly.

use gwk_domain::frame::{CellColor, CellStyle, PtyAnsiSlot, PtyDelta, PtyFrame, StyledCell};
use gwk_domain::ids::{PtyFrameSeq, PtySessionGeneration, PtySessionId, RequestId};
use gwk_domain::protocol::{KernelErrorCode, KernelResult, ServerControl};
use gwk_theme::tier::ColorTier;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::input::HitMap;
use crate::theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrilldownTarget {
    Session(PtySessionId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestDisposition {
    Applied,
    Counted,
    Unrelated,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WireDiagnostics {
    pub foreign_references: usize,
    pub invalid_frames: usize,
    pub invalid_updates: usize,
    pub stale_batches: usize,
    pub unseeded_updates: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamStatus {
    Waiting,
    Snapshot,
    Live,
    Closed {
        code: KernelErrorCode,
        last_seq: Option<PtyFrameSeq>,
    },
}

#[derive(Debug, Clone)]
pub struct DrilldownState {
    session_id: PtySessionId,
    attach_request_id: Option<RequestId>,
    generation: Option<PtySessionGeneration>,
    /// The client-side mirror, expanded to the per-cell model at the decode
    /// boundary: the wire's interned runs stop here, and everything below —
    /// updates, cropping, painting — works on plain cells.
    cells: Option<Vec<Vec<StyledCell>>>,
    frame_seq: Option<PtyFrameSeq>,
    last_seq: Option<PtyFrameSeq>,
    expected_size: Option<(u16, u16)>,
    viewport_row: u16,
    viewport_col: u16,
    diagnostics: WireDiagnostics,
    stream_status: StreamStatus,
}

impl DrilldownState {
    pub fn new(session_id: PtySessionId) -> Self {
        Self {
            session_id,
            attach_request_id: None,
            generation: None,
            cells: None,
            frame_seq: None,
            last_seq: None,
            expected_size: None,
            viewport_row: 0,
            viewport_col: 0,
            diagnostics: WireDiagnostics::default(),
            stream_status: StreamStatus::Waiting,
        }
    }

    pub fn session_id(&self) -> &PtySessionId {
        &self.session_id
    }

    /// The expanded mirror, when one is seeded — the per-cell grid every
    /// wire frame decodes into.
    pub fn cells(&self) -> Option<&Vec<Vec<StyledCell>>> {
        self.cells.as_ref()
    }

    pub fn generation(&self) -> Option<&PtySessionGeneration> {
        self.generation.as_ref()
    }

    pub fn frame_seq(&self) -> Option<PtyFrameSeq> {
        self.frame_seq
    }

    pub fn diagnostics(&self) -> WireDiagnostics {
        self.diagnostics
    }

    /// Register the request before sending a `PtyAttach` control.
    ///
    /// The request id must be known before its response arrives so a typed
    /// refusal can be attributed to this lens rather than ignored as unrelated.
    pub fn begin_attach(&mut self, request_id: RequestId) {
        self.attach_request_id = Some(request_id);
        self.stream_status = StreamStatus::Waiting;
    }

    pub fn set_viewport(&mut self, row: u16, col: u16) {
        let (cols, rows) = self.expected_size.unwrap_or((0, 0));
        self.viewport_row = row.min(rows.saturating_sub(1));
        self.viewport_col = col.min(cols.saturating_sub(1));
    }

    /// Consume one typed server control value from the shared wire.
    ///
    /// Non-PTY controls are unrelated. PTY controls carrying another session
    /// or attach request are counted as foreign references and never followed.
    pub fn ingest(&mut self, control: &ServerControl) -> IngestDisposition {
        match control {
            ServerControl::Response {
                request_id,
                result:
                    KernelResult::PtyAttached {
                        session_id,
                        generation,
                        rows,
                        cols,
                        cursor,
                    },
            } => {
                if session_id != &self.session_id
                    || self
                        .attach_request_id
                        .as_ref()
                        .is_some_and(|expected| expected != request_id)
                {
                    self.diagnostics.foreign_references =
                        self.diagnostics.foreign_references.saturating_add(1);
                    return IngestDisposition::Counted;
                }
                self.attach_request_id = Some(request_id.clone());
                let generation_changed = self.switch_generation(generation);
                self.expected_size = Some((*cols, *rows));
                if !generation_changed
                    && cursor.is_some_and(|cursor| self.frame_seq.is_none_or(|seq| cursor > seq))
                {
                    self.cells = None;
                    self.frame_seq = None;
                }
                self.last_seq = match (generation_changed, self.last_seq, *cursor) {
                    (true, _, attached) => attached,
                    (false, Some(current), Some(attached)) => Some(current.max(attached)),
                    (false, current, attached) => current.or(attached),
                };
                self.stream_status = StreamStatus::Live;
                self.clamp_viewport();
                IngestDisposition::Applied
            }
            ServerControl::Response {
                result:
                    KernelResult::PtySnapshot {
                        session_id,
                        generation,
                        seq,
                        frame,
                    },
                ..
            } => self.apply_snapshot(session_id, generation, *seq, frame),
            ServerControl::Response {
                request_id,
                result: KernelResult::Error { code, .. },
            } if self.attach_request_id.as_ref() == Some(request_id) => {
                self.attach_request_id = None;
                self.stream_status = StreamStatus::Closed {
                    code: *code,
                    last_seq: self.last_seq,
                };
                IngestDisposition::Applied
            }
            ServerControl::PtyDeltaBatch {
                request_id,
                session_id,
                generation,
                deltas,
                seq,
            } => self.apply_batch(request_id, session_id, generation, deltas, *seq),
            ServerControl::PtyStreamClosed {
                request_id,
                generation,
                code,
                last_seq,
            } => {
                if self.attach_request_id.as_ref() != Some(request_id)
                    || self.generation.as_ref() != Some(generation)
                {
                    self.diagnostics.foreign_references =
                        self.diagnostics.foreign_references.saturating_add(1);
                    IngestDisposition::Counted
                } else {
                    self.attach_request_id = None;
                    self.stream_status = StreamStatus::Closed {
                        code: *code,
                        last_seq: *last_seq,
                    };
                    IngestDisposition::Applied
                }
            }
            _ => IngestDisposition::Unrelated,
        }
    }

    fn apply_snapshot(
        &mut self,
        session_id: &PtySessionId,
        generation: &PtySessionGeneration,
        seq: PtyFrameSeq,
        frame: &PtyFrame,
    ) -> IngestDisposition {
        if session_id != &self.session_id {
            self.diagnostics.foreign_references =
                self.diagnostics.foreign_references.saturating_add(1);
            return IngestDisposition::Counted;
        }
        // The one place the wire's interned shape is expanded: a frame no
        // rectangular grid matches — a dangling style index, ragged rows, a
        // dimension past u16 — is counted and never painted, exactly as the
        // old ragged-grid check counted. Non-maximal runs expand fine (the
        // tolerate ruling); the kernel's strict decoder is where refusals
        // with an error vocabulary live.
        let Some(cells) = frame.cells() else {
            self.diagnostics.invalid_frames = self.diagnostics.invalid_frames.saturating_add(1);
            return IngestDisposition::Counted;
        };
        // Expansion bounded both dimensions to u16.
        let size = (
            cells.first().map_or(0, |row| row.len() as u16),
            cells.len() as u16,
        );
        // Only the correlated attach response may replace an established life.
        // A snapshot can seed an empty lens, but an untracked delayed response
        // must not roll an active stream backward to a retired generation.
        if self
            .generation
            .as_ref()
            .is_some_and(|current| current != generation)
        {
            self.diagnostics.foreign_references =
                self.diagnostics.foreign_references.saturating_add(1);
            return IngestDisposition::Counted;
        }
        if self.last_seq.is_some_and(|last| seq < last) {
            self.diagnostics.stale_batches = self.diagnostics.stale_batches.saturating_add(1);
            return IngestDisposition::Counted;
        }
        self.switch_generation(generation);
        self.cells = Some(cells);
        self.frame_seq = Some(seq);
        self.last_seq = Some(seq);
        self.expected_size = Some(size);
        if self.stream_status == StreamStatus::Waiting {
            self.stream_status = StreamStatus::Snapshot;
        }
        self.clamp_viewport();
        IngestDisposition::Applied
    }

    fn apply_batch(
        &mut self,
        request_id: &RequestId,
        session_id: &PtySessionId,
        generation: &PtySessionGeneration,
        deltas: &[PtyDelta],
        seq: PtyFrameSeq,
    ) -> IngestDisposition {
        if session_id != &self.session_id
            || self.attach_request_id.as_ref() != Some(request_id)
            || self.generation.as_ref() != Some(generation)
        {
            self.diagnostics.foreign_references =
                self.diagnostics.foreign_references.saturating_add(1);
            return IngestDisposition::Counted;
        }
        if self.last_seq.is_some_and(|last| seq <= last) {
            self.diagnostics.stale_batches = self.diagnostics.stale_batches.saturating_add(1);
            return IngestDisposition::Counted;
        }

        let before = self.diagnostics;
        for delta in deltas {
            match delta {
                PtyDelta::Resized { rows, cols } => {
                    self.cells = None;
                    self.frame_seq = None;
                    self.expected_size = Some((*cols, *rows));
                    self.clamp_viewport();
                }
                PtyDelta::CellsChanged { styles, updates } => {
                    let Some(cells) = self.cells.as_mut() else {
                        self.diagnostics.unseeded_updates = self
                            .diagnostics
                            .unseeded_updates
                            .saturating_add(updates.len());
                        continue;
                    };
                    for update in updates {
                        // Both lookups resolve against THIS batch: the cell
                        // by coordinates, the style through the batch's own
                        // table. Either failing is counted, not followed —
                        // an index into a table the message did not carry is
                        // the same class of orphan as a coordinate outside
                        // the grid.
                        let style = usize::try_from(update.style)
                            .ok()
                            .and_then(|index| styles.get(index));
                        let slot = cells
                            .get_mut(usize::from(update.row))
                            .and_then(|row| row.get_mut(usize::from(update.col)));
                        match (slot, style) {
                            (Some(cell), Some(style)) => {
                                *cell = StyledCell {
                                    glyph: update.glyph.clone(),
                                    style: style.clone(),
                                };
                            }
                            _ => {
                                self.diagnostics.invalid_updates =
                                    self.diagnostics.invalid_updates.saturating_add(1);
                            }
                        }
                    }
                }
            }
        }
        self.last_seq = Some(seq);
        if self.cells.is_some() {
            self.frame_seq = Some(seq);
        }
        if self.diagnostics == before {
            IngestDisposition::Applied
        } else {
            IngestDisposition::Counted
        }
    }

    fn switch_generation(&mut self, generation: &PtySessionGeneration) -> bool {
        if self.generation.as_ref() == Some(generation) {
            return false;
        }
        self.generation = Some(generation.clone());
        self.cells = None;
        self.frame_seq = None;
        self.last_seq = None;
        true
    }

    fn clamp_viewport(&mut self) {
        let (cols, rows) = self.expected_size.unwrap_or((0, 0));
        self.viewport_row = self.viewport_row.min(rows.saturating_sub(1));
        self.viewport_col = self.viewport_col.min(cols.saturating_sub(1));
    }
}

fn ansi_color(slot: PtyAnsiSlot) -> Color {
    match slot {
        PtyAnsiSlot::Black => Color::Black,
        PtyAnsiSlot::Red => Color::Red,
        PtyAnsiSlot::Green => Color::Green,
        PtyAnsiSlot::Yellow => Color::Yellow,
        PtyAnsiSlot::Blue => Color::Blue,
        PtyAnsiSlot::Magenta => Color::Magenta,
        PtyAnsiSlot::Cyan => Color::Cyan,
        PtyAnsiSlot::White => Color::Gray,
        PtyAnsiSlot::BrightBlack => Color::DarkGray,
        PtyAnsiSlot::BrightRed => Color::LightRed,
        PtyAnsiSlot::BrightGreen => Color::LightGreen,
        PtyAnsiSlot::BrightYellow => Color::LightYellow,
        PtyAnsiSlot::BrightBlue => Color::LightBlue,
        PtyAnsiSlot::BrightMagenta => Color::LightMagenta,
        PtyAnsiSlot::BrightCyan => Color::LightCyan,
        PtyAnsiSlot::BrightWhite => Color::White,
    }
}

fn wire_color(color: Option<CellColor>) -> Color {
    match color {
        Some(CellColor::Ansi16 { slot }) => ansi_color(slot),
        Some(CellColor::Xterm256 { index }) => Color::Indexed(index),
        Some(CellColor::Truecolor { r, g, b }) => Color::Rgb(r, g, b),
        None => Color::Reset,
    }
}

fn wire_style(style: &CellStyle) -> (Style, usize) {
    let mut modifiers = Modifier::empty();
    for (enabled, modifier) in [
        (style.bold, Modifier::BOLD),
        (style.dim, Modifier::DIM),
        (style.italic, Modifier::ITALIC),
        (style.blink, Modifier::SLOW_BLINK),
        (style.inverse, Modifier::REVERSED),
        (style.invisible, Modifier::HIDDEN),
        (style.strikethrough, Modifier::CROSSED_OUT),
        (style.underline.is_some(), Modifier::UNDERLINED),
    ] {
        if enabled {
            modifiers.insert(modifier);
        }
    }
    let approximations = usize::from(style.overline)
        + usize::from(
            style
                .underline
                .is_some_and(|underline| underline != gwk_domain::frame::CellUnderline::Single),
        );
    (
        Style::default()
            .fg(wire_color(style.fg))
            .bg(wire_color(style.bg))
            .underline_color(wire_color(style.underline_color))
            .add_modifier(modifiers),
        approximations,
    )
}

pub fn target_order(state: &DrilldownState) -> Vec<DrilldownTarget> {
    vec![DrilldownTarget::Session(state.session_id.clone())]
}

pub fn render(
    area: Rect,
    buf: &mut Buffer,
    state: &DrilldownState,
    selected: Option<&DrilldownTarget>,
    tier: ColorTier,
    hits: &mut HitMap<DrilldownTarget>,
) {
    hits.clear();
    if area.width == 0 || area.height == 0 {
        return;
    }

    let status_y = area.y + area.height - 1;
    let diagnostics = diagnostic_line(state.diagnostics);
    let diagnostic_rows = u16::from(diagnostics.is_some() && area.height > 1);
    if let Some(line) = diagnostics {
        let safe = theme::safe_text(&line, area.width as usize);
        buf.set_stringn(
            area.x,
            area.y,
            safe.as_ref(),
            area.width as usize,
            theme::state_style(theme::binding("needs_attention"), tier),
        );
    }

    let body_y = area.y + diagnostic_rows;
    let body_rows = area.height.saturating_sub(1 + diagnostic_rows);
    let mut style_approximations = 0usize;
    match &state.cells {
        Some(cells) => {
            let source_row = usize::from(state.viewport_row);
            let source_col = usize::from(state.viewport_col);
            for output_row in 0..body_rows {
                let Some(row) = cells.get(source_row + usize::from(output_row)) else {
                    break;
                };
                let mut x = area.x;
                for cell in row.iter().skip(source_col) {
                    let remaining = area.x.saturating_add(area.width).saturating_sub(x);
                    if remaining == 0 {
                        break;
                    }
                    let glyph = if cell.glyph.is_empty() {
                        theme::safe_text(" ", remaining as usize)
                    } else {
                        theme::safe_text(&cell.glyph, remaining as usize)
                    };
                    if glyph.is_empty() {
                        break;
                    }
                    let (style, approximations) = wire_style(&cell.style);
                    style_approximations = style_approximations.saturating_add(approximations);
                    buf.set_stringn(
                        x,
                        body_y + output_row,
                        glyph.as_ref(),
                        remaining as usize,
                        style,
                    );
                    let painted = u16::try_from(glyph.chars().count()).unwrap_or(u16::MAX);
                    x = x.saturating_add(painted);
                }
            }
        }
        None if body_rows > 0 => {
            let message = state.expected_size.map_or_else(
                || "waiting for the first hosted frame".to_owned(),
                |(cols, rows)| format!("snapshot required for {cols}x{rows}"),
            );
            let safe = theme::safe_text(&message, area.width as usize);
            buf.set_stringn(
                area.x,
                body_y,
                safe.as_ref(),
                area.width as usize,
                theme::state_style(theme::binding("idle"), tier),
            );
        }
        None => {}
    }

    let target = DrilldownTarget::Session(state.session_id.clone());
    let is_selected = selected == Some(&target);
    if is_selected {
        buf.set_string(
            area.x,
            status_y,
            " ",
            Style::default().add_modifier(Modifier::REVERSED),
        );
    }
    let mut status_style = Style::default();
    if is_selected && matches!(tier, ColorTier::Truecolor | ColorTier::Xterm256) {
        status_style = gwk_theme::SIGNAL
            .iter()
            .find(|token| token.name == "selection")
            .map_or(status_style, |token| theme::token_style(token, tier));
    }
    let seq = state
        .frame_seq
        .map_or_else(|| "-".to_owned(), |seq| seq.to_string());
    let (cols, rows) = state.expected_size.unwrap_or((0, 0));
    let style_notice = if style_approximations == 0 {
        String::new()
    } else {
        format!(
            "{style_approximations} style approximation{}  ",
            if style_approximations == 1 { "" } else { "s" }
        )
    };
    let stream_notice = match state.stream_status {
        StreamStatus::Waiting => "waiting  ".to_owned(),
        StreamStatus::Snapshot => "snapshot  ".to_owned(),
        StreamStatus::Live => "live  ".to_owned(),
        StreamStatus::Closed { code, last_seq } => format!(
            "closed {code} at {}  ",
            last_seq.map_or_else(|| "-".to_owned(), |seq| seq.to_string())
        ),
    };
    let status = format!(
        "{style_notice}{stream_notice}SESSION {}  seq {seq}  {cols}x{rows}  view {},{}",
        state.session_id, state.viewport_col, state.viewport_row
    );
    let status_width = area.width.saturating_sub(1);
    let safe_status = theme::safe_text(&status, status_width as usize);
    buf.set_stringn(
        area.x + 1,
        status_y,
        safe_status.as_ref(),
        status_width as usize,
        status_style,
    );
    hits.register(Rect::new(area.x, status_y, area.width, 1), target);
}

fn diagnostic_line(diagnostics: WireDiagnostics) -> Option<String> {
    let mut facts = Vec::new();
    for (count, singular, plural) in [
        (
            diagnostics.foreign_references,
            "foreign ref",
            "foreign refs",
        ),
        (
            diagnostics.invalid_frames,
            "invalid frame",
            "invalid frames",
        ),
        (diagnostics.invalid_updates, "invalid cell", "invalid cells"),
        (diagnostics.stale_batches, "stale batch", "stale batches"),
        (
            diagnostics.unseeded_updates,
            "unseeded update",
            "unseeded updates",
        ),
    ] {
        if count > 0 {
            facts.push(format!(
                "{count} {}",
                if count == 1 { singular } else { plural }
            ));
        }
    }
    (!facts.is_empty()).then(|| format!("wire diagnostics: {}", facts.join(", ")))
}

#[cfg(test)]
mod tests {
    use gwk_domain::frame::{
        CellColor, CellStyle, CellUnderline, PtyAnsiSlot, PtyCellUpdate, PtyDelta, StyledCell,
    };
    use gwk_domain::ids::PtySessionGeneration;
    use gwk_domain::protocol::{KernelResult, ServerControl};
    use ratatui::style::{Color, Modifier};

    use super::*;

    fn style() -> CellStyle {
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

    fn cell(glyph: &str) -> StyledCell {
        StyledCell {
            glyph: glyph.to_owned(),
            style: style(),
        }
    }

    fn frame(rows: &[&[&str]]) -> PtyFrame {
        // `from_cells` interns whatever grid it is handed, ragged included —
        // producer-side it never validates — which is exactly what lets the
        // ragged-frame tests below build their invalid wire values here too.
        PtyFrame::from_cells(
            &rows
                .iter()
                .map(|row| row.iter().map(|glyph| cell(glyph)).collect())
                .collect::<Vec<Vec<StyledCell>>>(),
            None,
        )
    }

    fn changed(updates: Vec<(u16, u16, &str)>) -> PtyDelta {
        PtyDelta::CellsChanged {
            styles: vec![style()],
            updates: updates
                .into_iter()
                .map(|(row, col, glyph)| PtyCellUpdate {
                    row,
                    col,
                    glyph: glyph.to_owned(),
                    style: 0,
                })
                .collect(),
        }
    }

    fn attached(request: &str, session: &str, rows: u16, cols: u16) -> ServerControl {
        attached_at(request, session, rows, cols, None)
    }

    fn attached_at(
        request: &str,
        session: &str,
        rows: u16,
        cols: u16,
        cursor: Option<u64>,
    ) -> ServerControl {
        attached_at_in_generation(request, session, "life-1", rows, cols, cursor)
    }

    fn attached_at_in_generation(
        request: &str,
        session: &str,
        generation: &str,
        rows: u16,
        cols: u16,
        cursor: Option<u64>,
    ) -> ServerControl {
        ServerControl::Response {
            request_id: RequestId::new(request),
            result: KernelResult::PtyAttached {
                session_id: PtySessionId::new(session),
                generation: PtySessionGeneration::new(generation),
                rows,
                cols,
                cursor: cursor.map(PtyFrameSeq::new),
            },
        }
    }

    fn snapshot(request: &str, session: &str, seq: u64, frame: PtyFrame) -> ServerControl {
        snapshot_in_generation(request, session, "life-1", seq, frame)
    }

    fn snapshot_in_generation(
        request: &str,
        session: &str,
        generation: &str,
        seq: u64,
        frame: PtyFrame,
    ) -> ServerControl {
        ServerControl::Response {
            request_id: RequestId::new(request),
            result: KernelResult::PtySnapshot {
                session_id: PtySessionId::new(session),
                generation: PtySessionGeneration::new(generation),
                seq: PtyFrameSeq::new(seq),
                frame,
            },
        }
    }

    fn batch(request: &str, session: &str, seq: u64, deltas: Vec<PtyDelta>) -> ServerControl {
        batch_in_generation(request, session, "life-1", seq, deltas)
    }

    fn batch_in_generation(
        request: &str,
        session: &str,
        generation: &str,
        seq: u64,
        deltas: Vec<PtyDelta>,
    ) -> ServerControl {
        ServerControl::PtyDeltaBatch {
            request_id: RequestId::new(request),
            session_id: PtySessionId::new(session),
            generation: PtySessionGeneration::new(generation),
            deltas,
            seq: PtyFrameSeq::new(seq),
        }
    }

    fn dump(
        state: &DrilldownState,
        width: u16,
        height: u16,
    ) -> (String, Buffer, HitMap<DrilldownTarget>) {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        let mut hits = HitMap::new();
        render(area, &mut buf, state, None, ColorTier::Mono, &mut hits);
        let mut text = String::new();
        for y in 0..height {
            let mut line = String::new();
            for x in 0..width {
                line.push_str(buf[(x, y)].symbol());
            }
            text.push_str(line.trim_end());
            text.push('\n');
        }
        (text, buf, hits)
    }

    fn assert_matches_golden(name: &str, rendered: &str) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("goldens")
            .join(format!("{name}.txt"));
        let committed = std::fs::read_to_string(&path)
            .unwrap_or_else(|why| panic!("{}: {why}", path.display()));
        assert_eq!(committed, rendered, "{} drifted", path.display());
    }

    #[test]
    fn drilldown_empty_state_has_its_own_frame() {
        let state = DrilldownState::new(PtySessionId::new("pty-demo"));
        let (dump, _, _) = dump(&state, 52, 5);

        assert!(
            dump.contains("waiting for the first hosted frame"),
            "{dump}"
        );
        assert_matches_golden("drilldown-empty", &dump);
    }

    #[test]
    fn drilldown_snapshot_without_an_attach_is_named_as_a_snapshot() {
        let mut state = DrilldownState::new(PtySessionId::new("pty-1"));
        state.ingest(&snapshot("snapshot-1", "pty-1", 4, frame(&[&["ready"]])));

        let (dump, _, _) = dump(&state, 64, 4);
        assert!(dump.contains("snapshot  SESSION"), "{dump}");
        assert!(!dump.contains("waiting  SESSION"), "{dump}");
    }

    #[test]
    fn drilldown_snapshot_and_typed_delta_update_the_same_wire_frame() {
        let mut state = DrilldownState::new(PtySessionId::new("pty-1"));
        assert_eq!(
            state.ingest(&attached("attach-1", "pty-1", 2, 2)),
            IngestDisposition::Applied
        );
        assert_eq!(
            state.ingest(&snapshot(
                "snapshot-1",
                "pty-1",
                4,
                frame(&[&["a", "b"], &["c", "d"]])
            )),
            IngestDisposition::Applied
        );
        assert_eq!(
            state.ingest(&batch(
                "attach-1",
                "pty-1",
                5,
                vec![changed(vec![(1, 0, "x")])]
            )),
            IngestDisposition::Applied
        );

        assert_eq!(state.frame_seq(), Some(PtyFrameSeq::new(5)));
        assert_eq!(state.cells().expect("mirror")[1][0].glyph, "x");
    }

    #[test]
    fn drilldown_foreign_stale_and_missing_references_are_counted_not_followed() {
        let mut state = DrilldownState::new(PtySessionId::new("pty-1"));
        state.ingest(&attached("attach-1", "pty-1", 1, 1));
        state.ingest(&snapshot("snapshot-1", "pty-1", 4, frame(&[&["a"]])));

        assert_eq!(
            state.ingest(&batch(
                "attach-other",
                "pty-1",
                5,
                vec![changed(vec![(0, 0, "wrong request")])]
            )),
            IngestDisposition::Counted
        );
        assert_eq!(
            state.ingest(&batch("attach-1", "pty-other", 5, Vec::new())),
            IngestDisposition::Counted
        );
        assert_eq!(
            state.ingest(&batch("attach-1", "pty-1", 4, Vec::new())),
            IngestDisposition::Counted
        );
        assert_eq!(
            state.ingest(&batch(
                "attach-1",
                "pty-1",
                5,
                vec![changed(vec![(9, 9, "orphan")])]
            )),
            IngestDisposition::Counted
        );

        assert_eq!(state.cells().expect("mirror")[0][0].glyph, "a");
        assert_eq!(
            state.diagnostics(),
            WireDiagnostics {
                foreign_references: 2,
                invalid_updates: 1,
                stale_batches: 1,
                ..WireDiagnostics::default()
            }
        );
        let (dump, _, _) = dump(&state, 96, 5);
        assert!(dump.contains("wire diagnostics:"), "{dump}");
        assert!(dump.contains("2 foreign refs"), "{dump}");
        assert!(dump.contains("1 invalid cell"), "{dump}");
    }

    #[test]
    fn drilldown_resize_invalidates_coordinates_until_a_new_snapshot() {
        let mut state = DrilldownState::new(PtySessionId::new("pty-1"));
        state.ingest(&attached("attach-1", "pty-1", 1, 1));
        state.ingest(&snapshot("snapshot-1", "pty-1", 1, frame(&[&["a"]])));

        let disposition = state.ingest(&batch(
            "attach-1",
            "pty-1",
            2,
            vec![
                PtyDelta::Resized { rows: 2, cols: 3 },
                changed(vec![(0, 0, "not followed")]),
            ],
        ));

        assert_eq!(disposition, IngestDisposition::Counted);
        assert!(
            state.cells().is_none(),
            "a resize invalidates the held grid"
        );
        assert_eq!(state.diagnostics().unseeded_updates, 1);
        let (dump, _, _) = dump(&state, 64, 5);
        assert!(dump.contains("snapshot required for 3x2"), "{dump}");
    }

    #[test]
    fn drilldown_attach_cursor_newer_than_the_frame_requires_a_fresh_snapshot() {
        let mut state = DrilldownState::new(PtySessionId::new("pty-1"));
        state.ingest(&snapshot("snapshot-1", "pty-1", 4, frame(&[&["stale"]])));

        state.ingest(&attached_at("attach-1", "pty-1", 1, 1, Some(7)));

        assert!(
            state.cells().is_none(),
            "a cursor ahead of the mirror proves the mirror missed revisions"
        );
        let (dump, _, _) = dump(&state, 64, 4);
        assert!(dump.contains("snapshot required for 1x1"), "{dump}");
    }

    #[test]
    fn drilldown_new_generation_discards_an_equal_sequence_mirror() {
        let mut state = DrilldownState::new(PtySessionId::new("pty-1"));
        state.ingest(&attached_at_in_generation(
            "attach-1",
            "pty-1",
            "life-1",
            1,
            1,
            Some(4),
        ));
        state.ingest(&snapshot_in_generation(
            "snapshot-1",
            "pty-1",
            "life-1",
            4,
            frame(&[&["old"]]),
        ));
        assert!(state.cells().is_some());

        state.begin_attach(RequestId::new("attach-2"));
        state.ingest(&attached_at_in_generation(
            "attach-2",
            "pty-1",
            "life-2",
            1,
            1,
            Some(4),
        ));

        assert!(
            state.cells().is_none(),
            "equal frame sequences from different lives cannot share a mirror"
        );
    }

    #[test]
    fn drilldown_snapshot_cannot_replace_the_attached_generation() {
        let mut state = DrilldownState::new(PtySessionId::new("pty-1"));
        state.ingest(&attached_at_in_generation(
            "attach-1",
            "pty-1",
            "life-1",
            1,
            1,
            Some(4),
        ));

        assert_eq!(
            state.ingest(&snapshot_in_generation(
                "snapshot-late",
                "pty-1",
                "life-2",
                4,
                frame(&[&["foreign"]]),
            )),
            IngestDisposition::Counted
        );
        assert_eq!(
            state.generation(),
            Some(&PtySessionGeneration::new("life-1"))
        );
        assert_eq!(state.diagnostics().foreign_references, 1);
        assert_eq!(
            state.ingest(&ServerControl::PtyStreamClosed {
                request_id: RequestId::new("attach-1"),
                generation: PtySessionGeneration::new("life-1"),
                code: gwk_domain::protocol::KernelErrorCode::SlowConsumer,
                last_seq: Some(PtyFrameSeq::new(4)),
            }),
            IngestDisposition::Applied,
            "the active generation's stream remains correlated"
        );
    }

    #[test]
    fn drilldown_invalid_snapshot_does_not_adopt_its_generation() {
        let mut state = DrilldownState::new(PtySessionId::new("pty-1"));
        let ragged = frame(&[&["a", "b"], &["c"]]);

        assert_eq!(
            state.ingest(&snapshot_in_generation(
                "snapshot-1",
                "pty-1",
                "life-invalid",
                1,
                ragged,
            )),
            IngestDisposition::Counted
        );
        assert!(state.generation().is_none());
        assert_eq!(state.diagnostics().invalid_frames, 1);
    }

    #[test]
    fn drilldown_stale_generation_pushes_cannot_mutate_or_close_the_current_life() {
        let mut state = DrilldownState::new(PtySessionId::new("pty-1"));
        state.begin_attach(RequestId::new("attach-2"));
        state.ingest(&attached_at_in_generation(
            "attach-2",
            "pty-1",
            "life-2",
            1,
            1,
            Some(4),
        ));
        state.ingest(&snapshot_in_generation(
            "snapshot-2",
            "pty-1",
            "life-2",
            4,
            frame(&[&["current"]]),
        ));

        assert_eq!(
            state.ingest(&batch_in_generation(
                "attach-1",
                "pty-1",
                "life-1",
                5,
                vec![changed(vec![(0, 0, "stale")])],
            )),
            IngestDisposition::Counted
        );
        assert_eq!(
            state.cells().expect("current mirror")[0][0].glyph,
            "current"
        );
        assert_eq!(
            state.ingest(&ServerControl::PtyStreamClosed {
                request_id: RequestId::new("attach-1"),
                generation: PtySessionGeneration::new("life-1"),
                code: gwk_domain::protocol::KernelErrorCode::SlowConsumer,
                last_seq: Some(PtyFrameSeq::new(5)),
            }),
            IngestDisposition::Counted
        );
        assert_eq!(state.diagnostics().foreign_references, 2);
        assert_eq!(
            state.ingest(&ServerControl::PtyStreamClosed {
                request_id: RequestId::new("attach-2"),
                generation: PtySessionGeneration::new("life-2"),
                code: gwk_domain::protocol::KernelErrorCode::SlowConsumer,
                last_seq: Some(PtyFrameSeq::new(4)),
            }),
            IngestDisposition::Applied,
            "the current stream remains correlated after stale pushes"
        );
    }

    #[test]
    fn drilldown_typed_stream_close_remains_visible() {
        let mut state = DrilldownState::new(PtySessionId::new("pty-1"));
        state.ingest(&attached("attach-1", "pty-1", 1, 1));
        state.ingest(&snapshot("snapshot-1", "pty-1", 4, frame(&[&["done"]])));

        assert_eq!(
            state.ingest(&ServerControl::PtyStreamClosed {
                request_id: RequestId::new("attach-1"),
                generation: PtySessionGeneration::new("life-1"),
                code: gwk_domain::protocol::KernelErrorCode::SlowConsumer,
                last_seq: Some(PtyFrameSeq::new(4)),
            }),
            IngestDisposition::Applied
        );
        let (dump, _, _) = dump(&state, 64, 4);
        assert!(dump.contains("closed slow_consumer at 4"), "{dump}");
    }

    #[test]
    fn drilldown_attach_refusal_cannot_leave_the_status_looking_live() {
        let mut state = DrilldownState::new(PtySessionId::new("pty-1"));
        state.begin_attach(RequestId::new("attach-1"));

        assert_eq!(
            state.ingest(&ServerControl::Response {
                request_id: RequestId::new("attach-1"),
                result: KernelResult::Error {
                    code: gwk_domain::protocol::KernelErrorCode::Overloaded,
                    message: "attach queue full".into(),
                    detail: None,
                },
            }),
            IngestDisposition::Applied
        );
        let (dump, _, _) = dump(&state, 64, 4);
        assert!(dump.contains("closed overloaded at -"), "{dump}");
        assert!(!dump.contains("live  SESSION"), "{dump}");
    }

    #[test]
    fn drilldown_tracked_attach_does_not_follow_another_request_for_the_same_session() {
        let mut state = DrilldownState::new(PtySessionId::new("pty-1"));
        state.begin_attach(RequestId::new("attach-1"));

        assert_eq!(
            state.ingest(&attached("attach-other", "pty-1", 1, 1)),
            IngestDisposition::Counted
        );
        assert_eq!(state.diagnostics().foreign_references, 1);

        assert_eq!(
            state.ingest(&attached("attach-1", "pty-1", 1, 1)),
            IngestDisposition::Applied
        );
        let (dump, _, _) = dump(&state, 64, 4);
        assert!(dump.contains("live  SESSION"), "{dump}");
    }

    #[test]
    fn drilldown_non_rectangular_snapshot_is_counted_and_not_painted() {
        let mut state = DrilldownState::new(PtySessionId::new("pty-1"));
        let ragged = frame(&[&["a", "b"], &["c"]]);

        assert_eq!(
            state.ingest(&snapshot("snapshot-1", "pty-1", 1, ragged)),
            IngestDisposition::Counted
        );
        assert!(state.cells().is_none());
        assert_eq!(state.diagnostics().invalid_frames, 1);
    }

    #[test]
    fn drilldown_dangling_style_indices_are_counted_not_followed() {
        // A frame whose run indexes past its table is counted as an invalid
        // frame and never painted; an update indexing past its BATCH table
        // is counted as an invalid cell and the mirror keeps its content.
        let mut state = DrilldownState::new(PtySessionId::new("pty-1"));
        let dangling = PtyFrame {
            styles: Vec::new(),
            rows: vec![vec![gwk_domain::frame::PtyRun::Cells {
                style: 0,
                glyphs: vec!["x".to_owned()],
            }]],
            cursor: None,
        };
        assert_eq!(
            state.ingest(&snapshot("snapshot-1", "pty-1", 1, dangling)),
            IngestDisposition::Counted
        );
        assert!(state.cells().is_none());
        assert_eq!(state.diagnostics().invalid_frames, 1);

        state.ingest(&attached("attach-1", "pty-1", 1, 1));
        state.ingest(&snapshot("snapshot-2", "pty-1", 2, frame(&[&["a"]])));
        assert_eq!(
            state.ingest(&batch(
                "attach-1",
                "pty-1",
                3,
                vec![PtyDelta::CellsChanged {
                    styles: vec![style()],
                    updates: vec![PtyCellUpdate {
                        row: 0,
                        col: 0,
                        glyph: "x".to_owned(),
                        style: 9,
                    }],
                }]
            )),
            IngestDisposition::Counted
        );
        assert_eq!(state.cells().expect("mirror")[0][0].glyph, "a");
        assert_eq!(state.diagnostics().invalid_updates, 1);
    }

    #[test]
    fn drilldown_passes_session_colors_and_supported_attributes_through() {
        let mut styled = cell("x");
        styled.style = CellStyle {
            bold: true,
            dim: true,
            italic: true,
            blink: true,
            inverse: true,
            invisible: true,
            strikethrough: true,
            overline: true,
            underline: Some(CellUnderline::Curly),
            fg: Some(CellColor::Truecolor { r: 1, g: 2, b: 3 }),
            bg: Some(CellColor::Xterm256 { index: 234 }),
            underline_color: Some(CellColor::Ansi16 {
                slot: PtyAnsiSlot::BrightRed,
            }),
        };
        let mut state = DrilldownState::new(PtySessionId::new("pty-1"));
        state.ingest(&snapshot(
            "snapshot-1",
            "pty-1",
            1,
            PtyFrame::from_cells(&[vec![styled]], None),
        ));

        let (dump, buf, _) = dump(&state, 64, 3);
        let painted = buf[(0, 0)].style();
        assert_eq!(painted.fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(painted.bg, Some(Color::Indexed(234)));
        assert_eq!(painted.underline_color, Some(Color::LightRed));
        for modifier in [
            Modifier::BOLD,
            Modifier::DIM,
            Modifier::ITALIC,
            Modifier::SLOW_BLINK,
            Modifier::REVERSED,
            Modifier::HIDDEN,
            Modifier::CROSSED_OUT,
            Modifier::UNDERLINED,
        ] {
            assert!(
                painted.add_modifier.contains(modifier),
                "missing {modifier:?}"
            );
        }
        assert!(
            dump.contains("2 style approximations"),
            "curly underline and overline are named, not silently dropped: {dump}"
        );
    }

    #[test]
    fn drilldown_absent_foreground_and_background_paint_as_terminal_reset() {
        let mut state = DrilldownState::new(PtySessionId::new("pty-1"));
        state.ingest(&snapshot("snapshot-1", "pty-1", 1, frame(&[&["x"]])));

        let (_, buf, _) = dump(&state, 20, 3);
        assert_eq!(buf[(0, 0)].style().fg, Some(Color::Reset));
        assert_eq!(buf[(0, 0)].style().bg, Some(Color::Reset));
    }

    #[test]
    fn drilldown_unsafe_foreign_glyphs_escape_before_the_buffer() {
        let mut state = DrilldownState::new(PtySessionId::new("pty-1"));
        state.ingest(&snapshot(
            "snapshot-1",
            "pty-1",
            1,
            frame(&[&["◆", "你", "\u{1b}", "ok"]]),
        ));

        let (dump, _, _) = dump(&state, 64, 3);
        for unsafe_glyph in ['◆', '你', '\u{1b}'] {
            assert!(
                !dump.contains(unsafe_glyph),
                "unsafe {unsafe_glyph:?}: {dump}"
            );
        }
        assert!(dump.contains("\\u{25C6}"), "{dump}");
        assert!(dump.contains("\\u{4F60}"), "{dump}");
        assert!(dump.contains("\\u{1B}"), "{dump}");
    }

    #[test]
    fn drilldown_viewport_crops_without_resizing_the_hosted_session() {
        let mut state = DrilldownState::new(PtySessionId::new("pty-1"));
        state.ingest(&snapshot(
            "snapshot-1",
            "pty-1",
            1,
            frame(&[
                &["a", "b", "c", "d"],
                &["e", "f", "g", "h"],
                &["i", "j", "k", "l"],
            ]),
        ));
        state.set_viewport(1, 1);

        let (dump, _, hits) = dump(&state, 3, 3);
        let lines: Vec<&str> = dump.lines().collect();
        assert_eq!(lines[0], "fgh");
        assert_eq!(lines[1], "jkl");
        assert_eq!(
            hits.targets().cloned().collect::<Vec<_>>(),
            target_order(&state)
        );
        assert_eq!(hits.hit(1, 2), Some(&target_order(&state)[0]));
    }
}
