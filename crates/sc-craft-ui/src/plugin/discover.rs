//! Finding plugins on disk.
//!
//! A plugin is a directory under `<state_dir>/plugins/` containing `plugin.json`:
//!
//! ```text
//! %APPDATA%\smart-coder-crafter\plugins\
//!     git-blame\
//!         plugin.json
//!         git-blame.exe
//! ```
//!
//! `plugin.json` says how to *start* the process; everything the plugin **contributes**
//! comes over the wire in the handshake instead. That division is deliberate: two
//! declarations of the same panel list would drift, and the one on disk would be the
//! stale one. The file on disk answers only the question the host cannot ask the plugin
//! without first running it.

use std::path::{Path, PathBuf};

/// A plugin found on disk, before it has been started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    /// Directory name, and the id used until the handshake confirms it.
    pub dir_name: String,
    /// The directory itself — the plugin's working directory when spawned.
    pub dir: PathBuf,
    /// Program to run, resolved against `dir` when relative.
    pub command: PathBuf,
    /// Arguments.
    pub args: Vec<String>,
    /// Whether the user has disabled it. Disabled plugins are listed but not started,
    /// so the Plugins panel can offer to re-enable one.
    pub enabled: bool,
}

/// Why a directory that looked like a plugin could not be used.
///
/// Carried rather than logged-and-dropped, because these are the failures a user has to
/// be able to see: a plugin that silently does not appear is indistinguishable from one
/// that was never installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub dir_name: String,
    pub reason: String,
}

/// The result of scanning the plugins directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    pub found: Vec<Discovered>,
    pub rejected: Vec<Rejected>,
}

/// The plugins directory for the running product.
///
/// Under the product's own state dir, so the two products do not share plugins — the
/// same split spec 21 already makes for `config.json`, and for a stronger reason here:
/// a plugin installed for the agent build is a process the editor build would otherwise
/// start.
pub fn plugins_dir() -> PathBuf {
    crate::config::state_dir().join("plugins")
}

/// Scan `dir` for plugins.
///
/// Never fails: a missing directory is the normal case on a fresh install and yields an
/// empty scan. Unreadable entries are rejected individually rather than aborting the
/// scan, so one bad plugin cannot hide every other.
pub fn scan(dir: &Path) -> Scan {
    let mut scan = Scan::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return scan;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    // Sorted, so the order plugins load in is stable across machines and runs. It
    // decides which of two plugins claiming one command id wins, and "whichever the
    // filesystem listed first" is not an answer anyone can act on.
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().to_string();
        let manifest_path = path.join("plugin.json");
        if !manifest_path.exists() {
            // Not a plugin directory at all. Silent on purpose: this is how a user's
            // stray folder or a `.git` directory looks, and reporting it as a broken
            // plugin would be noise.
            continue;
        }
        match parse_launch(&std::fs::read_to_string(&manifest_path).unwrap_or_default()) {
            Ok((command, args, enabled)) => scan.found.push(Discovered {
                dir_name,
                dir: path.clone(),
                command: resolve(&path, &command),
                args,
                enabled,
            }),
            Err(reason) => scan.rejected.push(Rejected { dir_name, reason }),
        }
    }
    scan
}

/// Resolve a relative program path against the plugin's own directory.
///
/// A bare name like `git-blame.exe` means "the one shipped beside this manifest", not
/// "whatever is on PATH" — a plugin that silently ran a same-named binary from
/// elsewhere would be a genuinely nasty surprise. A path with separators, or an
/// absolute one, is taken as given so a plugin can legitimately be `python` or a
/// script.
fn resolve(dir: &Path, command: &str) -> PathBuf {
    let p = Path::new(command);
    if p.is_absolute() || command.contains('/') || command.contains('\\') {
        return p.to_path_buf();
    }
    let local = dir.join(command);
    if local.exists() {
        local
    } else {
        p.to_path_buf()
    }
}

/// Turn a plugin on or off by writing `enabled` into its `plugin.json`.
///
/// **Takes effect on the next launch**, because plugins load at startup (spec 25). The
/// caller says so; this only records the intent.
///
/// Rewrites the file rather than regenerating it: a `plugin.json` is hand-written and may
/// carry keys this version does not know, and a plugin manager that silently dropped a
/// future field while toggling a checkbox would be a genuinely nasty surprise. Only the
/// one key changes.
///
/// Returns the reason on failure — a read-only file or a permissions problem is something
/// the user has to be told about, not something to swallow.
pub fn set_enabled(dir: &Path, enabled: bool) -> Result<(), String> {
    let path = dir.join("plugin.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let mut v: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| format!("{} is not valid JSON", path.display()))?;
    let Some(obj) = v.as_object_mut() else {
        return Err(format!("{} is not a JSON object", path.display()));
    };
    obj.insert("enabled".to_string(), serde_json::Value::Bool(enabled));
    // Pretty-printed: this file is hand-edited, and collapsing it to one line while
    // toggling a checkbox would be a hostile thing to do to someone's file.
    let out = serde_json::to_string_pretty(&v)
        .map_err(|e| format!("could not serialize {}: {e}", path.display()))?;
    std::fs::write(&path, out).map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// Parse the launch fields out of `plugin.json`.
///
/// Pure, so the file format is testable without a filesystem — the same reason
/// `claudecode::parse_line` is a free function.
fn parse_launch(text: &str) -> Result<(String, Vec<String>, bool), String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return Err("plugin.json is not valid JSON".to_string());
    };
    let command = v
        .get("command")
        .and_then(|c| c.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "plugin.json has no \"command\"".to_string())?;
    let args = v
        .get("args")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    // Absent means enabled. A plugin someone installed is one they wanted; requiring
    // an explicit `"enabled": true` would make every hand-written manifest wrong once.
    let enabled = v.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true);
    Ok((command.to_string(), args, enabled))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_manifest_needs_only_a_command() {
        let (cmd, args, enabled) = parse_launch(r#"{"command":"blame.exe"}"#).unwrap();
        assert_eq!(cmd, "blame.exe");
        assert!(args.is_empty());
        assert!(enabled, "absent means enabled");
    }

    #[test]
    fn args_and_disabled_are_carried() {
        let (cmd, args, enabled) =
            parse_launch(r#"{"command":"python","args":["-u","main.py"],"enabled":false}"#)
                .unwrap();
        assert_eq!(cmd, "python");
        assert_eq!(args, vec!["-u", "main.py"]);
        assert!(!enabled);
    }

    /// A manifest that cannot be used says *why*, because the message is what the user
    /// sees in the Plugins panel and "plugin failed to load" helps nobody.
    #[test]
    fn a_broken_manifest_explains_itself() {
        assert!(parse_launch("not json").unwrap_err().contains("valid JSON"));
        assert!(parse_launch("{}").unwrap_err().contains("command"));
        assert!(parse_launch(r#"{"command":"  "}"#)
            .unwrap_err()
            .contains("command"));
    }

    /// A bare program name means the binary shipped beside the manifest, so a plugin
    /// cannot be hijacked by a same-named binary earlier on PATH.
    #[test]
    fn a_bare_name_resolves_beside_the_manifest() {
        let dir = std::env::temp_dir().join(format!("sc-plug-res-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("blame.exe"), b"x").unwrap();

        assert_eq!(resolve(&dir, "blame.exe"), dir.join("blame.exe"));
        // Absolute and path-bearing commands are taken as written: `python` on PATH is
        // a legitimate plugin host.
        assert_eq!(
            resolve(&dir, "/usr/bin/python3"),
            Path::new("/usr/bin/python3")
        );
        assert_eq!(resolve(&dir, "missing.exe"), Path::new("missing.exe"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory with no `plugin.json` is not a plugin and not an error — that is
    /// what a stray folder looks like.
    #[test]
    fn a_directory_without_a_manifest_is_ignored_silently() {
        let root = std::env::temp_dir().join(format!("sc-plug-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("not-a-plugin")).unwrap();
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::write(root.join("real/plugin.json"), r#"{"command":"x"}"#).unwrap();
        std::fs::create_dir_all(root.join("broken")).unwrap();
        std::fs::write(root.join("broken/plugin.json"), "{").unwrap();

        let scan = scan(&root);
        assert_eq!(scan.found.len(), 1, "only the real one");
        assert_eq!(scan.found[0].dir_name, "real");
        assert_eq!(scan.rejected.len(), 1, "the broken one is reported");
        assert_eq!(scan.rejected[0].dir_name, "broken");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Toggling writes only the one key, and a rescan sees it.
    #[test]
    fn set_enabled_round_trips_through_the_file() {
        let dir = std::env::temp_dir().join(format!(
            "sc-plug-toggle-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.json"), r#"{"command":"x.exe"}"#).unwrap();

        set_enabled(&dir, false).expect("writes");
        let (_, _, enabled) =
            parse_launch(&std::fs::read_to_string(dir.join("plugin.json")).unwrap()).unwrap();
        assert!(!enabled);

        set_enabled(&dir, true).expect("writes");
        let (_, _, enabled) =
            parse_launch(&std::fs::read_to_string(dir.join("plugin.json")).unwrap()).unwrap();
        assert!(enabled);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Keys this version does not know must survive a toggle.** A plugin manager that
    /// dropped a future field while flipping a checkbox would silently break the plugin
    /// it was managing.
    #[test]
    fn toggling_preserves_unknown_keys() {
        let dir = std::env::temp_dir().join(format!(
            "sc-plug-keep-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            r#"{"command":"x.exe","args":["-u"],"future_field":{"nested":42}}"#,
        )
        .unwrap();

        set_enabled(&dir, false).expect("writes");

        let back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("plugin.json")).unwrap())
                .unwrap();
        assert_eq!(back["future_field"]["nested"], 42, "unknown key survived");
        assert_eq!(back["args"][0], "-u", "known keys survived too");
        assert_eq!(back["enabled"], false);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failure says which file and why — the message goes in front of the user.
    #[test]
    fn a_failed_toggle_explains_itself() {
        let missing = std::env::temp_dir().join("sc-plug-nope-does-not-exist");
        let err = set_enabled(&missing, false).unwrap_err();
        assert!(err.contains("plugin.json"), "{err}");
    }

    /// A missing plugins directory is the fresh-install case, not a failure.
    #[test]
    fn a_missing_directory_scans_empty() {
        let scan = scan(Path::new("C:/nope/does/not/exist"));
        assert_eq!(scan, Scan::default());
    }

    /// Load order is sorted, because it decides which of two plugins claiming one
    /// command id wins — and filesystem order is not an answer a user can act on.
    #[test]
    fn plugins_load_in_a_stable_order() {
        let root = std::env::temp_dir().join(format!("sc-plug-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for name in ["zebra", "alpha", "middle"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(root.join(name).join("plugin.json"), r#"{"command":"x"}"#).unwrap();
        }
        let names: Vec<String> = scan(&root).found.into_iter().map(|p| p.dir_name).collect();
        assert_eq!(names, vec!["alpha", "middle", "zebra"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
