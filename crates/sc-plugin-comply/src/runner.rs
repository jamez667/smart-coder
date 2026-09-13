//! Running the audit off the protocol loop.
//!
//! [`crate::comply::run`] is explicitly blocking — its own doc says callers must run it
//! off the UI thread, because a ten-framework audit walks the workspace once per pack and,
//! with a model chosen, makes a summary call plus a batched guidance call per framework.
//!
//! The plugin's loop is a blocking read on stdin. Running the audit inline would stop it
//! answering the host for minutes — no panel updates, no shutdown, nothing. So the audit
//! goes on a worker thread and reports back down a channel, which the loop drains after
//! every host message and on an idle tick. Same shape as the Claude plugin's runner, for
//! the same reason.
//!
//! There is no cancel. The audit is a single blocking call into `sc-comply`, with no
//! cooperative check inside it to observe a flag — and a cancel button that does nothing
//! is worse than no button. Closing the editor ends the process and the thread with it.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use crate::comply::{output_dir, ComplyModel, ComplyReport};
use crate::config::ComplyConfig;

/// What a finished audit reports back.
pub enum Event {
    /// The audit finished — with the report, or with the reason it could not.
    Done(Box<Result<ComplyReport, String>>),
}

/// An audit in flight. Holding one is what "running" means.
pub struct Audit {
    _thread: std::thread::JoinHandle<()>,
}

impl Audit {
    /// Start the audit on a worker thread.
    ///
    /// The config is loaded **here, on the worker**, not at startup: a user who fixes a
    /// missing API key and runs again should not have to restart the editor for the
    /// plugin to notice. `ComplyConfig::load` degrades to defaults on a missing or
    /// malformed file, so this cannot fail.
    pub fn start(workspace: PathBuf, choice: ComplyModel, tx: Sender<Event>) -> Self {
        let thread = std::thread::spawn(move || {
            let cfg = ComplyConfig::load();
            let out = output_dir(&workspace);
            // `ComplyError` implements Display and every variant already names the fix,
            // so the string the panel shows is the one the type wrote.
            let result =
                crate::comply::run(&workspace, &out, choice, &cfg).map_err(|e| e.to_string());
            // A send failure means the loop is gone, i.e. the host closed. Nothing to do.
            let _ = tx.send(Event::Done(Box::new(result)));
        });
        Self { _thread: thread }
    }
}
