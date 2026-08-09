use std::time::Duration;

use gwk_tui::hall::MotionMode;
use gwk_tui::runtime::{ADAPTIVE_SAMPLE_WINDOW, FramePacer, resolve_motion};

fn observe_window(pacer: &mut FramePacer, cost: Duration) {
    for _ in 0..ADAPTIVE_SAMPLE_WINDOW {
        pacer.observe_render(cost);
    }
}

#[test]
fn hall_runtime_freezes_motion_for_dumb_or_non_tty_outputs() {
    assert_eq!(
        resolve_motion(MotionMode::Full, Some("dumb"), true),
        MotionMode::Off
    );
    assert_eq!(
        resolve_motion(MotionMode::Reduced, Some("xterm-256color"), false),
        MotionMode::Off
    );
    assert_eq!(
        resolve_motion(MotionMode::Reduced, Some("xterm-256color"), true),
        MotionMode::Reduced
    );
}

#[test]
fn hall_runtime_adapts_through_the_ruled_cadence_ladder() {
    let mut pacer = FramePacer::new(MotionMode::Full);
    assert_eq!(pacer.interval(), Some(Duration::from_millis(33)));

    observe_window(&mut pacer, Duration::from_millis(40));
    assert_eq!(pacer.last_p95(), Some(Duration::from_millis(40)));
    assert_eq!(pacer.interval(), Some(Duration::from_millis(66)));
    observe_window(&mut pacer, Duration::from_millis(80));
    assert_eq!(pacer.interval(), Some(Duration::from_millis(125)));
    observe_window(&mut pacer, Duration::from_millis(130));
    assert_eq!(pacer.interval(), None);
    assert_eq!(pacer.motion_mode(), MotionMode::Off);
}

#[test]
fn hall_runtime_only_promotes_after_sustained_headroom() {
    let mut pacer = FramePacer::new(MotionMode::Reduced);
    for _ in 0..2 {
        observe_window(&mut pacer, Duration::from_millis(5));
        assert_eq!(pacer.interval(), Some(Duration::from_millis(33)));
    }
    observe_window(&mut pacer, Duration::from_millis(5));
    assert_eq!(pacer.interval(), Some(Duration::from_millis(16)));
    assert_eq!(pacer.motion_mode(), MotionMode::Reduced);
}

#[test]
fn hall_runtime_kill_switch_is_immediate_and_sticky() {
    let mut pacer = FramePacer::new(MotionMode::Full);
    assert!(pacer.is_animating(true, false));
    pacer.kill_motion();
    assert_eq!(pacer.motion_mode(), MotionMode::Off);
    assert_eq!(pacer.interval(), None);
    assert!(!pacer.is_animating(true, true));
    observe_window(&mut pacer, Duration::from_millis(1));
    assert_eq!(pacer.motion_mode(), MotionMode::Off);
}

#[test]
fn hall_runtime_render_cost_spends_from_the_cadence_budget() {
    let pacer = FramePacer::new(MotionMode::Full);
    assert_eq!(
        pacer.delay_after(Duration::from_millis(10)),
        Some(Duration::from_millis(23))
    );
    assert_eq!(
        pacer.delay_after(Duration::from_millis(40)),
        Some(Duration::ZERO)
    );
    assert!(!pacer.frame_due(Duration::from_millis(10)));
    assert!(!pacer.frame_due(Duration::from_millis(32)));
    assert!(pacer.frame_due(Duration::from_millis(33)));
}
