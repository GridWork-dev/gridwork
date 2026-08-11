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
    /// Terminal-state agents older than the caller's aging boundary, collapsed
    /// out of the agent slots into one heading count.
    pub aged_done: usize,
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

/// Motion inputs plus caller-owned page cursors for one rendered frame.
pub struct PagedMotionFrame<'a> {
    pub pages: &'a PagingState,
    pub motion: MotionFrame<'a>,
}

/// The literal density rung selected for the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DensityRung {
    Empty,
    Baseline,
    GutterShrink,
    DistrictCollapse,
    PairShrink,
    Paging,
}

/// A density decision, kept separate from painting for direct verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DensityPlan {
    pub rung: DensityRung,
    pub collapsed: Vec<DistrictId>,
    pub paging: usize,
}

/// Caller-owned page cursors. Each district cycles its agents independently.
/// The district cursor rotates every visible district so even an urgent set
/// taller than the viewport remains reachable; page zero stays attention-first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PagingState {
    district_page: usize,
    agent_pages: BTreeMap<DistrictId, usize>,
}

impl PagingState {
    pub fn next(&mut self, district: &DistrictId) {
        let page = self.agent_pages.entry(district.clone()).or_default();
        *page = page.saturating_add(1);
    }

    pub fn previous(&mut self, district: &DistrictId) {
        let page = self.agent_pages.entry(district.clone()).or_default();
        *page = page.saturating_sub(1);
    }

    pub fn next_district(&mut self) {
        self.district_page = self.district_page.saturating_add(1);
    }

    pub fn previous_district(&mut self) {
        self.district_page = self.district_page.saturating_sub(1);
    }

    fn agent_page(&self, district: &DistrictId) -> usize {
        self.agent_pages.get(district).copied().unwrap_or_default()
    }
}

fn active(district: &District) -> bool {
    district.aged_done > 0
        || district
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

fn unresolved_attention_count(input: &FrameInput, district: &DistrictId) -> usize {
    input
        .attention
        .iter()
        .filter(|attention| attention.unresolved && attention.district == *district)
        .count()
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

fn ordered_visible_districts(input: &FrameInput) -> Vec<&District> {
    let pinned = unresolved_districts(input);
    let mut districts: Vec<&District> = input
        .districts
        .iter()
        .filter(|district| active(district) || pinned.contains(district.id.as_str()))
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

fn agent_slot_width(agent: &Agent, budget: u16, pair_width: u16) -> u16 {
    if pair_width == 1 {
        return 1;
    }
    agent.duration.as_deref().map_or(2, |duration| {
        3u16.saturating_add(safe_width(duration, budget.saturating_sub(3)))
    })
}

fn agent_row_width(station: &Station, gutter: u16, pair_width: u16, budget: u16) -> u16 {
    let agents = ordered_agents(station);
    let slots = agents
        .iter()
        .map(|agent| agent_slot_width(agent, budget, pair_width))
        .fold(0u16, u16::saturating_add);
    slots.saturating_add(
        u16::try_from(agents.len().saturating_sub(1))
            .unwrap_or(u16::MAX)
            .saturating_mul(gutter),
    )
}

fn station_width(station: &Station, gutter: u16, pair_width: u16, budget: u16) -> u16 {
    safe_width(&station.label, budget).max(agent_row_width(station, gutter, pair_width, budget))
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

fn district_heading(district: &District, input: &FrameInput, budget: u16) -> String {
    let attention = unresolved_attention_count(input, &district.id);
    if district.aged_done == 0 {
        return attention_text(&district.label, attention, budget);
    }
    let suffix = format!(" +{} done", district.aged_done);
    let suffix_width = safe_width(&suffix, budget);
    if suffix_width >= budget {
        return attention_text(&district.label, attention, budget);
    }
    let head = attention_text(
        &district.label,
        attention,
        budget.saturating_sub(suffix_width),
    );
    format!("{head}{suffix}")
}

fn attention_text(label: &str, attention: usize, budget: u16) -> String {
    if attention == 0 {
        return bounded(label, budget);
    }
    let cue = format!("!{attention}");
    let cue_width = safe_width(&cue, budget);
    if cue_width >= budget {
        return bounded(&cue, budget);
    }
    let label = bounded(label, budget.saturating_sub(cue_width.saturating_add(1)));
    if label.is_empty() {
        bounded(&cue, budget)
    } else {
        format!("{label} {cue}")
    }
}

fn district_width(
    district: &District,
    input: &FrameInput,
    gutter: u16,
    pair_width: u16,
    collapsed: bool,
    budget: u16,
) -> u16 {
    if collapsed {
        return safe_width(&collapsed_text(district, budget), budget);
    }
    let stations = ordered_stations(district);
    let station_widths = stations
        .iter()
        .map(|station| station_width(station, gutter, pair_width, budget))
        .fold(0u16, u16::saturating_add);
    let heading_budget = if unresolved_attention_count(input, &district.id) == 0 {
        budget
    } else {
        budget.saturating_sub(1)
    };
    safe_width(
        &district_heading(district, input, heading_budget),
        heading_budget,
    )
    .max(
        station_widths.saturating_add(
            u16::try_from(stations.len().saturating_sub(1))
                .unwrap_or(u16::MAX)
                .saturating_mul(STATION_GUTTER),
        ),
    )
}

fn fits(
    area: Rect,
    input: &FrameInput,
    districts: &[&District],
    gutter: u16,
    pair_width: u16,
    collapsed: &BTreeSet<String>,
) -> bool {
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
                input,
                gutter,
                pair_width,
                collapsed.contains(district.id.as_str()),
                measure_budget,
            ) <= area.width
        })
}

fn paging_count(
    area: Rect,
    input: &FrameInput,
    districts: &[&District],
    gutter: u16,
    pair_width: u16,
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
        let fits_width = district_width(
            district,
            input,
            gutter,
            pair_width,
            is_collapsed,
            measure_budget,
        ) <= area.width;
        let fits_height = used_height.saturating_add(height) <= area.height;
        if !fits_width || !fits_height {
            break;
        }
        used_height = used_height.saturating_add(height);
        visible += 1;
    }
    districts.len().saturating_sub(visible).max(1)
}

/// Walk the fixed density ladder: baseline, gutter shrink, collapses, one-cell
/// expression glyphs, then paging.
///
/// This ladder compacts Hall layout only; it does not raise the hosted-screen
/// transport ceiling. A full screen is practically limited to about 13.8k
/// cells by the 2 MiB publish budget at roughly 152 serialized bytes per cell,
/// until the wire gains a compacter cell serialization.
pub fn solve_density(area: Rect, input: &FrameInput) -> DensityPlan {
    let districts = ordered_visible_districts(input);
    if districts.is_empty() {
        return DensityPlan {
            rung: DensityRung::Empty,
            collapsed: Vec::new(),
            paging: 0,
        };
    }

    let mut collapsed = BTreeSet::new();
    if fits(area, input, &districts, 1, 2, &collapsed) {
        return DensityPlan {
            rung: DensityRung::Baseline,
            collapsed: Vec::new(),
            paging: 0,
        };
    }
    if fits(area, input, &districts, 0, 2, &collapsed) {
        return DensityPlan {
            rung: DensityRung::GutterShrink,
            collapsed: Vec::new(),
            paging: 0,
        };
    }

    let candidates = collapse_order(input);
    for candidate in &candidates {
        collapsed.insert(candidate.as_str().to_owned());
        if fits(area, input, &districts, 0, 2, &collapsed) {
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

    if fits(area, input, &districts, 0, 1, &collapsed) {
        return DensityPlan {
            rung: DensityRung::PairShrink,
            collapsed: candidates,
            paging: 0,
        };
    }

    DensityPlan {
        rung: DensityRung::Paging,
        collapsed: candidates,
        paging: paging_count(area, input, &districts, 0, 1, &collapsed),
    }
}

/// Full deterministic keyboard order, including agents hidden by density.
pub fn target_order(input: &FrameInput) -> Vec<HallTarget> {
    let mut targets = Vec::new();
    for district in ordered_visible_districts(input) {
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

#[derive(Clone, Copy)]
struct MotionGeometry<'a> {
    lens: Rect,
    agent_regions: &'a BTreeMap<AgentId, Rect>,
    hits: &'a HitMap<HallTarget>,
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

    fn admits(&self, motion: &MotionInput) -> bool {
        self.allows(motion.verb)
            && (motion.verb == MotionVerb::Pulse
                || matches!(&motion.key.entity, MotionEntity::Agent(_)))
    }

    fn begin_frame(&mut self, motions: &[MotionInput]) {
        if self.mode == MotionMode::Off {
            self.slots.clear();
            return;
        }
        let active: BTreeSet<MotionKey> = motions
            .iter()
            .filter(|motion| self.admits(motion))
            .map(|motion| motion.key.clone())
            .collect();
        self.slots.retain(|key, _| active.contains(key));
    }

    fn active_inputs(&self, motions: &[MotionInput]) -> Vec<MotionInput> {
        let mut latest = BTreeMap::new();
        for motion in motions.iter().filter(|motion| self.admits(motion)) {
            latest.insert(motion.key.clone(), motion.clone());
        }
        latest.into_values().collect()
    }

    fn slot(&mut self, motion: &MotionInput) -> &mut EffectSlot {
        let slot = self
            .slots
            .entry(motion.key.clone())
            .or_insert_with(|| EffectSlot::new(motion.verb));
        // Absolute elapsed reconstructs the shader each frame; retaining it would
        // let tachyonfx process a cancelled effect once at stale geometry.
        slot.manager = EffectManager::default();
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
        geometry: MotionGeometry<'_>,
    ) {
        for motion in motions {
            if !matches!(motion.verb, MotionVerb::Tick | MotionVerb::Decay)
                || !self.allows(motion.verb)
            {
                continue;
            }
            let MotionEntity::Agent(id) = &motion.key.entity else {
                continue;
            };
            let Some(area) = agent_expression_area(id, geometry.agent_regions, geometry.hits)
            else {
                continue;
            };
            match motion.verb {
                MotionVerb::Tick => {
                    let mark = tick_mark(frame, id);
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
                    slot.manager.process_effects(delta, buf, geometry.lens);
                }
                MotionVerb::Decay => {
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
                    slot.manager.process_effects(delta, buf, geometry.lens);
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

fn agent_expression_area(
    id: &AgentId,
    regions: &BTreeMap<AgentId, Rect>,
    hits: &HitMap<HallTarget>,
) -> Option<Rect> {
    let pair = *regions.get(id)?;
    if !matches!(pair.width, 1 | 2) || pair.height != 1 {
        return None;
    }
    let owns_pair = (pair.x..pair.right())
        .all(|x| matches!(hits.hit(x, pair.y), Some(HallTarget::Agent(owner)) if owner == id));
    owns_pair.then(|| {
        Rect::new(
            pair.x.saturating_add(pair.width.saturating_sub(1)),
            pair.y,
            1,
            1,
        )
    })
}

fn tick_mark(frame: &FrameInput, id: &AgentId) -> &'static Mark {
    let state = frame
        .districts
        .iter()
        .flat_map(|district| &district.stations)
        .flat_map(|station| &station.agents)
        .find(|agent| agent.id == *id)
        .map_or(AgentState::Unknown, |agent| agent.state);
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

#[derive(Clone, Copy)]
struct PaintContext<'a> {
    tier: ColorTier,
    glyphs: GlyphSet,
    pages: &'a PagingState,
    transforms: &'a [PulseTransform],
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
) -> BTreeMap<AgentId, Rect> {
    let mut transformed = Vec::new();
    for moving in [false, true] {
        for item in text {
            let position = Position::new(area.x + item.x, area.y + item.y);
            if pulse_for(position, transforms).is_some() != moving {
                continue;
            }
            if let Some(item) = transform_text(area, item, transforms) {
                transformed.push(item);
            }
        }
    }
    canvas_paint(area, buf, &transformed);
    let mut visible_agents = BTreeMap::new();
    for moving in [false, true] {
        for (region, target) in regions {
            if pulse_for(Position::new(region.x, region.y), transforms).is_some() != moving {
                continue;
            }
            if let Some(region) = transform_region(area, *region, transforms) {
                hits.register(region, target.clone());
                let HallTarget::Agent(id) = target;
                visible_agents.insert(id.clone(), region);
            }
        }
    }
    visible_agents
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

fn attention_rank(state: AgentState) -> u8 {
    match state {
        AgentState::NeedsAttention => 0,
        AgentState::Blocked => 1,
        AgentState::Failed => 2,
        AgentState::Running | AgentState::Canceling => 3,
        AgentState::Queued | AgentState::Starting => 4,
        AgentState::Idle | AgentState::Unknown => 5,
        AgentState::Done | AgentState::Canceled => 6,
    }
}

struct AgentPage<'a> {
    agents: Vec<&'a Agent>,
    hidden: usize,
    current: usize,
    count: usize,
}

fn agent_page<'a>(district: &'a District, width: u16, requested: usize) -> AgentPage<'a> {
    let mut agents: Vec<&Agent> = district
        .stations
        .iter()
        .flat_map(|station| &station.agents)
        .collect();
    agents.sort_by(|left, right| {
        attention_rank(left.state)
            .cmp(&attention_rank(right.state))
            .then_with(|| left.id.cmp(&right.id))
    });
    let total = agents.len();
    if total <= usize::from(width) {
        return AgentPage {
            agents,
            hidden: 0,
            current: 0,
            count: 1,
        };
    }

    let pinned = agents
        .iter()
        .take_while(|agent| agent.state == AgentState::NeedsAttention)
        .copied()
        .collect::<Vec<_>>();
    let width = usize::from(width);
    let suffix_width = format!(" +{total}").len().min(width);
    let capacity = width.saturating_sub(suffix_width).max(1);
    if pinned.len() >= capacity {
        let count = total.div_ceil(capacity).max(1);
        let current = requested % count;
        let start = current.saturating_mul(capacity);
        let visible = agents
            .iter()
            .skip(start)
            .take(capacity)
            .copied()
            .collect::<Vec<_>>();
        return AgentPage {
            hidden: total.saturating_sub(visible.len()),
            agents: visible,
            current,
            count,
        };
    }

    let rest = agents[pinned.len()..].to_vec();
    let rest_capacity = capacity.saturating_sub(pinned.len()).max(1);
    let count = rest.len().div_ceil(rest_capacity).max(1);
    let current = requested % count;
    let start = current.saturating_mul(rest_capacity);
    let mut visible = pinned;
    visible.extend(rest.iter().skip(start).take(rest_capacity).copied());
    AgentPage {
        hidden: total.saturating_sub(visible.len()),
        agents: visible,
        current,
        count,
    }
}

fn paged_districts<'a>(input: &'a FrameInput, pages: &PagingState) -> Vec<&'a District> {
    let mut districts = ordered_visible_districts(input);
    if !districts.is_empty() {
        let offset = pages.district_page % districts.len();
        districts.rotate_left(offset);
    }
    districts
}

/// The district rectangle used by non-paged motion. Dense paging has no stable
/// off-page geometry, so callers suppress movement there rather than guess.
pub fn district_region(area: Rect, input: &FrameInput, district_id: &DistrictId) -> Option<Rect> {
    let plan = solve_density(area, input);
    if matches!(plan.rung, DensityRung::Empty | DensityRung::Paging) {
        return None;
    }
    let collapsed: BTreeSet<&str> = plan.collapsed.iter().map(DistrictId::as_str).collect();
    let mut y = 0u16;
    for district in ordered_visible_districts(input) {
        let height = if collapsed.contains(district.id.as_str()) {
            1
        } else {
            EXPANDED_DISTRICT_HEIGHT
        };
        if district.id == *district_id {
            return (y.saturating_add(height) <= area.height)
                .then(|| Rect::new(area.x, area.y.saturating_add(y), area.width, height));
        }
        y = y.saturating_add(height);
    }
    None
}

/// The style a district heading paints with. The focused district carries
/// the ratified `focus` SIGNAL token patched over the bold base — accent
/// color at the color tiers, reverse video at Ansi16/Mono, the one attribute
/// every tier can express — so j/k has visible feedback at every density
/// rung instead of only steering which district survives a collapse.
fn heading_style(is_focused: bool, tier: ColorTier) -> Style {
    let base = Style::default().add_modifier(Modifier::BOLD);
    if !is_focused {
        return base;
    }
    gwk_theme::SIGNAL
        .iter()
        .find(|token| token.name == "focus")
        .map_or(base, |token| base.patch(theme::token_style(token, tier)))
}

fn render_paging_frame(
    area: Rect,
    buf: &mut Buffer,
    input: &FrameInput,
    hits: &mut HitMap<HallTarget>,
    plan: &DensityPlan,
    context: PaintContext<'_>,
) -> BTreeMap<AgentId, Rect> {
    let muted = theme::state_style(theme::binding("idle"), context.tier);
    let collapsed: BTreeSet<&str> = plan.collapsed.iter().map(DistrictId::as_str).collect();
    let mut text = Vec::new();
    let mut regions = Vec::new();
    let mut y = 0u16;
    let mut hidden_districts = 0usize;
    let districts = paged_districts(input, context.pages);
    for (index, district) in districts.iter().copied().enumerate() {
        let is_collapsed = collapsed.contains(district.id.as_str());
        let height = if is_collapsed {
            1
        } else {
            EXPANDED_DISTRICT_HEIGHT
        };
        if y.saturating_add(height) > area.height {
            hidden_districts = districts.len().saturating_sub(index);
            break;
        }
        if is_collapsed {
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
            text: district_heading(district, input, area.width),
            style: heading_style(
                input
                    .focus
                    .as_ref()
                    .is_some_and(|focus| focus.district == district.id),
                context.tier,
            ),
            agent_pair: false,
        });
        let page = agent_page(district, area.width, context.pages.agent_page(&district.id));
        text.push(CanvasText {
            x: 0,
            y: y + 1,
            text: bounded(
                &format!("page {}/{}", page.current.saturating_add(1), page.count),
                area.width,
            ),
            style: muted,
            agent_pair: false,
        });
        let mut x = 0u16;
        for agent in page.agents {
            let binding = theme::binding(agent.state.binding_name());
            text.push(CanvasText {
                x,
                y: y + 2,
                text: theme::glyph(expression_mark(agent.state), 0, context.glyphs).to_string(),
                style: theme::state_style(binding, context.tier),
                agent_pair: false,
            });
            regions.push((
                Rect::new(area.x + x, area.y + y + 2, 1, 1),
                HallTarget::Agent(agent.id.clone()),
            ));
            x = x.saturating_add(1);
        }
        if page.hidden > 0 && x < area.width {
            text.push(CanvasText {
                x,
                y: y + 2,
                text: bounded(&format!(" +{}", page.hidden), area.width.saturating_sub(x)),
                style: muted,
                agent_pair: false,
            });
        }
        y = y.saturating_add(EXPANDED_DISTRICT_HEIGHT);
    }

    if hidden_districts > 0 {
        let first = districts
            .first()
            .map(|district| district_heading(district, input, area.width))
            .unwrap_or_default();
        text.push(CanvasText {
            x: 0,
            // Headings and collapsed summaries own no hit targets. Replacing
            // the first one names overflow without covering an agent cell.
            y: 0,
            text: bounded(
                &format!("{first} +{hidden_districts} districts paging"),
                area.width,
            ),
            style: theme::state_style(theme::binding("needs_attention"), context.tier),
            agent_pair: false,
        });
    }
    paint_frame(area, buf, hits, &text, &regions, context.transforms)
}

fn identity_glyph(role: Option<&str>, glyphs: GlyphSet) -> char {
    let Some(name) = role else {
        let mark = gwk_theme::marks::mark("role_absent").expect("role_absent mark is pinned");
        return theme::glyph(mark, 0, glyphs);
    };
    // The wire's role vocabulary is open, and the real roster names are
    // fleet-prefixed families — `gw-code-reviewer`, `gw-security-auditor` —
    // while the ratified identity marks carry the bare family words. Strip
    // the fleet prefix, then try the whole name and its trailing family word
    // against the identity marks, so a roster role lands on its family's
    // mark instead of collapsing to a first-letter 'G' with the rest of the
    // fleet. Only identity marks qualify: a role that happens to share an
    // expression or graph-tier name is not that thing.
    let family = name.strip_prefix("gw-").unwrap_or(name);
    let identity = |candidate: &str| {
        gwk_theme::marks::mark(candidate)
            .filter(|mark| mark.kind == gwk_theme::marks::MarkKind::Identity)
    };
    let family_mark = identity(family).or_else(|| family.rsplit('-').next().and_then(identity));
    if let Some(mark) = family_mark {
        return theme::glyph(mark, 0, glyphs);
    }
    // Unrecognized names stay deterministic without collapsing: the letter
    // comes from the name AFTER the fleet prefix, so `gw-rust-pro` reads 'R'
    // where every `gw-*` role once read 'G'.
    family
        .bytes()
        .find(u8::is_ascii_alphabetic)
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
    let pages = PagingState::default();
    render_frame(
        area,
        buf,
        input,
        hits,
        PaintContext {
            tier,
            glyphs,
            pages: &pages,
            transforms: &[],
        },
    );
}

/// Paint one frame with caller-owned page cursors and no motion.
pub fn render_with_pages(
    area: Rect,
    buf: &mut Buffer,
    input: &FrameInput,
    tier: ColorTier,
    glyphs: GlyphSet,
    hits: &mut HitMap<HallTarget>,
    pages: &PagingState,
) {
    render_frame(
        area,
        buf,
        input,
        hits,
        PaintContext {
            tier,
            glyphs,
            pages,
            transforms: &[],
        },
    );
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
    let pages = PagingState::default();
    render_with_motion_and_pages(
        area,
        buf,
        input,
        tier,
        glyphs,
        hits,
        PagedMotionFrame {
            pages: &pages,
            motion,
        },
    );
}

/// Paint one caller-paged frame with caller-timed tachyonfx motion.
pub fn render_with_motion_and_pages(
    area: Rect,
    buf: &mut Buffer,
    input: &FrameInput,
    tier: ColorTier,
    glyphs: GlyphSet,
    hits: &mut HitMap<HallTarget>,
    paged: PagedMotionFrame<'_>,
) {
    let PagedMotionFrame { pages, motion } = paged;
    let active = motion.driver.active_inputs(motion.inputs);
    motion.driver.begin_frame(&active);
    let transforms = motion.driver.prepare_pulses(&active, buf, area);
    let agent_regions = render_frame(
        area,
        buf,
        input,
        hits,
        PaintContext {
            tier,
            glyphs,
            pages,
            transforms: &transforms,
        },
    );
    motion.driver.apply_characters(
        &active,
        input,
        glyphs,
        buf,
        MotionGeometry {
            lens: area,
            agent_regions: &agent_regions,
            hits,
        },
    );
}

fn render_frame(
    area: Rect,
    buf: &mut Buffer,
    input: &FrameInput,
    hits: &mut HitMap<HallTarget>,
    context: PaintContext<'_>,
) -> BTreeMap<AgentId, Rect> {
    hits.clear();
    if area.width == 0 || area.height == 0 {
        return BTreeMap::new();
    }

    let plan = solve_density(area, input);
    let muted = theme::state_style(theme::binding("idle"), context.tier);
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
        return paint_frame(area, buf, hits, &text, &regions, context.transforms);
    }

    if plan.rung == DensityRung::Paging {
        return render_paging_frame(area, buf, input, hits, &plan, context);
    }

    let gutter = u16::from(plan.rung == DensityRung::Baseline);
    let pair_width = u16::from(plan.rung != DensityRung::PairShrink) + 1;
    let collapsed: BTreeSet<&str> = plan.collapsed.iter().map(DistrictId::as_str).collect();
    let mut y = 0u16;
    for district in ordered_visible_districts(input) {
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
            text: district_heading(district, input, area.width),
            style: heading_style(
                input
                    .focus
                    .as_ref()
                    .is_some_and(|focus| focus.district == district.id),
                context.tier,
            ),
            agent_pair: false,
        });
        let mut station_x = 0u16;
        for station in ordered_stations(district) {
            let width = station_width(station, gutter, pair_width, area.width);
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
                let style = theme::state_style(binding, context.tier);
                let pair = if pair_width == 1 {
                    theme::glyph(expression_mark(agent.state), 0, context.glyphs).to_string()
                } else {
                    String::from_iter([
                        identity_glyph(agent.role.as_deref(), context.glyphs),
                        theme::glyph(expression_mark(agent.state), 0, context.glyphs),
                    ])
                };
                text.push(CanvasText {
                    x,
                    y: y + 2,
                    text: pair,
                    style,
                    agent_pair: pair_width == 2,
                });
                regions.push((
                    Rect::new(area.x + x, area.y + y + 2, pair_width, 1),
                    HallTarget::Agent(agent.id.clone()),
                ));
                if pair_width == 2
                    && let Some(duration) = &agent.duration
                {
                    let duration_x = x.saturating_add(3);
                    let duration_budget =
                        agent_slot_width(agent, area.width, pair_width).saturating_sub(3);
                    text.push(CanvasText {
                        x: duration_x,
                        y: y + 2,
                        text: bounded(duration, duration_budget),
                        style: muted,
                        agent_pair: false,
                    });
                }
                agent_x = agent_x
                    .saturating_add(agent_slot_width(agent, area.width, pair_width))
                    .saturating_add(gutter);
            }
            station_x = station_x
                .saturating_add(width)
                .saturating_add(STATION_GUTTER);
        }
        y = y.saturating_add(EXPANDED_DISTRICT_HEIGHT);
    }
    paint_frame(area, buf, hits, &text, &regions, context.transforms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roster_roles_resolve_family_marks_instead_of_collapsing() {
        // Fleet-prefixed roster names land on their family's identity mark at
        // both tiers, exactly as the bare family words always have.
        assert_eq!(identity_glyph(Some("reviewer"), GlyphSet::Ascii), 'R');
        assert_eq!(
            identity_glyph(Some("gw-code-reviewer"), GlyphSet::Ascii),
            'R'
        );
        assert_eq!(
            identity_glyph(Some("gw-code-reviewer"), GlyphSet::Unicode),
            '◃'
        );
        assert_eq!(
            identity_glyph(Some("gw-security-auditor"), GlyphSet::Ascii),
            'U'
        );
        assert_eq!(
            identity_glyph(Some("gw-phase-researcher"), GlyphSet::Ascii),
            'S'
        );
        assert_eq!(identity_glyph(Some("gw-architect"), GlyphSet::Ascii), 'A');
        assert_eq!(
            identity_glyph(Some("gw-orchestrator"), GlyphSet::Ascii),
            'O'
        );
        // Unmapped roles stay deterministic without collapsing: the letter
        // follows the name after the fleet prefix, so the fleet no longer
        // reads as a column of 'G'.
        assert_eq!(identity_glyph(Some("gw-rust-pro"), GlyphSet::Ascii), 'R');
        assert_eq!(
            identity_glyph(Some("gw-typescript-pro"), GlyphSet::Ascii),
            'T'
        );
        assert_eq!(
            identity_glyph(Some("general-purpose"), GlyphSet::Ascii),
            'G'
        );
        // A role sharing a non-identity mark's name is not that thing: no
        // graph-tier or expression glyph ever poses as a WHO cell.
        assert_eq!(identity_glyph(Some("task"), GlyphSet::Ascii), 'T');
        // Absent stays the pinned role_absent mark.
        assert_eq!(identity_glyph(None, GlyphSet::Ascii), '.');
        assert_eq!(identity_glyph(None, GlyphSet::Unicode), '∙');
    }
}
