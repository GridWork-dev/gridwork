//! Painting the workspace's own furniture.
//!
//! Derivation: none — furniture painting only: chrome-role styling, border,
//! title, tab-strip and status cells written into a ratatui buffer, plus
//! hit-region registration for the mouse accelerator. No process is spawned,
//! no terminal byte is parsed or emitted (every cell goes through the
//! library's buffer), and no colour/SGR or keyboard-protocol fact is
//! asserted.
//!
//! Everything painted here is chrome in [`crate::chrome`]'s sense — the
//! workspace's own furniture, carrying no claim about a running system — so
//! every style resolves through a [`ChromeTheme`], the one surface a user
//! remap reaches. Pane *content* is deliberately not painted: panes are
//! structural leaves until the elements that bind sessions to them land, and
//! an empty interior is that fact stated rather than decorated over.
//!
//! The frame is three bands: a header row (workspace label, then the tab
//! strip), the pane body, and a one-row status line. Every mouse target
//! registered here — a tab, a pane — has a model operation behind it, so the
//! mouse stays an accelerator and never the only path.

use gwk_theme::ColorTier;
use gwk_theme::marks::GlyphSet;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::{PaneId, WorkspaceState};
use crate::chrome::{ChromeRole, ChromeTheme};
use crate::input::HitMap;

/// What a click on the workspace surface can land on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceTarget {
    /// A tab in the strip, by position — feeds
    /// [`WorkspaceState::select_tab`].
    Tab(usize),
    /// A pane's cells, border included — feeds
    /// [`WorkspaceState::focus_pane`].
    Pane(PaneId),
}

struct BorderGlyphs {
    horizontal: &'static str,
    vertical: &'static str,
    top_left: &'static str,
    top_right: &'static str,
    bottom_left: &'static str,
    bottom_right: &'static str,
}

const UNICODE_BORDER: BorderGlyphs = BorderGlyphs {
    horizontal: "─",
    vertical: "│",
    top_left: "┌",
    top_right: "┐",
    bottom_left: "└",
    bottom_right: "┘",
};

const ASCII_BORDER: BorderGlyphs = BorderGlyphs {
    horizontal: "-",
    vertical: "|",
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
};

/// Paint the workspace surface into `area` and register its click targets.
pub fn render(
    area: Rect,
    buf: &mut Buffer,
    state: &WorkspaceState,
    tier: ColorTier,
    glyphs: GlyphSet,
    chrome: &ChromeTheme,
    hits: &mut HitMap<WorkspaceTarget>,
) {
    hits.clear();
    if area.width == 0 || area.height == 0 {
        return;
    }

    let Some(workspace) = state.active_workspace() else {
        // The empty surface is a fact, not a failure; one quiet line says it.
        buf.set_stringn(
            area.x,
            area.y,
            "no workspaces open",
            area.width as usize,
            chrome.style(ChromeRole::StatusBar, tier),
        );
        return;
    };

    header(area, buf, workspace, tier, chrome, hits);

    let body = Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height.saturating_sub(2),
    );
    if let Some(tab) = workspace.active_tab()
        && body.height > 0
    {
        let border = match glyphs {
            GlyphSet::Unicode => &UNICODE_BORDER,
            GlyphSet::Ascii => &ASCII_BORDER,
        };
        for (id, rect) in tab.pane_rects(body) {
            if rect.width == 0 || rect.height == 0 {
                continue;
            }
            let role = if id == tab.focus() {
                ChromeRole::PaneBorderFocused
            } else {
                ChromeRole::PaneBorder
            };
            draw_border(buf, rect, chrome.style(role, tier), border);
            if rect.width > 2 {
                buf.set_stringn(
                    rect.x + 1,
                    rect.y,
                    id.to_string(),
                    (rect.width - 2) as usize,
                    chrome.style(ChromeRole::PaneTitle, tier),
                );
            }
            hits.register(rect, WorkspaceTarget::Pane(id));
        }
    }

    if area.height >= 2 {
        status(area, buf, state, workspace, tier, chrome);
    }
}

fn header(
    area: Rect,
    buf: &mut Buffer,
    workspace: &super::Workspace,
    tier: ColorTier,
    chrome: &ChromeTheme,
    hits: &mut HitMap<WorkspaceTarget>,
) {
    let y = area.y;
    let right = area.x + area.width;
    let label = format!("ws {}", workspace.name());
    buf.set_stringn(
        area.x,
        y,
        &label,
        (right - area.x) as usize,
        chrome.style(ChromeRole::WorkspaceLabel, tier),
    );
    // Labels and titles are minted ordinals, so byte length is cell width.
    let mut x = (area.x + label.len() as u16 + 2).min(right);
    for (index, tab) in workspace.tabs().enumerate() {
        if x >= right {
            break;
        }
        let role = if index == workspace.active_index() {
            ChromeRole::TabActive
        } else {
            ChromeRole::TabInactive
        };
        let width = (tab.title().len() as u16).min(right - x);
        buf.set_stringn(x, y, tab.title(), width as usize, chrome.style(role, tier));
        hits.register(Rect::new(x, y, width, 1), WorkspaceTarget::Tab(index));
        x = x.saturating_add(width + 1);
    }
}

fn status(
    area: Rect,
    buf: &mut Buffer,
    state: &WorkspaceState,
    workspace: &super::Workspace,
    tier: ColorTier,
    chrome: &ChromeTheme,
) {
    let panes = workspace
        .active_tab()
        .map(super::Tab::pane_count)
        .unwrap_or_default();
    let text = format!(
        "ws {}/{}  tab {}/{}  {} pane{}",
        state.active_index() + 1,
        state.workspace_count(),
        workspace.active_index() + 1,
        workspace.tab_count(),
        panes,
        if panes == 1 { "" } else { "s" },
    );
    buf.set_stringn(
        area.x,
        area.y + area.height - 1,
        &text,
        area.width as usize,
        chrome.style(ChromeRole::StatusBar, tier),
    );
}

fn draw_border(buf: &mut Buffer, rect: Rect, style: Style, glyphs: &BorderGlyphs) {
    let x0 = rect.x;
    let y0 = rect.y;
    let x1 = rect.x + rect.width - 1;
    let y1 = rect.y + rect.height - 1;
    for x in x0..=x1 {
        buf.set_string(x, y0, glyphs.horizontal, style);
        buf.set_string(x, y1, glyphs.horizontal, style);
    }
    for y in y0..=y1 {
        buf.set_string(x0, y, glyphs.vertical, style);
        buf.set_string(x1, y, glyphs.vertical, style);
    }
    // Corners last, so a one-cell-wide or one-cell-tall pane still reads as
    // bounded rather than as a stray rule.
    buf.set_string(x0, y0, glyphs.top_left, style);
    buf.set_string(x1, y0, glyphs.top_right, style);
    buf.set_string(x0, y1, glyphs.bottom_left, style);
    buf.set_string(x1, y1, glyphs.bottom_right, style);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::token_style;
    use crate::workspace::{Axis, WorkspaceState};

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 40,
        height: 12,
    };

    fn painted(state: &WorkspaceState, glyphs: GlyphSet) -> (Buffer, HitMap<WorkspaceTarget>) {
        let mut buf = Buffer::empty(AREA);
        let mut hits = HitMap::new();
        render(
            AREA,
            &mut buf,
            state,
            ColorTier::Truecolor,
            glyphs,
            &ChromeTheme::signal(),
            &mut hits,
        );
        (buf, hits)
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
    }

    fn chrome_style(role: ChromeRole) -> Style {
        token_style(ChromeTheme::signal().token(role), ColorTier::Truecolor)
    }

    #[test]
    fn render_the_opening_frame_carries_label_tab_border_and_status() {
        let state = WorkspaceState::new();
        let (buf, hits) = painted(&state, GlyphSet::Unicode);

        assert!(
            row(&buf, 0).starts_with("ws 1  1"),
            "header: label then strip"
        );
        assert!(
            row(&buf, 1).starts_with("┌p1"),
            "the pane border opens on the body's first row, titled"
        );
        assert!(
            row(&buf, 10).starts_with("└"),
            "the border closes above status"
        );
        assert!(row(&buf, 11).starts_with("ws 1/1  tab 1/1  1 pane"));

        // The single pane is focused, so its border carries the focused
        // role. Foregrounds compared, because that is all a chrome style
        // sets — the buffer cell keeps its own background.
        assert_eq!(
            buf[(0, 1)].style().fg,
            chrome_style(ChromeRole::PaneBorderFocused).fg
        );
        assert_eq!(
            buf[(1, 1)].style().fg,
            chrome_style(ChromeRole::PaneTitle).fg
        );
        assert_eq!(
            buf[(0, 0)].style().fg,
            chrome_style(ChromeRole::WorkspaceLabel).fg
        );
        assert_eq!(
            buf[(0, 11)].style().fg,
            chrome_style(ChromeRole::StatusBar).fg
        );

        // Both target kinds are clickable where they were painted.
        assert!(matches!(hits.hit(6, 0), Some(WorkspaceTarget::Tab(0))));
        assert!(matches!(hits.hit(5, 5), Some(WorkspaceTarget::Pane(_))));
    }

    #[test]
    fn render_only_the_focused_pane_wears_the_focused_border() {
        let mut state = WorkspaceState::new();
        state.split(Axis::Columns);
        let (buf, _) = painted(&state, GlyphSet::Unicode);

        // Focus sits in the right pane; the left border row is unfocused.
        assert_eq!(
            buf[(0, 1)].style().fg,
            chrome_style(ChromeRole::PaneBorder).fg
        );
        assert_eq!(
            buf[(20, 1)].style().fg,
            chrome_style(ChromeRole::PaneBorderFocused).fg,
            "the right pane opens at the split boundary"
        );
    }

    #[test]
    fn render_a_click_target_drives_the_matching_model_operation() {
        let mut state = WorkspaceState::new();
        let first = state.active_tab().expect("tab").focus();
        state.split(Axis::Columns);
        state.create_tab();
        state.select_tab(0);
        let (_, hits) = painted(&state, GlyphSet::Unicode);

        // Click the left pane: the target names a pane the model will focus.
        let Some(&WorkspaceTarget::Pane(id)) = hits.hit(3, 4) else {
            panic!("no pane target under the left pane's cells");
        };
        assert_eq!(id, first);
        assert!(state.focus_pane(id));
        assert_eq!(state.active_tab().expect("tab").focus(), first);

        // Click the second tab: the target names its strip position.
        let Some(&WorkspaceTarget::Tab(index)) = hits.hit(8, 0) else {
            panic!("no tab target under the strip");
        };
        assert_eq!(index, 1);
        state.select_tab(index);
        assert_eq!(state.active_workspace().expect("ws").active_index(), 1);
    }

    #[test]
    fn render_the_ascii_glyph_set_paints_no_wide_glyph() {
        let mut state = WorkspaceState::new();
        state.split(Axis::Rows);
        let (buf, _) = painted(&state, GlyphSet::Ascii);
        for y in 0..AREA.height {
            for x in 0..AREA.width {
                let symbol = buf[(x, y)].symbol();
                assert!(
                    symbol.is_ascii(),
                    "({x},{y}) paints {symbol:?} under the ascii glyph set"
                );
            }
        }
        assert!(
            row(&buf, 1).starts_with("+p1"),
            "borders fall back to ascii"
        );
    }

    #[test]
    fn render_the_empty_surface_says_so_and_registers_nothing() {
        let mut state = WorkspaceState::new();
        state.close_workspace();
        let (buf, hits) = painted(&state, GlyphSet::Unicode);
        assert!(row(&buf, 0).starts_with("no workspaces open"));
        assert_eq!(hits.targets().count(), 0);
    }

    #[test]
    fn render_a_zero_area_paints_and_registers_nothing() {
        let state = WorkspaceState::new();
        let mut buf = Buffer::empty(Rect::new(0, 0, 0, 0));
        let mut hits = HitMap::new();
        render(
            Rect::new(0, 0, 0, 0),
            &mut buf,
            &state,
            ColorTier::Truecolor,
            GlyphSet::Unicode,
            &ChromeTheme::signal(),
            &mut hits,
        );
        assert_eq!(hits.targets().count(), 0);
    }

    #[test]
    fn render_a_user_remap_reaches_the_furniture() {
        let mut chrome = ChromeTheme::signal();
        chrome
            .bind(ChromeRole::PaneBorderFocused, "warn")
            .expect("bind");
        let state = WorkspaceState::new();
        let mut buf = Buffer::empty(AREA);
        let mut hits = HitMap::new();
        render(
            AREA,
            &mut buf,
            &state,
            ColorTier::Truecolor,
            GlyphSet::Unicode,
            &chrome,
            &mut hits,
        );
        assert_eq!(
            buf[(0, 1)].style().fg,
            chrome
                .style(ChromeRole::PaneBorderFocused, ColorTier::Truecolor)
                .fg,
            "the focused border paints the remapped token"
        );
        assert_ne!(
            buf[(0, 1)].style().fg,
            chrome_style(ChromeRole::PaneBorderFocused).fg,
            "and the remap is visible against the Signal default"
        );
    }
}
