//! Pure scheduling decisions for the production terminal loop.

use std::time::Duration;

use crate::hall::MotionMode;

const CADENCE: [Duration; 4] = [
    Duration::from_millis(16),
    Duration::from_millis(33),
    Duration::from_millis(66),
    Duration::from_millis(125),
];
const DEFAULT_RUNG: usize = 1;
const OFF_RUNG: usize = CADENCE.len();
const PROMOTION_WINDOWS: u8 = 3;

/// Enough observations for p95 to describe a tail rather than one frame.
pub const ADAPTIVE_SAMPLE_WINDOW: usize = 20;

/// Apply the terminal gates that force a complete static frame.
pub fn resolve_motion(
    requested: MotionMode,
    term: Option<&str>,
    stdout_is_tty: bool,
) -> MotionMode {
    if !stdout_is_tty || term == Some("dumb") {
        MotionMode::Off
    } else {
        requested
    }
}

/// Adaptive frame pacing over measured full draw/flush cost.
#[derive(Debug, Clone)]
pub struct FramePacer {
    requested: MotionMode,
    rung: usize,
    samples: Vec<Duration>,
    last_p95: Option<Duration>,
    promotion_windows: u8,
    killed: bool,
}

impl FramePacer {
    pub fn new(requested: MotionMode) -> Self {
        let killed = requested == MotionMode::Off;
        Self {
            requested,
            rung: if killed { OFF_RUNG } else { DEFAULT_RUNG },
            samples: Vec::with_capacity(ADAPTIVE_SAMPLE_WINDOW),
            last_p95: None,
            promotion_windows: 0,
            killed,
        }
    }

    pub fn interval(&self) -> Option<Duration> {
        (!self.killed)
            .then(|| CADENCE.get(self.rung).copied())
            .flatten()
    }

    /// Remaining wait before the next frame start. Rendering spends from the
    /// cadence budget; it is not added on top of that budget.
    pub fn delay_after(&self, render_elapsed: Duration) -> Option<Duration> {
        self.interval()
            .map(|interval| interval.saturating_sub(render_elapsed))
    }

    /// Whether another frame may start. Dirty input changes state immediately,
    /// but this gate coalesces paints until the active cadence deadline.
    pub fn frame_due(&self, since_last_frame: Duration) -> bool {
        self.interval()
            .is_none_or(|interval| since_last_frame >= interval)
    }

    pub fn motion_mode(&self) -> MotionMode {
        if self.interval().is_some() {
            self.requested
        } else {
            MotionMode::Off
        }
    }

    pub fn last_p95(&self) -> Option<Duration> {
        self.last_p95
    }

    pub fn is_animating(&self, has_ticks: bool, has_one_shots: bool) -> bool {
        self.motion_mode() != MotionMode::Off && (has_ticks || has_one_shots)
    }

    /// The runtime key is a kill switch, not a toggle: one press cannot
    /// accidentally re-enable motion during the same session.
    pub fn kill_motion(&mut self) {
        self.killed = true;
        self.rung = OFF_RUNG;
        self.samples.clear();
        self.promotion_windows = 0;
    }

    pub fn observe_render(&mut self, cost: Duration) {
        if self.killed {
            return;
        }
        self.samples.push(cost);
        if self.samples.len() < ADAPTIVE_SAMPLE_WINDOW {
            return;
        }
        self.samples.sort_unstable();
        let rank = (self.samples.len() * 95).div_ceil(100).saturating_sub(1);
        let p95 = self.samples[rank];
        self.samples.clear();
        self.last_p95 = Some(p95);

        if let Some(budget) = CADENCE.get(self.rung)
            && p95 > *budget
        {
            self.rung = self.rung.saturating_add(1).min(OFF_RUNG);
            self.promotion_windows = 0;
            return;
        }

        let Some(previous_budget) = self
            .rung
            .checked_sub(1)
            .and_then(|previous| CADENCE.get(previous))
        else {
            self.promotion_windows = 0;
            return;
        };
        if p95 <= *previous_budget / 2 {
            self.promotion_windows = self.promotion_windows.saturating_add(1);
            if self.promotion_windows >= PROMOTION_WINDOWS {
                self.rung = self.rung.saturating_sub(1);
                self.promotion_windows = 0;
            }
        } else {
            self.promotion_windows = 0;
        }
    }
}
