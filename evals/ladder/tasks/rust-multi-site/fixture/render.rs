//! Renders an event as one line of operator-facing text.

#[path = "router.rs"]
pub mod router;

use router::event::Event;

/// One line per event, in the house format `#<id> <state>[: <detail>]`.
pub fn line(ev: &Event) -> String {
    match ev {
        Event::Queued { id } => format!("#{id} queued"),
        Event::Done { id } => format!("#{id} done"),
        Event::Failed { id, why } => format!("#{id} failed: {why}"),
    }
}
