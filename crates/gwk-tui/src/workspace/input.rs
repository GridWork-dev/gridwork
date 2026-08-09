//! The workspace input grammar: leader, command mode, palette.
//!
//! # The grammar, and where each piece comes from
//!
//! The workspace's resting state is **pass-through**: panes will host live
//! sessions, so nearly every key belongs to the pane's content and is handed
//! back to the caller untouched. One key is intercepted — the leader — and
//! it opens **command mode**, where a single further key selects one
//! workspace action and the mode ends.
//
// Derivation: TMUX-MANUAL §DEFAULT KEY BINDINGS — the prefix-key convention:
// one leader key, then one command key, selects one action; the mode is
// one-shot by that grammar.
//
// Derivation: ZELLIJ-PRESETS §Unlock-First (non-colliding) preset — Ctrl-g as
// the mode-entry default, the preset's own entry key, chosen there to avoid
// colliding with keys running programs expect; single-character selections
// inside the entered mode, and `n` new pane / `x` close focused pane as the
// pane-verb defaults.
//
// Derivation: ZELLIJ-KEYBINDS §keybinds — the mode-division convention:
// bindings grouped into named modes, each mode mapping keys to named actions.
//
// Derivation: POSIX-VI §EXTENDED DESCRIPTION — the command-mode /
// text-input-mode distinction the palette rides (command keys act, typed text
// is text); `h`/`j`/`k`/`l` as left/down/up/right motion; `:` beginning a
// typed command line.
//!
//! From command mode, `:` opens the **palette** — a typed command line with
//! prefix completion over this module's own command names. Palette design is
//! original work; the two command names the registry's command vocabulary
//! already states (`resize-pane`, `list-keys`) carry that lineage, the rest
//! are this model's own nouns.
//!
//! # The lineage table
//!
//! **Every default keybinding and command name here carries a lineage cell:
//! either a registry row that states the convention it adopts, or an
//! explicit `original` marker.** The table is data ([`DEFAULTS`]), so it is
//! testable, printable by the palette's `list-keys` view, and quotable in a
//! review without re-deriving it from the dispatch code.
//!
//! No configuration vocabulary ships in this change — there is no rebinding
//! surface yet. The day one lands, its words owe this same lineage
//! discipline before they ship.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;

use super::{Axis, Direction, Resize, WorkspaceState};

/// Which input mode the workspace surface is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Keys belong to the focused pane's content and are handed back to the
    /// caller. Only the leader is intercepted.
    #[default]
    Passthrough,
    /// The leader was pressed: the next key selects one workspace action.
    Command,
    /// A typed command line with prefix completion.
    Palette,
}

/// One workspace action a key or palette command selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    NewWorkspace,
    CloseWorkspace,
    NewTab,
    CloseTab,
    SplitColumns,
    SplitRows,
    ClosePane,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    NextTab,
    PreviousTab,
    NextWorkspace,
    PreviousWorkspace,
    /// Zero-based tab position.
    SelectTab(usize),
    GrowColumns,
    ShrinkColumns,
    GrowRows,
    ShrinkRows,
}

/// What handling one key produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The key belongs to the pane's content; the caller relays it.
    Forward,
    /// Consumed by the grammar (a mode change, palette typing, a refusal).
    Handled,
    /// Consumed, and this workspace action should be applied.
    Act(Action),
}

/// Apply one action to the model. `area` is the pane body — the same
/// rectangle the panes are painted into — because directional focus is
/// judged on the geometry the operator actually sees.
pub fn apply(action: Action, state: &mut WorkspaceState, area: Rect) {
    match action {
        Action::NewWorkspace => state.create_workspace(),
        Action::CloseWorkspace => state.close_workspace(),
        Action::NewTab => state.create_tab(),
        Action::CloseTab => state.close_tab(),
        Action::SplitColumns => state.split(Axis::Columns),
        Action::SplitRows => state.split(Axis::Rows),
        Action::ClosePane => state.close_pane(),
        Action::FocusLeft => state.move_focus(Direction::Left, area),
        Action::FocusRight => state.move_focus(Direction::Right, area),
        Action::FocusUp => state.move_focus(Direction::Up, area),
        Action::FocusDown => state.move_focus(Direction::Down, area),
        Action::NextTab => state.next_tab(),
        Action::PreviousTab => state.previous_tab(),
        Action::NextWorkspace => state.next_workspace(),
        Action::PreviousWorkspace => state.previous_workspace(),
        Action::SelectTab(index) => state.select_tab(index),
        Action::GrowColumns => state.resize(Axis::Columns, Resize::Grow),
        Action::ShrinkColumns => state.resize(Axis::Columns, Resize::Shrink),
        Action::GrowRows => state.resize(Axis::Rows, Resize::Grow),
        Action::ShrinkRows => state.resize(Axis::Rows, Resize::Shrink),
    }
}

/// One row of the lineage table: a default, what it does, and where it came
/// from. `lineage` is either `original …` or starts with the registry row ID
/// that states the adopted convention — a test pins that shape.
#[derive(Debug, Clone, Copy)]
pub struct BindingRow {
    /// The mode the default lives in.
    pub mode: &'static str,
    /// The key or command name, in display form.
    pub keys: &'static str,
    /// What it does.
    pub does: &'static str,
    /// The named lineage or the explicit original marker.
    pub lineage: &'static str,
}

/// Every default this module ships, with its lineage cell.
pub const DEFAULTS: &[BindingRow] = &[
    BindingRow {
        mode: "pass-through",
        keys: "Ctrl-g",
        does: "enter command mode; every other key belongs to the pane",
        lineage: "TMUX-MANUAL -- the prefix-key convention (one leader key, then one \
                  command key); ZELLIJ-PRESETS -- Ctrl-g, the non-colliding preset's \
                  own entry key",
    },
    BindingRow {
        mode: "command",
        keys: "Esc",
        does: "leave command mode without acting",
        lineage: "original",
    },
    BindingRow {
        mode: "command",
        keys: "n",
        does: "split the focused pane into columns; the new pane takes focus",
        lineage: "ZELLIJ-PRESETS -- `n` new pane; the columns-axis default is original",
    },
    BindingRow {
        mode: "command",
        keys: "N",
        does: "split the focused pane into rows; the new pane takes focus",
        lineage: "original -- the shift variant of `n`",
    },
    BindingRow {
        mode: "command",
        keys: "x",
        does: "close the focused pane",
        lineage: "ZELLIJ-PRESETS -- `x` close focused pane",
    },
    BindingRow {
        mode: "command",
        keys: "h / j / k / l",
        does: "move focus left / down / up / right",
        lineage: "POSIX-VI -- cursor motion in EXTENDED DESCRIPTION",
    },
    BindingRow {
        mode: "command",
        keys: "Left / Down / Up / Right",
        does: "move focus left / down / up / right",
        lineage: "original -- arrow synonyms for the motion keys",
    },
    BindingRow {
        mode: "command",
        keys: "H / L",
        does: "shrink / grow the focused pane's columns",
        lineage: "original -- shifted motion keys as resize",
    },
    BindingRow {
        mode: "command",
        keys: "K / J",
        does: "shrink / grow the focused pane's rows",
        lineage: "original -- shifted motion keys as resize",
    },
    BindingRow {
        mode: "command",
        keys: "t / T",
        does: "new tab / close tab",
        lineage: "original -- this model's own noun",
    },
    BindingRow {
        mode: "command",
        keys: "w / W",
        does: "new workspace / close workspace",
        lineage: "original -- this model's own noun",
    },
    BindingRow {
        mode: "command",
        keys: "] / [",
        does: "next / previous tab",
        lineage: "original",
    },
    BindingRow {
        mode: "command",
        keys: "} / {",
        does: "next / previous workspace",
        lineage: "original",
    },
    BindingRow {
        mode: "command",
        keys: "1-9",
        does: "select tab by position",
        lineage: "original",
    },
    BindingRow {
        mode: "command",
        keys: ":",
        does: "open the palette",
        lineage: "POSIX-VI -- `:` begins a typed command line",
    },
    BindingRow {
        mode: "palette",
        keys: "typed text; Tab completes; Enter runs; Esc closes",
        does: "prefix completion over the command names below",
        lineage: "original -- palette design carries no lineage claim",
    },
    BindingRow {
        mode: "palette",
        keys: "new-workspace / close-workspace / new-tab / close-tab",
        does: "the container verbs",
        lineage: "original -- this model's own nouns",
    },
    BindingRow {
        mode: "palette",
        keys: "split-pane / split-pane-rows / close-pane",
        does: "the pane verbs",
        lineage: "original -- this model's own nouns",
    },
    BindingRow {
        mode: "palette",
        keys: "focus-left / focus-right / focus-up / focus-down",
        does: "directional focus",
        lineage: "original -- this model's own nouns",
    },
    BindingRow {
        mode: "palette",
        keys: "next-tab / previous-tab / next-workspace / previous-workspace",
        does: "cyclic navigation",
        lineage: "original -- this model's own nouns",
    },
    BindingRow {
        mode: "palette",
        keys: "select-tab <n>",
        does: "select tab by one-based position",
        lineage: "original -- this model's own noun",
    },
    BindingRow {
        mode: "palette",
        keys: "resize-pane grow-columns | shrink-columns | grow-rows | shrink-rows",
        does: "one resize step along an axis",
        lineage: "TMUX-MANUAL -- `resize-pane` is in the named command vocabulary; \
                  the argument words are original",
    },
    BindingRow {
        mode: "palette",
        keys: "list-keys",
        does: "show this table",
        lineage: "TMUX-MANUAL -- `list-keys` is in the named command vocabulary",
    },
];

/// One palette command: its full display name and what it resolves to.
struct PaletteEntry {
    name: &'static str,
    action: Option<Action>,
}

/// `select-tab` and `list-keys` are special-cased in [`Palette::resolve`]:
/// the first takes an argument, the second switches the palette's view.
const PALETTE_ENTRIES: &[PaletteEntry] = &[
    PaletteEntry {
        name: "close-pane",
        action: Some(Action::ClosePane),
    },
    PaletteEntry {
        name: "close-tab",
        action: Some(Action::CloseTab),
    },
    PaletteEntry {
        name: "close-workspace",
        action: Some(Action::CloseWorkspace),
    },
    PaletteEntry {
        name: "focus-down",
        action: Some(Action::FocusDown),
    },
    PaletteEntry {
        name: "focus-left",
        action: Some(Action::FocusLeft),
    },
    PaletteEntry {
        name: "focus-right",
        action: Some(Action::FocusRight),
    },
    PaletteEntry {
        name: "focus-up",
        action: Some(Action::FocusUp),
    },
    PaletteEntry {
        name: "list-keys",
        action: None,
    },
    PaletteEntry {
        name: "new-tab",
        action: Some(Action::NewTab),
    },
    PaletteEntry {
        name: "new-workspace",
        action: Some(Action::NewWorkspace),
    },
    PaletteEntry {
        name: "next-tab",
        action: Some(Action::NextTab),
    },
    PaletteEntry {
        name: "next-workspace",
        action: Some(Action::NextWorkspace),
    },
    PaletteEntry {
        name: "previous-tab",
        action: Some(Action::PreviousTab),
    },
    PaletteEntry {
        name: "previous-workspace",
        action: Some(Action::PreviousWorkspace),
    },
    PaletteEntry {
        name: "resize-pane grow-columns",
        action: Some(Action::GrowColumns),
    },
    PaletteEntry {
        name: "resize-pane grow-rows",
        action: Some(Action::GrowRows),
    },
    PaletteEntry {
        name: "resize-pane shrink-columns",
        action: Some(Action::ShrinkColumns),
    },
    PaletteEntry {
        name: "resize-pane shrink-rows",
        action: Some(Action::ShrinkRows),
    },
    PaletteEntry {
        name: "select-tab",
        action: None,
    },
    PaletteEntry {
        name: "split-pane",
        action: Some(Action::SplitColumns),
    },
    PaletteEntry {
        name: "split-pane-rows",
        action: Some(Action::SplitRows),
    },
];

/// What the palette is currently showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteView {
    /// The typed command line and its candidates.
    Input,
    /// The `list-keys` view: the [`DEFAULTS`] table.
    Keys,
}

/// The palette's state while [`Mode::Palette`] is active.
#[derive(Debug, Clone, Default)]
pub struct Palette {
    input: String,
    view: Option<PaletteView>,
    notice: Option<String>,
}

impl Palette {
    /// The typed line, for painting.
    pub fn input(&self) -> &str {
        &self.input
    }

    /// The current view; `None` when the palette is closed.
    pub fn view(&self) -> Option<PaletteView> {
        self.view
    }

    /// The refusal to show, if the last Enter could not run.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Command names matching the typed prefix, in list order. An empty
    /// line matches everything: the palette doubles as the command list.
    pub fn candidates(&self) -> impl Iterator<Item = &'static str> + '_ {
        PALETTE_ENTRIES
            .iter()
            .map(|entry| entry.name)
            .filter(|name| name.starts_with(self.input.trim_start()))
    }

    fn open(&mut self) {
        self.input.clear();
        self.view = Some(PaletteView::Input);
        self.notice = None;
    }

    fn close(&mut self) {
        self.input.clear();
        self.view = None;
        self.notice = None;
    }

    /// Complete the line to the first candidate.
    fn complete(&mut self) {
        let first: Option<&'static str> = self.candidates().next();
        if let Some(first) = first {
            self.input = first.to_owned();
        }
    }

    /// Resolve the typed line. `Ok(Some(action))` runs it, `Ok(None)` means
    /// the line switched the view (`list-keys`), `Err` is the refusal to
    /// show — a stated no, never a silent one.
    fn resolve(&self) -> Result<Option<Action>, String> {
        let line = self.input.trim();
        if line.is_empty() {
            return Err("type a command name".to_owned());
        }
        if line == "list-keys" {
            return Ok(None);
        }
        if let Some(rest) = line.strip_prefix("select-tab") {
            let argument = rest.trim();
            if argument.is_empty() {
                return Err("select-tab needs a one-based tab number".to_owned());
            }
            return match argument.parse::<usize>() {
                Ok(position) if position >= 1 => Ok(Some(Action::SelectTab(position - 1))),
                _ => Err(format!("{argument} is not a one-based tab number")),
            };
        }
        if let Some(entry) = PALETTE_ENTRIES.iter().find(|entry| entry.name == line) {
            // Every remaining entry resolves to an action; `list-keys` and
            // `select-tab` were handled above.
            return Ok(entry.action);
        }
        let mut matches = PALETTE_ENTRIES
            .iter()
            .filter(|entry| entry.name.starts_with(line));
        match (matches.next(), matches.next()) {
            (Some(only), None) => {
                if only.name == "select-tab" {
                    Err("select-tab needs a one-based tab number".to_owned())
                } else if only.name == "list-keys" {
                    Ok(None)
                } else {
                    Ok(only.action)
                }
            }
            (Some(_), Some(_)) => Err(format!("{line}... is ambiguous -- keep typing or Tab")),
            (None, _) => Err(format!("no such command: {line}")),
        }
    }
}

/// The workspace input state machine.
#[derive(Debug, Clone, Default)]
pub struct InputState {
    mode: Mode,
    palette: Palette,
}

impl InputState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The mode, for the status indicator.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The palette, for painting. Live only while the mode is
    /// [`Mode::Palette`].
    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    /// Handle one key event.
    pub fn handle(&mut self, key: KeyEvent) -> Outcome {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            // Release events carry no intent here; in pass-through they still
            // belong to the pane's owner.
            return match self.mode {
                Mode::Passthrough => Outcome::Forward,
                _ => Outcome::Handled,
            };
        }
        match self.mode {
            Mode::Passthrough => self.handle_passthrough(key),
            Mode::Command => self.handle_command(key),
            Mode::Palette => self.handle_palette(key),
        }
    }

    fn handle_passthrough(&mut self, key: KeyEvent) -> Outcome {
        if key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.mode = Mode::Command;
            return Outcome::Handled;
        }
        Outcome::Forward
    }

    fn handle_command(&mut self, key: KeyEvent) -> Outcome {
        // One leader, one command key: the mode ends with this keypress
        // whatever it selects, so a stray key cannot leave the surface
        // half-armed. `:` is the one exception — it opens the palette.
        self.mode = Mode::Passthrough;
        if key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::ALT)
        {
            // No command default carries a modifier; a chorded key is not a
            // command, and swallowing it here would hide a typo.
            return Outcome::Handled;
        }
        let action = match key.code {
            KeyCode::Esc => return Outcome::Handled,
            KeyCode::Char(':') => {
                self.mode = Mode::Palette;
                self.palette.open();
                return Outcome::Handled;
            }
            KeyCode::Char('n') => Action::SplitColumns,
            KeyCode::Char('N') => Action::SplitRows,
            KeyCode::Char('x') => Action::ClosePane,
            KeyCode::Char('h') | KeyCode::Left => Action::FocusLeft,
            KeyCode::Char('j') | KeyCode::Down => Action::FocusDown,
            KeyCode::Char('k') | KeyCode::Up => Action::FocusUp,
            KeyCode::Char('l') | KeyCode::Right => Action::FocusRight,
            KeyCode::Char('H') => Action::ShrinkColumns,
            KeyCode::Char('L') => Action::GrowColumns,
            KeyCode::Char('K') => Action::ShrinkRows,
            KeyCode::Char('J') => Action::GrowRows,
            KeyCode::Char('t') => Action::NewTab,
            KeyCode::Char('T') => Action::CloseTab,
            KeyCode::Char('w') => Action::NewWorkspace,
            KeyCode::Char('W') => Action::CloseWorkspace,
            KeyCode::Char(']') => Action::NextTab,
            KeyCode::Char('[') => Action::PreviousTab,
            KeyCode::Char('}') => Action::NextWorkspace,
            KeyCode::Char('{') => Action::PreviousWorkspace,
            KeyCode::Char(digit @ '1'..='9') => {
                // `to_digit` cannot fail inside this arm's range.
                let position = digit.to_digit(10).unwrap_or(1) as usize;
                Action::SelectTab(position - 1)
            }
            // An unmapped key cancels the mode rather than leaking into the
            // pane: the operator pressed it believing a command would run.
            _ => return Outcome::Handled,
        };
        Outcome::Act(action)
    }

    fn handle_palette(&mut self, key: KeyEvent) -> Outcome {
        if self.palette.view() == Some(PaletteView::Keys) {
            // The keys view is read-only: any key closes it.
            self.palette.close();
            self.mode = Mode::Passthrough;
            return Outcome::Handled;
        }
        match key.code {
            KeyCode::Esc => {
                self.palette.close();
                self.mode = Mode::Passthrough;
                Outcome::Handled
            }
            KeyCode::Tab => {
                self.palette.complete();
                Outcome::Handled
            }
            KeyCode::Backspace => {
                self.palette.input.pop();
                self.palette.notice = None;
                Outcome::Handled
            }
            KeyCode::Enter => match self.palette.resolve() {
                Ok(Some(action)) => {
                    self.palette.close();
                    self.mode = Mode::Passthrough;
                    Outcome::Act(action)
                }
                Ok(None) => {
                    self.palette.view = Some(PaletteView::Keys);
                    self.palette.notice = None;
                    Outcome::Handled
                }
                Err(notice) => {
                    self.palette.notice = Some(notice);
                    Outcome::Handled
                }
            },
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.palette.input.push(c);
                self.palette.notice = None;
                Outcome::Handled
            }
            _ => Outcome::Handled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 24,
    };

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press_with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn leader() -> KeyEvent {
        press_with(KeyCode::Char('g'), KeyModifiers::CONTROL)
    }

    fn type_line(input: &mut InputState, line: &str) {
        for c in line.chars() {
            assert_eq!(input.handle(press(KeyCode::Char(c))), Outcome::Handled);
        }
    }

    #[test]
    fn input_every_lineage_cell_is_a_registry_row_or_original() {
        // The four rows live in docs/derivation/SPECS.md; a cell either
        // adopts one of them by name or says `original`. Nothing else — and
        // never the name this predicate seeds red below.
        fn cell_ok(lineage: &str) -> bool {
            let lower = lineage.to_lowercase();
            if lower.contains("screen") {
                return false;
            }
            lineage.starts_with("original")
                || [
                    "TMUX-MANUAL",
                    "ZELLIJ-KEYBINDS",
                    "ZELLIJ-PRESETS",
                    "POSIX-VI",
                ]
                .iter()
                .any(|row| lineage.starts_with(row))
        }
        for row in DEFAULTS {
            assert!(
                cell_ok(row.lineage),
                "{} `{}` carries an unresolvable lineage cell: {}",
                row.mode,
                row.keys,
                row.lineage
            );
        }
        // The predicate itself is proven able to fail — a gate that cannot
        // go red is a defect.
        assert!(!cell_ok("SCREEN-MANUAL — anything"));
        assert!(!cell_ok("some prose with no marker"));
    }

    #[test]
    fn input_passthrough_forwards_everything_but_the_leader() {
        let mut input = InputState::new();
        for key in [
            press(KeyCode::Char('q')),
            press(KeyCode::Char(':')),
            press(KeyCode::Esc),
            press(KeyCode::Enter),
            press_with(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            assert_eq!(input.handle(key), Outcome::Forward, "{key:?}");
            assert_eq!(input.mode(), Mode::Passthrough);
        }
        assert_eq!(input.handle(leader()), Outcome::Handled);
        assert_eq!(input.mode(), Mode::Command);
    }

    #[test]
    fn input_the_command_mode_is_one_shot() {
        let mut input = InputState::new();
        input.handle(leader());
        assert_eq!(
            input.handle(press(KeyCode::Char('n'))),
            Outcome::Act(Action::SplitColumns)
        );
        assert_eq!(
            input.mode(),
            Mode::Passthrough,
            "one leader, one command key — then the keys belong to the pane again"
        );
    }

    #[test]
    fn input_every_command_default_selects_its_tabled_action() {
        let cases: &[(KeyEvent, Action)] = &[
            (press(KeyCode::Char('n')), Action::SplitColumns),
            (press(KeyCode::Char('N')), Action::SplitRows),
            (press(KeyCode::Char('x')), Action::ClosePane),
            (press(KeyCode::Char('h')), Action::FocusLeft),
            (press(KeyCode::Char('j')), Action::FocusDown),
            (press(KeyCode::Char('k')), Action::FocusUp),
            (press(KeyCode::Char('l')), Action::FocusRight),
            (press(KeyCode::Left), Action::FocusLeft),
            (press(KeyCode::Down), Action::FocusDown),
            (press(KeyCode::Up), Action::FocusUp),
            (press(KeyCode::Right), Action::FocusRight),
            (press(KeyCode::Char('H')), Action::ShrinkColumns),
            (press(KeyCode::Char('L')), Action::GrowColumns),
            (press(KeyCode::Char('K')), Action::ShrinkRows),
            (press(KeyCode::Char('J')), Action::GrowRows),
            (press(KeyCode::Char('t')), Action::NewTab),
            (press(KeyCode::Char('T')), Action::CloseTab),
            (press(KeyCode::Char('w')), Action::NewWorkspace),
            (press(KeyCode::Char('W')), Action::CloseWorkspace),
            (press(KeyCode::Char(']')), Action::NextTab),
            (press(KeyCode::Char('[')), Action::PreviousTab),
            (press(KeyCode::Char('}')), Action::NextWorkspace),
            (press(KeyCode::Char('{')), Action::PreviousWorkspace),
            (press(KeyCode::Char('1')), Action::SelectTab(0)),
            (press(KeyCode::Char('9')), Action::SelectTab(8)),
        ];
        for (key, expected) in cases {
            let mut input = InputState::new();
            input.handle(leader());
            assert_eq!(input.handle(*key), Outcome::Act(*expected), "{key:?}");
            assert_eq!(input.mode(), Mode::Passthrough, "{key:?} must end the mode");
        }
    }

    #[test]
    fn input_esc_unknown_and_chorded_keys_cancel_without_leaking() {
        for key in [
            press(KeyCode::Esc),
            press(KeyCode::Char('z')),
            press(KeyCode::Enter),
            press_with(KeyCode::Char('n'), KeyModifiers::CONTROL),
            press_with(KeyCode::Char('x'), KeyModifiers::ALT),
        ] {
            let mut input = InputState::new();
            input.handle(leader());
            assert_eq!(
                input.handle(key),
                Outcome::Handled,
                "{key:?} is consumed, never forwarded into the pane"
            );
            assert_eq!(input.mode(), Mode::Passthrough);
        }
    }

    #[test]
    fn input_the_palette_completes_and_runs_a_typed_command() {
        let mut input = InputState::new();
        input.handle(leader());
        input.handle(press(KeyCode::Char(':')));
        assert_eq!(input.mode(), Mode::Palette);

        type_line(&mut input, "split-pane-r");
        assert_eq!(input.handle(press(KeyCode::Tab)), Outcome::Handled);
        assert_eq!(input.palette().input(), "split-pane-rows");
        assert_eq!(
            input.handle(press(KeyCode::Enter)),
            Outcome::Act(Action::SplitRows)
        );
        assert_eq!(input.mode(), Mode::Passthrough);
    }

    #[test]
    fn input_a_unique_prefix_runs_without_completion() {
        let mut input = InputState::new();
        input.handle(leader());
        input.handle(press(KeyCode::Char(':')));
        type_line(&mut input, "new-w");
        assert_eq!(
            input.handle(press(KeyCode::Enter)),
            Outcome::Act(Action::NewWorkspace)
        );
    }

    #[test]
    fn input_palette_refusals_are_stated_and_keep_the_palette_open() {
        let mut input = InputState::new();
        input.handle(leader());
        input.handle(press(KeyCode::Char(':')));

        // Ambiguous prefix.
        type_line(&mut input, "close-");
        assert_eq!(input.handle(press(KeyCode::Enter)), Outcome::Handled);
        assert!(
            input
                .palette()
                .notice()
                .expect("a refusal is stated")
                .contains("ambiguous")
        );
        assert_eq!(
            input.mode(),
            Mode::Palette,
            "a refusal never closes the line"
        );

        // Unknown command.
        for _ in 0.."close-".len() {
            input.handle(press(KeyCode::Backspace));
        }
        type_line(&mut input, "detach");
        assert_eq!(input.handle(press(KeyCode::Enter)), Outcome::Handled);
        assert!(
            input
                .palette()
                .notice()
                .expect("a refusal is stated")
                .contains("no such command")
        );

        // A typed character clears the stale notice.
        input.handle(press(KeyCode::Backspace));
        assert!(input.palette().notice().is_none());
    }

    #[test]
    fn input_select_tab_takes_a_one_based_argument() {
        let mut input = InputState::new();
        input.handle(leader());
        input.handle(press(KeyCode::Char(':')));
        type_line(&mut input, "select-tab 3");
        assert_eq!(
            input.handle(press(KeyCode::Enter)),
            Outcome::Act(Action::SelectTab(2))
        );

        for (line, wanted) in [
            ("select-tab", "needs a one-based tab number"),
            ("select-tab 0", "not a one-based tab number"),
            ("select-tab x", "not a one-based tab number"),
        ] {
            let mut fresh = InputState::new();
            fresh.handle(leader());
            fresh.handle(press(KeyCode::Char(':')));
            type_line(&mut fresh, line);
            assert_eq!(fresh.handle(press(KeyCode::Enter)), Outcome::Handled);
            assert!(
                fresh
                    .palette()
                    .notice()
                    .expect("a refusal is stated")
                    .contains(wanted),
                "{line:?}"
            );
        }
    }

    #[test]
    fn input_list_keys_shows_the_table_and_any_key_closes_it() {
        let mut input = InputState::new();
        input.handle(leader());
        input.handle(press(KeyCode::Char(':')));
        type_line(&mut input, "list-keys");
        assert_eq!(input.handle(press(KeyCode::Enter)), Outcome::Handled);
        assert_eq!(input.palette().view(), Some(PaletteView::Keys));
        assert_eq!(input.mode(), Mode::Palette);

        assert_eq!(input.handle(press(KeyCode::Char('q'))), Outcome::Handled);
        assert_eq!(input.mode(), Mode::Passthrough);
        assert_eq!(input.palette().view(), None);
    }

    #[test]
    fn input_the_empty_line_lists_every_command() {
        let mut input = InputState::new();
        input.handle(leader());
        input.handle(press(KeyCode::Char(':')));
        assert_eq!(
            input.palette().candidates().count(),
            PALETTE_ENTRIES.len(),
            "an empty line doubles as the command list"
        );
    }

    #[test]
    fn input_apply_maps_every_action_onto_the_model() {
        let mut state = WorkspaceState::new();
        apply(Action::SplitColumns, &mut state, AREA);
        assert_eq!(state.active_tab().expect("tab").pane_count(), 2);
        apply(Action::SplitRows, &mut state, AREA);
        assert_eq!(state.active_tab().expect("tab").pane_count(), 3);
        apply(Action::FocusUp, &mut state, AREA);
        apply(Action::ClosePane, &mut state, AREA);
        assert_eq!(state.active_tab().expect("tab").pane_count(), 2);
        apply(Action::GrowColumns, &mut state, AREA);
        apply(Action::ShrinkColumns, &mut state, AREA);
        apply(Action::GrowRows, &mut state, AREA);
        apply(Action::ShrinkRows, &mut state, AREA);

        apply(Action::NewTab, &mut state, AREA);
        apply(Action::PreviousTab, &mut state, AREA);
        apply(Action::NextTab, &mut state, AREA);
        assert_eq!(state.active_workspace().expect("ws").tab_count(), 2);
        apply(Action::SelectTab(0), &mut state, AREA);
        assert_eq!(state.active_workspace().expect("ws").active_index(), 0);
        apply(Action::CloseTab, &mut state, AREA);
        assert_eq!(state.active_workspace().expect("ws").tab_count(), 1);

        apply(Action::NewWorkspace, &mut state, AREA);
        assert_eq!(state.workspace_count(), 2);
        apply(Action::PreviousWorkspace, &mut state, AREA);
        apply(Action::NextWorkspace, &mut state, AREA);
        apply(Action::CloseWorkspace, &mut state, AREA);
        assert_eq!(state.workspace_count(), 1);
    }

    #[test]
    fn input_release_events_carry_no_intent() {
        let mut input = InputState::new();
        let mut release = leader();
        release.kind = KeyEventKind::Release;
        assert_eq!(
            input.handle(release),
            Outcome::Forward,
            "a release in pass-through still belongs to the pane's owner"
        );
        assert_eq!(input.mode(), Mode::Passthrough);

        input.handle(leader());
        let mut released_n = press(KeyCode::Char('n'));
        released_n.kind = KeyEventKind::Release;
        assert_eq!(input.handle(released_n), Outcome::Handled);
        assert_eq!(
            input.mode(),
            Mode::Command,
            "a release does not spend the mode"
        );
    }
}
