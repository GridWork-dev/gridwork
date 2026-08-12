//! The production terminal loop over live kernel projections and events.

use std::collections::BTreeSet;
use std::io::IsTerminal as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::QueueableCommand as _;
use crossterm::cursor::Show;
use crossterm::event::{self, Event};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use gwk_domain::entity::{CostEntry, Evidence};
use gwk_domain::envelope::EventEnvelope;
use gwk_domain::fsm::GateVerdict;
use gwk_domain::ids::{AttentionItemId, EvidenceId, PtySessionId, RequestId, Seq, Timestamp};
use gwk_domain::protocol::{
    KernelErrorCode, KernelRequest, KernelResult, ProjectionKind, ProjectionRecord, ServerControl,
};
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::{ColorChoice, ColorTier, TerminalEnv};
use gwk_tui::board::{self, BoardState, BoardTarget, BoardView, EventTail};
use gwk_tui::config::{self, ConfigForm, ConfigRepository, ConfigState, ConfigTarget};
use gwk_tui::console::{self, FleetContext, HallContext, LoadState};
use gwk_tui::drilldown::{self, DrilldownState, DrilldownTarget, IngestDisposition};
use gwk_tui::estate::{EstateSnapshot, EventIndex, ProjectionSnapshot, Stamped};
use gwk_tui::hall::{
    AgentState, DistrictId, Focus, FrameInput, HallTarget, MotionMode, district_stack_order,
};
use gwk_tui::input::{self, HitMap};
use gwk_tui::queue::{self, QueueState, QueueTarget};
use gwk_tui::replay::ReplayTimeline;
use gwk_tui::runtime::{FramePacer, resolve_motion};
use gwk_tui::shell::{
    self, AttachRailItem, AttachRailState, AttachRailTerm, BudgetFormState, ContextVerb,
    GateDecisionState, Lens, ShellAction, ShellState, Surface, TermState, TermTarget,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc;

use crate::client::Client;
use crate::exit::Failure;

const PAGE_LIMIT: u32 = 256;
const SNAPSHOT_ATTEMPTS: usize = 3;
/// Stream closes survived back-to-back (no delivered batch between them)
/// before the live loop stops resubscribing and reports the close.
const STREAM_RESUME_ATTEMPTS: u32 = 3;
const INPUT_POLL: Duration = Duration::from_millis(100);
const EVENT_TAIL_ROWS: usize = 512;

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

struct InputPump {
    receiver: mpsc::Receiver<InputEvent>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InputPump {
    fn start() -> Self {
        let (sender, receiver) = mpsc::channel(32);
        let stop = Arc::new(AtomicBool::new(false));
        let thread = spawn_input(sender, Arc::clone(&stop));
        Self {
            receiver,
            stop,
            thread: Some(thread),
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.receiver.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    fn restart(&mut self) {
        let replacement = Self::start();
        *self = replacement;
    }
}

/// Client-side buffer between the socket reader and the render loop. Kept as
/// deep as the kernel's own batch queue so that when the loop stalls, the
/// pump stops reading the socket and the kernel's slow-consumer accounting
/// (BATCH_QUEUE_DEPTH + SLOW_CONSUMER_TIMEOUT_SECS) stays the binding
/// constraint — an unbounded buffer here would absorb batches forever and
/// make SlowConsumer structurally unreachable while memory and staleness
/// grew without witness.
const CONTROL_PUMP_DEPTH: usize = 8;

struct ControlPump {
    receiver: mpsc::Receiver<Result<Option<ServerControl>, Failure>>,
    task: tokio::task::JoinHandle<()>,
}

impl ControlPump {
    fn start(mut client: Client) -> Self {
        let (sender, receiver) = mpsc::channel(CONTROL_PUMP_DEPTH);
        let task = tokio::spawn(async move {
            loop {
                let result = client.receive().await;
                let closed = matches!(result, Ok(None) | Err(_));
                if sender.send(result).await.is_err() || closed {
                    break;
                }
            }
        });
        Self { receiver, task }
    }

    async fn receive(&mut self) -> Result<Option<ServerControl>, Failure> {
        self.receiver.recv().await.unwrap_or(Ok(None))
    }

    /// Already-buffered control, if any — the coalescing drain. Never waits.
    fn try_receive(&mut self) -> Option<Result<Option<ServerControl>, Failure>> {
        self.receiver.try_recv().ok()
    }
}

impl Drop for ControlPump {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Drop for InputPump {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Set (non-empty) to silence the startup degradation notices.
const DEGRADE_NOTICE_ENV: &str = "GWK_NO_DEGRADE_NOTICE";

/// One stderr line, printed before the alternate screen opens, naming a
/// capability this terminal lost and why — instead of leaving a short
/// status-bar tag as the only witness. Raw mode may already be active, so the
/// line carries its own carriage return.
fn degrade_notice(message: &str) {
    let quiet = std::env::var_os(DEGRADE_NOTICE_ENV).is_some_and(|value| !value.is_empty());
    if !quiet {
        eprint!("gw: {message} ({DEGRADE_NOTICE_ENV}=1 hides this)\r\n");
    }
}

/// [`ColorTier::resolve`], saying out loud when the answer was the
/// unrecognized-TERM fallthrough — the one degradation the resolution stack
/// would otherwise apply silently. Deliberate monochrome (a flag, NO_COLOR,
/// a dumb terminal, a pipe) stays quiet.
fn resolve_tier(terminal_env: &TerminalEnv) -> ColorTier {
    let (tier, unrecognized_term) = ColorTier::resolve_loud(terminal_env);
    if unrecognized_term {
        degrade_notice(&format!(
            "TERM {:?} is not in the color ladder; rendering monochrome",
            terminal_env.term.as_deref().unwrap_or_default()
        ));
    }
    tier
}

/// Resolve the glyph tier, naming what a degradation costs and why. Runs
/// under raw mode but before the alternate screen, so the notice lands on
/// the primary screen where it survives the session.
fn probe_glyphs(stdout: &mut std::io::Stdout, terminal_env: &TerminalEnv) -> GlyphSet {
    use gwk_theme::probe::ProbeOutcome;
    if terminal_env.term.as_deref() == Some("dumb") {
        return GlyphSet::Ascii;
    }
    let outcome = gwk_tui::probe::probe_terminal(stdout);
    let inventory = gwk_theme::probe::inventory_codepoints().len();
    match &outcome {
        ProbeOutcome::UnicodeSafe => {}
        ProbeOutcome::Sheared(glyphs) => degrade_notice(&format!(
            "{} of {inventory} probed glyphs render at the wrong width here; drawing ascii marks",
            glyphs.len()
        )),
        ProbeOutcome::Inconclusive(glyphs) => degrade_notice(&format!(
            "the terminal left {} of {inventory} glyph probes unanswered; drawing ascii marks",
            glyphs.len()
        )),
    }
    outcome.glyph_set()
}

#[derive(Debug, Clone)]
struct WorkspaceStart {
    surface: Surface,
    event_cursor: Option<Seq>,
    aggregate_type: Option<String>,
    event_type: Option<String>,
    attach: Option<PtySessionId>,
    subscribe_from_estate: bool,
}

impl WorkspaceStart {
    fn hall() -> Self {
        Self {
            surface: Surface::Hall,
            event_cursor: None,
            aggregate_type: None,
            event_type: None,
            attach: None,
            subscribe_from_estate: true,
        }
    }

    fn events(
        cursor: Option<Seq>,
        aggregate_type: Option<String>,
        event_type: Option<String>,
        view: BoardView,
    ) -> Self {
        // Without an explicit cursor, subscribe from the estate's own cursor
        // rather than the head: the startup reads happen before the
        // subscription opens, so a head subscription would leave a write
        // landing in that window invisible until the next unrelated event.
        let subscribe_from_estate = cursor.is_none();
        Self {
            surface: surface_for_board_view(view),
            event_cursor: cursor,
            aggregate_type,
            event_type,
            attach: None,
            subscribe_from_estate,
        }
    }

    fn attach(session_id: PtySessionId) -> Self {
        Self {
            surface: Surface::TermAttach,
            event_cursor: None,
            aggregate_type: None,
            event_type: None,
            attach: Some(session_id),
            subscribe_from_estate: true,
        }
    }
}

fn surface_for_board_view(view: BoardView) -> Surface {
    match view {
        BoardView::Estate => Surface::Hall,
        BoardView::Activity => Surface::WorkQueue,
        BoardView::Runs => Surface::WorkRuns,
        BoardView::Dag => Surface::WorkTasks,
        BoardView::Flow => Surface::FlowMessages,
        BoardView::Events => Surface::FlowEvents,
        BoardView::Replay => Surface::TermLifetimes,
        BoardView::Fleet => Surface::FleetAgents,
        BoardView::CostHealth => Surface::FleetCost,
        BoardView::Audit => Surface::FlowAudit,
    }
}

#[derive(Debug, Clone)]
enum ConfirmationTarget {
    Queue(QueueTarget),
    Attempt(BoardTarget),
    Budget(BoardTarget),
}

struct ConsoleModel {
    shell: ShellState,
    estate: EstateSnapshot,
    board: BoardState,
    queue: QueueState,
    config: ConfigState,
    config_repository: Option<ConfigRepository>,
    config_form: Option<ConfigForm>,
    config_notice: Option<String>,
    gate_decision: Option<GateDecisionState>,
    budget_form: Option<BudgetFormState>,
    confirmation_target: Option<ConfirmationTarget>,
    terms: TermState,
    drilldown: Option<DrilldownState>,
    hall_phase: usize,
    hall_cost_micros: u64,
    hall_selected: Option<DistrictId>,
    board_selected: Option<BoardTarget>,
    queue_selected: Option<QueueTarget>,
    config_selected: Option<ConfigTarget>,
    term_selected: Option<TermTarget>,
    /// The terminal was suspended and resumed (an $EDITOR ran) since the
    /// loop last polled — the signal listener must be re-armed so a SIGINT
    /// latched during the cooked-mode windows doesn't quit the console on
    /// the next poll.
    terminal_resumed: bool,
}

impl ConsoleModel {
    fn update_attention_marks(&mut self) {
        let now = &self.queue.now;
        let attention = self.queue.attention.iter().any(|item| {
            item.resolved_at.is_none()
                && item.acked_at.is_none()
                && !item.muted_until.as_ref().is_some_and(|until| until > now)
        }) || self
            .queue
            .gates
            .iter()
            .any(|gate| gate.question.is_some() && gate.verdict == GateVerdict::Pending);
        self.shell.set_attention(Lens::Work, attention);
    }
}

#[derive(Default)]
struct ConsoleHits {
    hall: HitMap<HallTarget>,
    board: HitMap<BoardTarget>,
    queue: HitMap<QueueTarget>,
    config: HitMap<ConfigTarget>,
}

impl ConsoleHits {
    fn clear(&mut self) {
        self.hall.clear();
        self.board.clear();
        self.queue.clear();
        self.config.clear();
    }
}

async fn run_workspace(start: WorkspaceStart, requested_motion: MotionMode) -> Result<(), Failure> {
    let attach_started = Instant::now();
    let subscribe_from_estate = start.subscribe_from_estate;
    let requested_event_cursor = start.event_cursor;
    let mut data = connect().await?;
    let mut events = EventIndex::default();
    let estate = refresh_estate(&mut data, &mut events).await?;
    let board = event_board(
        start.surface.board_view().unwrap_or(BoardView::Dag),
        start.event_cursor,
        start.aggregate_type,
        start.event_type,
        true,
    );
    let queue = refresh_queue(&mut data).await?;
    let terms = TermState {
        sessions: Vec::new(),
        watermark: None,
        complete: true,
    };
    let (config, config_repository, config_evidence, config_notice) =
        refresh_config(&mut data).await;
    let hall_selected = visible_district_order(&estate.frame).first().cloned();
    let mut model = ConsoleModel {
        shell: ShellState::new(start.surface),
        estate,
        board,
        queue,
        config,
        config_repository,
        config_form: None,
        config_notice,
        gate_decision: None,
        budget_form: None,
        confirmation_target: None,
        terms,
        drilldown: None,
        hall_phase: 0,
        hall_cost_micros: refresh_hall_cost(&mut data).await?,
        hall_selected,
        board_selected: None,
        queue_selected: None,
        config_selected: None,
        term_selected: None,
        terminal_resumed: false,
    };
    if let Some(repository) = model.config_repository.as_ref() {
        match repository.reconcile_startup(
            config_evidence.as_ref(),
            AttentionItemId::new("config-divergence"),
        ) {
            Ok(Some(command)) => {
                let key = format!(
                    "tui:config-reconcile:{}:{}",
                    model.config.config_head.as_deref().unwrap_or("none"),
                    model.config.last_evidence_ref.as_deref().unwrap_or("none")
                );
                if let Err(error) = submit_console_command(&mut data, &command, &key).await {
                    model.config_notice = Some(format!(
                        "config divergence attention could not be submitted -- {error}"
                    ));
                } else {
                    model.queue = refresh_queue(&mut data).await?;
                }
            }
            Ok(None) => {}
            Err(error) => {
                model.config_notice = Some(format!("config reconciliation unavailable -- {error}"));
            }
        }
    }
    refresh_surface(&mut data, &mut model).await?;
    reconcile_selection(&mut model);
    model.update_attention_marks();

    let mut pty_stream = None;
    if let Some(session_id) = start.attach {
        open_workspace_attach(&mut data, &mut model, &mut pty_stream, session_id).await?;
    }

    let terminal_env = TerminalEnv::from_process(ColorChoice::Auto);
    let tier = resolve_tier(&terminal_env);
    let resolved_motion = resolve_motion(
        requested_motion,
        terminal_env.term.as_deref(),
        terminal_env.stdout_is_tty,
    );
    let mut stream_client = connect().await?;
    let cursor = requested_event_cursor.or_else(|| {
        subscribe_from_estate
            .then(|| subscription_cursor(&model.estate))
            .flatten()
    });
    let mut stream_id = stream_client.subscribe(cursor).await?;
    let mut stream = ControlPump::start(stream_client);

    let restore = TerminalRestore::install();
    enable_raw_mode().map_err(|why| Failure::unreachable(format!("enable raw mode: {why}")))?;
    let mut stdout = std::io::stdout();
    let glyphs = probe_glyphs(&mut stdout, &terminal_env);
    input::enter(&mut stdout)
        .map_err(|why| Failure::unreachable(format!("enter terminal screen: {why}")))?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))
        .map_err(|why| Failure::unreachable(format!("open terminal: {why}")))?;
    let mut input = InputPump::start();

    let result = workspace_loop(
        &mut terminal,
        &mut data,
        &mut stream,
        &mut stream_id,
        &mut pty_stream,
        &mut events,
        &mut model,
        tier,
        glyphs,
        resolved_motion,
        attach_started.elapsed(),
        &mut input,
    )
    .await;

    input.stop();
    let _ = terminal.show_cursor();
    drop(terminal);
    drop(restore);
    result
}

#[allow(clippy::too_many_arguments)]
async fn workspace_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    data: &mut Client,
    stream: &mut ControlPump,
    stream_id: &mut RequestId,
    pty_stream: &mut Option<ControlPump>,
    events: &mut EventIndex,
    model: &mut ConsoleModel,
    tier: ColorTier,
    glyphs: GlyphSet,
    motion: MotionMode,
    attach_elapsed: Duration,
    input: &mut InputPump,
) -> Result<(), Failure> {
    let mut pacer = FramePacer::new(motion);
    let mut hits = ConsoleHits::default();
    let mut dirty = true;
    // Closes survived since the last delivered batch. A subscription that
    // dies immediately after every resume is a kernel-side condition the
    // loop cannot heal by resubscribing again.
    let mut consecutive_closes: u32 = 0;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|why| Failure::unreachable(format!("listen for SIGTERM: {why}")))?;
    let motion_started = Instant::now();
    let mut clock = tokio::time::interval(Duration::from_secs(1));
    clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ensure_focus(&mut model.estate.frame, model.hall_selected.as_ref());
        if dirty {
            let render_started = Instant::now();
            draw_workspace(
                terminal,
                model,
                &mut hits,
                tier,
                glyphs,
                &pacer,
                attach_elapsed,
            )?;
            pacer.observe_render(render_started.elapsed());
            dirty = false;
        }
        let hall_tick = (model.shell.surface() == Surface::Hall)
            .then(|| pacer.interval())
            .flatten();
        tokio::select! {
            result = &mut ctrl_c => {
                result.map_err(|why| Failure::unreachable(format!("listen for Ctrl-C: {why}")))?;
                return Ok(());
            }
            _ = terminate.recv() => return Ok(()),
            _ = async {
                match hall_tick {
                    Some(interval) => tokio::time::sleep(interval).await,
                    None => std::future::pending().await,
                }
            } => {
                let ticks = motion_started.elapsed().as_millis() / input::TICK.as_millis();
                model.hall_phase = usize::try_from(ticks).unwrap_or(usize::MAX);
                dirty = true;
            },
            _ = clock.tick() => {
                model.queue.now = current_timestamp();
                model.update_attention_marks();
                dirty = true;
            },
            input_event = input.receiver.recv() => match input_event {
                Some(InputEvent::Terminal(event)) => {
                    if handle_workspace_input(
                        event,
                        terminal,
                        input,
                        data,
                        pty_stream,
                        model,
                        &hits,
                        &mut pacer,
                    ).await? {
                        return Ok(());
                    }
                    if std::mem::take(&mut model.terminal_resumed) {
                        // An $EDITOR ran in cooked mode; a ^C typed in those
                        // windows latches a SIGINT the pinned listener would
                        // deliver on this poll, quitting the console long
                        // after the keystroke. A fresh listener discards it.
                        ctrl_c.set(tokio::signal::ctrl_c());
                    }
                    dirty = true;
                }
                Some(InputEvent::Fault(why)) => {
                    return Err(Failure::unreachable(format!("read terminal input: {why}")));
                }
                None => return Ok(()),
            },
            control = stream.receive() => {
                // Ingest every already-buffered batch before refreshing once:
                // each refresh cycle costs a full projection sweep, so a
                // backlog replay (or a burst) folds N batches into one sweep
                // instead of N.
                let mut next = control?;
                let mut batched = false;
                loop {
                    match next {
                        None => return Ok(()),
                        Some(ServerControl::EventBatch { request_id, events: batch, cursor })
                            if request_id == *stream_id =>
                        {
                            append_event_batch(&mut model.board, batch.clone(), Some(cursor));
                            events.ingest(batch).map_err(estate_failure)?;
                            batched = true;
                            consecutive_closes = 0;
                        }
                        Some(ServerControl::StreamClosed { request_id, code, last_cursor })
                            if request_id == *stream_id =>
                        {
                            // A closed subscription names its resume cursor
                            // (slow consumer, eviction) — in an interactive
                            // shell that is a resumable condition, not a
                            // crash. Resubscribe and refresh; the fold heals
                            // whatever the gap skipped.
                            consecutive_closes += 1;
                            if consecutive_closes > STREAM_RESUME_ATTEMPTS {
                                let resume = last_cursor
                                    .map(|seq| seq.value().to_string())
                                    .unwrap_or_else(|| "the beginning".to_owned());
                                return Err(Failure::new(code, format!("event stream closed; resume from {resume}")));
                            }
                            let mut replacement = connect().await?;
                            *stream_id = replacement
                                .subscribe(last_cursor.or_else(|| subscription_cursor(&model.estate)))
                                .await?;
                            *stream = ControlPump::start(replacement);
                            model.shell.set_notice(format!("event stream resumed after close ({code})"));
                            batched = true;
                            break;
                        }
                        Some(_) => {}
                    }
                    match stream.try_receive() {
                        Some(buffered) => next = buffered?,
                        None => break,
                    }
                }
                if batched {
                    model.estate = refresh_estate(data, events).await?;
                    model.queue = refresh_queue(data).await?;
                    model.hall_cost_micros = refresh_hall_cost(data).await?;
                    refresh_surface(data, model).await?;
                    reconcile_selection(model);
                    model.update_attention_marks();
                    dirty = true;
                }
            },
            control = async {
                match pty_stream.as_mut() {
                    Some(client) => client.receive().await,
                    None => std::future::pending().await,
                }
            } => {
                let Some(control) = control? else {
                    // The hosted terminal's stream ended (host hangup, kernel
                    // close). A frozen frame is indistinguishable from a
                    // quiet one, so the end becomes visible state.
                    pty_stream.take();
                    model.shell.set_notice("terminal stream closed");
                    dirty = true;
                    continue;
                };
                if let Some(drilldown) = model.drilldown.as_mut() {
                    let disposition = drilldown.ingest(&control);
                    if disposition != IngestDisposition::Unrelated && drilldown.cells().is_none() {
                        refresh_session_snapshot(data, drilldown).await?;
                    }
                    dirty = disposition != IngestDisposition::Unrelated;
                }
            }
        }
    }
}

fn draw_workspace(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    model: &ConsoleModel,
    hits: &mut ConsoleHits,
    tier: ColorTier,
    glyphs: GlyphSet,
    pacer: &FramePacer,
    _attach_elapsed: Duration,
) -> Result<(), Failure> {
    terminal
        .draw(|frame| {
            let area = frame.area();
            if area.width >= console::MIN_WIDTH && area.height >= console::MIN_HEIGHT {
                match model.shell.surface() {
                    Surface::Hall => {
                        let running = model
                            .estate
                            .frame
                            .districts
                            .iter()
                            .flat_map(|district| &district.stations)
                            .flat_map(|station| &station.agents)
                            .filter(|agent| {
                                matches!(
                                    agent.state,
                                    AgentState::Running
                                        | AgentState::Starting
                                        | AgentState::Canceling
                                )
                            })
                            .count();
                        let attention = model
                            .estate
                            .frame
                            .attention
                            .iter()
                            .filter(|item| item.unresolved)
                            .count();
                        console::render_hall_at(
                            area,
                            frame.buffer_mut(),
                            &model.estate.frame,
                            &HallContext {
                                now: current_timestamp(),
                                running,
                                attention,
                                cost_micros: model.hall_cost_micros,
                                load: LoadState::Ready,
                            },
                            tier,
                            glyphs,
                            model.hall_phase,
                            &mut hits.hall,
                        );
                    }
                    Surface::WorkQueue => {
                        let state = filtered_queue(&model.queue, model.shell.filter());
                        queue::render(
                            shell::body_area(area),
                            frame.buffer_mut(),
                            &state,
                            model.queue_selected.as_ref(),
                            tier,
                            glyphs,
                            &mut hits.queue,
                        );
                        if let Some(decision) = &model.gate_decision {
                            shell::render_gate_decision(area, frame.buffer_mut(), decision, tier);
                        }
                    }
                    Surface::WorkConfig => {
                        if let Some(form) = &model.config_form {
                            hits.config.clear();
                            config::render_form(
                                shell::body_area(area),
                                frame.buffer_mut(),
                                form,
                                tier,
                            );
                        } else {
                            let state = filtered_config(&model.config, model.shell.filter());
                            config::render(
                                shell::body_area(area),
                                frame.buffer_mut(),
                                &state,
                                model.config_selected.as_ref(),
                                tier,
                                glyphs,
                                &mut hits.config,
                            );
                        }
                        if let Some(notice) = &model.config_notice {
                            let body = shell::body_area(area);
                            frame.buffer_mut().set_stringn(
                                body.x.saturating_add(1),
                                body.y.saturating_add(1),
                                notice,
                                body.width.saturating_sub(1) as usize,
                                ratatui::style::Style::default(),
                            );
                        }
                    }
                    Surface::FleetAgents => {
                        if let Some(form) = &model.budget_form {
                            hits.board.clear();
                            shell::render_budget_form(
                                shell::body_area(area),
                                frame.buffer_mut(),
                                form,
                                tier,
                            );
                        } else {
                            let state = filtered_board(&model.board, model.shell.filter());
                            console::render_fleet(
                                area,
                                frame.buffer_mut(),
                                &state,
                                &FleetContext {
                                    now: current_timestamp(),
                                    load: LoadState::Ready,
                                },
                                model.board_selected.as_ref(),
                                tier,
                                glyphs,
                                &mut hits.board,
                            );
                        }
                    }
                    Surface::TermLifetimes => {
                        let state = filtered_terms(&model.terms, model.shell.filter());
                        shell::render_terms(
                            shell::body_area(area),
                            frame.buffer_mut(),
                            &state,
                            model.term_selected.as_ref(),
                            tier,
                        );
                    }
                    Surface::TermAttach => {
                        if let Some(drilldown) = &model.drilldown {
                            let selected = DrilldownTarget::Session(drilldown.session_id().clone());
                            let body = shell::body_area(area);
                            let session_area = if model.shell.rail_visible() && area.width > 100 {
                                let rail = Rect::new(body.x, body.y, 28, body.height);
                                shell::render_attach_rail(
                                    rail,
                                    frame.buffer_mut(),
                                    &attach_rail_state(model, drilldown.session_id()),
                                    tier,
                                );
                                for y in body.y..body.y.saturating_add(body.height) {
                                    frame.buffer_mut().set_string(
                                        body.x.saturating_add(28),
                                        y,
                                        "|",
                                        ratatui::style::Style::default(),
                                    );
                                }
                                Rect::new(
                                    body.x.saturating_add(30),
                                    body.y,
                                    body.width.saturating_sub(30),
                                    body.height,
                                )
                            } else {
                                body
                            };
                            drilldown::render(
                                session_area,
                                frame.buffer_mut(),
                                drilldown,
                                Some(&selected),
                                tier,
                                &mut HitMap::new(),
                            );
                        }
                    }
                    surface => {
                        let mut state = filtered_board(&model.board, model.shell.filter());
                        if let Some(view) = surface.board_view() {
                            state.view = view;
                        }
                        board::render_embedded(
                            shell::body_area(area),
                            frame.buffer_mut(),
                            &state,
                            model.board_selected.as_ref(),
                            tier,
                            glyphs,
                            &mut hits.board,
                        );
                    }
                }
            } else {
                hits.clear();
            }
            shell::render_chrome(
                area,
                frame.buffer_mut(),
                &model.shell,
                tier,
                glyphs,
                motion_name(pacer.motion_mode()),
            );
        })
        .map(|_| ())
        .map_err(|why| Failure::unreachable(format!("paint workspace frame: {why}")))
}

#[allow(clippy::too_many_arguments)]
async fn handle_workspace_input(
    event: Event,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    input_pump: &mut InputPump,
    data: &mut Client,
    pty_stream: &mut Option<ControlPump>,
    model: &mut ConsoleModel,
    hits: &ConsoleHits,
    pacer: &mut FramePacer,
) -> Result<bool, Failure> {
    let area = terminal
        .size()
        .map_err(|why| Failure::unreachable(format!("read terminal size: {why}")))?;
    if area.width < console::MIN_WIDTH || area.height < console::MIN_HEIGHT {
        return Ok(matches!(
            event,
            Event::Key(key)
                if (key.code == crossterm::event::KeyCode::Char('q')
                    && key.modifiers.is_empty())
                    || (key.code == crossterm::event::KeyCode::Char('c')
                        && key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL))
        ));
    }
    if let Event::Mouse(mouse) = &event {
        if model.shell.mode() != shell::ShellMode::View {
            return Ok(false);
        }
        match model.shell.surface() {
            Surface::Hall => {
                if let Some(HallTarget::Agent(agent)) = hits.hall.click(mouse)
                    && let Some(district) = model.estate.frame.districts.iter().find(|district| {
                        district
                            .stations
                            .iter()
                            .flat_map(|station| &station.agents)
                            .any(|candidate| candidate.id == *agent)
                    })
                {
                    model.hall_selected = Some(district.id.clone());
                }
            }
            Surface::WorkQueue => {
                model.queue_selected = hits.queue.click(mouse).cloned();
            }
            Surface::WorkConfig => {
                model.config_selected = hits.config.click(mouse).cloned();
            }
            Surface::TermLifetimes | Surface::TermAttach => {}
            _ => model.board_selected = hits.board.click(mouse).cloned(),
        }
        return Ok(false);
    }
    let Event::Key(key) = event else {
        return Ok(false);
    };
    let previous = model.shell.surface();
    let action = model.shell.handle(key);
    match action {
        ShellAction::None => {}
        ShellAction::FilterChanged(_) => reconcile_selection(model),
        ShellAction::Quit => return Ok(true),
        ShellAction::PreviewRoute(_) => {
            refresh_surface(data, model).await?;
            reconcile_selection(model);
        }
        ShellAction::NavigatorCanceled(_) => {
            refresh_surface(data, model).await?;
            reconcile_selection(model);
        }
        ShellAction::RouteChanged(destination) => {
            if model.drilldown.is_some() && destination != Surface::TermAttach {
                model.drilldown = None;
                pty_stream.take();
            }
            if previous == Surface::WorkConfig && model.shell.surface() != Surface::WorkConfig {
                model.config_form = None;
            }
            if previous == Surface::WorkQueue && model.shell.surface() != Surface::WorkQueue {
                model.gate_decision = None;
            }
            if previous == Surface::FleetAgents && model.shell.surface() != Surface::FleetAgents {
                model.budget_form = None;
            }
            refresh_surface(data, model).await?;
            reconcile_selection(model);
            if destination == Surface::TermAttach && model.drilldown.is_none() {
                if let Some(TermTarget::Session(session_id)) = model.term_selected.clone() {
                    // A refused attach (swept host, closed lifetime, stale
                    // generation) is a property of one row, not of the
                    // console: land back on the list with the kernel's
                    // reason instead of tearing the whole workspace down.
                    if let Err(error) =
                        open_workspace_attach(data, model, pty_stream, session_id).await
                    {
                        model.shell.route(Surface::TermLifetimes);
                        model.shell.set_notice(format!("attach refused -- {error}"));
                    }
                } else {
                    model.shell.route(Surface::TermLifetimes);
                    model
                        .shell
                        .set_notice("select a terminal lifetime before attaching");
                }
            }
        }
        ShellAction::MoveSelection(delta) => move_workspace_selection(model, delta),
        ShellAction::Drill => {
            if model.shell.surface() == Surface::TermLifetimes
                && let Some(TermTarget::Session(session_id)) = model.term_selected.clone()
            {
                if let Err(error) = open_workspace_attach(data, model, pty_stream, session_id).await
                {
                    model.shell.set_notice(format!("attach refused -- {error}"));
                }
            } else if model.shell.surface() == Surface::WorkConfig {
                open_config_edit(terminal, input_pump, data, model).await?;
            }
        }
        ShellAction::ToggleMotion => pacer.kill_motion(),
        ShellAction::ToggleRail => model.shell.toggle_rail(),
        ShellAction::EnterInput | ShellAction::LeaveInput | ShellAction::ForwardInput => {}
        ShellAction::OpenGateDecision => open_gate_decision(model),
        ShellAction::OpenBudgetForm => open_budget_form(model),
        ShellAction::MoveGateOption(delta) => {
            if let Some(decision) = model.gate_decision.as_mut() {
                decision.move_selection(delta);
            }
        }
        ShellAction::SelectGateOption(index) => {
            if let Some(decision) = model.gate_decision.as_mut() {
                decision.select(index);
            }
        }
        ShellAction::CommitGateDecision => commit_gate_decision(data, model).await?,
        ShellAction::CancelGateDecision => model.gate_decision = None,
        ShellAction::MoveFormField(delta) => {
            if let Some(form) = model.config_form.as_mut() {
                form.move_selection(delta);
            } else if let Some(form) = model.budget_form.as_mut() {
                form.move_selection(delta);
            }
        }
        ShellAction::EditForm(character) => {
            if let Some(form) = model.config_form.as_mut()
                && !(character == ' ' && form.toggle_boolean())
            {
                form.insert(character);
            } else if let Some(form) = model.budget_form.as_mut() {
                form.insert(character);
            }
        }
        ShellAction::BackspaceForm => {
            if let Some(form) = model.config_form.as_mut() {
                form.backspace();
            } else if let Some(form) = model.budget_form.as_mut() {
                form.backspace();
            }
        }
        ShellAction::CommitForm => {
            if model.config_form.is_some() {
                commit_config_form(data, model).await?;
            } else if let Some(form) = model.budget_form.as_ref() {
                match form.budget() {
                    Ok(_) => {
                        prepare_console_confirmation(model, ContextVerb::EditBudget);
                        if model.confirmation_target.is_some() {
                            model.shell.confirm(ContextVerb::EditBudget);
                        }
                    }
                    Err(error) => model.shell.set_notice(error),
                }
            }
        }
        ShellAction::CancelForm => {
            model.config_form = None;
            model.budget_form = None;
        }
        ShellAction::PrepareConfirmation(verb) => prepare_console_confirmation(model, verb),
        ShellAction::CancelConfirmation => {
            model.confirmation_target = None;
        }
        ShellAction::Confirmed(verb) => execute_console_verb(data, model, verb).await?,
    }
    Ok(false)
}

fn open_gate_decision(model: &mut ConsoleModel) {
    let Some(QueueTarget::Gate(id)) = model.queue_selected.as_ref() else {
        model.shell.cancel_modal();
        model
            .shell
            .set_notice("select an open gate before deciding");
        return;
    };
    let Some(gate) = model.queue.gates.iter().find(|gate| &gate.id == id) else {
        model.shell.cancel_modal();
        model.shell.set_notice("selected gate is no longer present");
        return;
    };
    if gate.verdict != GateVerdict::Pending
        || gate.question.is_none()
        || gate.options.as_ref().is_none_or(Vec::is_empty)
    {
        model.shell.cancel_modal();
        model
            .shell
            .set_notice("selected gate has no open decision options");
        return;
    }
    model.gate_decision = Some(GateDecisionState::new(gate.clone()));
}

fn open_budget_form(model: &mut ConsoleModel) {
    let Some(BoardTarget::Attempt(id)) = model.board_selected.as_ref() else {
        model
            .shell
            .set_notice("select an attempt before editing its budget");
        return;
    };
    let Some(attempt) = model
        .board
        .attempts
        .iter()
        .find(|attempt| &attempt.id == id)
    else {
        model
            .shell
            .set_notice("selected attempt is no longer present");
        return;
    };
    model.budget_form = Some(BudgetFormState::new(attempt));
    model.shell.enter_form();
}

async fn commit_gate_decision(data: &mut Client, model: &mut ConsoleModel) -> Result<(), Failure> {
    let Some(decision) = model.gate_decision.as_ref() else {
        model.shell.cancel_modal();
        model.shell.set_notice("gate decision is no longer open");
        return Ok(());
    };
    let Some(option) = decision.chosen().map(str::to_owned) else {
        model.shell.set_notice("gate carries no selectable option");
        return Ok(());
    };
    let Some(command) = queue::decide(&decision.gate, &option) else {
        model.shell.set_notice(format!(
            "option {option:?} has no typed pass/fail/partial mapping"
        ));
        return Ok(());
    };
    let key = format!(
        "tui:decide:{}:{}",
        decision.gate.id.as_str(),
        decision.gate.version
    );
    if let Err(error) = submit_console_command(data, &command, &key).await {
        model
            .shell
            .set_notice(format!("gate decision refused -- {error}"));
        model.queue = refresh_queue(data).await?;
        model.update_attention_marks();
        return Ok(());
    }
    model.gate_decision = None;
    model.shell.cancel_modal();
    model.shell.set_notice(format!("decided gate as {option}"));
    model.queue = refresh_queue(data).await?;
    model.update_attention_marks();
    Ok(())
}

async fn open_config_edit(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    input_pump: &mut InputPump,
    data: &mut Client,
    model: &mut ConsoleModel,
) -> Result<(), Failure> {
    let Some(ConfigTarget::File(path)) = model.config_selected.as_ref().cloned() else {
        model
            .shell
            .set_notice("select a configured file before editing");
        return Ok(());
    };
    let Some(repository) = model.config_repository.clone() else {
        model.shell.set_notice("config repository is unavailable");
        return Ok(());
    };
    match repository.generated_form(path) {
        Ok(form) => {
            model.config_form = Some(form);
            model.shell.enter_form();
        }
        Err(config::ConfigError::EditorRequired { .. })
        | Err(config::ConfigError::FormSourceMismatch { .. }) => {
            input_pump.stop();
            suspend_workspace_terminal()?;
            let evidence_id = EvidenceId::new(format!("config-{}", crate::nonce()));
            let edit = repository.commit_editor(path, evidence_id.clone());
            resume_workspace_terminal(terminal, input_pump)?;
            model.terminal_resumed = true;
            match edit {
                Ok(command) => {
                    match submit_console_command(
                        data,
                        &command,
                        &format!("tui:config:{}", evidence_id.as_str()),
                    )
                    .await
                    {
                        Ok(()) => model
                            .shell
                            .set_notice("config committed and evidence submitted"),
                        Err(error) => model.shell.set_notice(format!(
                            "config committed but evidence submission failed -- {error}"
                        )),
                    }
                    let (state, repository, _, notice) = refresh_config(data).await;
                    model.config = state;
                    model.config_repository = repository;
                    model.config_notice = notice;
                }
                Err(error) => model
                    .shell
                    .set_notice(format!("config editor refused -- {error}")),
            }
        }
        Err(error) => model
            .shell
            .set_notice(format!("config form refused -- {error}")),
    }
    Ok(())
}

fn suspend_workspace_terminal() -> Result<(), Failure> {
    disable_raw_mode().map_err(|why| Failure::unreachable(format!("suspend raw mode: {why}")))?;
    let mut stdout = std::io::stdout();
    stdout
        .queue(Show)
        .map_err(|why| Failure::unreachable(format!("show cursor before editor: {why}")))?;
    input::exit(&mut stdout)
        .map_err(|why| Failure::unreachable(format!("leave terminal screen for editor: {why}")))
}

fn resume_workspace_terminal(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    input_pump: &mut InputPump,
) -> Result<(), Failure> {
    enable_raw_mode().map_err(|why| Failure::unreachable(format!("resume raw mode: {why}")))?;
    let mut stdout = std::io::stdout();
    if let Err(why) = input::enter(&mut stdout) {
        restore_terminal();
        return Err(Failure::unreachable(format!(
            "resume terminal screen: {why}"
        )));
    }
    terminal
        .clear()
        .map_err(|why| Failure::unreachable(format!("clear resumed terminal: {why}")))?;
    input_pump.restart();
    Ok(())
}

async fn commit_config_form(data: &mut Client, model: &mut ConsoleModel) -> Result<(), Failure> {
    let Some(form) = model.config_form.as_ref() else {
        model.shell.cancel_modal();
        model.shell.set_notice("config form is no longer open");
        return Ok(());
    };
    if let Err(error) = form.validate_current() {
        model
            .shell
            .set_notice(format!("config form refused -- {error}"));
        return Ok(());
    }
    if !form.fields().iter().any(|field| field.changed()) {
        // finish() would refuse this as NoFormChanges AFTER the form was
        // consumed, destroying the session and the typed commit message;
        // catching it here keeps the form open like every other refusal.
        model
            .shell
            .set_notice("config form has no changes to commit");
        return Ok(());
    }
    let Some(repository) = model.config_repository.clone() else {
        model.shell.set_notice("config repository is unavailable");
        return Ok(());
    };
    let form = model.config_form.take().expect("form was checked above");
    let (validated, message) = match form.finish() {
        Ok(result) => result,
        Err(error) => {
            model
                .shell
                .set_notice(format!("config form refused -- {error}"));
            model.shell.cancel_modal();
            return Ok(());
        }
    };
    let evidence_id = EvidenceId::new(format!("config-{}", crate::nonce()));
    let command = match repository.commit_form(validated, &message, evidence_id.clone()) {
        Ok(command) => command,
        Err(error) => {
            model
                .shell
                .set_notice(format!("config form refused -- {error}"));
            model.shell.cancel_modal();
            return Ok(());
        }
    };
    let submitted = submit_console_command(
        data,
        &command,
        &format!("tui:config:{}", evidence_id.as_str()),
    )
    .await;
    model.shell.cancel_modal();
    match submitted {
        Ok(()) => model
            .shell
            .set_notice("config committed and evidence submitted"),
        Err(error) => model.shell.set_notice(format!(
            "config committed but evidence submission failed -- {error}"
        )),
    }
    let (state, repository, _, notice) = refresh_config(data).await;
    model.config = state;
    model.config_repository = repository;
    model.config_notice = notice;
    Ok(())
}

fn prepare_console_confirmation(model: &mut ConsoleModel, verb: ContextVerb) {
    let prepared = match verb {
        ContextVerb::AckAttention
        | ContextVerb::MuteAttention
        | ContextVerb::UnmuteAttention
        | ContextVerb::ResolveAttention => model.queue_selected.clone().and_then(|target| {
            let QueueTarget::Attention(id) = &target else {
                return None;
            };
            let label = format!("attention {}", id.as_str());
            Some((ConfirmationTarget::Queue(target), label))
        }),
        ContextVerb::StopAttempt => model.board_selected.clone().and_then(|target| {
            let BoardTarget::Attempt(id) = &target else {
                return None;
            };
            let label = format!("attempt {}", id.as_str());
            Some((ConfirmationTarget::Attempt(target), label))
        }),
        ContextVerb::EditBudget => model.budget_form.as_ref().map(|form| {
            let target = BoardTarget::Attempt(form.attempt_id.clone());
            (
                ConfirmationTarget::Budget(target),
                format!("attempt {}", form.attempt_id.as_str()),
            )
        }),
        ContextVerb::DecideGate | ContextVerb::EditConfig => None,
    };
    let Some((target, label)) = prepared else {
        model.confirmation_target = None;
        model.shell.cancel_modal();
        model
            .shell
            .set_notice("selected row does not support that action");
        return;
    };
    model.confirmation_target = Some(target);
    model.shell.set_confirmation_target(label);
}

async fn execute_console_verb(
    data: &mut Client,
    model: &mut ConsoleModel,
    verb: ContextVerb,
) -> Result<(), Failure> {
    let Some(target) = model.confirmation_target.take() else {
        model
            .shell
            .set_notice("confirmation target was lost; choose the row again");
        return Ok(());
    };
    if !verb_target_is_visible(model, verb, &target) {
        model
            .shell
            .set_notice("selected row is no longer visible; choose it again");
        return Ok(());
    }
    let command = match (verb, &target) {
        (ContextVerb::AckAttention, ConfirmationTarget::Queue(target)) => {
            queue::ack(target).map(|command| (command, format!("tui:ack:{}", crate::nonce())))
        }
        (ContextVerb::UnmuteAttention, ConfirmationTarget::Queue(target)) => {
            queue::unmute(target).map(|command| (command, format!("tui:unmute:{}", crate::nonce())))
        }
        (ContextVerb::ResolveAttention, ConfirmationTarget::Queue(target)) => {
            let QueueTarget::Attention(id) = target else {
                return Ok(());
            };
            queue::resolve(target, None)
                .map(|command| (command, format!("tui:resolve:{}", id.as_str())))
        }
        (ContextVerb::StopAttempt, ConfirmationTarget::Attempt(target)) => {
            let BoardTarget::Attempt(id) = target else {
                return Ok(());
            };
            board::stop_attempt(target)
                .map(|command| (command, format!("tui:stop:{}", id.as_str())))
        }
        (ContextVerb::MuteAttention, ConfirmationTarget::Queue(target)) => {
            match mute_deadline(model.shell.prompt_value(), &current_timestamp()) {
                Ok(until) => {
                    let QueueTarget::Attention(id) = target else {
                        return Ok(());
                    };
                    queue::mute(target, until).map(|command| {
                        (
                            command,
                            format!(
                                "tui:mute:{}:{}:{}",
                                id.as_str(),
                                model.shell.prompt_value(),
                                crate::nonce()
                            ),
                        )
                    })
                }
                Err(reason) => {
                    model.shell.set_notice(reason);
                    None
                }
            }
        }
        (ContextVerb::EditBudget, ConfirmationTarget::Budget(BoardTarget::Attempt(target_id))) => {
            model
                .budget_form
                .as_ref()
                .filter(|form| form.attempt_id == *target_id)
                .and_then(|form| {
                    form.budget().ok().map(|budget| {
                        (
                            board::replace_attempt_budget(
                                form.attempt_id.clone(),
                                form.expected_version,
                                budget,
                            ),
                            format!(
                                "tui:budget:{}:{}",
                                form.attempt_id.as_str(),
                                form.expected_version
                            ),
                        )
                    })
                })
        }
        (ContextVerb::EditConfig, _) => {
            model
                .shell
                .set_notice("select a configured file before editing");
            None
        }
        _ => None,
    };
    let Some((command, key)) = command else {
        if model.shell.notice().is_none() {
            model
                .shell
                .set_notice("selected row does not support that action");
        }
        return Ok(());
    };
    if let Err(error) = submit_console_command(data, &command, &key).await {
        model
            .shell
            .set_notice(format!("{} refused -- {error}", command.command_type()));
        if verb == ContextVerb::EditBudget {
            // The form was built against a version the kernel just refused;
            // key handling is already back in view mode, so a kept form
            // would paint dead fields over live keys. Drop it — 'b' rebuilds
            // from the refreshed attempt.
            model.budget_form = None;
        }
        model.queue = refresh_queue(data).await?;
        refresh_surface(data, model).await?;
        reconcile_selection(model);
        model.update_attention_marks();
        return Ok(());
    }
    if verb == ContextVerb::EditBudget {
        model.budget_form = None;
    }
    model
        .shell
        .set_notice(format!("submitted {}", command.command_type()));
    model.queue = refresh_queue(data).await?;
    refresh_surface(data, model).await?;
    reconcile_selection(model);
    model.update_attention_marks();
    Ok(())
}

fn mute_deadline(value: &str, now: &Timestamp) -> Result<Timestamp, &'static str> {
    let bytes = value.as_bytes();
    let canonical = bytes.len() == 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19) || byte.is_ascii_digit()
        });
    if !canonical {
        return Err("mute deadline must be YYYY-MM-DDTHH:MM:SSZ");
    }
    let number = |range: std::ops::Range<usize>| {
        value[range]
            .parse::<u32>()
            .expect("canonical timestamp ranges contain digits")
    };
    let year = number(0..4);
    let month = number(5..7);
    let day = number(8..10);
    let hour = number(11..13);
    let minute = number(14..16);
    let second = number(17..19);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    };
    if year == 0 || day == 0 || day > month_days || hour > 23 || minute > 59 || second > 59 {
        return Err("mute deadline is not a real UTC calendar instant");
    }
    if value <= now.as_str() {
        return Err("mute deadline must be later than the Queue clock");
    }
    Ok(Timestamp::new(value))
}

async fn submit_console_command(
    data: &mut Client,
    command: &gwk_domain::command::KernelCommand,
    key: &str,
) -> Result<(), Failure> {
    let envelope = crate::envelope(command, key, gwk_kernel::SYSTEM_PROJECT);
    let expected_id = envelope.command_id.clone();
    match data.ask(KernelRequest::SubmitCommand { envelope }).await? {
        KernelResult::Error { code, message, .. } => Err(Failure::new(code, message)),
        KernelResult::CommandApplied { command_id, .. } if command_id == expected_id => Ok(()),
        KernelResult::CommandApplied { command_id, .. } => Err(Failure::new(
            KernelErrorCode::Schema,
            format!(
                "submit command {} returned command {}",
                expected_id.as_str(),
                command_id.as_str()
            ),
        )),
        other => Err(Failure::new(
            KernelErrorCode::Schema,
            format!("submit command returned {other:?}"),
        )),
    }
}

fn verb_target_is_visible(
    model: &ConsoleModel,
    verb: ContextVerb,
    target: &ConfirmationTarget,
) -> bool {
    match (verb, target) {
        (
            ContextVerb::AckAttention
            | ContextVerb::MuteAttention
            | ContextVerb::UnmuteAttention
            | ContextVerb::ResolveAttention,
            ConfirmationTarget::Queue(target),
        ) => queue::target_order(&filtered_queue(&model.queue, model.shell.filter()))
            .contains(target),
        (ContextVerb::StopAttempt, ConfirmationTarget::Attempt(target)) => {
            board::target_order(&filtered_board(&model.board, model.shell.filter()))
                .contains(target)
        }
        (ContextVerb::EditBudget, ConfirmationTarget::Budget(BoardTarget::Attempt(attempt_id))) => {
            model
                .budget_form
                .as_ref()
                .is_some_and(|form| form.attempt_id == *attempt_id)
        }
        _ => false,
    }
}

fn move_workspace_selection(model: &mut ConsoleModel, delta: i8) {
    match model.shell.surface() {
        Surface::Hall => move_focus(&model.estate.frame, &mut model.hall_selected, delta),
        Surface::WorkQueue => {
            let state = filtered_queue(&model.queue, model.shell.filter());
            move_in_order(
                queue::target_order(&state),
                &mut model.queue_selected,
                delta,
            );
        }
        Surface::WorkConfig => {
            let state = filtered_config(&model.config, model.shell.filter());
            move_in_order(
                config::target_order(&state),
                &mut model.config_selected,
                delta,
            );
        }
        Surface::TermLifetimes => {
            let state = filtered_terms(&model.terms, model.shell.filter());
            move_in_order(
                shell::term_target_order(&state),
                &mut model.term_selected,
                delta,
            );
        }
        Surface::TermAttach => {
            if let Some(drilldown) = model.drilldown.as_mut() {
                drilldown.move_viewport_rows(delta);
            }
        }
        _ => {
            let state = filtered_board(&model.board, model.shell.filter());
            move_in_order(
                board::target_order(&state),
                &mut model.board_selected,
                delta,
            );
        }
    }
}

fn reconcile_selection(model: &mut ConsoleModel) {
    let queue = filtered_queue(&model.queue, model.shell.filter());
    reconcile_in_order(queue::target_order(&queue), &mut model.queue_selected);

    let config = filtered_config(&model.config, model.shell.filter());
    reconcile_in_order(config::target_order(&config), &mut model.config_selected);

    let terms = filtered_terms(&model.terms, model.shell.filter());
    reconcile_in_order(shell::term_target_order(&terms), &mut model.term_selected);

    let board = filtered_board(&model.board, model.shell.filter());
    reconcile_in_order(board::target_order(&board), &mut model.board_selected);
}

fn reconcile_in_order<T: Clone + PartialEq>(order: Vec<T>, selected: &mut Option<T>) {
    if order.is_empty() {
        *selected = None;
    } else if selected
        .as_ref()
        .is_none_or(|target| !order.contains(target))
    {
        *selected = order.first().cloned();
    }
}

fn attach_rail_state(model: &ConsoleModel, attached: &PtySessionId) -> AttachRailState {
    let agents = model
        .estate
        .frame
        .districts
        .iter()
        .flat_map(|district| &district.stations)
        .flat_map(|station| &station.agents);
    let mut running = 0;
    let mut blocked = 0;
    for agent in agents {
        match agent.state {
            AgentState::Running | AgentState::Starting | AgentState::Canceling => running += 1,
            AgentState::Blocked => blocked += 1,
            _ => {}
        }
    }
    let audible_attention = model.queue.attention.iter().filter(|item| {
        item.resolved_at.is_none()
            && item.acked_at.is_none()
            && !item
                .muted_until
                .as_ref()
                .is_some_and(|until| until > &model.queue.now)
    });
    let attention_count = audible_attention.clone().count()
        + model
            .queue
            .gates
            .iter()
            .filter(|gate| gate.verdict == GateVerdict::Pending && gate.question.is_some())
            .count();
    let mut queue = model
        .queue
        .gates
        .iter()
        .filter(|gate| gate.verdict == GateVerdict::Pending && gate.question.is_some())
        .map(|gate| AttachRailItem {
            text: format!("gate {}", gate.kind.as_deref().unwrap_or(gate.id.as_str())),
            attention: true,
        })
        .collect::<Vec<_>>();
    queue.extend(audible_attention.map(|item| AttachRailItem {
        text: item.summary.clone(),
        attention: true,
    }));
    queue.truncate(5);

    AttachRailState {
        running,
        attention: attention_count,
        blocked,
        cost: console::dollars(model.hall_cost_micros),
        terms: model
            .terms
            .sessions
            .iter()
            .map(|session| AttachRailTerm {
                subject: format!("{}:{}", session.id, session.generation),
                state: session.state.clone(),
                attached: &session.id == attached,
            })
            .collect(),
        queue,
    }
}

fn move_in_order<T: Clone + PartialEq>(order: Vec<T>, selected: &mut Option<T>, delta: i8) {
    if order.is_empty() {
        *selected = None;
        return;
    }
    let current = selected
        .as_ref()
        .and_then(|target| order.iter().position(|candidate| candidate == target))
        .unwrap_or_else(|| if delta < 0 { 0 } else { order.len() - 1 });
    let next = if delta < 0 {
        current.checked_sub(1).unwrap_or(order.len() - 1)
    } else {
        (current + 1) % order.len()
    };
    *selected = Some(order[next].clone());
}

async fn open_workspace_attach(
    data: &mut Client,
    model: &mut ConsoleModel,
    pty_stream: &mut Option<ControlPump>,
    session_id: PtySessionId,
) -> Result<(), Failure> {
    let mut drilldown = DrilldownState::new(session_id.clone());
    refresh_session_snapshot(data, &mut drilldown).await?;
    let mut stream = connect().await?;
    let (request_id, attached) = stream
        .attach_pty(
            session_id,
            drilldown.generation().cloned(),
            drilldown.frame_seq(),
        )
        .await?;
    drilldown.begin_attach(request_id);
    if drilldown.ingest(&attached) == IngestDisposition::Unrelated {
        return Err(Failure::internal(
            "PTY attach acknowledgement was unrelated",
        ));
    }
    if drilldown.cells().is_none() {
        refresh_session_snapshot(data, &mut drilldown).await?;
    }
    let generation = drilldown
        .generation()
        .map_or("?", |generation| generation.as_str());
    let title = model
        .terms
        .sessions
        .iter()
        .find(|session| session.id == *drilldown.session_id())
        .and_then(|session| session.title.as_deref())
        .unwrap_or("terminal");
    model.shell.set_attach_subject(format!(
        "{}:{}  {title}",
        drilldown.session_id().as_str(),
        generation
    ));
    model.drilldown = Some(drilldown);
    model.shell.route(Surface::TermAttach);
    *pty_stream = Some(ControlPump::start(stream));
    Ok(())
}

async fn refresh_surface(data: &mut Client, model: &mut ConsoleModel) -> Result<(), Failure> {
    match model.shell.surface() {
        Surface::Hall | Surface::WorkQueue => {}
        Surface::TermAttach => model.terms = refresh_terms(data).await?,
        Surface::WorkConfig => {
            let (state, repository, _, notice) = refresh_config(data).await;
            model.config = state;
            model.config_repository = repository;
            model.config_notice = notice;
        }
        Surface::FleetAgents => {
            model.board.view = BoardView::Fleet;
            refresh_board_projections(data, &mut model.board).await?;
        }
        Surface::TermLifetimes => model.terms = refresh_terms(data).await?,
        surface => {
            if let Some(view) = surface.board_view() {
                model.board.view = view;
                refresh_board_projections(data, &mut model.board).await?;
            }
        }
    }
    Ok(())
}

async fn refresh_queue(client: &mut Client) -> Result<QueueState, Failure> {
    let mut state = QueueState {
        attention: Vec::new(),
        gates: Vec::new(),
        receipts: Vec::new(),
        messages: Vec::new(),
        watermark: None,
        now: current_timestamp(),
    };
    for kind in [
        ProjectionKind::AttentionItem,
        ProjectionKind::Gate,
        ProjectionKind::Receipt,
        ProjectionKind::Message,
    ] {
        let (records, watermark) = read_projection_records(client, kind).await?;
        state.watermark = watermark.or(state.watermark);
        for record in records {
            match (kind, record) {
                (
                    ProjectionKind::AttentionItem,
                    ProjectionRecord::AttentionItem { attention_item },
                ) => {
                    state.attention.push(attention_item);
                }
                (ProjectionKind::Gate, ProjectionRecord::Gate { gate }) => state.gates.push(gate),
                (ProjectionKind::Receipt, ProjectionRecord::Receipt { receipt }) => {
                    state.receipts.push(receipt);
                }
                (ProjectionKind::Message, ProjectionRecord::Message { message }) => {
                    state.messages.push(message);
                }
                (_, record) => {
                    return Err(Failure::new(
                        KernelErrorCode::Schema,
                        format!(
                            "{} projection page carried a {} row",
                            kind.as_str(),
                            record.kind().as_str()
                        ),
                    ));
                }
            }
        }
    }
    Ok(state)
}

async fn refresh_terms(client: &mut Client) -> Result<TermState, Failure> {
    let (records, watermark) = read_projection_records(client, ProjectionKind::PtySession).await?;
    let mut sessions = Vec::with_capacity(records.len());
    for record in records {
        let ProjectionRecord::PtySession { pty_session } = record else {
            return Err(Failure::new(
                KernelErrorCode::Schema,
                "pty_session projection page carried another row",
            ));
        };
        sessions.push(pty_session);
    }
    Ok(TermState {
        sessions,
        watermark,
        complete: true,
    })
}

async fn refresh_config(
    client: &mut Client,
) -> (
    ConfigState,
    Option<ConfigRepository>,
    Option<Evidence>,
    Option<String>,
) {
    let empty = || ConfigState {
        files: Vec::new(),
        config_head: None,
        last_evidence_ref: None,
        dirty: false,
        divergent: false,
    };
    let current = match config_repository_root() {
        Ok(current) => current,
        Err(error) => {
            return (
                empty(),
                None,
                None,
                Some(format!("config unavailable -- {error}")),
            );
        }
    };
    let repository = match ConfigRepository::open(&current) {
        Ok(repository) => repository,
        Err(error) => {
            return (
                empty(),
                None,
                None,
                Some(format!("config unavailable -- {error}")),
            );
        }
    };
    let evidence = match read_projection_records(client, ProjectionKind::Evidence).await {
        Ok((records, _)) => records
            .into_iter()
            .filter_map(|record| match record {
                ProjectionRecord::Evidence { evidence } if evidence.kind == "config_change" => {
                    Some(evidence)
                }
                _ => None,
            })
            .max_by(|left, right| left.created_at.as_str().cmp(right.created_at.as_str())),
        Err(error) => {
            return (
                empty(),
                Some(repository),
                None,
                Some(format!("config evidence unavailable -- {error}")),
            );
        }
    };
    match repository.state(evidence.as_ref()) {
        Ok(state) => (state, Some(repository), evidence, None),
        Err(error) => (
            empty(),
            Some(repository),
            evidence,
            Some(format!("config unavailable -- {error}")),
        ),
    }
}

fn config_repository_root() -> Result<std::path::PathBuf, std::io::Error> {
    Ok(std::env::var_os("GRIDWORK_CORE_ROOT")
        .or_else(|| std::env::var_os("GRIDWORK_CORE_REPO"))
        .map(std::path::PathBuf::from)
        .unwrap_or(std::env::current_dir()?))
}

async fn refresh_hall_cost(client: &mut Client) -> Result<u64, Failure> {
    let now = current_timestamp();
    let (records, _) = read_projection_records(client, ProjectionKind::CostEntry).await?;
    records
        .into_iter()
        .map(|record| match record {
            ProjectionRecord::CostEntry { cost_entry } => Ok(cost_entry),
            other => Err(Failure::new(
                KernelErrorCode::Schema,
                format!(
                    "cost_entry projection page carried a {} row",
                    other.kind().as_str()
                ),
            )),
        })
        .collect::<Result<Vec<CostEntry>, Failure>>()
        .map(|entries| {
            entries
                .into_iter()
                .filter(|entry| console::same_utc_date(&entry.recorded_at, &now))
                .filter_map(|entry| entry.cost_micros)
                .fold(0u64, |total, value| total.saturating_add(value.value()))
        })
}

async fn read_projection_records(
    client: &mut Client,
    kind: ProjectionKind,
) -> Result<(Vec<ProjectionRecord>, Option<Seq>), Failure> {
    let mut records = Vec::new();
    let mut cursor = None;
    let mut seen = BTreeSet::new();
    let mut watermark = None;
    loop {
        let result = client
            .ask(KernelRequest::ListProjection {
                projection: kind,
                cursor: cursor.clone(),
                limit: Some(PAGE_LIMIT),
            })
            .await?;
        let KernelResult::ProjectionPage {
            records: page,
            next_cursor,
            watermark: page_watermark,
        } = result
        else {
            return result_failure(result, "read workspace projection");
        };
        watermark = page_watermark.or(watermark);
        records.extend(page);
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
    Ok((records, watermark))
}

fn current_timestamp() -> Timestamp {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    Timestamp::new(crate::rfc3339(seconds))
}

fn filtered_board(state: &BoardState, query: &str) -> BoardState {
    let mut filtered = state.clone();
    if query.is_empty() {
        return filtered;
    }
    filtered.tasks.retain(|row| debug_matches(row, query));
    filtered.runs.retain(|row| debug_matches(row, query));
    filtered.attempts.retain(|row| debug_matches(row, query));
    filtered.nodes.retain(|row| debug_matches(row, query));
    filtered.messages.retain(|row| debug_matches(row, query));
    filtered.events.retain(|row| debug_matches(row, query));
    filtered.attention.retain(|row| debug_matches(row, query));
    filtered.sessions.retain(|row| debug_matches(row, query));
    filtered.worktrees.retain(|row| debug_matches(row, query));
    filtered.leases.retain(|row| debug_matches(row, query));
    filtered.costs.retain(|row| debug_matches(row, query));
    filtered.receipts.retain(|row| debug_matches(row, query));
    filtered.ingested.retain(|row| debug_matches(row, query));
    filtered
}

fn filtered_queue(state: &QueueState, query: &str) -> QueueState {
    let mut filtered = state.clone();
    if query.is_empty() {
        return filtered;
    }
    filtered.attention.retain(|row| debug_matches(row, query));
    filtered.gates.retain(|row| debug_matches(row, query));
    filtered.receipts.retain(|row| debug_matches(row, query));
    filtered.messages.retain(|row| debug_matches(row, query));
    filtered
}

fn filtered_config(state: &ConfigState, query: &str) -> ConfigState {
    let mut filtered = state.clone();
    if !query.is_empty() {
        filtered.files.retain(|row| debug_matches(row, query));
    }
    filtered
}

fn filtered_terms(state: &TermState, query: &str) -> TermState {
    let mut filtered = state.clone();
    if !query.is_empty() {
        filtered.sessions.retain(|row| debug_matches(row, query));
    }
    filtered
}

fn debug_matches(value: &impl std::fmt::Debug, query: &str) -> bool {
    format!("{value:?}")
        .to_ascii_lowercase()
        .contains(&query.to_ascii_lowercase())
}

/// Run until the operator quits or the event subscription closes.
pub async fn run(requested_motion: MotionMode) -> Result<(), Failure> {
    let terminal_env = TerminalEnv::from_process(ColorChoice::Auto);
    let tier = resolve_tier(&terminal_env);
    if !interactive_terminal(&terminal_env) {
        let mut data = connect().await?;
        let mut events = EventIndex::default();
        let estate = refresh_estate(&mut data, &mut events).await?;
        return render_snapshot(&estate.frame, tier);
    }
    run_workspace(WorkspaceStart::hall(), requested_motion).await
}

/// Follow the durable event log in a bounded Board tail. Filters are exact
/// contract strings and never alter the subscription cursor, so reconnecting
/// from the last visible sequence remains gap-free. `view` names the panel
/// the Board opens on; every other panel stays one keystroke away.
pub async fn event_tail(
    cursor: Option<Seq>,
    aggregate_type: Option<String>,
    event_type: Option<String>,
    view: BoardView,
) -> Result<(), Failure> {
    let terminal_env = TerminalEnv::from_process(ColorChoice::Auto);
    let tier = resolve_tier(&terminal_env);
    if view == BoardView::Replay {
        check_snapshot_filters(
            view,
            cursor.as_ref(),
            aggregate_type.as_ref(),
            event_type.as_ref(),
        )?;
        return board_view_snapshot(view, tier).await;
    }
    if !interactive_terminal(&terminal_env) {
        check_snapshot_filters(
            view,
            cursor.as_ref(),
            aggregate_type.as_ref(),
            event_type.as_ref(),
        )?;
        return if view == BoardView::Events {
            event_tail_snapshot(cursor, aggregate_type, event_type, tier).await
        } else {
            board_view_snapshot(view, tier).await
        };
    }

    run_workspace(
        WorkspaceStart::events(cursor, aggregate_type, event_type, view),
        MotionMode::Full,
    )
    .await
}

fn event_board(
    view: BoardView,
    cursor: Option<Seq>,
    aggregate_type: Option<String>,
    event_type: Option<String>,
    live: bool,
) -> BoardState {
    BoardState {
        view,
        tasks: Vec::new(),
        runs: Vec::new(),
        attempts: Vec::new(),
        nodes: Vec::new(),
        messages: Vec::new(),
        events: Vec::new(),
        event_tail: EventTail {
            cursor,
            aggregate_type,
            event_type,
            live,
            dropped: 0,
        },
        attention: Vec::new(),
        replay: ReplayTimeline::empty(),
        sessions: Vec::new(),
        worktrees: Vec::new(),
        leases: Vec::new(),
        costs: Vec::new(),
        ingested: Vec::new(),
        receipts: Vec::new(),
        complete: true,
        watermark: None,
    }
}

fn append_event_batch(state: &mut BoardState, events: Vec<EventEnvelope>, cursor: Option<Seq>) {
    state.events.extend(events);
    state.event_tail.cursor = cursor.or(state.event_tail.cursor);
    if state.view == BoardView::Events {
        state.watermark = cursor.or(state.watermark);
    }
    if state.events.len() > EVENT_TAIL_ROWS {
        let remove = state.events.len() - EVENT_TAIL_ROWS;
        state.events.drain(..remove);
        state.event_tail.dropped = state.event_tail.dropped.saturating_add(remove);
    }
}

async fn event_tail_snapshot(
    cursor: Option<Seq>,
    aggregate_type: Option<String>,
    event_type: Option<String>,
    tier: ColorTier,
) -> Result<(), Failure> {
    let mut client = connect().await?;
    let result = client
        .ask(KernelRequest::ReadEvents {
            cursor,
            limit: EVENT_TAIL_ROWS as u32,
        })
        .await?;
    let KernelResult::Events {
        events,
        cursor: delivered,
        watermark,
    } = result
    else {
        return result_failure(result, "read event tail");
    };
    let mut state = event_board(BoardView::Events, cursor, aggregate_type, event_type, false);
    append_event_batch(&mut state, events, delivered);
    state.watermark = watermark;
    render_board_snapshot(&state, tier)
}

/// The tail filters only reach the events panel. Interactively that is fine —
/// the panel is one keystroke away and the filters are waiting when it opens —
/// but a snapshot renders one panel and stops, so there they would be absorbed
/// and never applied. Refuse instead: the parser's own rule is that a flag it
/// cannot honor is an error, not a shrug.
fn check_snapshot_filters(
    view: BoardView,
    cursor: Option<&Seq>,
    aggregate_type: Option<&String>,
    event_type: Option<&String>,
) -> Result<(), Failure> {
    if view == BoardView::Events {
        return Ok(());
    }
    let named: Vec<&str> = [
        cursor.map(|_| "--cursor"),
        aggregate_type.map(|_| "--aggregate-type"),
        event_type.map(|_| "--event-type"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if named.is_empty() {
        return Ok(());
    }
    Err(Failure::usage(format!(
        "{} filter the event tail, which --view {} does not render",
        named.join(" and "),
        view.as_str()
    )))
}

/// The non-interactive twin for every panel that folds projections instead of
/// the event log: one coherent read, one frame, done.
async fn board_view_snapshot(view: BoardView, tier: ColorTier) -> Result<(), Failure> {
    let state = crate::load_board_state(view_projections(view), view).await?;
    render_board_snapshot(&state, tier)
}

fn render_board_snapshot(state: &BoardState, tier: ColorTier) -> Result<(), Failure> {
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(std::io::stdout()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 100, 30)),
        },
    )
    .map_err(|why| Failure::unreachable(format!("open board snapshot terminal: {why}")))?;
    let mut hits = HitMap::new();
    terminal
        .draw(|frame| {
            board::render(
                frame.area(),
                frame.buffer_mut(),
                state,
                None,
                tier,
                GlyphSet::Ascii,
                &mut hits,
            );
        })
        .map(|_| ())
        .map_err(|why| Failure::unreachable(format!("paint board snapshot: {why}")))
}

/// The projections a Board panel folds — the kinds the live loop must page
/// before that panel has anything true to say. Events reads the tail the
/// subscription already delivers, and Replay wants a persisted recording
/// selected by id, which no Board code path fetches yet, so both map to
/// nothing here and render their own honest empties.
fn view_projections(view: BoardView) -> &'static [ProjectionKind] {
    match view {
        BoardView::Events | BoardView::Replay => &[],
        BoardView::Estate => crate::ESTATE_PROJECTIONS,
        BoardView::Activity => crate::ACTIVITY_PROJECTIONS,
        BoardView::Runs => &[ProjectionKind::WorkflowRun],
        BoardView::Dag => &[
            ProjectionKind::Task,
            ProjectionKind::Attempt,
            ProjectionKind::DispatchNode,
        ],
        BoardView::Flow => &[ProjectionKind::Message],
        BoardView::Fleet => crate::FLEET_PROJECTIONS,
        // Wider than the one-shot `cost rollup` list: the health half of the
        // panel folds ingested records, so the live view pages them too.
        BoardView::CostHealth => &[ProjectionKind::CostEntry, ProjectionKind::IngestedRecord],
        BoardView::Audit => &[ProjectionKind::Receipt],
    }
}

/// Re-page the active panel's projections and retain their projector watermark.
///
/// Every page is its own request, so a write landing mid-sweep skews the
/// watermarks between pages. The sweep retries a bounded number of times for
/// a coherent read and then accepts the torn join — it heals on the next
/// event batch, and this runs on every batch, exactly when writes are in
/// flight, so a torn fold must never be fatal. The stamp keeps the OLDEST
/// watermark seen, so the as-of line never claims freshness some page
/// doesn't have.
async fn refresh_board_projections(
    client: &mut Client,
    state: &mut BoardState,
) -> Result<(), Failure> {
    if view_projections(state.view).is_empty() {
        // Nothing paged (the Events tail folds from the stream): leave the
        // tail-maintained as-of stamp alone.
        return Ok(());
    }
    let mut watermarks = Vec::new();
    for _ in 0..SNAPSHOT_ATTEMPTS {
        watermarks.clear();
        for &kind in view_projections(state.view) {
            clear_board_projection(state, kind);
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
                let (records, next_cursor, watermark) = match result {
                    KernelResult::ProjectionPage {
                        records,
                        next_cursor,
                        watermark,
                    } => (records, next_cursor, watermark),
                    KernelResult::Error { code, message, .. } => {
                        return Err(Failure::new(code, message));
                    }
                    other => {
                        return Err(Failure::internal(format!(
                            "refresh board projections: kernel answered with {other:?}"
                        )));
                    }
                };
                watermarks.push(watermark);
                for record in records {
                    crate::push_board_record(state, kind, record)?;
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
        let coherent = watermarks
            .first()
            .is_none_or(|first| watermarks.iter().all(|watermark| watermark == first));
        if coherent {
            break;
        }
    }
    state.watermark = watermarks.iter().copied().flatten().min();
    Ok(())
}

fn clear_board_projection(state: &mut BoardState, kind: ProjectionKind) {
    match kind {
        ProjectionKind::Task => state.tasks.clear(),
        ProjectionKind::Attempt => state.attempts.clear(),
        ProjectionKind::AttentionItem => state.attention.clear(),
        ProjectionKind::Worktree => state.worktrees.clear(),
        ProjectionKind::Lease => state.leases.clear(),
        ProjectionKind::CostEntry => state.costs.clear(),
        ProjectionKind::EngineSession => state.sessions.clear(),
        ProjectionKind::DispatchNode => state.nodes.clear(),
        ProjectionKind::WorkflowRun => state.runs.clear(),
        ProjectionKind::Message => state.messages.clear(),
        ProjectionKind::Receipt => state.receipts.clear(),
        ProjectionKind::IngestedRecord => state.ingested.clear(),
        // Kinds no Board panel folds; `view_projections` never asks for them.
        _ => {}
    }
}

/// Attach to one explicitly named hosted PTY session. Engine-session ids are
/// deliberately not coerced into PTY ids: the contract keeps those namespaces
/// separate, and callers use `session list/inspect` for the former.
pub async fn term_attach(session_id: PtySessionId) -> Result<(), Failure> {
    let terminal_env = TerminalEnv::from_process(ColorChoice::Auto);
    let tier = resolve_tier(&terminal_env);
    if !interactive_terminal(&terminal_env) {
        let mut data = connect().await?;
        let mut state = DrilldownState::new(session_id);
        refresh_session_snapshot(&mut data, &mut state).await?;
        return render_session_snapshot(&state, tier);
    }
    run_workspace(WorkspaceStart::attach(session_id), MotionMode::Off).await
}

fn interactive_terminal(environment: &TerminalEnv) -> bool {
    environment.stdout_is_tty && std::io::stdin().is_terminal()
}

async fn refresh_session_snapshot(
    client: &mut Client,
    state: &mut DrilldownState,
) -> Result<(), Failure> {
    let result = client
        .ask(KernelRequest::PtySnapshot {
            session_id: state.session_id().clone(),
        })
        .await?;
    let result = match result {
        KernelResult::Error { code, message, .. } => return Err(Failure::new(code, message)),
        result => result,
    };
    let response = ServerControl::Response {
        request_id: RequestId::new("gw-session-snapshot"),
        result,
    };
    if state.ingest(&response) == IngestDisposition::Unrelated {
        return Err(Failure::internal(
            "TERM snapshot answered with an unrelated value",
        ));
    }
    Ok(())
}

fn render_session_snapshot(state: &DrilldownState, tier: ColorTier) -> Result<(), Failure> {
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(std::io::stdout()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 100, 30)),
        },
    )
    .map_err(|why| Failure::unreachable(format!("open TERM snapshot terminal: {why}")))?;
    let mut hits = HitMap::new();
    terminal
        .draw(|frame| {
            drilldown::render(
                frame.area(),
                frame.buffer_mut(),
                state,
                None,
                tier,
                &mut hits,
            );
        })
        .map(|_| ())
        .map_err(|why| Failure::unreachable(format!("paint TERM snapshot: {why}")))
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

/// Terminal-state attempts older than this collapse into the district
/// heading's `+N done` count instead of holding agent slots forever.
const AGED_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// The caller-resolved aging boundary: the lens reads no clock, so the runtime
/// stamps it once per refresh, in the kernel's own UTC RFC 3339 shape.
fn aged_cutoff() -> Timestamp {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
        .saturating_sub(AGED_AFTER.as_secs());
    Timestamp::new(crate::rfc3339(secs))
}

async fn refresh_estate(
    client: &mut Client,
    events: &mut EventIndex,
) -> Result<EstateSnapshot, Failure> {
    let mut last_retry = None;
    for _ in 0..SNAPSHOT_ATTEMPTS {
        let projections = load_projection_pages(client).await?;
        load_events_to_head(client, events).await?;
        match events.build(&projections, Some(&aged_cutoff())) {
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
    use gwk_domain::envelope::{Actor, Origin};
    use gwk_domain::ids::{AggregateId, EventId, ProjectId};
    use gwk_tui::hall::{Agent, AgentId, Attention, AttentionId, District, Station, StationId};

    fn event(seq: u64) -> EventEnvelope {
        EventEnvelope {
            event_id: EventId::new(format!("event-{seq}")),
            project_id: ProjectId::new("system"),
            aggregate_type: "attempt".into(),
            aggregate_id: AggregateId::new("attempt-1"),
            aggregate_version: u32::try_from(seq).unwrap_or(u32::MAX),
            event_type: "attempt_changed".into(),
            schema_version: 1,
            global_sequence: Seq::new(seq),
            occurred_at: Timestamp::new("2026-08-09T12:00:00Z"),
            appended_at: Timestamp::new("2026-08-09T12:00:00Z"),
            actor: Actor {
                kind: "kernel".into(),
                id: None,
            },
            origin: Origin {
                system: "gwk".into(),
                r#ref: None,
            },
            causation_id: None,
            correlation_id: None,
            idempotency_key: None,
            payload: serde_json::Value::Null,
            payload_ref: None,
        }
    }

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
            aged_done: 0,
            changed_seq: Seq::new(1),
        }
    }

    fn agent(seq: u64) -> Agent {
        Agent {
            id: AgentId::new("agent-a").expect("agent id"),
            role: None,
            state: AgentState::Running,
            started_at: None,
            duration: Some("live".to_owned()),
            changed_seq: Seq::new(seq),
        }
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

    #[test]
    fn event_tail_bounds_memory_and_keeps_the_resume_sequence() {
        let mut state = event_board(BoardView::Events, None, None, None, true);
        let events = (1..=(EVENT_TAIL_ROWS as u64 + 2)).map(event).collect();
        append_event_batch(
            &mut state,
            events,
            Some(Seq::new(EVENT_TAIL_ROWS as u64 + 2)),
        );
        assert_eq!(state.events.len(), EVENT_TAIL_ROWS);
        assert_eq!(state.event_tail.dropped, 2);
        assert_eq!(state.events[0].global_sequence, Seq::new(3));
        assert_eq!(state.watermark, Some(Seq::new(EVENT_TAIL_ROWS as u64 + 2)));
    }

    #[test]
    fn snapshot_refuses_tail_filters_it_would_never_apply() {
        let cursor = Seq::new(7);
        let kind = "attempt".to_owned();
        // The events snapshot is the one that reads them.
        assert!(
            check_snapshot_filters(BoardView::Events, Some(&cursor), Some(&kind), Some(&kind))
                .is_ok()
        );
        // Any other panel without filters is an ordinary snapshot.
        assert!(check_snapshot_filters(BoardView::Runs, None, None, None).is_ok());
        for filtered in [
            check_snapshot_filters(BoardView::Runs, Some(&cursor), None, None),
            check_snapshot_filters(BoardView::Fleet, None, Some(&kind), None),
            check_snapshot_filters(BoardView::Audit, None, None, Some(&kind)),
        ] {
            let failure = filtered.expect_err("a dropped filter must refuse");
            assert_eq!(failure.exit, crate::exit::USAGE, "{failure:?}");
        }
    }

    #[test]
    fn mute_deadline_is_explicit_canonical_and_future() {
        let now = Timestamp::new("2026-08-11T17:30:00Z");
        assert_eq!(
            mute_deadline("2026-08-11T18:00:00Z", &now),
            Ok(Timestamp::new("2026-08-11T18:00:00Z"))
        );
        for invalid in [
            "2026-08-11T17:29:59Z",
            "2026-02-29T18:00:00Z",
            "2026-08-11 18:00:00Z",
            "2026-08-11T18:00:00+00:00",
        ] {
            assert!(mute_deadline(invalid, &now).is_err(), "{invalid}");
        }
        assert!(
            mute_deadline("2028-02-29T18:00:00Z", &now).is_ok(),
            "leap-day deadlines remain valid"
        );
    }

    #[test]
    fn every_view_names_its_projections() {
        // Events reads the subscription and Replay wants a selected recording;
        // every other panel must page at least one projection kind or it can
        // only ever render its empty state.
        for view in BoardView::ALL {
            let kinds = view_projections(view);
            match view {
                BoardView::Events | BoardView::Replay => assert!(kinds.is_empty(), "{view:?}"),
                _ => assert!(!kinds.is_empty(), "{view:?} pages nothing"),
            }
        }
    }
}
