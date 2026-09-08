// Contract test for the event pipeline. FROZEN: a solver must not modify this file.
#[path = "render.rs"]
mod render;

use render::router;
use render::router::event::Event;
use render::router::Channel;

// --- the existing variants must keep behaving exactly as they do now ---

#[test]
fn queued_is_unchanged() {
    let ev = Event::Queued { id: 7 };
    assert_eq!(ev.id(), 7);
    assert_eq!(router::channel_for(&ev), Channel::Progress);
    assert_eq!(router::should_retry(&ev), false);
    assert_eq!(render::line(&ev), "#7 queued");
}

#[test]
fn done_is_unchanged() {
    let ev = Event::Done { id: 8 };
    assert_eq!(ev.id(), 8);
    assert_eq!(router::channel_for(&ev), Channel::Progress);
    assert_eq!(router::should_retry(&ev), false);
    assert_eq!(render::line(&ev), "#8 done");
}

#[test]
fn failed_is_unchanged() {
    let ev = Event::Failed { id: 9, why: "disk full".to_string() };
    assert_eq!(ev.id(), 9);
    assert_eq!(router::channel_for(&ev), Channel::Alert);
    assert_eq!(router::should_retry(&ev), true);
    assert_eq!(render::line(&ev), "#9 failed: disk full");
}

// --- the new variant, at every site ---

#[test]
fn cancelled_reports_its_id() {
    let ev = Event::Cancelled { id: 12, by: "ops".to_string() };
    assert_eq!(ev.id(), 12);
}

#[test]
fn cancelled_is_an_alert_not_progress() {
    // A human cancelled a job. An operator wants to see that.
    let ev = Event::Cancelled { id: 12, by: "ops".to_string() };
    assert_eq!(router::channel_for(&ev), Channel::Alert);
}

#[test]
fn cancelled_is_never_retried() {
    // Retrying something a human deliberately stopped is the wrong answer, even
    // though `Failed` -- the other Alert-channel variant -- IS retried.
    let ev = Event::Cancelled { id: 12, by: "ops".to_string() };
    assert_eq!(router::should_retry(&ev), false);
}

#[test]
fn cancelled_renders_with_who_cancelled_it() {
    let ev = Event::Cancelled { id: 12, by: "ops".to_string() };
    assert_eq!(render::line(&ev), "#12 cancelled by ops");
}
