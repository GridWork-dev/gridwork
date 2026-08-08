//! The production terminal loop over live kernel projections and events.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::QueueableCommand as _;
use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use gwk_domain::ids::{RequestId, Seq};
use gwk_domain::protocol::{
    KernelErrorCode, KernelRequest, KernelResult, ProjectionKind, ProjectionRecord, ServerControl,
};
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::{ColorChoice, ColorTier, TerminalEnv};
use gwk_tui::estate::{EstateSnapshot, EventIndex, ProjectionSnapshot, Stamped};
use gwk_tui::hall::{
    Agent, AgentId, AgentState, DECAY_DURATION, DistrictId, Focus, FrameInput, HallTarget,
    MotionDriver, MotionEntity, MotionFrame, MotionInput, MotionKey, MotionMode, MotionVerb,
    PULSE_DURATION, PagedMotionFrame, PagingState, district_region, district_stack_order,
    render_with_motion_and_pages,
};
use gwk_tui::input::{self, HitMap};
use gwk_tui::runtime::{FramePacer, resolve_motion};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc;

use crate::client::Client;
use crate::exit::Failure;

const PAGE_LIMIT: u32 = 256;
const SNAPSHOT_ATTEMPTS: usize = 3;
const INPUT_POLL: Duration = Duration::from_millis(100);
const PARKED_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static>;

struct TerminalRestore {
    previous_hook: Arc<Mutex<Option<PanicHook>>>,
}

impl TerminalRestore {
    fn install() -> Self {
        let previous_hook = Arc::new(Mutex::new(Some(std::panic::take_hook())));
        let panic_previous = Arc::clone(&previous_hook);
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            if let Ok(previous) = panic_previous.lock()
                && let Some(previous) = previous.as_ref()
            {
                previous(info);
            }
        }));
        Self { previous_hook }
    }
}

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        restore_terminal();
        let _installed = std::panic::take_hook();
        if let Ok(mut previous) = self.previous_hook.lock()
            && let Some(previous) = previous.take()
        {
            std::panic::set_hook(previous);
        }
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut stdout = std::io::stdout();
    let _ = stdout.queue(Show);
    let _ = input::exit(&mut stdout);
}

enum InputEvent {
    Terminal(Event),
    Fault(String),
}

/// Run until the operator quits or the event subscription closes.
pub async fn run(requested_motion: MotionMode) -> Result<(), Failure> {
    let attach_started = Instant::now();
    let mut data = connect().await?;
    let mut events = EventIndex::default();
    let mut estate = refresh_estate(&mut data, &mut events).await?;
    let terminal_env = TerminalEnv::from_process(ColorChoice::Auto);
    let tier = ColorTier::resolve(&terminal_env);
    let resolved_motion = resolve_motion(
        requested_motion,
        terminal_env.term.as_deref(),
        terminal_env.stdout_is_tty,
    );

    if !terminal_env.stdout_is_tty {
        return render_snapshot(&estate.frame, tier);
    }

    let mut stream = connect().await?;
    // Subscribe from what the frame actually contains, not from the later head
    // the provenance index may have read while assembling it. Replayed events
    // are accepted and force the first refresh across that race window.
    let stream_id = stream.subscribe(subscription_cursor(&estate)).await?;
    let restore = TerminalRestore::install();
    enable_raw_mode().map_err(|why| Failure::unreachable(format!("enable raw mode: {why}")))?;
    let mut stdout = std::io::stdout();
    input::enter(&mut stdout)
        .map_err(|why| Failure::unreachable(format!("enter terminal screen: {why}")))?;
    let glyphs = if terminal_env.term.as_deref() == Some("dumb") {
        GlyphSet::Ascii
    } else {
        gwk_tui::probe::probe_terminal(&mut stdout).glyph_set()
    };
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))
        .map_err(|why| Failure::unreachable(format!("open terminal: {why}")))?;

    let stop_input = Arc::new(AtomicBool::new(false));
    let (input_tx, mut input_rx) = mpsc::channel(32);
    let input_thread = spawn_input(input_tx, Arc::clone(&stop_input));
    let result = terminal_loop(
        &mut terminal,
        &mut data,
        &mut stream,
        &stream_id,
        &mut events,
        &mut estate,
        tier,
        glyphs,
        resolved_motion,
        attach_started.elapsed(),
        &mut input_rx,
    )
    .await;

    stop_input.store(true, Ordering::Relaxed);
    drop(input_rx);
    let _ = tokio::task::spawn_blocking(move || input_thread.join()).await;
    let _ = terminal.show_cursor();
    drop(terminal);
    drop(restore);
    result
}

fn spawn_input(
    sender: mpsc::Sender<InputEvent>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match event::poll(INPUT_POLL) {
                Ok(true) => match event::read() {
                    Ok(event) => {
                        if sender.blocking_send(InputEvent::Terminal(event)).is_err() {
                            break;
                        }
                    }
                    Err(why) => {
                        let _ = sender.blocking_send(InputEvent::Fault(why.to_string()));
                        break;
                    }
                },
                Ok(false) => {}
                Err(why) => {
                    let _ = sender.blocking_send(InputEvent::Fault(why.to_string()));
                    break;
                }
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn terminal_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    data: &mut Client,
    stream: &mut Client,
    stream_id: &RequestId,
    events: &mut EventIndex,
    estate: &mut EstateSnapshot,
    tier: ColorTier,
    glyphs: GlyphSet,
    motion: MotionMode,
    attach_elapsed: Duration,
    input_rx: &mut mpsc::Receiver<InputEvent>,
) -> Result<(), Failure> {
    let mut pacer = FramePacer::new(motion);
    let mut driver = MotionDriver::new(pacer.motion_mode());
    let mut pages = PagingState::default();
    let mut hits = HitMap::new();
    let mut selected = visible_district_order(&estate.frame).first().cloned();
    let mut runtime_motions = RuntimeMotions::default();
    let mut last_frame = Instant::now();
    let mut first_frame = true;
    let mut dirty = true;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|why| Failure::unreachable(format!("listen for SIGTERM: {why}")))?;

    loop {
        ensure_focus(&mut estate.frame, selected.as_ref());
        let has_ticks = estate.frame.districts.iter().any(|district| {
            district
                .stations
                .iter()
                .any(|station| station.agents.iter().any(|agent| tickable(agent.state)))
        });
        let animating = pacer.is_animating(has_ticks, driver.active_one_shots() > 0);
        let wants_frame = dirty || animating;
        let frame_due = first_frame || pacer.frame_due(last_frame.elapsed());
        if wants_frame && frame_due {
            let now = Instant::now();
            let frame_delta = now.saturating_duration_since(last_frame);
            let (columns, rows) = crossterm::terminal::size()
                .map_err(|why| Failure::unreachable(format!("read terminal size: {why}")))?;
            let body = Rect::new(0, 0, columns, rows.saturating_sub(1));
            let motions = runtime_motions.frame(&estate.frame, now, frame_delta, body);
            driver.set_mode(pacer.motion_mode());
            draw(
                terminal,
                DrawFrame {
                    input: &estate.frame,
                    tier,
                    glyphs,
                    hits: &mut hits,
                    pages: &pages,
                    driver: &mut driver,
                    motions: &motions,
                    pacer: &pacer,
                    attach_elapsed,
                },
            )?;
            let previous_mode = pacer.motion_mode();
            pacer.observe_render(now.elapsed());
            last_frame = now;
            first_frame = false;
            dirty = previous_mode != pacer.motion_mode();
        }

        let animating = pacer.is_animating(has_ticks, driver.active_one_shots() > 0);
        let interval = if dirty || animating {
            if first_frame {
                Duration::ZERO
            } else {
                pacer
                    .delay_after(last_frame.elapsed())
                    .unwrap_or(Duration::ZERO)
            }
        } else {
            PARKED_INTERVAL
        };
        let sleep = tokio::time::sleep(interval);
        tokio::pin!(sleep);
        tokio::select! {
            result = &mut ctrl_c => {
                result.map_err(|why| Failure::unreachable(format!("listen for Ctrl-C: {why}")))?;
                return Ok(());
            }
            _ = terminate.recv() => return Ok(()),
            _ = &mut sleep => {
                dirty = true;
            }
            input = input_rx.recv() => {
                match input {
                    Some(InputEvent::Terminal(event)) => {
                        if handle_input(
                            event,
                            &estate.frame,
                            &hits,
                            &mut selected,
                            &mut pages,
                            &mut pacer,
                        ) {
                            return Ok(());
                        }
                        dirty = true;
                    }
                    Some(InputEvent::Fault(why)) => {
                        return Err(Failure::unreachable(format!("read terminal input: {why}")));
                    }
                    None => return Ok(()),
                }
            }
            control = stream.receive() => {
                let Some(control) = control? else {
                    return Ok(());
                };
                match control {
                    ServerControl::EventBatch { request_id, events: batch, .. }
                        if request_id == *stream_id =>
                    {
                        events.ingest(batch).map_err(estate_failure)?;
                        let next = refresh_estate(data, events).await?;
                        runtime_motions.observe_transition(&estate.frame, &next.frame, Instant::now());
                        *estate = next;
                        if selected.as_ref().is_none_or(|id| {
                            !estate.frame.districts.iter().any(|district| district.id == *id)
                        }) {
                            selected = visible_district_order(&estate.frame).first().cloned();
                        }
                        dirty = true;
                    }
                    ServerControl::StreamClosed { request_id, code, last_cursor }
                        if request_id == *stream_id =>
                    {
                        let resume = last_cursor
                            .map(|seq| seq.value().to_string())
                            .unwrap_or_else(|| "the beginning".to_owned());
                        return Err(Failure::new(code, format!("event stream closed; resume from {resume}")));
                    }
                    _ => {}
                }
            }
        }
    }
}

struct DrawFrame<'a> {
    input: &'a FrameInput,
    tier: ColorTier,
    glyphs: GlyphSet,
    hits: &'a mut HitMap<HallTarget>,
    pages: &'a PagingState,
    driver: &'a mut MotionDriver,
    motions: &'a [MotionInput],
    pacer: &'a FramePacer,
    attach_elapsed: Duration,
}

fn draw(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    draw: DrawFrame<'_>,
) -> Result<(), Failure> {
    let DrawFrame {
        input,
        tier,
        glyphs,
        hits,
        pages,
        driver,
        motions,
        pacer,
        attach_elapsed,
    } = draw;
    terminal
        .draw(|frame| {
            let area = frame.area();
            let body = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
            render_with_motion_and_pages(
                body,
                frame.buffer_mut(),
                input,
                tier,
                glyphs,
                hits,
                PagedMotionFrame {
                    pages,
                    motion: MotionFrame {
                        driver,
                        inputs: motions,
                    },
                },
            );
            if area.height > 0 {
                let cadence = pacer
                    .interval()
                    .map(|duration| format!("{}ms", duration.as_millis()))
                    .unwrap_or_else(|| "off".to_owned());
                let p95 = pacer
                    .last_p95()
                    .map(|duration| format!(" p95 {}ms", duration.as_millis()))
                    .unwrap_or_default();
                let status = Line::from(format!(
                    " q quit  m motion off  j/k project  [/] page  motion {} {cadence}{p95}  glyph {}  attach {}ms",
                    motion_name(pacer.motion_mode()),
                    glyph_name(glyphs),
                    attach_elapsed.as_millis(),
                ));
                frame.render_widget(
                    Paragraph::new(status),
                    Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
                );
            }
        })
        .map(|_| ())
        .map_err(|why| Failure::unreachable(format!("paint terminal frame: {why}")))
}

fn render_snapshot(input: &FrameInput, tier: ColorTier) -> Result<(), Failure> {
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(std::io::stdout()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 100, 30)),
        },
    )
    .map_err(|why| Failure::unreachable(format!("open snapshot terminal: {why}")))?;
    let mut hits = HitMap::new();
    terminal
        .draw(|frame| {
            gwk_tui::hall::render(
                frame.area(),
                frame.buffer_mut(),
                input,
                tier,
                GlyphSet::Ascii,
                &mut hits,
            );
        })
        .map(|_| ())
        .map_err(|why| Failure::unreachable(format!("paint snapshot frame: {why}")))
}

fn motion_name(mode: MotionMode) -> &'static str {
    match mode {
        MotionMode::Off => "off",
        MotionMode::Reduced => "reduced",
        MotionMode::Full => "full",
    }
}

fn glyph_name(glyphs: GlyphSet) -> &'static str {
    match glyphs {
        GlyphSet::Unicode => "unicode",
        GlyphSet::Ascii => "ascii",
    }
}

fn tickable(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::Queued
            | AgentState::Starting
            | AgentState::Running
            | AgentState::Canceling
            | AgentState::NeedsAttention
    )
}

#[derive(Debug, Clone)]
struct PulseShot {
    previous: FrameInput,
    district: DistrictId,
    started: Instant,
}

#[derive(Debug, Default)]
struct RuntimeMotions {
    ticks: BTreeMap<MotionKey, Instant>,
    decays: BTreeMap<MotionKey, Instant>,
    pulses: BTreeMap<MotionKey, PulseShot>,
}

impl RuntimeMotions {
    fn observe_transition(&mut self, previous: &FrameInput, next: &FrameInput, now: Instant) {
        for agent in all_agents(next) {
            let changed = find_agent(previous, &agent.id)
                .is_some_and(|old| old.changed_seq != agent.changed_seq);
            if changed && !tickable(agent.state) {
                self.decays.insert(
                    MotionKey {
                        entity: MotionEntity::Agent(agent.id.clone()),
                        changed_seq: agent.changed_seq,
                    },
                    now,
                );
            }
        }

        let previous_order = district_stack_order(previous);
        let next_order = district_stack_order(next);
        for attention in &next.attention {
            let changed = previous
                .attention
                .iter()
                .find(|old| old.id == attention.id)
                .is_none_or(|old| {
                    old.unresolved != attention.unresolved
                        || old.changed_seq != attention.changed_seq
                });
            let moved = previous_order
                .iter()
                .position(|id| id == &attention.district)
                != next_order.iter().position(|id| id == &attention.district);
            if changed && moved {
                self.pulses.insert(
                    MotionKey {
                        entity: MotionEntity::Attention(attention.id.clone()),
                        changed_seq: attention.changed_seq,
                    },
                    PulseShot {
                        previous: previous.clone(),
                        district: attention.district.clone(),
                        started: now,
                    },
                );
            }
        }
    }

    fn frame(
        &mut self,
        frame: &FrameInput,
        now: Instant,
        frame_delta: Duration,
        area: Rect,
    ) -> Vec<MotionInput> {
        self.decays.retain(|key, started| {
            now.saturating_duration_since(*started) <= DECAY_DURATION
                && matches!(&key.entity, MotionEntity::Agent(id) if find_agent(frame, id).is_some())
        });
        self.pulses
            .retain(|_, shot| now.saturating_duration_since(shot.started) <= PULSE_DURATION);

        let mut motions = tick_motions(frame, &mut self.ticks, now, frame_delta);
        motions.extend(self.decays.iter().map(|(key, started)| MotionInput {
            key: key.clone(),
            verb: MotionVerb::Decay,
            elapsed: now.saturating_duration_since(*started),
            frame_delta,
            source: Rect::new(0, 0, 1, 1),
            target: Rect::new(0, 0, 1, 1),
        }));
        for (key, shot) in &self.pulses {
            let Some(source) = district_region(area, &shot.previous, &shot.district) else {
                continue;
            };
            let Some(target) = district_region(area, frame, &shot.district) else {
                continue;
            };
            if source.height != target.height || source == target {
                continue;
            }
            motions.push(MotionInput {
                key: key.clone(),
                verb: MotionVerb::Pulse,
                elapsed: now.saturating_duration_since(shot.started),
                frame_delta,
                source,
                target,
            });
        }
        motions
    }
}

fn all_agents(frame: &FrameInput) -> impl Iterator<Item = &Agent> {
    frame
        .districts
        .iter()
        .flat_map(|district| &district.stations)
        .flat_map(|station| &station.agents)
}

fn find_agent<'a>(frame: &'a FrameInput, id: &AgentId) -> Option<&'a Agent> {
    all_agents(frame).find(|agent| agent.id == *id)
}

fn tick_motions(
    frame: &FrameInput,
    started: &mut BTreeMap<MotionKey, Instant>,
    now: Instant,
    frame_delta: Duration,
) -> Vec<MotionInput> {
    let mut active = BTreeSet::new();
    let mut motions = Vec::new();
    for agent in frame
        .districts
        .iter()
        .flat_map(|district| &district.stations)
        .flat_map(|station| &station.agents)
        .filter(|agent| tickable(agent.state))
    {
        let key = MotionKey {
            entity: MotionEntity::Agent(agent.id.clone()),
            changed_seq: agent.changed_seq,
        };
        active.insert(key.clone());
        let observed = *started.entry(key.clone()).or_insert(now);
        motions.push(MotionInput {
            key,
            verb: MotionVerb::Tick,
            elapsed: now.saturating_duration_since(observed),
            frame_delta,
            source: Rect::new(0, 0, 1, 1),
            target: Rect::new(0, 0, 1, 1),
        });
    }
    started.retain(|key, _| active.contains(key));
    motions
}

fn handle_input(
    event: Event,
    frame: &FrameInput,
    hits: &HitMap<HallTarget>,
    selected: &mut Option<DistrictId>,
    pages: &mut PagingState,
    pacer: &mut FramePacer,
) -> bool {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            if quit_key(key) {
                return true;
            }
            match key.code {
                KeyCode::Char('m') => pacer.kill_motion(),
                KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => move_focus(frame, selected, 1),
                KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => {
                    move_focus(frame, selected, -1)
                }
                KeyCode::Char(']') | KeyCode::Right => {
                    if let Some(district) = selected.as_ref() {
                        pages.next(district);
                    }
                }
                KeyCode::Char('[') | KeyCode::Left => {
                    if let Some(district) = selected.as_ref() {
                        pages.previous(district);
                    }
                }
                KeyCode::PageDown => pages.next_district(),
                KeyCode::PageUp => pages.previous_district(),
                _ => {}
            }
        }
        Event::Mouse(mouse) => {
            if let Some(HallTarget::Agent(agent)) = hits.click(&mouse)
                && let Some(district) = frame.districts.iter().find(|district| {
                    district
                        .stations
                        .iter()
                        .flat_map(|station| &station.agents)
                        .any(|candidate| candidate.id == *agent)
                })
            {
                *selected = Some(district.id.clone());
            }
        }
        Event::Resize(_, _) => {}
        _ => {}
    }
    false
}

fn quit_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Esc
        || key.code == KeyCode::Char('q')
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

fn move_focus(frame: &FrameInput, selected: &mut Option<DistrictId>, delta: i8) {
    let order = visible_district_order(frame);
    if order.is_empty() {
        *selected = None;
        return;
    }
    let current = selected
        .as_ref()
        .and_then(|selected| order.iter().position(|district| district == selected))
        .unwrap_or_default();
    let next = if delta < 0 {
        current.checked_sub(1).unwrap_or(order.len() - 1)
    } else {
        (current + 1) % order.len()
    };
    *selected = Some(order[next].clone());
}

fn visible_district_order(frame: &FrameInput) -> Vec<DistrictId> {
    let attention: BTreeSet<&str> = frame
        .attention
        .iter()
        .filter(|item| item.unresolved)
        .map(|item| item.district.as_str())
        .collect();
    district_stack_order(frame)
        .into_iter()
        .filter(|district| {
            attention.contains(district.as_str())
                || frame
                    .districts
                    .iter()
                    .find(|candidate| candidate.id == *district)
                    .is_some_and(|candidate| {
                        candidate
                            .stations
                            .iter()
                            .any(|station| !station.agents.is_empty())
                    })
        })
        .collect()
}

fn subscription_cursor(estate: &EstateSnapshot) -> Option<Seq> {
    estate.frame.watermark
}

fn ensure_focus(frame: &mut FrameInput, selected: Option<&DistrictId>) {
    frame.focus = selected.and_then(|selected| {
        frame
            .districts
            .iter()
            .find(|district| district.id == *selected)
            .map(|district| Focus {
                district: selected.clone(),
                changed_seq: district.changed_seq,
            })
    });
}

async fn connect() -> Result<Client, Failure> {
    let (client, _hello) = Client::connect(&crate::socket_path()).await?;
    Ok(client)
}

async fn refresh_estate(
    client: &mut Client,
    events: &mut EventIndex,
) -> Result<EstateSnapshot, Failure> {
    let mut last_retry = None;
    for _ in 0..SNAPSHOT_ATTEMPTS {
        let projections = load_projection_pages(client).await?;
        load_events_to_head(client, events).await?;
        match events.build(&projections) {
            Ok(estate) => return Ok(estate),
            Err(why) if EventIndex::is_retryable(&why) => last_retry = Some(why),
            Err(why) => return Err(estate_failure(why)),
        }
    }
    let Some(last_retry) = last_retry else {
        return Err(Failure::internal("snapshot retry ended without a cause"));
    };
    Err(estate_failure(last_retry))
}

async fn load_events_to_head(client: &mut Client, index: &mut EventIndex) -> Result<(), Failure> {
    let mut cursor = index.watermark();
    let mut target = None;
    loop {
        let result = client
            .ask(KernelRequest::ReadEvents {
                cursor,
                limit: PAGE_LIMIT,
            })
            .await?;
        let KernelResult::Events {
            events,
            cursor: delivered,
            watermark,
        } = result
        else {
            return result_failure(result, "read event provenance");
        };
        target.get_or_insert(watermark);
        index.ingest(events).map_err(estate_failure)?;
        let Some(delivered) = delivered else {
            break;
        };
        if cursor == Some(delivered) {
            return Err(Failure::internal(format!(
                "event provenance page did not advance past {delivered}"
            )));
        }
        cursor = Some(delivered);
        if target
            .flatten()
            .is_none_or(|watermark| delivered >= watermark)
        {
            break;
        }
    }
    Ok(())
}

async fn load_projection_pages(client: &mut Client) -> Result<ProjectionSnapshot, Failure> {
    let mut snapshot = ProjectionSnapshot::default();
    for kind in [
        ProjectionKind::Task,
        ProjectionKind::Attempt,
        ProjectionKind::Message,
        ProjectionKind::AttentionItem,
    ] {
        let mut cursor = None;
        let mut seen = BTreeSet::new();
        loop {
            let result = client
                .ask(KernelRequest::ListProjection {
                    projection: kind,
                    cursor: cursor.clone(),
                    limit: Some(PAGE_LIMIT),
                })
                .await?;
            let KernelResult::ProjectionPage {
                records,
                next_cursor,
                watermark,
            } = result
            else {
                return result_failure(result, "read estate projections");
            };
            snapshot.watermarks.push(watermark);
            if !records.is_empty() && watermark.is_none() {
                return Err(Failure::new(
                    KernelErrorCode::Schema,
                    format!(
                        "{} projection rows arrived without a page watermark",
                        kind.as_str()
                    ),
                ));
            }
            for record in records {
                push_projection(&mut snapshot, kind, record, watermark)?;
            }
            let Some(next) = next_cursor else {
                break;
            };
            if !seen.insert(next.clone()) {
                return Err(Failure::internal(format!(
                    "{} projection cursor repeated {next:?}",
                    kind.as_str()
                )));
            }
            cursor = Some(next);
        }
    }
    Ok(snapshot)
}

fn push_projection(
    snapshot: &mut ProjectionSnapshot,
    wanted: ProjectionKind,
    record: ProjectionRecord,
    watermark: Option<Seq>,
) -> Result<(), Failure> {
    let watermark = watermark.ok_or_else(|| {
        Failure::new(
            KernelErrorCode::Schema,
            format!("{} projection row has no page watermark", wanted.as_str()),
        )
    })?;
    match (wanted, record) {
        (ProjectionKind::Task, ProjectionRecord::Task { task }) => {
            snapshot.tasks.push(Stamped::new(task, watermark));
        }
        (ProjectionKind::Attempt, ProjectionRecord::Attempt { attempt }) => {
            snapshot.attempts.push(Stamped::new(attempt, watermark));
        }
        (ProjectionKind::Message, ProjectionRecord::Message { message }) => {
            snapshot.messages.push(Stamped::new(message, watermark));
        }
        (ProjectionKind::AttentionItem, ProjectionRecord::AttentionItem { attention_item }) => {
            snapshot
                .attention
                .push(Stamped::new(attention_item, watermark));
        }
        (_, record) => {
            return Err(Failure::new(
                KernelErrorCode::Schema,
                format!(
                    "{} projection page carried a {} row",
                    wanted.as_str(),
                    record.kind().as_str()
                ),
            ));
        }
    }
    Ok(())
}

fn result_failure<T>(result: KernelResult, context: &str) -> Result<T, Failure> {
    match result {
        KernelResult::Error { code, message, .. } => Err(Failure::new(code, message)),
        other => Err(Failure::internal(format!(
            "{context}: kernel answered with {other:?}"
        ))),
    }
}

fn estate_failure(why: impl std::fmt::Display) -> Failure {
    Failure::new(KernelErrorCode::Schema, why.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gwk_tui::hall::{Agent, AgentId, Attention, AttentionId, District, Station, StationId};

    fn district(id: &str, agents: Vec<Agent>) -> District {
        District {
            id: DistrictId::new(id).expect("district id"),
            label: id.to_owned(),
            stations: (!agents.is_empty())
                .then(|| Station {
                    id: StationId::new(format!("station-{id}")).expect("station id"),
                    label: "execute".to_owned(),
                    template_ordinal: 1,
                    agents,
                    changed_seq: Seq::new(1),
                })
                .into_iter()
                .collect(),
            changed_seq: Seq::new(1),
        }
    }

    fn agent(seq: u64) -> Agent {
        Agent {
            id: AgentId::new("agent-a").expect("agent id"),
            role: None,
            state: AgentState::Running,
            duration: Some("live".to_owned()),
            changed_seq: Seq::new(seq),
        }
    }

    #[test]
    fn runtime_tick_elapsed_comes_from_observation_time_not_sequence_math() {
        let mut frame = FrameInput {
            districts: vec![district("district-a", vec![agent(7)])],
            focus: None,
            attention: Vec::new(),
            watermark: Some(Seq::new(700)),
        };
        let first = Instant::now();
        let mut started = BTreeMap::new();
        let initial = tick_motions(&frame, &mut started, first, Duration::ZERO);
        assert_eq!(initial[0].elapsed, Duration::ZERO);

        let later = first + Duration::from_millis(100);
        let continued = tick_motions(&frame, &mut started, later, Duration::from_millis(33));
        assert_eq!(continued[0].elapsed, Duration::from_millis(100));

        frame.districts[0].stations[0].agents[0].changed_seq = Seq::new(701);
        let changed = tick_motions(&frame, &mut started, later, Duration::from_millis(33));
        assert_eq!(changed[0].elapsed, Duration::ZERO);
        assert_eq!(started.len(), 1);
    }

    #[test]
    fn runtime_focus_walk_skips_empty_shells_but_keeps_attention_shells() {
        let attention_district = DistrictId::new("district-c").expect("district id");
        let frame = FrameInput {
            districts: vec![
                district("district-a", vec![agent(1)]),
                district("district-b", Vec::new()),
                district("district-c", Vec::new()),
            ],
            focus: None,
            attention: vec![Attention {
                id: AttentionId::new("attention-c").expect("attention id"),
                district: attention_district.clone(),
                unresolved: true,
                changed_seq: Seq::new(2),
            }],
            watermark: Some(Seq::new(2)),
        };

        assert_eq!(
            visible_district_order(&frame),
            vec![
                attention_district,
                DistrictId::new("district-a").expect("district id")
            ]
        );
    }

    #[test]
    fn runtime_transition_emits_decay_and_attention_reorder_pulse() {
        let mut previous = FrameInput {
            districts: vec![
                district("district-a", vec![agent(1)]),
                district(
                    "district-b",
                    vec![Agent {
                        id: AgentId::new("agent-b").expect("agent id"),
                        role: None,
                        state: AgentState::Running,
                        duration: Some("live".to_owned()),
                        changed_seq: Seq::new(1),
                    }],
                ),
            ],
            focus: None,
            attention: Vec::new(),
            watermark: Some(Seq::new(1)),
        };
        let mut next = previous.clone();
        next.districts[0].stations[0].agents[0].state = AgentState::Done;
        next.districts[0].stations[0].agents[0].changed_seq = Seq::new(2);
        next.attention.push(Attention {
            id: AttentionId::new("attention-b").expect("attention id"),
            district: DistrictId::new("district-b").expect("district id"),
            unresolved: true,
            changed_seq: Seq::new(3),
        });
        previous.focus = Some(Focus {
            district: DistrictId::new("district-a").expect("district id"),
            changed_seq: Seq::new(1),
        });
        next.focus = previous.focus.clone();

        let now = Instant::now();
        let mut runtime = RuntimeMotions::default();
        runtime.observe_transition(&previous, &next, now);
        let motions = runtime.frame(
            &next,
            now + Duration::from_millis(33),
            Duration::from_millis(33),
            Rect::new(0, 0, 40, 6),
        );

        assert!(
            motions
                .iter()
                .any(|motion| motion.verb == MotionVerb::Decay)
        );
        assert!(
            motions
                .iter()
                .any(|motion| motion.verb == MotionVerb::Pulse)
        );
        assert!(motions.iter().any(|motion| motion.verb == MotionVerb::Tick));
    }

    #[test]
    fn runtime_subscribes_from_the_rendered_snapshot_not_the_later_event_head() {
        let estate = EstateSnapshot {
            frame: FrameInput {
                districts: Vec::new(),
                focus: None,
                attention: Vec::new(),
                watermark: Some(Seq::new(7)),
            },
            messages: Vec::new(),
        };

        assert_eq!(subscription_cursor(&estate), Some(Seq::new(7)));
    }
}
