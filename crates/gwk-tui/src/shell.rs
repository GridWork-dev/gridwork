//! Shared state and chrome for the five-lens console.
//!
//! The runtime owns I/O and projection refreshes. This module owns the flat
//! navigation grammar and pure terminal paint so every deep link enters the
//! same interaction model.

use std::collections::BTreeSet;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use gwk_domain::entity::{Attempt, Budget, Gate, PtySession};
use gwk_domain::ids::{AttemptId, CostMicros, PtySessionId, Seq};
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::board::BoardView;
use crate::console::tier_badge;
use crate::theme;

const MIN_WIDTH: u16 = 80;
const MIN_HEIGHT: u16 = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lens {
    Hall,
    Work,
    Fleet,
    Flow,
    Term,
}

impl Lens {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hall => "hall",
            Self::Work => "work",
            Self::Fleet => "fleet",
            Self::Flow => "flow",
            Self::Term => "term",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Surface {
    Hall,
    WorkQueue,
    WorkTasks,
    WorkRuns,
    WorkConfig,
    FleetAgents,
    FleetLeases,
    FleetCost,
    FlowEvents,
    FlowMessages,
    FlowAudit,
    TermLifetimes,
    TermAttach,
}

const WORK: &[Surface] = &[
    Surface::WorkQueue,
    Surface::WorkTasks,
    Surface::WorkRuns,
    Surface::WorkConfig,
];
const FLEET: &[Surface] = &[
    Surface::FleetAgents,
    Surface::FleetLeases,
    Surface::FleetCost,
];
const FLOW: &[Surface] = &[
    Surface::FlowEvents,
    Surface::FlowMessages,
    Surface::FlowAudit,
];
const TERM: &[Surface] = &[Surface::TermLifetimes, Surface::TermAttach];

impl Surface {
    pub const fn lens(self) -> Lens {
        match self {
            Self::Hall => Lens::Hall,
            Self::WorkQueue | Self::WorkTasks | Self::WorkRuns | Self::WorkConfig => Lens::Work,
            Self::FleetAgents | Self::FleetLeases | Self::FleetCost => Lens::Fleet,
            Self::FlowEvents | Self::FlowMessages | Self::FlowAudit => Lens::Flow,
            Self::TermLifetimes | Self::TermAttach => Lens::Term,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Hall => "hall",
            Self::WorkQueue => "queue",
            Self::WorkTasks => "tasks",
            Self::WorkRuns => "runs",
            Self::WorkConfig => "config",
            Self::FleetAgents => "agents",
            Self::FleetLeases => "leases",
            Self::FleetCost => "cost",
            Self::FlowEvents => "events",
            Self::FlowMessages => "messages",
            Self::FlowAudit => "audit",
            Self::TermLifetimes => "term",
            Self::TermAttach => "attach",
        }
    }

    pub const fn board_view(self) -> Option<BoardView> {
        match self {
            Self::WorkTasks => Some(BoardView::Dag),
            Self::WorkRuns => Some(BoardView::Runs),
            Self::FleetLeases => Some(BoardView::Fleet),
            Self::FleetCost => Some(BoardView::CostHealth),
            Self::FlowEvents => Some(BoardView::Events),
            Self::FlowMessages => Some(BoardView::Flow),
            Self::FlowAudit => Some(BoardView::Audit),
            _ => None,
        }
    }

    fn siblings(self) -> &'static [Surface] {
        match self.lens() {
            Lens::Hall => &[Self::Hall],
            Lens::Work => WORK,
            Lens::Fleet => FLEET,
            Lens::Flow => FLOW,
            Lens::Term => TERM,
        }
    }

    fn adjacent(self, delta: i8) -> Self {
        let siblings = self.siblings();
        let current = siblings
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(0);
        let next = if delta < 0 {
            current.checked_sub(1).unwrap_or(siblings.len() - 1)
        } else {
            (current + 1) % siblings.len()
        };
        siblings[next]
    }
}

#[derive(Debug, Clone, Copy)]
struct Destination {
    name: &'static str,
    surface: Surface,
}

const DESTINATIONS: &[Destination] = &[
    Destination {
        name: "hall",
        surface: Surface::Hall,
    },
    Destination {
        name: "work",
        surface: Surface::WorkQueue,
    },
    Destination {
        name: "queue",
        surface: Surface::WorkQueue,
    },
    Destination {
        name: "tasks",
        surface: Surface::WorkTasks,
    },
    Destination {
        name: "dag",
        surface: Surface::WorkTasks,
    },
    Destination {
        name: "runs",
        surface: Surface::WorkRuns,
    },
    Destination {
        name: "config",
        surface: Surface::WorkConfig,
    },
    Destination {
        name: "fleet",
        surface: Surface::FleetAgents,
    },
    Destination {
        name: "agents",
        surface: Surface::FleetAgents,
    },
    Destination {
        name: "leases",
        surface: Surface::FleetLeases,
    },
    Destination {
        name: "cost",
        surface: Surface::FleetCost,
    },
    Destination {
        name: "flow",
        surface: Surface::FlowEvents,
    },
    Destination {
        name: "events",
        surface: Surface::FlowEvents,
    },
    Destination {
        name: "messages",
        surface: Surface::FlowMessages,
    },
    Destination {
        name: "audit",
        surface: Surface::FlowAudit,
    },
    Destination {
        name: "term",
        surface: Surface::TermLifetimes,
    },
    Destination {
        name: "attach",
        surface: Surface::TermAttach,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextVerb {
    AckAttention,
    MuteAttention,
    UnmuteAttention,
    ResolveAttention,
    DecideGate,
    StopAttempt,
    EditBudget,
    EditConfig,
}

impl ContextVerb {
    const fn label(self) -> &'static str {
        match self {
            Self::AckAttention => "acknowledge attention",
            Self::MuteAttention => "mute attention",
            Self::UnmuteAttention => "unmute attention",
            Self::ResolveAttention => "resolve attention",
            Self::DecideGate => "decide gate",
            Self::StopAttempt => "stop attempt",
            Self::EditBudget => "edit attempt budget",
            Self::EditConfig => "edit config",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellMode {
    View,
    Navigator,
    Filter,
    Prompt(ContextVerb),
    Confirm(ContextVerb),
    GateDecision,
    Form,
    Input,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellAction {
    None,
    Quit,
    PreviewRoute(Surface),
    RouteChanged(Surface),
    NavigatorCanceled(Surface),
    FilterChanged(String),
    MoveSelection(i8),
    Drill,
    ToggleMotion,
    ToggleRail,
    EnterInput,
    LeaveInput,
    ForwardInput,
    OpenGateDecision,
    OpenBudgetForm,
    MoveGateOption(i8),
    SelectGateOption(usize),
    CommitGateDecision,
    CancelGateDecision,
    MoveFormField(i8),
    EditForm(char),
    BackspaceForm,
    CommitForm,
    CancelForm,
    PrepareConfirmation(ContextVerb),
    CancelConfirmation,
    Confirmed(ContextVerb),
}

#[derive(Debug, Clone)]
pub struct ShellState {
    surface: Surface,
    mode: ShellMode,
    navigator_origin: Surface,
    navigator_filter_origin: String,
    navigator_input: String,
    navigator_index: usize,
    filter: String,
    filter_origin: String,
    prompt_input: String,
    confirmation_target: Option<String>,
    confirmation_origin: ShellMode,
    attention: BTreeSet<Lens>,
    rail_visible: bool,
    attach_subject: Option<String>,
    notice: Option<String>,
}

impl ShellState {
    pub fn new(surface: Surface) -> Self {
        Self {
            surface,
            mode: ShellMode::View,
            navigator_origin: surface,
            navigator_filter_origin: String::new(),
            navigator_input: String::new(),
            navigator_index: 0,
            filter: String::new(),
            filter_origin: String::new(),
            prompt_input: String::new(),
            confirmation_target: None,
            confirmation_origin: ShellMode::View,
            attention: BTreeSet::new(),
            rail_visible: true,
            attach_subject: None,
            notice: None,
        }
    }

    pub const fn surface(&self) -> Surface {
        self.surface
    }

    pub const fn mode(&self) -> ShellMode {
        self.mode
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn prompt_value(&self) -> &str {
        &self.prompt_input
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    pub fn set_attention(&mut self, lens: Lens, active: bool) {
        if active {
            self.attention.insert(lens);
        } else {
            self.attention.remove(&lens);
        }
    }

    pub const fn rail_visible(&self) -> bool {
        self.rail_visible
    }

    pub fn toggle_rail(&mut self) {
        self.rail_visible = !self.rail_visible;
    }

    pub fn set_attach_subject(&mut self, subject: impl Into<String>) {
        self.attach_subject = Some(subject.into());
    }

    pub fn route(&mut self, surface: Surface) {
        if self.surface != surface {
            self.filter.clear();
        }
        self.surface = surface;
        self.mode = ShellMode::View;
    }

    pub fn enter_form(&mut self) {
        self.mode = ShellMode::Form;
    }

    pub fn confirm(&mut self, verb: ContextVerb) {
        self.confirmation_origin = self.mode;
        self.mode = ShellMode::Confirm(verb);
    }

    pub fn set_confirmation_target(&mut self, target: impl Into<String>) {
        self.confirmation_target = Some(target.into());
    }

    pub fn cancel_modal(&mut self) {
        self.mode = ShellMode::View;
    }

    pub fn handle(&mut self, key: KeyEvent) -> ShellAction {
        self.notice = None;
        if self.mode == ShellMode::Input {
            return self.handle_input(key);
        }
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ShellAction::None;
        }
        match self.mode {
            ShellMode::View => self.handle_view(key),
            ShellMode::Navigator => self.handle_navigator(key),
            ShellMode::Filter => self.handle_filter(key),
            ShellMode::Prompt(verb) => self.handle_prompt(key, verb),
            ShellMode::Confirm(verb) => self.handle_confirm(key, verb),
            ShellMode::GateDecision => self.handle_gate_decision(key),
            ShellMode::Form => self.handle_form(key),
            ShellMode::Input => unreachable!("input mode returned above"),
        }
    }

    fn handle_view(&mut self, key: KeyEvent) -> ShellAction {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return ShellAction::Quit;
        }
        if !key.modifiers.is_empty() {
            return ShellAction::None;
        }
        match key.code {
            KeyCode::Char('q') if self.surface == Surface::TermAttach => {
                self.change_route(Surface::TermLifetimes)
            }
            KeyCode::Char('q') if self.surface.lens() == Lens::Work => {
                self.change_route(Surface::Hall)
            }
            KeyCode::Char('q') => ShellAction::Quit,
            KeyCode::Char(':') => {
                self.navigator_origin = self.surface;
                self.navigator_filter_origin.clone_from(&self.filter);
                self.navigator_input.clear();
                self.navigator_index = 0;
                self.mode = ShellMode::Navigator;
                ShellAction::None
            }
            KeyCode::Char('/') => {
                self.filter_origin.clone_from(&self.filter);
                self.mode = ShellMode::Filter;
                ShellAction::None
            }
            KeyCode::Home => self.change_route(Surface::Hall),
            KeyCode::Esc if self.surface == Surface::TermAttach => {
                self.change_route(Surface::TermLifetimes)
            }
            KeyCode::Esc if self.surface != Surface::Hall => self.change_route(Surface::Hall),
            KeyCode::Char('[') | KeyCode::Left => self.change_route(self.surface.adjacent(-1)),
            KeyCode::Char(']') | KeyCode::Right => self.change_route(self.surface.adjacent(1)),
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => ShellAction::MoveSelection(1),
            KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => ShellAction::MoveSelection(-1),
            KeyCode::Enter => ShellAction::Drill,
            KeyCode::Char('m') if self.surface == Surface::Hall => ShellAction::ToggleMotion,
            KeyCode::Char('r') if self.surface == Surface::TermAttach => ShellAction::ToggleRail,
            KeyCode::Char('i') if self.surface == Surface::TermAttach => {
                self.mode = ShellMode::Input;
                ShellAction::EnterInput
            }
            KeyCode::Char(character) => self.begin_confirm(character),
            _ => ShellAction::None,
        }
    }

    fn begin_confirm(&mut self, character: char) -> ShellAction {
        if self.surface == Surface::WorkQueue && character == 'd' {
            self.mode = ShellMode::GateDecision;
            return ShellAction::OpenGateDecision;
        }
        if self.surface == Surface::WorkConfig && character == 'e' {
            return ShellAction::Drill;
        }
        if self.surface == Surface::FleetAgents && character == 'b' {
            return ShellAction::OpenBudgetForm;
        }
        if self.surface == Surface::WorkQueue && character == 'm' {
            self.prompt_input.clear();
            self.confirmation_origin = ShellMode::View;
            self.mode = ShellMode::Prompt(ContextVerb::MuteAttention);
            return ShellAction::PrepareConfirmation(ContextVerb::MuteAttention);
        }
        let verb = match (self.surface, character) {
            (Surface::WorkQueue, 'a') => Some(ContextVerb::AckAttention),
            (Surface::WorkQueue, 'u') => Some(ContextVerb::UnmuteAttention),
            (Surface::WorkQueue, 'r') => Some(ContextVerb::ResolveAttention),
            (Surface::FleetAgents, 's') => Some(ContextVerb::StopAttempt),
            _ => None,
        };
        if let Some(verb) = verb {
            self.confirmation_origin = ShellMode::View;
            self.mode = ShellMode::Confirm(verb);
            return ShellAction::PrepareConfirmation(verb);
        }
        ShellAction::None
    }

    fn handle_navigator(&mut self, key: KeyEvent) -> ShellAction {
        match key.code {
            KeyCode::Esc => {
                let origin = self.navigator_origin;
                self.surface = origin;
                self.filter.clone_from(&self.navigator_filter_origin);
                self.mode = ShellMode::View;
                ShellAction::NavigatorCanceled(origin)
            }
            KeyCode::Enter => {
                let destination = self.navigator_matches().get(self.navigator_index).copied();
                if let Some(destination) = destination {
                    self.surface = destination.surface;
                    self.filter.clear();
                    self.mode = ShellMode::View;
                    ShellAction::RouteChanged(destination.surface)
                } else {
                    ShellAction::None
                }
            }
            KeyCode::Down | KeyCode::Tab => self.move_navigator(1),
            KeyCode::Up | KeyCode::BackTab => self.move_navigator(-1),
            KeyCode::Backspace => {
                self.navigator_input.pop();
                self.navigator_index = 0;
                self.preview_navigator()
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.navigator_input.push(character.to_ascii_lowercase());
                self.navigator_index = 0;
                self.preview_navigator()
            }
            _ => ShellAction::None,
        }
    }

    fn navigator_matches(&self) -> Vec<Destination> {
        let query = self.navigator_input.trim();
        DESTINATIONS
            .iter()
            .copied()
            .filter(|destination| destination.name.starts_with(query))
            .collect()
    }

    fn preview_navigator(&mut self) -> ShellAction {
        let destination = self.navigator_matches().get(self.navigator_index).copied();
        if let Some(destination) = destination {
            self.surface = destination.surface;
            self.filter.clear();
            ShellAction::PreviewRoute(destination.surface)
        } else {
            ShellAction::None
        }
    }

    fn move_navigator(&mut self, delta: i8) -> ShellAction {
        let matches = self.navigator_matches();
        if matches.is_empty() {
            return ShellAction::None;
        }
        self.navigator_index = if delta < 0 {
            self.navigator_index
                .checked_sub(1)
                .unwrap_or(matches.len() - 1)
        } else {
            (self.navigator_index + 1) % matches.len()
        };
        let destination = matches[self.navigator_index];
        self.surface = destination.surface;
        self.filter.clear();
        ShellAction::PreviewRoute(destination.surface)
    }

    fn handle_filter(&mut self, key: KeyEvent) -> ShellAction {
        match key.code {
            KeyCode::Esc => {
                self.filter.clone_from(&self.filter_origin);
                self.mode = ShellMode::View;
                ShellAction::FilterChanged(self.filter.clone())
            }
            KeyCode::Enter => {
                self.mode = ShellMode::View;
                ShellAction::FilterChanged(self.filter.clone())
            }
            KeyCode::Backspace => {
                self.filter.pop();
                ShellAction::FilterChanged(self.filter.clone())
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter.push(character);
                ShellAction::FilterChanged(self.filter.clone())
            }
            _ => ShellAction::None,
        }
    }

    fn handle_confirm(&mut self, key: KeyEvent, verb: ContextVerb) -> ShellAction {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') => {
                self.mode = self.confirmation_origin;
                self.confirmation_target = None;
                if verb == ContextVerb::MuteAttention {
                    self.prompt_input.clear();
                }
                ShellAction::CancelConfirmation
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                self.mode = ShellMode::View;
                self.confirmation_target = None;
                ShellAction::Confirmed(verb)
            }
            _ => ShellAction::None,
        }
    }

    fn handle_prompt(&mut self, key: KeyEvent, verb: ContextVerb) -> ShellAction {
        match key.code {
            KeyCode::Esc => {
                self.prompt_input.clear();
                self.confirmation_target = None;
                self.mode = ShellMode::View;
                ShellAction::CancelConfirmation
            }
            KeyCode::Enter if !self.prompt_input.is_empty() => {
                self.confirmation_origin = ShellMode::View;
                self.mode = ShellMode::Confirm(verb);
                ShellAction::None
            }
            KeyCode::Backspace => {
                self.prompt_input.pop();
                ShellAction::None
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.prompt_input.push(character);
                ShellAction::None
            }
            _ => ShellAction::None,
        }
    }

    fn handle_gate_decision(&mut self, key: KeyEvent) -> ShellAction {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') => {
                self.mode = ShellMode::View;
                ShellAction::CancelGateDecision
            }
            KeyCode::Enter | KeyCode::Char('y') => ShellAction::CommitGateDecision,
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => ShellAction::MoveGateOption(1),
            KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => ShellAction::MoveGateOption(-1),
            KeyCode::Char(character) if character.is_ascii_digit() && character != '0' => {
                ShellAction::SelectGateOption(character.to_digit(10).unwrap_or(1) as usize - 1)
            }
            _ => ShellAction::None,
        }
    }

    fn handle_form(&mut self, key: KeyEvent) -> ShellAction {
        match key.code {
            KeyCode::Esc => {
                self.mode = ShellMode::View;
                ShellAction::CancelForm
            }
            KeyCode::Enter => ShellAction::CommitForm,
            KeyCode::Tab | KeyCode::Down => ShellAction::MoveFormField(1),
            KeyCode::BackTab | KeyCode::Up => ShellAction::MoveFormField(-1),
            KeyCode::Backspace => ShellAction::BackspaceForm,
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                ShellAction::EditForm(character)
            }
            _ => ShellAction::None,
        }
    }

    fn handle_input(&mut self, key: KeyEvent) -> ShellAction {
        if key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char(']')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.mode = ShellMode::View;
            ShellAction::LeaveInput
        } else {
            ShellAction::ForwardInput
        }
    }

    fn change_route(&mut self, surface: Surface) -> ShellAction {
        if self.surface != surface {
            self.filter.clear();
        }
        self.surface = surface;
        ShellAction::RouteChanged(surface)
    }
}

pub fn body_area(area: Rect) -> Rect {
    Rect::new(
        area.x,
        area.y.saturating_add(2),
        area.width,
        area.height.saturating_sub(3),
    )
}

pub fn render_chrome(
    area: Rect,
    buffer: &mut Buffer,
    shell: &ShellState,
    tier: ColorTier,
    glyphs: GlyphSet,
    motion: &str,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        clear(area, buffer);
        put(buffer, area, 0, 0, "GRIDWORK", bold("gws_fg", tier));
        put(
            buffer,
            area,
            1,
            1,
            &format!(
                "terminal too small: {}x{} (minimum {MIN_WIDTH}x{MIN_HEIGHT})",
                area.width, area.height
            ),
            style("gws_warn", tier),
        );
        return;
    }

    if shell.surface == Surface::FleetAgents {
        clear_row(area, buffer, 1);
    } else if shell.surface.lens() != Lens::Hall {
        clear_row(area, buffer, 0);
    }
    clear_row(area, buffer, area.height - 1);
    if shell.surface == Surface::FleetAgents {
        paint_fleet_tabs(area, buffer, shell, tier);
    } else {
        paint_header(area, buffer, shell, tier);
    }
    if let Some(notice) = shell.notice() {
        put(buffer, area, 2, 1, notice, style("gws_warn", tier));
    }
    match shell.mode {
        ShellMode::Navigator => {
            clear_panel(
                area,
                buffer,
                1,
                area.width.min(40),
                area.height.saturating_sub(2),
            );
            paint_navigator(area, buffer, shell, tier);
        }
        ShellMode::Filter => {
            put(
                buffer,
                area,
                2,
                2,
                &format!("/{}", shell.filter),
                bold("gws_hue", tier),
            );
        }
        ShellMode::Prompt(verb) => paint_prompt(area, buffer, shell, verb, tier),
        ShellMode::Confirm(verb) => paint_confirm(area, buffer, shell, verb, tier),
        ShellMode::View | ShellMode::GateDecision | ShellMode::Form | ShellMode::Input => {}
    }
    paint_keybar(area, buffer, shell, tier, glyphs, motion);
}

fn paint_fleet_tabs(area: Rect, buffer: &mut Buffer, shell: &ShellState, tier: ColorTier) {
    let mut x = 1;
    for tab in shell.surface.siblings() {
        let text = if *tab == shell.surface {
            format!("[{}]", tab.name())
        } else {
            format!(" {} ", tab.name())
        };
        put(
            buffer,
            area,
            x,
            1,
            &text,
            if *tab == shell.surface {
                bold("gws_hue", tier)
            } else {
                style("gws_muted", tier)
            },
        );
        x = x.saturating_add(text.len() as u16 + 1);
    }
}

fn paint_header(area: Rect, buffer: &mut Buffer, shell: &ShellState, tier: ColorTier) {
    let surface = shell.surface;
    let lens = surface.lens();
    let title = if lens == Lens::Hall {
        "GRIDWORK"
    } else {
        lens.as_str()
    };
    put(
        buffer,
        area,
        1,
        0,
        &title.to_ascii_uppercase(),
        bold("gws_fg", tier),
    );
    if lens == Lens::Hall {
        return;
    }
    if surface == Surface::TermAttach {
        if let Some(subject) = &shell.attach_subject {
            put(buffer, area, 8, 0, subject, style("gws_fg", tier));
        }
        return;
    }
    let mut x = 9;
    for tab in surface.siblings() {
        let text = if *tab == surface {
            format!("[{}]", tab.name())
        } else {
            format!(" {} ", tab.name())
        };
        put(
            buffer,
            area,
            x,
            0,
            &text,
            if *tab == surface {
                bold("gws_hue", tier)
            } else {
                style("gws_muted", tier)
            },
        );
        x = x.saturating_add(text.len() as u16 + 1);
    }
}

fn paint_navigator(area: Rect, buffer: &mut Buffer, shell: &ShellState, tier: ColorTier) {
    let matches = shell.navigator_matches();
    put(
        buffer,
        area,
        2,
        2,
        &format!("NAVIGATE  :{}", shell.navigator_input),
        bold("gws_hue", tier),
    );
    let capacity = area.height.saturating_sub(6) as usize;
    for (index, destination) in matches.iter().take(capacity).enumerate() {
        let attention = destination.surface.lens() != shell.surface.lens()
            && shell.attention.contains(&destination.surface.lens());
        let label = format!("{}{}", destination.name, if attention { " !" } else { "" });
        put(
            buffer,
            area,
            2,
            4 + index as u16,
            if index == shell.navigator_index {
                ">"
            } else {
                " "
            },
            bold("gws_focus", tier),
        );
        put(
            buffer,
            area,
            4,
            4 + index as u16,
            &label,
            if index == shell.navigator_index {
                bold("gws_fg", tier)
            } else {
                style("gws_muted", tier)
            },
        );
    }
    if matches.len() > capacity {
        put(
            buffer,
            area,
            4,
            4 + capacity as u16,
            &format!("+{} more", matches.len() - capacity),
            style("gws_muted", tier),
        );
    }
}

fn paint_prompt(
    area: Rect,
    buffer: &mut Buffer,
    shell: &ShellState,
    verb: ContextVerb,
    tier: ColorTier,
) {
    let y = area.height.saturating_sub(7);
    put(buffer, area, 2, y, "DEADLINE", bold("gws_warn", tier));
    let label = shell.confirmation_target.as_ref().map_or_else(
        || verb.label().to_owned(),
        |target| format!("{}  {target}", verb.label()),
    );
    put(buffer, area, 12, y, &label, bold("gws_fg", tier));
    put(
        buffer,
        area,
        2,
        y.saturating_add(2),
        &format!("{}_", shell.prompt_input),
        bold("gws_focus", tier),
    );
    put(
        buffer,
        area,
        2,
        y.saturating_add(3),
        "YYYY-MM-DDTHH:MM:SSZ   enter continue   esc cancel",
        style("gws_muted", tier),
    );
}

fn paint_confirm(
    area: Rect,
    buffer: &mut Buffer,
    shell: &ShellState,
    verb: ContextVerb,
    tier: ColorTier,
) {
    let y = area.height.saturating_sub(7);
    put(buffer, area, 2, y, "CONFIRM", bold("gws_warn", tier));
    let label = if verb == ContextVerb::MuteAttention {
        format!(
            "{} {} until {}",
            verb.label(),
            shell
                .confirmation_target
                .as_deref()
                .unwrap_or("selected row"),
            shell.prompt_input
        )
    } else if let Some(target) = &shell.confirmation_target {
        format!("{}  {target}", verb.label())
    } else {
        verb.label().to_owned()
    };
    put(buffer, area, 12, y, &label, bold("gws_fg", tier));
    put(
        buffer,
        area,
        2,
        y.saturating_add(2),
        "enter/y confirm   esc/n cancel",
        style("gws_muted", tier),
    );
}

fn paint_keybar(
    area: Rect,
    buffer: &mut Buffer,
    shell: &ShellState,
    tier: ColorTier,
    glyphs: GlyphSet,
    motion: &str,
) {
    let y = area.height - 1;
    let mode = match shell.mode {
        ShellMode::View => "VIEW",
        ShellMode::Navigator => "GO",
        ShellMode::Filter => "FILTER",
        ShellMode::Prompt(_) => "DEADLINE",
        ShellMode::Confirm(_) => "CONFIRM",
        ShellMode::GateDecision => "DECIDE",
        ShellMode::Form => "FORM",
        ShellMode::Input => "INPUT",
    };
    let left = format!(" {mode} ");
    let right = format!("motion {motion}  {}", tier_badge(tier, glyphs));
    put(
        buffer,
        area,
        0,
        y,
        &left,
        bold(
            if shell.mode == ShellMode::Input {
                "gws_warn"
            } else {
                "gws_fg"
            },
            tier,
        ),
    );
    put_right(buffer, area, y, &right, style("gws_muted", tier));

    let hints = key_hints(shell);
    let start = left.len() as u16 + 1;
    let right_width = right.len() as u16;
    let end = area.width.saturating_sub(right_width + 2);
    let available = end.saturating_sub(start) as usize;
    if available == 0 {
        return;
    }
    if hints.len() <= available {
        put(buffer, area, start, y, hints, style("gws_muted", tier));
        return;
    }
    let mut used = 0usize;
    for segment in hints.split("   ") {
        let addition = segment.len() + usize::from(used > 0) * 3;
        if used + addition + 2 > available {
            break;
        }
        if used > 0 {
            put(
                buffer,
                area,
                start + used as u16,
                y,
                "   ",
                style("gws_muted", tier),
            );
            used += 3;
        }
        put(
            buffer,
            area,
            start + used as u16,
            y,
            segment,
            style("gws_muted", tier),
        );
        used += segment.len();
    }
    put(
        buffer,
        area,
        end.saturating_sub(1),
        y,
        "+",
        style("gws_muted", tier),
    );
}

fn key_hints(shell: &ShellState) -> &'static str {
    match shell.mode {
        ShellMode::Navigator => "type narrow   j/k choose   enter go   esc cancel",
        ShellMode::Filter => "type filter   enter keep   esc restore",
        ShellMode::Prompt(_) => "type UTC deadline   enter continue   esc cancel",
        ShellMode::Confirm(_) => "enter/y confirm   esc/n cancel",
        ShellMode::GateDecision => "j/k choose   1-9 select   enter decide   esc cancel",
        ShellMode::Form => "tab field   type edit   enter commit   esc cancel",
        // The send path belongs to the input-path lane; until it lands the
        // keybar must not claim delivery a no-op handler silently swallows.
        ShellMode::Input => {
            "ctrl-] leave input   send path not wired yet -- keys are not delivered"
        }
        ShellMode::View => match shell.surface {
            Surface::Hall => ": go   / filter   enter open   j/k district   m motion   q quit",
            Surface::WorkQueue => {
                ": go   / filter   enter open   a ack   m mute   u unmute   r resolve   d decide   q back"
            }
            Surface::WorkConfig => ": go   / filter   enter open   e edit   [/] tab   q back",
            Surface::FleetAgents => {
                ": go   / filter   enter open   s stop   b budget   [/] tab   q quit"
            }
            Surface::TermAttach => "Ctrl-g command   Ctrl-g q back   mouse focus   input -> pane",
            // q leaves a WORK surface for the Hall (the ruled "q back");
            // everywhere else in this arm it quits.
            Surface::WorkTasks | Surface::WorkRuns => {
                ": go   / filter   enter open   j/k row   [/] tab   q back"
            }
            _ => ": go   / filter   enter open   j/k row   [/] tab   q quit",
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetFormState {
    pub attempt_id: AttemptId,
    pub expected_version: u32,
    fields: [String; 4],
    original: [String; 4],
    selected: usize,
    replace_on_type: bool,
}

impl BudgetFormState {
    pub fn new(attempt: &Attempt) -> Self {
        let budget = attempt.budget.as_ref();
        let fields = [
            budget
                .and_then(|value| value.max_tokens)
                .map_or_else(String::new, |value| value.to_string()),
            budget
                .and_then(|value| value.max_tool_calls)
                .map_or_else(String::new, |value| value.to_string()),
            budget
                .and_then(|value| value.max_wall_ms)
                .map_or_else(String::new, |value| value.to_string()),
            budget
                .and_then(|value| value.max_cost_micros)
                .map_or_else(String::new, |value| value.to_string()),
        ];
        Self {
            attempt_id: attempt.id.clone(),
            expected_version: attempt.version,
            original: fields.clone(),
            fields,
            selected: 0,
            replace_on_type: true,
        }
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn fields(&self) -> &[String; 4] {
        &self.fields
    }

    pub fn move_selection(&mut self, delta: i8) {
        self.selected = if delta < 0 {
            self.selected.checked_sub(1).unwrap_or(3)
        } else {
            (self.selected + 1) % 4
        };
        self.replace_on_type = true;
    }

    pub fn insert(&mut self, character: char) {
        if !character.is_ascii_digit() {
            return;
        }
        if self.replace_on_type {
            self.fields[self.selected].clear();
            self.replace_on_type = false;
        }
        self.fields[self.selected].push(character);
    }

    pub fn backspace(&mut self) {
        self.fields[self.selected].pop();
        self.replace_on_type = false;
    }

    pub fn budget(&self) -> Result<Budget, &'static str> {
        let parse_u32 = |value: &str| {
            if value.is_empty() {
                Ok(None)
            } else {
                value
                    .parse::<u32>()
                    .map(Some)
                    .map_err(|_| "budget values must fit an unsigned 32-bit integer")
            }
        };
        let parse_cost = |value: &str| {
            if value.is_empty() {
                Ok(None)
            } else {
                value
                    .parse::<u64>()
                    .map(|value| Some(CostMicros::new(value)))
                    .map_err(|_| "max cost micros must fit an unsigned 64-bit integer")
            }
        };
        Ok(Budget {
            max_tokens: parse_u32(&self.fields[0])?,
            max_tool_calls: parse_u32(&self.fields[1])?,
            max_wall_ms: parse_u32(&self.fields[2])?,
            max_cost_micros: parse_cost(&self.fields[3])?,
        })
    }
}

pub fn render_budget_form(
    area: Rect,
    buffer: &mut Buffer,
    form: &BudgetFormState,
    tier: ColorTier,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    put(buffer, area, 1, 0, "BUDGET", bold("gws_fg", tier));
    put(
        buffer,
        area,
        10,
        0,
        &format!(
            "attempt {}  version {}",
            form.attempt_id, form.expected_version
        ),
        style("gws_muted", tier),
    );
    put(buffer, area, 2, 2, "AXIS", style("gws_muted", tier));
    put(buffer, area, 34, 2, "CAP", style("gws_muted", tier));
    for (index, label) in [
        "max tokens",
        "max tool calls",
        "max wall ms",
        "max cost micros",
    ]
    .into_iter()
    .enumerate()
    {
        let y = 3 + u16::try_from(index).unwrap_or(u16::MAX);
        let selected = form.selected == index;
        put(
            buffer,
            area,
            0,
            y,
            if selected { ">" } else { " " },
            bold("gws_focus", tier),
        );
        put(buffer, area, 2, y, label, style("gws_fg", tier));
        let value = if form.fields[index].is_empty() {
            "uncapped"
        } else {
            &form.fields[index]
        };
        put(
            buffer,
            area,
            34,
            y,
            value,
            if selected {
                bold("gws_focus", tier)
            } else {
                style("gws_fg", tier)
            },
        );
        if selected {
            put(
                buffer,
                area,
                34u16.saturating_add(u16::try_from(value.len()).unwrap_or(u16::MAX)),
                y,
                "_",
                bold("gws_focus", tier),
            );
        }
        if form.fields[index] != form.original[index] {
            put(buffer, area, 58, y, "changed", style("gws_warn", tier));
        }
    }
    put(
        buffer,
        area,
        2,
        9,
        "blank means uncapped, never zero; Enter reviews the typed replacement command",
        style("gws_muted", tier),
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateDecisionState {
    pub gate: Gate,
    pub selected: usize,
}

impl GateDecisionState {
    pub fn new(gate: Gate) -> Self {
        Self { gate, selected: 0 }
    }

    pub fn options(&self) -> &[String] {
        self.gate.options.as_deref().unwrap_or_default()
    }

    pub fn chosen(&self) -> Option<&str> {
        self.options().get(self.selected).map(String::as_str)
    }

    pub fn move_selection(&mut self, delta: i8) {
        let count = self.options().len();
        if count == 0 {
            self.selected = 0;
        } else if delta < 0 {
            self.selected = self.selected.checked_sub(1).unwrap_or(count - 1);
        } else {
            self.selected = (self.selected + 1) % count;
        }
    }

    pub fn select(&mut self, index: usize) {
        if index < self.options().len() {
            self.selected = index;
        }
    }
}

pub fn render_gate_decision(
    area: Rect,
    buffer: &mut Buffer,
    decision: &GateDecisionState,
    tier: ColorTier,
) {
    if area.width == 0 || area.height < 7 {
        return;
    }
    let options = decision.options();
    let height = u16::try_from(options.len())
        .unwrap_or(u16::MAX)
        .saturating_add(6)
        .min(area.height.saturating_sub(1));
    let top = area.height.saturating_sub(height + 1);
    for y in top..area.height.saturating_sub(1) {
        clear_row(area, buffer, y);
    }
    put(
        buffer,
        area,
        0,
        top,
        &"-".repeat(area.width as usize),
        style("gws_muted", tier),
    );
    let gate = &decision.gate;
    put(buffer, area, 2, top + 1, "DECIDE", bold("gws_warn", tier));
    put(
        buffer,
        area,
        11,
        top + 1,
        &format!(
            "gate {}   kind {}   raised {}",
            gate.id,
            gate.kind.as_deref().unwrap_or("-"),
            clock(&gate.created_at)
        ),
        style("gws_fg", tier),
    );
    put(
        buffer,
        area,
        2,
        top + 2,
        gate.question.as_deref().unwrap_or("question unavailable"),
        style("gws_fg", tier),
    );
    let capacity = usize::from(height.saturating_sub(5));
    let start = decision
        .selected
        .saturating_sub(capacity.saturating_sub(1))
        .min(options.len().saturating_sub(capacity));
    for (offset, option) in options.iter().skip(start).take(capacity).enumerate() {
        let index = start + offset;
        let y = top + 4 + u16::try_from(offset).unwrap_or(u16::MAX);
        put(
            buffer,
            area,
            2,
            y,
            if index == decision.selected { ">" } else { " " },
            bold("gws_focus", tier),
        );
        put(
            buffer,
            area,
            4,
            y,
            &format!("{}  {option}", index + 1),
            if index == decision.selected {
                bold("gws_fg", tier)
            } else {
                style("gws_fg", tier)
            },
        );
    }
    put(
        buffer,
        area,
        2,
        area.height.saturating_sub(2),
        "decides as operator -- receipted; the gate aggregate records no actor",
        style("gws_muted", tier),
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachRailTerm {
    pub subject: String,
    pub state: String,
    pub attached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachRailItem {
    pub text: String,
    pub attention: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachRailState {
    pub running: usize,
    pub attention: usize,
    pub blocked: usize,
    pub cost: String,
    pub terms: Vec<AttachRailTerm>,
    pub queue: Vec<AttachRailItem>,
}

pub fn render_attach_rail(
    area: Rect,
    buffer: &mut Buffer,
    state: &AttachRailState,
    tier: ColorTier,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    put(buffer, area, 1, 0, "ESTATE", bold("gws_fg", tier));
    for (index, (label, value)) in [
        ("running", state.running.to_string()),
        ("attention", state.attention.to_string()),
        ("blocked", state.blocked.to_string()),
        ("today", state.cost.clone()),
    ]
    .into_iter()
    .enumerate()
    {
        let y = 2 + u16::try_from(index).unwrap_or(u16::MAX);
        put(buffer, area, 1, y, label, style("gws_muted", tier));
        put(buffer, area, 14, y, &value, style("gws_fg", tier));
    }

    let mut y = 8;
    put(buffer, area, 1, y, "TERMS", bold("gws_fg", tier));
    y += 1;
    for term in &state.terms {
        if y >= area.height.saturating_sub(5) {
            break;
        }
        put(
            buffer,
            area,
            0,
            y,
            if term.attached { ">" } else { " " },
            bold("gws_focus", tier),
        );
        let subject = theme::safe_text(&term.subject, area.width.saturating_sub(2) as usize);
        put(buffer, area, 2, y, subject.as_ref(), style("gws_fg", tier));
        crate::row::paint_tail(
            buffer,
            area,
            area.y.saturating_add(y),
            area.x
                .saturating_add(2)
                .saturating_add(u16::try_from(subject.chars().count()).unwrap_or(u16::MAX)),
            &term.state,
            style("gws_muted", tier),
        );
        y += 1;
    }

    y = y.saturating_add(1);
    if y < area.height.saturating_sub(2) {
        put(buffer, area, 1, y, "QUEUE", bold("gws_fg", tier));
        y += 1;
    }
    for item in &state.queue {
        if y >= area.height.saturating_sub(1) {
            break;
        }
        put(
            buffer,
            area,
            1,
            y,
            if item.attention { "!" } else { " " },
            if item.attention {
                style("gws_warn", tier)
            } else {
                style("gws_muted", tier)
            },
        );
        put(buffer, area, 3, y, &item.text, style("gws_fg", tier));
        y += 1;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermState {
    pub sessions: Vec<PtySession>,
    pub watermark: Option<Seq>,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermTarget {
    Session(PtySessionId),
}

pub fn term_target_order(state: &TermState) -> Vec<TermTarget> {
    state
        .sessions
        .iter()
        .map(|session| TermTarget::Session(session.id.clone()))
        .collect()
}

struct TermColumn {
    header: &'static str,
    priority: u8,
}

const TERM_COLUMNS: &[TermColumn] = &[
    TermColumn {
        header: "SESSION",
        priority: 0,
    },
    TermColumn {
        header: "GEN",
        priority: 1,
    },
    TermColumn {
        header: "STATE",
        priority: 0,
    },
    TermColumn {
        header: "ATT/DET",
        priority: 3,
    },
    TermColumn {
        header: "ATTEMPT",
        priority: 2,
    },
    TermColumn {
        header: "TITLE",
        priority: 2,
    },
    TermColumn {
        header: "OPENED",
        priority: 3,
    },
];

pub fn render_terms(
    area: Rect,
    buffer: &mut Buffer,
    state: &TermState,
    selected: Option<&TermTarget>,
    tier: ColorTier,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let rows = state
        .sessions
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
            .into_iter()
            .map(|cell| theme::safe_text(&cell, area.width as usize).into_owned())
            .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let (keep, widths) = term_layout(&rows, area.width.saturating_sub(2) as usize);
    let mut x = 2u16;
    for (slot, column) in keep.iter().enumerate() {
        put(
            buffer,
            area,
            x,
            0,
            TERM_COLUMNS[*column].header,
            style("gws_muted", tier),
        );
        x = x.saturating_add(widths[slot] as u16 + 2);
    }
    let capacity = area.height.saturating_sub(2) as usize;
    let selected_row = selected.and_then(|target| {
        let TermTarget::Session(id) = target;
        state.sessions.iter().position(|session| &session.id == id)
    });
    let needs_notice = state.sessions.len() > capacity && capacity > 1;
    let visible = capacity
        .saturating_sub(usize::from(needs_notice))
        .min(state.sessions.len());
    let start = if visible == 0 {
        0
    } else {
        selected_row
            .filter(|index| *index >= visible)
            .map_or(0, |index| index + 1 - visible)
            .min(state.sessions.len().saturating_sub(visible))
    };
    let end = (start + visible).min(state.sessions.len());
    for (index, (session, row)) in state.sessions[start..end]
        .iter()
        .zip(&rows[start..end])
        .enumerate()
    {
        let y = 1 + index as u16;
        let target = TermTarget::Session(session.id.clone());
        put(
            buffer,
            area,
            0,
            y,
            if selected == Some(&target) { ">" } else { " " },
            bold("gws_focus", tier),
        );
        x = 2;
        for (slot, column) in keep.iter().enumerate() {
            put_n(
                buffer,
                area,
                x,
                y,
                &row[*column],
                widths[slot],
                style("gws_fg", tier),
            );
            x = x.saturating_add(widths[slot] as u16 + 2);
        }
    }
    if needs_notice {
        let notice = if start == 0 {
            format!("+{} more", state.sessions.len() - end)
        } else if end == state.sessions.len() {
            format!("+{start} before")
        } else {
            format!("+{start} before  +{} more", state.sessions.len() - end)
        };
        put(
            buffer,
            area,
            1,
            area.height.saturating_sub(2),
            &notice,
            style("gws_muted", tier),
        );
    }
    let count = if state.complete {
        format!("{} rows", state.sessions.len())
    } else {
        format!("at least {} rows", state.sessions.len())
    };
    let watermark = state
        .watermark
        .map_or_else(|| "-".to_owned(), |seq| seq.to_string());
    put(
        buffer,
        area,
        0,
        area.height - 1,
        &format!("{count}  watermark {watermark}"),
        style("gws_muted", tier),
    );
}

fn term_layout(rows: &[Vec<String>], width: usize) -> (Vec<usize>, Vec<usize>) {
    let mut keep = (0..TERM_COLUMNS.len()).collect::<Vec<_>>();
    loop {
        let widths = keep
            .iter()
            .map(|column| {
                rows.iter()
                    .map(|row| row[*column].chars().count())
                    .chain(std::iter::once(TERM_COLUMNS[*column].header.len()))
                    .max()
                    .unwrap_or(0)
            })
            .collect::<Vec<_>>();
        let used = widths.iter().sum::<usize>() + keep.len().saturating_sub(1) * 2;
        if used <= width || keep.len() == 1 {
            return (keep, widths);
        }
        let victim = keep
            .iter()
            .enumerate()
            .max_by_key(|(slot, column)| (TERM_COLUMNS[**column].priority, *slot))
            .map(|(slot, _)| slot)
            .expect("term table has a column");
        keep.remove(victim);
    }
}

fn clock(timestamp: &gwk_domain::ids::Timestamp) -> String {
    timestamp
        .as_str()
        .get(11..16)
        .unwrap_or(timestamp.as_str())
        .to_owned()
}

fn put(buffer: &mut Buffer, area: Rect, x: u16, y: u16, text: &str, paint: Style) {
    if x >= area.width || y >= area.height {
        return;
    }
    let width = area.width.saturating_sub(x) as usize;
    let safe = theme::safe_text(text, width);
    buffer.set_stringn(area.x + x, area.y + y, safe.as_ref(), width, paint);
}

fn put_n(
    buffer: &mut Buffer,
    area: Rect,
    x: u16,
    y: u16,
    text: &str,
    cell_budget: usize,
    paint: Style,
) {
    if x >= area.width || y >= area.height || cell_budget == 0 {
        return;
    }
    let width = cell_budget.min(area.width.saturating_sub(x) as usize);
    let safe = theme::safe_text(text, width);
    buffer.set_stringn(area.x + x, area.y + y, safe.as_ref(), width, paint);
}

fn clear(area: Rect, buffer: &mut Buffer) {
    for y in 0..area.height {
        clear_row(area, buffer, y);
    }
}

fn clear_row(area: Rect, buffer: &mut Buffer, y: u16) {
    if y < area.height {
        buffer.set_stringn(
            area.x,
            area.y + y,
            " ".repeat(area.width as usize),
            area.width as usize,
            Style::default(),
        );
    }
}

fn clear_panel(area: Rect, buffer: &mut Buffer, top: u16, width: u16, height: u16) {
    for y in top..top.saturating_add(height).min(area.height) {
        buffer.set_stringn(
            area.x,
            area.y + y,
            " ".repeat(width as usize),
            width as usize,
            Style::default(),
        );
    }
}

fn put_right(buffer: &mut Buffer, area: Rect, y: u16, text: &str, paint: Style) {
    let safe = theme::safe_text(text, area.width as usize);
    let width = safe.chars().count() as u16;
    if width < area.width {
        put(
            buffer,
            area,
            area.width - width - 1,
            y,
            safe.as_ref(),
            paint,
        );
    }
}

fn style(name: &str, tier: ColorTier) -> Style {
    let token = gwk_theme::SIGNAL
        .iter()
        .find(|token| token.name == name)
        .expect("the SIGNAL token inventory is pinned");
    theme::token_style(token, tier)
}

fn bold(name: &str, tier: ColorTier) -> Style {
    style(name, tier).add_modifier(Modifier::BOLD)
}
