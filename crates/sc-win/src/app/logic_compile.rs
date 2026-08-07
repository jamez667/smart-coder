//! Running the project's compiler and turning its output into problems.
//!
//! The rules live in [`sc_win::project`] (what to run) and [`sc_win::diagnostics`] (what the
//! output means); this is the glue that spawns the child, streams it, and hands back a report.
//!
//! Two things it must never do: block the UI thread (a cold Unity build is minutes), and flash a
//! console window (every spawn goes through [`sc_win::proc::command`]). Spec 21.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use iced::Task;

use sc_win::diagnostics::CompileReport;
use sc_win::project::{self, ProjectKind};

use super::{App, BottomTab, Message};

impl App {
    /// Re-detect the project kind. Called on workspace change, so the button reflects whatever
    /// is actually open rather than whatever was open at launch.
    pub(crate) fn refresh_project_kind(&mut self) {
        self.project_kind = if self.picked_workspace.is_some() {
            project::detect(&self.workspace_root())
        } else {
            ProjectKind::Unknown
        };
        // A previous project's problems must not linger against a new one.
        self.compile_report = None;
    }

    /// Start a compile, unless one is already running.
    pub(crate) fn start_compile(&mut self) -> Task<Message> {
        if self.compiling {
            return Task::none(); // never stack two builds
        }
        let root = self.workspace_root();
        let kind = self.project_kind;
        let unity_override = self.unity_path_input.clone();

        // Resolve the command BEFORE showing a spinner: "Unity 2022.3.10f1 was not found" is an
        // answer, and making the user wait for it would be theatre.
        let cmd = match project::compile_command(
            &root,
            kind,
            Some(unity_override.as_str()).filter(|s| !s.trim().is_empty()),
        ) {
            Ok(c) => c,
            Err(why) => {
                self.compile_report = Some(CompileReport {
                    failure: Some(why),
                    ..CompileReport::default()
                });
                self.bottom_tab = BottomTab::Problems;
                return Task::none();
            }
        };

        let cancel = Arc::new(AtomicBool::new(false));
        self.compile_cancel = Some(cancel.clone());
        self.compiling = true;
        self.compile_report = None;
        self.bottom_tab = BottomTab::Problems; // put the user where the answer will appear

        Task::perform(
            async move {
                // Blocking: this is a full compiler run. Never on the UI thread.
                tokio::task::spawn_blocking(move || run_compile(&cmd, &root, &cancel))
                    .await
                    .unwrap_or_else(|e| CompileReport {
                        failure: Some(format!("the compile thread panicked: {e}")),
                        ..CompileReport::default()
                    })
            },
            |r| Message::CompileDone(Box::new(r)),
        )
    }

    /// Ask an in-flight compile to stop.
    pub(crate) fn cancel_compile(&mut self) {
        if let Some(c) = &self.compile_cancel {
            c.store(true, Ordering::Relaxed);
        }
    }

    /// Open the file a diagnostic points at, and scroll to its line.
    ///
    /// This is the feature: a list you can act on, rather than a log you have to read. Opens in
    /// the REVIEW view — a compile error is something to look at, and it keeps the diff wash.
    pub(crate) fn open_diagnostic(&mut self, index: usize) -> Task<Message> {
        let Some(d) = self
            .compile_report
            .as_ref()
            .and_then(|r| r.diagnostics.get(index))
        else {
            return Task::none();
        };
        let (file, line) = (d.file.clone(), d.line);
        // Only open files that are actually in this workspace. A diagnostic can point into a
        // package cache or an absolute path elsewhere; silently opening nothing is better than
        // a tab full of "(file not found)".
        if !self.workspace_root().join(&file).is_file() {
            return Task::none();
        }
        self.follow_agent = false;
        self.select_file_for_review(file);
        // Deferred, like the git-tab jump: the scroll has to run against the newly laid-out
        // content, not the previous file's.
        self.panes.focused_mut().pending_scroll_line = Some(line);
        Task::done(Message::JumpToPendingLine)
    }
}

/// Run `cmd` in `root`, streaming its output and parsing diagnostics as they arrive.
///
/// Reads stdout and stderr both — toolchains disagree about which one diagnostics belong on, and
/// Unity's `-logFile -` writes to stdout while its fatal errors go to stderr.
fn run_compile(
    cmd: &project::CompileCommand,
    root: &std::path::Path,
    cancel: &Arc<AtomicBool>,
) -> CompileReport {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = match sc_win::proc::command(&cmd.program)
        .args(&cmd.args)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return CompileReport {
                // Distinguish "couldn't launch the compiler" from "your code is broken" — they
                // have entirely different fixes.
                failure: Some(format!("Could not run `{}`: {e}", cmd.display())),
                ..CompileReport::default()
            };
        }
    };

    // Drain stdout on a helper thread so a full pipe can't deadlock the child while we read
    // stderr (Unity is chatty enough to fill one).
    let mut out = String::new();
    let stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_string(&mut buf);
        }
        buf
    });

    // Poll for exit so cancellation is responsive rather than waiting out the whole build.
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return CompileReport {
                failure: Some("Compile cancelled.".to_string()),
                ..CompileReport::default()
            };
        }
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(120)),
            Err(_) => break None,
        }
    };

    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut out);
    }
    if let Ok(s) = reader.join() {
        out.push_str(&s);
    }

    // A locked project is Unity's most common failure and its message is easy to lose in a long
    // log — surface the actionable version instead.
    if project::is_unity_lock_error(&out) {
        return CompileReport {
            failure: Some(
                "Unity has this project open. Close the Unity editor and compile again."
                    .to_string(),
            ),
            ..CompileReport::default()
        };
    }

    CompileReport {
        diagnostics: sc_win::diagnostics::parse(&out, root),
        exit_code: status.and_then(|s| s.code()),
        failure: None,
    }
}
