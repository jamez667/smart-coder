//! The event kinds the daemon can emit.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A job was accepted onto the queue.
    Queued { id: u32 },
    /// A job finished successfully.
    Done { id: u32 },
    /// A job failed with a message.
    Failed { id: u32, why: String },
    /// A human stopped the job before it finished.
    Cancelled { id: u32, by: String },
}

impl Event {
    /// The job this event is about.
    pub fn id(&self) -> u32 {
        match self {
            Event::Queued { id } => *id,
            Event::Done { id } => *id,
            Event::Failed { id, .. } => *id,
            Event::Cancelled { id, .. } => *id,
        }
    }
}
