//! The run options and the argv they build.
//!
//! Ported verbatim from `sc_win::claudecode` — every item here was already pure, and
//! re-deriving it would be a chance to change behaviour by accident. The argument list is
//! the contract the whole stream parser is written against: a silent change to
//! `--output-format` would turn every line into an unparseable one, which is why `args`
//! is a separate function with its own test rather than being built inline at the spawn.
//!
//! The session-listing half is ported from `sc_win::claudesessions`, which reads the CLI's
//! own JSONL logs under `~/.claude/projects/<slugged-workspace>/`. It moved here because
//! nothing else ever used it.

use std::path::{Path, PathBuf};

/// Which model a run uses. `Default` passes no `--model`, deferring to the CLI's own choice —
/// which is the honest default, since that choice is Claude Code's to make and it changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Model {
    #[default]
    Default,
    Opus,
    Sonnet,
    Haiku,
}

impl Model {
    /// The `--model` alias. `None` for [`Model::Default`] — the flag is omitted entirely.
    pub fn flag(self) -> Option<&'static str> {
        match self {
            Model::Default => None,
            Model::Opus => Some("opus"),
            Model::Sonnet => Some("sonnet"),
            Model::Haiku => Some("haiku"),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Model::Default => "Default",
            Model::Opus => "Opus",
            Model::Sonnet => "Sonnet",
            Model::Haiku => "Haiku",
        }
    }
    /// The cycle order for the menu's selector.
    pub const ALL: [Model; 4] = [Model::Default, Model::Opus, Model::Sonnet, Model::Haiku];

    /// Parse a persisted alias. Unknown ⇒ [`Model::Default`], never a guess — a config naming a
    /// model this build doesn't know should fall back to the CLI's own choice rather than
    /// pinning something arbitrary.
    pub fn from_slug(s: &str) -> Self {
        match s.trim() {
            "opus" => Model::Opus,
            "sonnet" => Model::Sonnet,
            "haiku" => Model::Haiku,
            _ => Model::Default,
        }
    }
}

/// How much Claude Code asks before acting.
///
/// **`BypassPermissions` is deliberately not offered.** It lets an agent take every action
/// without asking, in the user's real project, with no gate anywhere in this app — spec 00's
/// "no unattended *approval*" non-goal is about exactly that judgement, and a one-click path to
/// it in a side menu is not a considered decision. Someone who genuinely wants it can run the
/// CLI directly, where the choice is at least explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Permission {
    /// Claude Code asks as it normally would.
    #[default]
    Default,
    /// File edits are auto-accepted; other tools still ask.
    AcceptEdits,
    /// Plan only — it works out what it would do without doing it.
    Plan,
}

impl Permission {
    pub fn flag(self) -> Option<&'static str> {
        match self {
            Permission::Default => None,
            Permission::AcceptEdits => Some("acceptEdits"),
            Permission::Plan => Some("plan"),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Permission::Default => "Ask as usual",
            Permission::AcceptEdits => "Auto-accept edits",
            Permission::Plan => "Plan only",
        }
    }
    pub const ALL: [Permission; 3] = [
        Permission::Default,
        Permission::AcceptEdits,
        Permission::Plan,
    ];

    /// Parse a persisted mode. **Unknown ⇒ [`Permission::Default`]**, which is the mode that
    /// asks the most. That direction is deliberate: a config carrying `bypassPermissions` —
    /// hand-edited, or written by some future build — must fall back to asking rather than to
    /// the permissive thing it names.
    pub fn from_slug(s: &str) -> Self {
        match s.trim() {
            "acceptEdits" => Permission::AcceptEdits,
            "plan" => Permission::Plan,
            _ => Permission::Default,
        }
    }
}

/// Everything the panel's ⚙ menu can set for a run (spec 22).
///
/// One struct so [`args`] has a single input and the whole flag surface is asserted in one
/// place. Every field's default is "pass no flag", so a fresh install behaves exactly as the
/// CLI would on its own — the options add to Claude Code's behaviour, they never silently
/// replace it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub model: Model,
    pub permission: Permission,
    /// Carry the previous run's context (`--continue`) instead of starting cold.
    pub continue_session: bool,
    /// Resume one SPECIFIC past conversation (`--resume <id>`), chosen from the
    /// picker. Takes precedence over [`Self::continue_session`], which only ever
    /// means "the most recent".
    pub resume_session: Option<String>,
    /// Extra directories the run may touch, beyond the workspace (`--add-dir`).
    pub add_dirs: Vec<String>,
    /// Restrict the run to these tools (`--allowedTools`). Empty ⇒ no restriction.
    pub allowed_tools: Vec<String>,
    /// Forbid these tools (`--disallowedTools`). Empty ⇒ nothing forbidden.
    pub disallowed_tools: Vec<String>,
}

/// Split a tool list on whitespace, but **keep bracketed patterns whole**.
///
/// The CLI's own examples include `Bash(git *)` — a single tool spec containing a space. A
/// naive `split_whitespace` turns that into `Bash(git` and `*)`, neither of which names a tool,
/// so a restriction the user carefully typed silently stops restricting anything.
pub fn split_tools(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
            }
            c => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// The arguments for a run over `task`, with the panel's options applied.
///
/// Split out so the argument list is asserted in a test rather than buried in the spawn: the
/// output format is the contract this whole module is written against, and a silent change to
/// `--output-format` would turn every line into an unparseable one.
pub fn args(task: &str, opts: &Options) -> Vec<String> {
    let mut v = vec![
        "-p".to_string(),
        task.to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        // stream-json refuses to run without --verbose.
        "--verbose".to_string(),
    ];
    if let Some(m) = opts.model.flag() {
        v.push("--model".to_string());
        v.push(m.to_string());
    }
    if let Some(p) = opts.permission.flag() {
        v.push("--permission-mode".to_string());
        v.push(p.to_string());
    }
    // A specific session wins over "the most recent": the picker is an explicit
    // choice and `--continue` is a default, and passing both would be two flags
    // asking for different conversations.
    if let Some(id) = &opts.resume_session {
        v.push("--resume".to_string());
        v.push(id.clone());
    } else if opts.continue_session {
        v.push("--continue".to_string());
    }
    for d in &opts.add_dirs {
        v.push("--add-dir".to_string());
        v.push(d.clone());
    }
    // Space-separated in ONE argument, which is the shape the CLI documents. Passing each tool
    // as its own argv entry works for bare names but breaks a pattern like `Bash(git *)`, whose
    // space would then split it into two tools that mean nothing.
    if !opts.allowed_tools.is_empty() {
        v.push("--allowedTools".to_string());
        v.push(opts.allowed_tools.join(" "));
    }
    if !opts.disallowed_tools.is_empty() {
        v.push("--disallowedTools".to_string());
        v.push(opts.disallowed_tools.join(" "));
    }
    v
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

impl Model {
    /// The persisted spelling. The inverse of `from_slug`, which the host had but did
    /// not need the other direction of — it stored the flag instead.
    pub fn slug(self) -> &'static str {
        match self {
            Model::Default => "default",
            Model::Opus => "opus",
            Model::Sonnet => "sonnet",
            Model::Haiku => "haiku",
        }
    }
}

impl Permission {
    /// The persisted spelling.
    pub fn slug(self) -> &'static str {
        match self {
            Permission::Default => "default",
            Permission::AcceptEdits => "acceptEdits",
            Permission::Plan => "plan",
        }
    }
}

impl Options {
    /// Where the options live: `options.json` beside the plugin's own binary.
    ///
    /// The plugin's own file rather than the host's `config.json`. The host has no
    /// business holding settings that mean nothing to it (spec 25), and a plugin that
    /// wrote into the editor's config would be reaching somewhere it does not belong.
    fn path() -> Option<std::path::PathBuf> {
        let exe = std::env::current_exe().ok()?;
        Some(exe.parent()?.join("options.json"))
    }

    /// Load, or the defaults. A missing or malformed file is the fresh-install case, not
    /// a failure — the same rule the editor's own config follows.
    pub fn load() -> Self {
        let Some(text) = Self::path().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return Self::default();
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Self::default();
        };
        let field = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
        Self {
            model: field("model")
                .map(|s| Model::from_slug(&s))
                .unwrap_or_default(),
            permission: field("permission")
                .map(|s| Permission::from_slug(&s))
                .unwrap_or_default(),
            continue_session: v
                .get("continue_session")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            // Never persisted: a resume points at one specific past conversation, and
            // silently resuming it on a later launch is not what the user asked for.
            resume_session: None,
            add_dirs: strings(v.get("add_dirs")),
            allowed_tools: field("allowed_tools")
                .map(|s| split_tools(&s))
                .unwrap_or_default(),
            disallowed_tools: field("disallowed_tools")
                .map(|s| split_tools(&s))
                .unwrap_or_default(),
        }
    }

    /// Persist. Best-effort: a read-only directory loses a setting rather than breaking
    /// the plugin.
    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        let mut obj = serde_json::Map::new();
        obj.insert("model".into(), self.model.slug().into());
        obj.insert("permission".into(), self.permission.slug().into());
        obj.insert("continue_session".into(), self.continue_session.into());
        obj.insert(
            "add_dirs".into(),
            serde_json::Value::Array(self.add_dirs.iter().map(|d| d.clone().into()).collect()),
        );
        obj.insert("allowed_tools".into(), self.allowed_tools.join(" ").into());
        obj.insert(
            "disallowed_tools".into(),
            self.disallowed_tools.join(" ").into(),
        );
        let _ = std::fs::write(path, serde_json::Value::Object(obj).to_string());
    }

    /// Advance to the next model. Cycling rather than choosing, because the content model
    /// has no dropdown — and for four values a cycle is faster anyway.
    pub fn cycle_model(&mut self) {
        let i = Model::ALL
            .iter()
            .position(|m| *m == self.model)
            .unwrap_or(0);
        self.model = Model::ALL[(i + 1) % Model::ALL.len()];
    }

    /// Advance to the next permission mode.
    pub fn cycle_permission(&mut self) {
        let i = Permission::ALL
            .iter()
            .position(|p| *p == self.permission)
            .unwrap_or(0);
        self.permission = Permission::ALL[(i + 1) % Permission::ALL.len()];
    }

    /// Toggle "continue the last session".
    ///
    /// Turning it on clears a chosen resume: they are two answers to one question, and
    /// leaving both set would have the argv builder silently prefer one.
    pub fn toggle_continue(&mut self) {
        self.continue_session = !self.continue_session;
        if self.continue_session {
            self.resume_session = None;
        }
    }

    /// Apply the options form's `field=value` lines.
    pub fn apply_form(&mut self, encoded: &str) {
        for line in encoded.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k {
                "allowed" => self.allowed_tools = split_tools(v),
                "disallowed" => self.disallowed_tools = split_tools(v),
                _ => {}
            }
        }
    }
}

/// A JSON array of strings, or empty.
fn strings(v: Option<&serde_json::Value>) -> Vec<String> {
    v.and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Past conversations
// ---------------------------------------------------------------------------

/// One resumable conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The CLI's session id — the filename stem, and what `--resume` takes.
    pub id: String,
    /// The first thing the user asked, trimmed to one line. The only label that tells you
    /// which conversation this was.
    pub summary: String,
    /// Unix seconds of last modification, for ordering. Newest first is what a picker
    /// wants; "which did I use last" is the question being asked.
    pub modified: u64,
}

/// map to the same directory every time.
pub fn project_dir(workspace: &Path) -> Option<PathBuf> {
    let home = home_dir()?;
    let projects = home.join(".claude").join("projects");
    let slug: String = workspace
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();

    // Try the slug BOTH WAYS, because the CLI writes whichever case the invoking
    // path had and does not normalise. Both spellings genuinely exist on this
    // machine: `C--Users-mail-working-Personal-SmartCoder-smart-coder` alongside
    // `c--Users-mail-working-Personal-Games-void-claim`. Windows resolves either,
    // but picking one and hoping is how a folder with eight conversations lists
    // none on a case-sensitive filesystem -- and guessing wrong is invisible,
    // because an empty directory and a missing one look identical here.
    let mut lower = slug.clone();
    if let Some(first) = lower.get_mut(0..1) {
        first.make_ascii_lowercase();
    }
    let mut upper = slug;
    if let Some(first) = upper.get_mut(0..1) {
        first.make_ascii_uppercase();
    }

    let as_written = projects.join(&upper);
    if as_written.is_dir() {
        return Some(as_written);
    }
    let alt = projects.join(&lower);
    if alt.is_dir() {
        return Some(alt);
    }
    // Neither exists: return the upper form so the caller reports "no conversations
    // here" against a sensible path rather than failing.
    Some(projects.join(upper))
}

/// The user's home directory, without pulling in a crate for it.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// Every resumable conversation for `workspace`, newest first.
///
/// Empty when the CLI has never run here, which is the honest answer — the picker
/// then says there is nothing to resume rather than showing an error.
pub fn list(workspace: &Path) -> Vec<Session> {
    let Some(dir) = project_dir(workspace) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut out: Vec<Session> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension()?.to_str()? != "jsonl" {
                return None;
            }
            let id = path.file_stem()?.to_str()?.to_string();
            let modified = e
                .metadata()
                .ok()?
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs();
            // A session opened from an IDE context block can genuinely have no plain
            // user prompt in its opening lines. "(no prompt)" is honest but useless
            // in a picker, so fall back to the short id -- which at least tells two
            // unlabelled rows apart, and is what `--resume` takes anyway.
            let summary = first_user_message(&path)
                .unwrap_or_else(|| format!("(session {})", &id[..id.len().min(8)]));
            Some(Session {
                summary,
                id,
                modified,
            })
        })
        .collect();

    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    out
}

/// The first real thing the user typed in a session log.
///
/// Skips the machinery: a session opens with tool results, system reminders and
/// resumed-context blocks, none of which identify the conversation. The first line
/// that is genuinely a person asking something is the only useful label, so lines
/// that start with `<` (the `<system-reminder>` / `<command-name>` wrappers) are
/// passed over.
fn first_user_message(path: &Path) -> Option<String> {
    use std::io::BufRead;

    let file = std::fs::File::open(path).ok()?;
    // Streamed, not read whole: these logs run to tens of megabytes and the answer
    // is almost always in the first few lines.
    for line in std::io::BufReader::new(file).lines().take(400) {
        let Ok(line) = line else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        let content = v.get("message")?.get("content")?;
        let text = match content {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(parts) => parts
                .iter()
                .find(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                .and_then(|p| p.get("text"))
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            _ => continue,
        };
        let text = text.trim();
        // Skip the machinery. A session commonly opens with `<local-command-caveat>`,
        // `<command-name>/clear</command-name>`, or an `<ide_opened_file>` block --
        // and the last of those arrives inside the ARRAY form's text part, so
        // checking the raw string alone missed it and a 22MB conversation showed as
        // "(no prompt)".
        if text.is_empty() || text.starts_with('<') {
            continue;
        }
        // 44 chars, not 72: the menu is 340px wide with padding and an age column
        // beside it, which is about 45 characters at this text size. A longer summary
        // does not wrap -- it runs past the panel edge.
        return Some(one_line(text, 44));
    }
    None
}

/// Collapse to a single line and clip, so a row cannot wrap the menu.
fn one_line(s: &str, max: usize) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// One resumable conversation, as the options panel shows it.
pub struct Listed {
    pub id: String,
    pub summary: String,
    pub age: String,
}

/// Past conversations for `workspace`, newest first.
pub fn sessions(workspace: &std::path::Path) -> Vec<Listed> {
    list(workspace)
        .into_iter()
        .map(|s| Listed {
            age: relative_age(s.modified),
            id: s.id,
            summary: s.summary,
        })
        .collect()
}

/// "3h" / "2d" / "5w" — short enough for a row's detail column.
fn relative_age(modified: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = now.saturating_sub(modified);
    match secs {
        s if s < 3600 => format!("{}m", (s / 60).max(1)),
        s if s < 86_400 => format!("{}h", s / 3600),
        s if s < 604_800 => format!("{}d", s / 86_400),
        s => format!("{}w", s / 604_800),
    }
}
