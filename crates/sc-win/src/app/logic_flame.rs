//! The profiler's behaviour: loading a profile, recording one, and moving around it.
//!
//! Its own file rather than more lines in `update.rs`, which is already at this crate's size
//! ceiling. The pure parts — parsing, layout, search — live in [`sc_win::flame`]; what is here
//! is the part that touches the disk, spawns a process, or mutates app state.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use iced::Task;

use super::{App, Message};
use sc_win::flame::tool;

/// How large a folded file we are willing to read.
///
/// A folded file is text, and a long profile can be tens of megabytes; but a *gigabyte* here
/// means the user picked the wrong file, and reading it would wedge the app for minutes. 256 MB
/// is far above any real profile and far below anything that hurts.
const MAX_FOLDED: u64 = 256 * 1024 * 1024;

impl App {
    /// Probe for a sampling profiler. Called once at startup.
    ///
    /// Also called from the panel's "Check again" button. The boot probe alone was a bug: a
    /// user who reads "install one of these", installs it, and comes back to a panel still
    /// saying nothing is there has been told a lie the app could have checked. Re-probing is
    /// two process spawns on an explicit click, which is nothing.
    pub(crate) fn probe_flame_tool(&mut self) {
        self.flame_tool = tool::detect();
    }

    /// Re-probe on demand, and say what changed.
    ///
    /// Sets `flame_error` either way: silently doing nothing on a click that found nothing is
    /// indistinguishable from a broken button.
    pub(crate) fn recheck_flame_tool(&mut self) {
        self.probe_flame_tool();
        self.flame_error = match self.flame_tool {
            Some(t) => Some(format!("Found {}. Recording is available.", t.label())),
            None => Some(
                "Still no sampling profiler on PATH. If you just installed one, its                  directory (usually ~/.cargo/bin) may not be on this app's PATH —                  restarting the app picks up a changed environment."
                    .to_string(),
            ),
        };
    }

    /// Ask for a folded-stack file and load it.
    ///
    /// The picker is blocking, which is fine on a button press, but *parsing* is not: a large
    /// profile is real work, so it goes to a blocking task and comes back as a message.
    pub(crate) fn open_profile(&mut self) -> Task<Message> {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Open a folded-stack profile")
            // `folded` and `collapsed` are the conventional extensions; `txt` because
            // `perf script | stackcollapse-perf.pl > out.txt` is what people actually type.
            .add_filter("Folded stacks", &["folded", "collapsed", "txt"])
            .add_filter("All files", &["*"])
            .pick_file()
        else {
            return Task::none();
        };
        self.load_profile_from(path)
    }

    /// Read and parse a folded file in the background.
    pub(crate) fn load_profile_from(&mut self, path: std::path::PathBuf) -> Task<Message> {
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        self.flame_error = None;

        Task::perform(
            async move {
                let r = tokio::task::spawn_blocking(move || read_folded(&path))
                    .await
                    .unwrap_or_else(|e| Err(format!("the reader thread panicked: {e}")));
                r.map(Box::new)
            },
            move |r| Message::ProfileLoaded(label.clone(), r),
        )
    }

    /// A profile finished loading.
    pub(crate) fn profile_loaded(
        &mut self,
        source: String,
        result: Result<Box<sc_win::flame::Profile>, String>,
    ) {
        match result {
            Ok(p) => {
                // A file that parsed to nothing is a failure with a useful message, not an
                // empty graph — almost always the wrong file, and saying so beats a blank panel.
                if p.is_empty() {
                    self.flame_error = Some(format!(
                        "{source} has no readable stacks. Expected lines like \
                         `main;work 42` — {} unreadable.",
                        p.skipped
                    ));
                    self.flame_profile = None;
                    return;
                }
                self.flame_profile = Some(*p);
                self.flame_source = source;
                // A new profile invalidates any view state derived from the old one. Leaving a
                // stale zoom would silently show a different subtree than the one named.
                self.flame_zoom.clear();
                self.flame_hover = None;
                self.flame_error = None;
            }
            Err(e) => {
                self.flame_error = Some(e);
                self.flame_profile = None;
            }
        }
    }

    /// Record a fresh profile with the detected tool.
    pub(crate) fn record_profile(&mut self) -> Task<Message> {
        if self.flame_running {
            return Task::none(); // never stack two recordings
        }
        let root = self.workspace_root();
        // Resolve the command BEFORE showing a spinner, exactly as `start_compile` does: "no
        // profiler installed" is an answer, and making the user wait for it would be theatre.
        let cmd = match tool::profile_command(
            &root,
            self.project_kind,
            self.flame_tool,
            &self.flame_target,
            &self.flame_args,
        ) {
            Ok(c) => c,
            Err(why) => {
                self.flame_error = Some(why.reason());
                return Task::none();
            }
        };

        let cancel = Arc::new(AtomicBool::new(false));
        self.flame_cancel = Some(cancel.clone());
        self.flame_running = true;
        self.flame_error = None;
        let folded = tool::folded_path(&root);

        Task::perform(
            async move {
                // Blocking: this builds AND runs the program under a sampler. Never on the
                // UI thread.
                tokio::task::spawn_blocking(move || run_record(&cmd, &root, &folded, &cancel))
                    .await
                    .unwrap_or_else(|e| Err(format!("the profiler thread panicked: {e}")))
            },
            Message::ProfileRecorded,
        )
    }

    /// A recording run finished.
    pub(crate) fn profile_recorded(&mut self, result: Result<String, String>) -> Task<Message> {
        self.flame_running = false;
        self.flame_cancel = None;
        match result {
            Ok(text) => {
                let p = sc_win::flame::parse_folded(&text);
                self.profile_loaded("recorded".to_string(), Ok(Box::new(p)));
            }
            Err(e) => self.flame_error = Some(e),
        }
        Task::none()
    }

    /// Zoom to a frame. An empty path zooms all the way out.
    pub(crate) fn flame_zoom_to(&mut self, path: Vec<String>) {
        // Hover is cleared because the frame under the cursor almost certainly moved: the same
        // pixel now sits over something else, and a stale detail line would misreport it.
        self.flame_hover = None;
        self.flame_zoom = path;
    }
}

/// Read a folded file, refusing an implausible one.
fn read_folded(path: &std::path::Path) -> Result<sc_win::flame::Profile, String> {
    let meta =
        std::fs::metadata(path).map_err(|e| format!("Could not read {}: {e}", path.display()))?;
    if meta.len() > MAX_FOLDED {
        return Err(format!(
            "{} is {:.1} MB. That is far larger than any folded profile — is it the right file?",
            path.display(),
            meta.len() as f64 / 1e6
        ));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("Could not read {}: {e}", path.display()))?;
    Ok(sc_win::flame::parse_folded(&text))
}

/// Run the profiler and return the folded stacks it produced.
///
/// Mirrors `run_compile`: a helper thread drains stdout so a full pipe cannot deadlock the
/// child, and exit is polled so cancelling is responsive.
fn run_record(
    cmd: &sc_win::project::CompileCommand,
    root: &std::path::Path,
    folded: &std::path::Path,
    cancel: &Arc<AtomicBool>,
) -> Result<String, String> {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = sc_win::proc::command(&cmd.program)
        .args(&cmd.args)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        // "Couldn't launch the profiler" and "your program crashed" have entirely different
        // fixes, so they get different messages.
        .map_err(|e| format!("Could not run `{}`: {e}", cmd.display()))?;

    let stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_string(&mut buf);
        }
        buf
    });

    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            // The TREE, not the child. A recording is `samply record -- cargo run -- prog`:
            // killing the sampler alone would leave `cargo` and the profiled binary running
            // with nobody waiting on them.
            sc_win::proc::kill_tree(child.id());
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err("Recording cancelled.".to_string());
        }
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(120)),
            Err(_) => break None,
        }
    };

    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    let out = reader.join().unwrap_or_default();

    // `--print-folded` puts the stacks on stdout; samply writes the file. Prefer the file when
    // it exists, since stdout may also carry the tool's own chatter.
    let from_file = std::fs::read_to_string(folded)
        .ok()
        .filter(|s| !s.trim().is_empty());
    if let Some(text) = from_file {
        return Ok(text);
    }
    if looks_folded(&out) {
        return Ok(out);
    }

    // Nothing usable. Report the tool's own words — on Windows this is where "requires
    // Administrator" surfaces, and paraphrasing it would hide the fix.
    let code = status.and_then(|s| s.code());
    let detail = if err.trim().is_empty() {
        out.trim().to_string()
    } else {
        err.trim().to_string()
    };
    Err(format!(
        "`{}` produced no folded stacks{}.\n{}",
        cmd.display(),
        match code {
            Some(c) if c != 0 => format!(" (exit {c})"),
            _ => String::new(),
        },
        // Long tool output is truncated: the panel shows a sentence, not a build log.
        detail.chars().take(600).collect::<String>()
    ))
}

/// Whether text plausibly holds folded stacks.
///
/// Used to tell a tool's stdout apart from its progress chatter. Deliberately weak — one line
/// ending in a number after a semicolon-separated path is enough — because the parser itself
/// tolerates junk, and a stricter check would reject real output over a stray banner.
fn looks_folded(s: &str) -> bool {
    s.lines()
        .filter(|l| !l.trim().is_empty())
        .take(200)
        .any(|l| {
            l.rsplit_once(char::is_whitespace)
                .map(|(stack, n)| stack.contains(';') && n.trim().parse::<u64>().is_ok())
                .unwrap_or(false)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folded_output_is_recognised_and_chatter_is_not() {
        assert!(looks_folded("main;a;b 42"));
        assert!(looks_folded("Compiling foo v0.1.0\nmain;a 7\n"));
        // A build log alone must not be mistaken for a profile.
        assert!(!looks_folded("Compiling foo v0.1.0\nFinished in 3.2s\n"));
        assert!(!looks_folded(""));
        // A number with no call path is not a stack.
        assert!(!looks_folded("total 42"));
    }

    #[test]
    fn an_oversized_file_is_refused_by_name() {
        // The guard is on metadata, so a missing file reports the read error instead — both
        // paths must produce a sentence naming the file rather than a bare io error.
        let e = read_folded(std::path::Path::new("does-not-exist.folded")).unwrap_err();
        assert!(e.contains("does-not-exist.folded"), "{e}");
    }

    #[test]
    fn a_real_folded_file_round_trips_from_disk() {
        let dir = std::env::temp_dir().join("sc-flame-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.folded");
        std::fs::write(&p, "main;a 10\nmain;b 5\n").unwrap();
        let prof = read_folded(&p).unwrap();
        assert_eq!(prof.total(), 15);
        let _ = std::fs::remove_file(&p);
    }
}
