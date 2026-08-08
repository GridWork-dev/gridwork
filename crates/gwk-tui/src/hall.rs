//! The estate frame: deterministic spatial layout over caller-normalized facts.
//!
//! This lens fetches nothing and reads no clock. Sequence values carry only
//! ledger provenance and ordering; callers provide every fact needed to build
//! a frame. Districts stack vertically, stations run horizontally, and every
//! visible agent occupies the same two cells used by hit testing.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::rc::Rc;
use std::time::Duration;

use gwk_domain::ids::Seq;
use gwk_theme::marks::{GlyphSet, Mark};
use gwk_theme::tier::ColorTier;
use ratatui::buffer::Buffer;
use ratatui::layout::{Offset, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::Span;
use ratatui::widgets::Widget;
use ratatui::widgets::canvas::Canvas;
use tachyonfx::fx::RepeatMode;
use tachyonfx::{EffectManager, fx};
use unicode_width::UnicodeWidthStr;

use crate::input::HitMap;
use crate::theme;

const VIEW_ID_BUDGET: usize = 64;
const STATION_GUTTER: u16 = 2;
const EXPANDED_DISTRICT_HEIGHT: u16 = 3;

/// One full eight-frame expression cycle at the ruled console cadence.
pub const TICK_CYCLE: Duration = crate::input::TICK.saturating_mul(8);
/// The attention reorder movement boundary.
pub const PULSE_DURATION: Duration = Duration::from_millis(400);
/// The character-domain departure boundary.
pub const DECAY_DURATION: Duration = Duration::from_millis(600);

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
    /// Caller-formatted standing liveness text, visible even with motion off.
    pub duration: Option<String>,
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

/// The caller-resolved motion posture for this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionMode {
    Off,
    Reduced,
    Full,
}

/// The three ratified motion verbs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionVerb {
    Tick,
    Pulse,
    Decay,
}

/// A typed namespace for the ledger entity that owns an effect.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MotionEntity {
    Agent(AgentId),
    District(DistrictId),
    Attention(AttentionId),
}

/// Effect identity: entity plus the exact ledger change that triggered it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MotionKey {
    pub entity: MotionEntity,
    pub changed_seq: Seq,
}

// tachyonfx 0.25.1's derived EffectManager default unnecessarily carries a
// K: Default bound. The manager never reads this value; real effects always
// enter through add_unique_effect with a caller-provided provenance key.
impl Default for MotionKey {
    fn default() -> Self {
        Self {
            entity: MotionEntity::Agent(AgentId("agent-effect-manager".into())),
            changed_seq: Seq::new(0),
        }
    }
}

/// One caller-timed effect input. Sequence is provenance only; both time axes
/// are explicit durations and are never derived from sequence subtraction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MotionInput {
    pub key: MotionKey,
    pub verb: MotionVerb,
    pub elapsed: Duration,
    pub frame_delta: Duration,
    pub source: Rect,
    pub target: Rect,
}

/// The stateful effects driver and active inputs for one rendered frame.
pub struct MotionFrame<'a> {
    pub driver: &'a mut MotionDriver,
    pub inputs: &'a [MotionInput],
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

fn agent_slot_width(agent: &Agent, budget: u16) -> u16 {
    agent.duration.as_deref().map_or(2, |duration| {
        3u16.saturating_add(safe_width(duration, budget.saturating_sub(3)))
    })
}

fn agent_row_width(station: &Station, gutter: u16, budget: u16) -> u16 {
    let agents = ordered_agents(station);
    let slots = agents
        .iter()
        .map(|agent| agent_slot_width(agent, budget))
        .fold(0u16, u16::saturating_add);
    slots.saturating_add(
        u16::try_from(agents.len().saturating_sub(1))
            .unwrap_or(u16::MAX)
            .saturating_mul(gutter),
    )
}

fn station_width(station: &Station, gutter: u16, budget: u16) -> u16 {
    safe_width(&station.label, budget).max(agent_row_width(station, gutter, budget))
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
struct EffectSlot {
    manager: EffectManager<MotionKey>,
    verb: MotionVerb,
    elapsed: Duration,
}

impl EffectSlot {
    fn new(verb: MotionVerb) -> Self {
        Self {
            manager: EffectManager::default(),
            verb,
            elapsed: Duration::ZERO,
        }
    }
}

/// Frame-scoped tachyonfx managers keyed by entity plus ledger provenance.
#[derive(Debug)]
pub struct MotionDriver {
    mode: MotionMode,
    slots: BTreeMap<MotionKey, EffectSlot>,
}

impl MotionDriver {
    pub fn new(mode: MotionMode) -> Self {
        Self {
            mode,
            slots: BTreeMap::new(),
        }
    }

    pub fn mode(&self) -> MotionMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: MotionMode) {
        self.mode = mode;
        if mode == MotionMode::Off {
            self.slots.clear();
        }
    }

    /// Active PULSE/DECAY keys after same-key replacement.
    pub fn active_one_shots(&self) -> usize {
        self.slots
            .values()
            .filter(|slot| match slot.verb {
                MotionVerb::Tick => false,
                MotionVerb::Pulse => slot.elapsed < PULSE_DURATION,
                MotionVerb::Decay => slot.elapsed < DECAY_DURATION,
            })
            .count()
    }

    fn allows(&self, verb: MotionVerb) -> bool {
        match self.mode {
            MotionMode::Off => false,
            MotionMode::Reduced => verb == MotionVerb::Tick,
            MotionMode::Full => true,
        }
    }

    fn begin_frame(&mut self, motions: &[MotionInput]) {
        if self.mode == MotionMode::Off {
            self.slots.clear();
            return;
        }
        let active: BTreeSet<MotionKey> = motions
            .iter()
            .filter(|motion| self.allows(motion.verb))
            .map(|motion| motion.key.clone())
            .collect();
        self.slots.retain(|key, _| active.contains(key));
    }

    fn active_inputs(&self, motions: &[MotionInput]) -> Vec<MotionInput> {
        let mut latest = BTreeMap::new();
        for motion in motions.iter().filter(|motion| self.allows(motion.verb)) {
            latest.insert(motion.key.clone(), motion.clone());
        }
        latest.into_values().collect()
    }

    fn slot(&mut self, motion: &MotionInput) -> &mut EffectSlot {
        let slot = self
            .slots
            .entry(motion.key.clone())
            .or_insert_with(|| EffectSlot::new(motion.verb));
        slot.verb = motion.verb;
        slot.elapsed = motion.elapsed;
        slot
    }

    fn prepare_pulses(
        &mut self,
        motions: &[MotionInput],
        buf: &mut Buffer,
        lens: Rect,
    ) -> Vec<PulseTransform> {
        let mut transforms = Vec::new();
        for motion in motions {
            if motion.verb != MotionVerb::Pulse || !self.allows(motion.verb) {
                continue;
            }
            let current = Rc::new(RefCell::new(None));
            let probe = Rc::clone(&current);
            let inner = fx::effect_fn((), PULSE_DURATION, move |_state, context, _cells| {
                *probe.borrow_mut() = Some(context.area)
            });
            let offset = Offset {
                x: i32::from(motion.target.x) - i32::from(motion.source.x),
                y: i32::from(motion.target.y) - i32::from(motion.source.y),
            };
            let mut effect = fx::translate(inner, offset, PULSE_DURATION).with_area(motion.source);
            let (warm, delta) = split_elapsed(motion, PULSE_DURATION);
            if !warm.is_zero() {
                let mut scratch = Buffer::empty(buf.area);
                effect.process(warm, &mut scratch, motion.source);
            }
            let slot = self.slot(motion);
            slot.manager.add_unique_effect(motion.key.clone(), effect);
            slot.manager.process_effects(delta, buf, lens);
            let translated = *current.borrow();
            transforms.push(PulseTransform {
                target: motion.target,
                current: translated,
            });
        }
        transforms
    }

    fn apply_characters(
        &mut self,
        motions: &[MotionInput],
        frame: &FrameInput,
        glyphs: GlyphSet,
        buf: &mut Buffer,
        lens: Rect,
        transforms: &[PulseTransform],
    ) {
        for motion in motions {
            if !matches!(motion.verb, MotionVerb::Tick | MotionVerb::Decay)
                || !self.allows(motion.verb)
            {
                continue;
            }
            match motion.verb {
                MotionVerb::Tick => {
                    let Some(target) = transform_region(lens, motion.target, transforms) else {
                        continue;
                    };
                    let Some(area) = expression_area(target, lens) else {
                        continue;
                    };
                    let mark = tick_mark(frame, &motion.key.entity);
                    let delta = motion.frame_delta.min(motion.elapsed);
                    let state = TickState {
                        elapsed: motion.elapsed.saturating_sub(delta),
                        mark,
                        glyphs,
                    };
                    let effect = fx::repeat(
                        fx::effect_fn(state, TICK_CYCLE, |state, context, cells| {
                            state.elapsed = state.elapsed.saturating_add(context.last_tick);
                            let tick_ms = crate::input::TICK.as_millis();
                            let frame = usize::try_from((state.elapsed.as_millis() / tick_ms) % 8)
                                .unwrap_or_default();
                            let glyph = theme::glyph(state.mark, frame, state.glyphs);
                            for (_, cell) in cells {
                                cell.set_char(glyph);
                            }
                        })
                        .with_area(area),
                        RepeatMode::Forever,
                    );
                    let slot = self.slot(motion);
                    slot.manager.add_unique_effect(motion.key.clone(), effect);
                    slot.manager.process_effects(delta, buf, lens);
                }
                MotionVerb::Decay => {
                    let Some(area) = transform_region(lens, motion.target, transforms) else {
                        continue;
                    };
                    let mut effect = fx::effect_fn((), DECAY_DURATION, |_state, context, cells| {
                        for (_, cell) in cells {
                            if context.alpha() >= 1.0 {
                                cell.set_char(' ');
                            } else if context.alpha() >= 0.5 {
                                cell.set_char('.');
                            }
                        }
                    })
                    .with_area(area);
                    let (warm, delta) = split_elapsed(motion, DECAY_DURATION);
                    if !warm.is_zero() {
                        let mut scratch = Buffer::empty(buf.area);
                        effect.process(warm, &mut scratch, area);
                    }
                    let slot = self.slot(motion);
                    slot.manager.add_unique_effect(motion.key.clone(), effect);
                    slot.manager.process_effects(delta, buf, lens);
                }
                MotionVerb::Pulse => {}
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PulseTransform {
    target: Rect,
    current: Option<Rect>,
}

#[derive(Clone)]
struct TickState {
    elapsed: Duration,
    mark: &'static Mark,
    glyphs: GlyphSet,
}

fn split_elapsed(motion: &MotionInput, boundary: Duration) -> (Duration, Duration) {
    let elapsed = motion.elapsed.min(boundary);
    let mut delta = motion.frame_delta.min(elapsed);
    if delta.is_zero() && !elapsed.is_zero() {
        delta = crate::input::TICK.min(elapsed);
    }
    (elapsed.saturating_sub(delta), delta)
}

fn expression_area(target: Rect, lens: Rect) -> Option<Rect> {
    intersect_rect(Rect::new(target.x, target.y, 1, 1), lens)
}

fn tick_mark(frame: &FrameInput, entity: &MotionEntity) -> &'static Mark {
    let state = match entity {
        MotionEntity::Agent(id) => frame
            .districts
            .iter()
            .flat_map(|district| &district.stations)
            .flat_map(|station| &station.agents)
            .find(|agent| agent.id == *id)
            .map_or(AgentState::Unknown, |agent| agent.state),
        MotionEntity::District(_) | MotionEntity::Attention(_) => AgentState::Unknown,
    };
    expression_mark(state)
}

#[derive(Debug, Clone)]
struct CanvasText {
    x: u16,
    y: u16,
    text: String,
    style: Style,
    agent_pair: bool,
}

fn intersect_rect(left: Rect, right: Rect) -> Option<Rect> {
    let x = left.x.max(right.x);
    let y = left.y.max(right.y);
    let right_edge = left.right().min(right.right());
    let bottom_edge = left.bottom().min(right.bottom());
    (right_edge > x && bottom_edge > y).then(|| Rect::new(x, y, right_edge - x, bottom_edge - y))
}

fn rect_inside(inner: Rect, outer: Rect) -> bool {
    inner.width > 0
        && inner.height > 0
        && inner.x >= outer.x
        && inner.y >= outer.y
        && inner.right() <= outer.right()
        && inner.bottom() <= outer.bottom()
}

fn shift_rect(rect: Rect, dx: i32, dy: i32) -> Option<Rect> {
    let x = i32::from(rect.x).checked_add(dx)?;
    let y = i32::from(rect.y).checked_add(dy)?;
    Some(Rect::new(
        u16::try_from(x).ok()?,
        u16::try_from(y).ok()?,
        rect.width,
        rect.height,
    ))
}

fn pulse_for(position: Position, transforms: &[PulseTransform]) -> Option<PulseTransform> {
    transforms
        .iter()
        .rev()
        .find(|transform| transform.target.contains(position))
        .copied()
}

fn transform_text(
    area: Rect,
    item: &CanvasText,
    transforms: &[PulseTransform],
) -> Option<CanvasText> {
    let absolute = Position::new(area.x + item.x, area.y + item.y);
    let Some(transform) = pulse_for(absolute, transforms) else {
        return Some(item.clone());
    };
    let current = transform.current?;
    let dx = i32::from(current.x) - i32::from(transform.target.x);
    let dy = i32::from(current.y) - i32::from(transform.target.y);
    let footprint = Rect::new(absolute.x, absolute.y, u16::from(item.agent_pair) + 1, 1);
    let shifted = shift_rect(footprint, dx, dy)?;
    let visible = intersect_rect(current, area)?;
    if item.agent_pair {
        if !rect_inside(shifted, visible) || !rect_inside(shifted, area) {
            return None;
        }
    } else if !visible.contains(Position::new(shifted.x, shifted.y)) {
        return None;
    }
    Some(CanvasText {
        x: shifted.x - area.x,
        y: shifted.y - area.y,
        ..item.clone()
    })
}

fn transform_region(area: Rect, region: Rect, transforms: &[PulseTransform]) -> Option<Rect> {
    let Some(transform) = pulse_for(Position::new(region.x, region.y), transforms) else {
        return rect_inside(region, area).then_some(region);
    };
    let current = transform.current?;
    let dx = i32::from(current.x) - i32::from(transform.target.x);
    let dy = i32::from(current.y) - i32::from(transform.target.y);
    let shifted = shift_rect(region, dx, dy)?;
    let visible = intersect_rect(current, area)?;
    (rect_inside(shifted, visible) && rect_inside(shifted, area)).then_some(shifted)
}

fn paint_frame(
    area: Rect,
    buf: &mut Buffer,
    hits: &mut HitMap<HallTarget>,
    text: &[CanvasText],
    regions: &[(Rect, HallTarget)],
    transforms: &[PulseTransform],
) {
    let transformed: Vec<CanvasText> = text
        .iter()
        .filter_map(|item| transform_text(area, item, transforms))
        .collect();
    canvas_paint(area, buf, &transformed);
    for (region, target) in regions {
        if let Some(region) = transform_region(area, *region, transforms) {
            hits.register(region, target.clone());
        }
    }
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

/// Paint one frame with every expression frozen at inventory frame zero.
pub fn render(
    area: Rect,
    buf: &mut Buffer,
    input: &FrameInput,
    tier: ColorTier,
    glyphs: GlyphSet,
    hits: &mut HitMap<HallTarget>,
) {
    render_frame(area, buf, input, tier, glyphs, hits, &[]);
}

/// Paint one frame with caller-timed tachyonfx motion.
pub fn render_with_motion(
    area: Rect,
    buf: &mut Buffer,
    input: &FrameInput,
    tier: ColorTier,
    glyphs: GlyphSet,
    hits: &mut HitMap<HallTarget>,
    motion: MotionFrame<'_>,
) {
    let active = motion.driver.active_inputs(motion.inputs);
    motion.driver.begin_frame(&active);
    let transforms = motion.driver.prepare_pulses(&active, buf, area);
    render_frame(area, buf, input, tier, glyphs, hits, &transforms);
    motion
        .driver
        .apply_characters(&active, input, glyphs, buf, area, &transforms);
}

fn render_frame(
    area: Rect,
    buf: &mut Buffer,
    input: &FrameInput,
    tier: ColorTier,
    glyphs: GlyphSet,
    hits: &mut HitMap<HallTarget>,
    transforms: &[PulseTransform],
) {
    hits.clear();
    if area.width == 0 || area.height == 0 {
        return;
    }

    let plan = solve_density(area, input);
    let muted = theme::state_style(theme::binding("idle"), tier);
    let mut text = Vec::new();
    let mut regions = Vec::new();
    if plan.rung == DensityRung::Empty {
        let first_y = area.height.saturating_sub(2) / 2;
        let x = u16::from(area.width > 2);
        text.push(CanvasText {
            x,
            y: first_y,
            text: bounded("No active work yet", area.width.saturating_sub(x)),
            style: Style::default().add_modifier(Modifier::BOLD),
            agent_pair: false,
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
                agent_pair: false,
            });
        }
        paint_frame(area, buf, hits, &text, &regions, transforms);
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
            agent_pair: false,
        });
        paint_frame(area, buf, hits, &text, &regions, transforms);
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
                agent_pair: false,
            });
            y = y.saturating_add(1);
            continue;
        }

        text.push(CanvasText {
            x: 0,
            y,
            text: bounded(&district.label, area.width),
            style: Style::default().add_modifier(Modifier::BOLD),
            agent_pair: false,
        });
        let mut station_x = 0u16;
        for station in ordered_stations(district) {
            let width = station_width(station, gutter, area.width);
            text.push(CanvasText {
                x: station_x,
                y: y + 1,
                text: bounded(&station.label, width),
                style: muted,
                agent_pair: false,
            });
            let mut agent_x = station_x;
            for agent in ordered_agents(station) {
                let x = agent_x;
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
                    agent_pair: true,
                });
                regions.push((
                    Rect::new(area.x + x, area.y + y + 2, 2, 1),
                    HallTarget::Agent(agent.id.clone()),
                ));
                if let Some(duration) = &agent.duration {
                    let duration_x = x.saturating_add(3);
                    let duration_budget = agent_slot_width(agent, area.width).saturating_sub(3);
                    text.push(CanvasText {
                        x: duration_x,
                        y: y + 2,
                        text: bounded(duration, duration_budget),
                        style: muted,
                        agent_pair: false,
                    });
                }
                agent_x = agent_x
                    .saturating_add(agent_slot_width(agent, area.width))
                    .saturating_add(gutter);
            }
            station_x = station_x
                .saturating_add(width)
                .saturating_add(STATION_GUTTER);
        }
        y = y.saturating_add(EXPANDED_DISTRICT_HEIGHT);
    }
    paint_frame(area, buf, hits, &text, &regions, transforms);
}
