//! Decides where an event goes and whether it should be retried.

#[path = "event.rs"]
pub mod event;

use event::Event;

/// Which subscriber channel an event is delivered on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Routine progress. Cheap, high volume.
    Progress,
    /// Something an operator must look at.
    Alert,
}

/// Route an event to its channel.
///
/// Anything an operator must act on goes to `Alert`; ordinary progress goes to
/// `Progress`.
pub fn channel_for(ev: &Event) -> Channel {
    match ev {
        Event::Queued { .. } => Channel::Progress,
        Event::Done { .. } => Channel::Progress,
        Event::Failed { .. } => Channel::Alert,
    }
}

/// Whether the job behind this event should be retried automatically.
///
/// Only a genuine failure is worth retrying.
pub fn should_retry(ev: &Event) -> bool {
    match ev {
        Event::Queued { .. } => false,
        Event::Done { .. } => false,
        Event::Failed { .. } => true,
    }
}
