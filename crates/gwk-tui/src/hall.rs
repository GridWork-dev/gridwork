//! The estate frame: deterministic spatial layout over caller-normalized facts.
//!
//! This lens fetches nothing and reads no clock. Sequence values carry only
//! ledger provenance and ordering; callers provide every fact needed to build
//! a frame. Districts stack vertically, stations run horizontally, and every
//! visible agent occupies the same two cells used by hit testing.

use std::collections::BTreeSet;
use std::fmt;

use gwk_domain::ids::Seq;
use gwk_theme::marks::{GlyphSet, Mark};
use gwk_theme::tier::ColorTier;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::Span;
use ratatui::widgets::Widget;
use ratatui::widgets::canvas::Canvas;
use unicode_width::UnicodeWidthStr;

use crate::input::HitMap;
use crate::theme;

const VIEW_ID_BUDGET: usize = 64;
const STATION_GUTTER: u16 = 2;
const EXPANDED_DISTRICT_HEIGHT: u16 = 3;

/// Why a normalized view identifier cannot enter the frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewIdError {
    Empty(&'static str),
    WrongNamespace {
        expected: &'static str,
        value: String,
    },
    TooLong {
        namespace: &'static str,
        length: usize,
    },
    Unretypable {
        namespace: &'static str,
        character: char,
    },
}

impl fmt::Display for ViewIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ViewIdError::Empty(namespace) => write!(formatter, "{namespace} id is empty"),
            ViewIdError::WrongNamespace { expected, value } => {
                write!(formatter, "{value:?} is not in the {expected} namespace")
            }
            ViewIdError::TooLong { namespace, length } => write!(
                formatter,
                "{namespace} id is {length} characters (budget {VIEW_ID_BUDGET})"
            ),
            ViewIdError::Unretypable {
                namespace,
                character,
            } => write!(
                formatter,
                "{namespace} id contains unretypable character {character:?}"
            ),
        }
    }
}

impl std::error::Error for ViewIdError {}

macro_rules! view_id {
    ($name:ident, $namespace:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ViewIdError> {
                let value = value.into();
                if value.is_empty() {
                    return Err(ViewIdError::Empty($namespace));
                }
                if !value.starts_with(concat!($namespace, "-")) {
                    return Err(ViewIdError::WrongNamespace {
                        expected: $namespace,
                        value,
                    });
                }
                if let Some(character) = value
                    .chars()
                    .find(|character| !character.is_ascii_graphic())
                {
                    return Err(ViewIdError::Unretypable {
                        namespace: $namespace,
                        character,
                    });
                }
                if value.len() > VIEW_ID_BUDGET {
                    return Err(ViewIdError::TooLong {
                        namespace: $namespace,
                        length: value.len(),
                    });
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

view_id!(DistrictId, "district");
view_id!(StationId, "station");
view_id!(AgentId, "agent");
view_id!(AttentionId, "attention");

/// The normalized state vocabulary that selects the right expression mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Queued,
    Starting,
    Running,
    Canceling,
    NeedsAttention,
    Blocked,
    Failed,
    Done,
    Canceled,
    Unknown,
}

impl AgentState {
    fn binding_name(self) -> &'static str {
        match self {
            AgentState::Idle => "idle",
            AgentState::Queued => "queued",
            AgentState::Starting => "starting",
            AgentState::Running => "running",
            AgentState::Canceling => "canceling",
            AgentState::NeedsAttention => "needs_attention",
            AgentState::Blocked => "blocked",
            AgentState::Failed => "failed",
            AgentState::Done => "done",
            AgentState::Canceled => "canceled",
            AgentState::Unknown => "unknown",
        }
    }
}

/// One two-cell agent slot. `changed_seq` is provenance, never elapsed time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub id: AgentId,
    pub role: Option<String>,
    pub state: AgentState,
    pub changed_seq: Seq,
}

/// One horizontal station in a district.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Station {
    pub id: StationId,
    pub label: String,
    pub template_ordinal: u16,
    pub agents: Vec<Agent>,
    pub changed_seq: Seq,
}

/// One vertical district in the estate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct District {
    pub id: DistrictId,
    pub label: String,
    pub stations: Vec<Station>,
    pub changed_seq: Seq,
}

/// A mutable focus fact supplied by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Focus {
    pub district: DistrictId,
    pub changed_seq: Seq,
}

/// One attention fact and the sequence that raised or resolved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attention {
    pub id: AttentionId,
    pub district: DistrictId,
    pub unresolved: bool,
    pub changed_seq: Seq,
}

/// Everything needed to render one deterministic estate frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameInput {
    pub districts: Vec<District>,
    pub focus: Option<Focus>,
    pub attention: Vec<Attention>,
    pub watermark: Option<Seq>,
}

/// The target represented by one painted agent pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HallTarget {
    Agent(AgentId),
}

/// The literal density rung selected for the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DensityRung {
    Empty,
    Baseline,
    GutterShrink,
    DistrictCollapse,
    Paging,
}

/// A density decision, kept separate from painting for direct verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DensityPlan {
    pub rung: DensityRung,
    pub collapsed: Vec<DistrictId>,
    pub paging: usize,
}

fn active(district: &District) -> bool {
    district
        .stations
        .iter()
        .any(|station| !station.agents.is_empty())
}

fn unresolved_districts(input: &FrameInput) -> BTreeSet<&str> {
    input
        .attention
        .iter()
        .filter(|attention| attention.unresolved)
        .map(|attention| attention.district.as_str())
        .collect()
}

/// District stack order: attention first, then the typed id byte order.
pub fn district_stack_order(input: &FrameInput) -> Vec<DistrictId> {
    let pinned = unresolved_districts(input);
    let mut districts: Vec<&District> = input.districts.iter().collect();
    districts.sort_by(|left, right| {
        (!pinned.contains(left.id.as_str()))
            .cmp(&(!pinned.contains(right.id.as_str())))
            .then_with(|| left.id.cmp(&right.id))
    });
    districts
        .into_iter()
        .map(|district| district.id.clone())
        .collect()
}

/// Independent collapse order: oldest change first, excluding focus and pins.
pub fn collapse_order(input: &FrameInput) -> Vec<DistrictId> {
    let pinned = unresolved_districts(input);
    let focused = input.focus.as_ref().map(|focus| focus.district.as_str());
    let mut districts: Vec<&District> = input
        .districts
        .iter()
        .filter(|district| active(district))
        .filter(|district| Some(district.id.as_str()) != focused)
        .filter(|district| !pinned.contains(district.id.as_str()))
        .collect();
    districts.sort_by(|left, right| {
        left.changed_seq
            .cmp(&right.changed_seq)
            .then_with(|| left.id.cmp(&right.id))
    });
    districts
        .into_iter()
        .map(|district| district.id.clone())
        .collect()
}

/// Station order: template ordinal, then typed id.
pub fn station_order(district: &District) -> Vec<StationId> {
    let mut stations: Vec<&Station> = district.stations.iter().collect();
    stations.sort_by(|left, right| {
        left.template_ordinal
            .cmp(&right.template_ordinal)
            .then_with(|| left.id.cmp(&right.id))
    });
    stations
        .into_iter()
        .map(|station| station.id.clone())
        .collect()
}

/// Agent slot order: typed id byte order.
pub fn agent_order(station: &Station) -> Vec<AgentId> {
    let mut agents: Vec<&Agent> = station.agents.iter().collect();
    agents.sort_by(|left, right| left.id.cmp(&right.id));
    agents.into_iter().map(|agent| agent.id.clone()).collect()
}

fn ordered_active_districts(input: &FrameInput) -> Vec<&District> {
    let pinned = unresolved_districts(input);
    let mut districts: Vec<&District> = input
        .districts
        .iter()
        .filter(|district| active(district))
        .collect();
    districts.sort_by(|left, right| {
        (!pinned.contains(left.id.as_str()))
            .cmp(&(!pinned.contains(right.id.as_str())))
            .then_with(|| left.id.cmp(&right.id))
    });
    districts
}

fn ordered_stations(district: &District) -> Vec<&Station> {
    let mut stations: Vec<&Station> = district
        .stations
        .iter()
        .filter(|station| !station.agents.is_empty())
        .collect();
    stations.sort_by(|left, right| {
        left.template_ordinal
            .cmp(&right.template_ordinal)
            .then_with(|| left.id.cmp(&right.id))
    });
    stations
}

fn ordered_agents(station: &Station) -> Vec<&Agent> {
    let mut agents: Vec<&Agent> = station.agents.iter().collect();
    agents.sort_by(|left, right| left.id.cmp(&right.id));
    agents
}

fn safe_width(text: &str, budget: u16) -> u16 {
    let safe = theme::safe_text(text, usize::from(budget));
    u16::try_from(UnicodeWidthStr::width(safe.as_ref())).unwrap_or(u16::MAX)
}

fn agent_row_width(count: usize, gutter: u16) -> u16 {
    let count = u16::try_from(count).unwrap_or(u16::MAX);
    count
        .saturating_mul(2)
        .saturating_add(count.saturating_sub(1).saturating_mul(gutter))
}

fn station_width(station: &Station, gutter: u16, budget: u16) -> u16 {
    safe_width(&station.label, budget).max(agent_row_width(station.agents.len(), gutter))
}

fn collapsed_text(district: &District, budget: u16) -> String {
    let agents = district
        .stations
        .iter()
        .map(|station| station.agents.len())
        .sum::<usize>();
    let suffix = format!("  {agents}");
    let label_budget = budget.saturating_sub(u16::try_from(2 + suffix.len()).unwrap_or(u16::MAX));
    let label = theme::safe_text(&district.label, usize::from(label_budget));
    format!("+ {label}{suffix}")
}

fn district_width(district: &District, gutter: u16, collapsed: bool, budget: u16) -> u16 {
    if collapsed {
        return safe_width(&collapsed_text(district, budget), budget);
    }
    let stations = ordered_stations(district);
    let station_widths = stations
        .iter()
        .map(|station| station_width(station, gutter, budget))
        .fold(0u16, u16::saturating_add);
    safe_width(&district.label, budget).max(
        station_widths.saturating_add(
            u16::try_from(stations.len().saturating_sub(1))
                .unwrap_or(u16::MAX)
                .saturating_mul(STATION_GUTTER),
        ),
    )
}

fn fits(area: Rect, districts: &[&District], gutter: u16, collapsed: &BTreeSet<String>) -> bool {
    if area.width == 0 || area.height == 0 {
        return false;
    }
    let measure_budget = area.width.saturating_add(1);
    let height = districts.iter().fold(0u16, |height, district| {
        height.saturating_add(if collapsed.contains(district.id.as_str()) {
            1
        } else {
            EXPANDED_DISTRICT_HEIGHT
        })
    });
    height <= area.height
        && districts.iter().all(|district| {
            district_width(
                district,
                gutter,
                collapsed.contains(district.id.as_str()),
                measure_budget,
            ) <= area.width
        })
}

fn paging_count(
    area: Rect,
    districts: &[&District],
    gutter: u16,
    collapsed: &BTreeSet<String>,
) -> usize {
    let mut used_height = 0u16;
    let mut visible = 0usize;
    let measure_budget = area.width.saturating_add(1);
    for district in districts {
        let is_collapsed = collapsed.contains(district.id.as_str());
        let height = if is_collapsed {
            1
        } else {
            EXPANDED_DISTRICT_HEIGHT
        };
        let fits_width =
            district_width(district, gutter, is_collapsed, measure_budget) <= area.width;
        let fits_height = used_height.saturating_add(height) <= area.height;
        if !fits_width || !fits_height {
            break;
        }
        used_height = used_height.saturating_add(height);
        visible += 1;
    }
    districts.len().saturating_sub(visible).max(1)
}

/// Walk the fixed density ladder: baseline, gutter shrink, then collapses.
pub fn solve_density(area: Rect, input: &FrameInput) -> DensityPlan {
    let districts = ordered_active_districts(input);
    if districts.is_empty() {
        return DensityPlan {
            rung: DensityRung::Empty,
            collapsed: Vec::new(),
            paging: 0,
        };
    }

    let mut collapsed = BTreeSet::new();
    if fits(area, &districts, 1, &collapsed) {
        return DensityPlan {
            rung: DensityRung::Baseline,
            collapsed: Vec::new(),
            paging: 0,
        };
    }
    if fits(area, &districts, 0, &collapsed) {
        return DensityPlan {
            rung: DensityRung::GutterShrink,
            collapsed: Vec::new(),
            paging: 0,
        };
    }

    let candidates = collapse_order(input);
    for candidate in &candidates {
        collapsed.insert(candidate.as_str().to_owned());
        if fits(area, &districts, 0, &collapsed) {
            return DensityPlan {
                rung: DensityRung::DistrictCollapse,
                collapsed: candidates
                    .iter()
                    .take_while(|id| id.as_str() != candidate.as_str())
                    .cloned()
                    .chain(std::iter::once(candidate.clone()))
                    .collect(),
                paging: 0,
            };
        }
    }

    DensityPlan {
        rung: DensityRung::Paging,
        collapsed: candidates,
        paging: paging_count(area, &districts, 0, &collapsed),
    }
}

/// Full deterministic keyboard order, including agents hidden by density.
pub fn target_order(input: &FrameInput) -> Vec<HallTarget> {
    let mut targets = Vec::new();
    for district in ordered_active_districts(input) {
        for station in ordered_stations(district) {
            targets.extend(
                ordered_agents(station)
                    .into_iter()
                    .map(|agent| HallTarget::Agent(agent.id.clone())),
            );
        }
    }
    targets
}

#[derive(Debug)]
struct CanvasText {
    x: u16,
    y: u16,
    text: String,
    style: Style,
}

fn canvas_paint(area: Rect, buf: &mut Buffer, text: &[CanvasText]) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let x_max = f64::from(area.width.saturating_sub(1).max(1));
    let y_max = f64::from(area.height.saturating_sub(1).max(1));
    Canvas::default()
        .marker(Marker::Dot)
        .x_bounds([0.0, x_max])
        .y_bounds([0.0, y_max])
        .paint(|context| {
            for item in text {
                context.print(
                    f64::from(item.x),
                    y_max - f64::from(item.y),
                    Span::styled(item.text.clone(), item.style),
                );
            }
        })
        .render(area, buf);
}

fn bounded(text: &str, budget: u16) -> String {
    theme::safe_text(text, usize::from(budget)).into_owned()
}

fn identity_glyph(role: Option<&str>, glyphs: GlyphSet) -> char {
    let role_mark = role.and_then(gwk_theme::marks::mark);
    if let Some(mark) = role_mark {
        return theme::glyph(mark, 0, glyphs);
    }
    if role.is_none() {
        let mark = gwk_theme::marks::mark("role_absent").expect("role_absent mark is pinned");
        return theme::glyph(mark, 0, glyphs);
    }
    role.and_then(|name| name.bytes().find(u8::is_ascii_alphabetic))
        .map(char::from)
        .map(|character| character.to_ascii_uppercase())
        .unwrap_or('?')
}

fn expression_mark(state: AgentState) -> &'static Mark {
    let binding = theme::binding(state.binding_name());
    gwk_theme::marks::mark(binding.mark).expect("state binding mark is pinned")
}

/// Paint one estate frame and rebuild the frame-scoped two-cell hit map.
pub fn render(
    area: Rect,
    buf: &mut Buffer,
    input: &FrameInput,
    tier: ColorTier,
    glyphs: GlyphSet,
    hits: &mut HitMap<HallTarget>,
) {
    hits.clear();
    if area.width == 0 || area.height == 0 {
        return;
    }

    let plan = solve_density(area, input);
    let muted = theme::state_style(theme::binding("idle"), tier);
    let mut text = Vec::new();
    if plan.rung == DensityRung::Empty {
        let first_y = area.height.saturating_sub(2) / 2;
        let x = u16::from(area.width > 2);
        text.push(CanvasText {
            x,
            y: first_y,
            text: bounded("No active work yet", area.width.saturating_sub(x)),
            style: Style::default().add_modifier(Modifier::BOLD),
        });
        if first_y + 1 < area.height {
            text.push(CanvasText {
                x,
                y: first_y + 1,
                text: bounded(
                    "Projects and running agents appear when the ledger records them.",
                    area.width.saturating_sub(x),
                ),
                style: muted,
            });
        }
        canvas_paint(area, buf, &text);
        return;
    }

    if plan.rung == DensityRung::Paging {
        text.push(CanvasText {
            x: 0,
            y: 0,
            text: bounded(
                &format!("+{} districts need paging", plan.paging),
                area.width,
            ),
            style: theme::state_style(theme::binding("needs_attention"), tier),
        });
        canvas_paint(area, buf, &text);
        return;
    }

    let gutter = u16::from(plan.rung == DensityRung::Baseline);
    let collapsed: BTreeSet<&str> = plan.collapsed.iter().map(DistrictId::as_str).collect();
    let mut y = 0u16;
    for district in ordered_active_districts(input) {
        if collapsed.contains(district.id.as_str()) {
            text.push(CanvasText {
                x: 0,
                y,
                text: collapsed_text(district, area.width),
                style: muted,
            });
            y = y.saturating_add(1);
            continue;
        }

        text.push(CanvasText {
            x: 0,
            y,
            text: bounded(&district.label, area.width),
            style: Style::default().add_modifier(Modifier::BOLD),
        });
        let mut station_x = 0u16;
        for station in ordered_stations(district) {
            let width = station_width(station, gutter, area.width);
            text.push(CanvasText {
                x: station_x,
                y: y + 1,
                text: bounded(&station.label, width),
                style: muted,
            });
            for (index, agent) in ordered_agents(station).into_iter().enumerate() {
                let offset = u16::try_from(index)
                    .unwrap_or(u16::MAX)
                    .saturating_mul(2 + gutter);
                let x = station_x.saturating_add(offset);
                let binding = theme::binding(agent.state.binding_name());
                let style = theme::state_style(binding, tier);
                let pair = String::from_iter([
                    identity_glyph(agent.role.as_deref(), glyphs),
                    theme::glyph(expression_mark(agent.state), 0, glyphs),
                ]);
                text.push(CanvasText {
                    x,
                    y: y + 2,
                    text: pair,
                    style,
                });
                hits.register(
                    Rect::new(area.x + x, area.y + y + 2, 2, 1),
                    HallTarget::Agent(agent.id.clone()),
                );
            }
            station_x = station_x
                .saturating_add(width)
                .saturating_add(STATION_GUTTER);
        }
        y = y.saturating_add(EXPANDED_DISTRICT_HEIGHT);
    }
    canvas_paint(area, buf, &text);
}
