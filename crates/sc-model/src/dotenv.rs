//! A tiny dependency-free `.env` loader.
//!
//! Both front-ends (`sc-win`, `sc-cli`) call [`load_dotenv`] once at startup so a secret kept in a
//! root `.env` — e.g. `GEMINI_API_KEY=...` for the Gemini planner — is visible to the env layer of
//! config loading without pulling in a `dotenv` crate. It lives here in the model gateway because
//! this is the crate both front-ends already share and that ultimately consumes the key.

use std::path::PathBuf;

/// Load a `.env` file into the process environment (best-effort). Call ONCE at startup, before any
/// config is read.
///
/// Rules, chosen to be safe and predictable:
/// * A variable already present in the real environment is **never** overwritten — an explicitly
///   exported value always wins over the file (the same precedence as env-over-config elsewhere).
/// * Lines are `KEY=VALUE`; blank lines and `#` comments are skipped; an optional `export ` prefix
///   is tolerated; one layer of matching quotes around the value is stripped. Malformed lines are
///   ignored.
/// * The file is searched for next to the executable and by walking up from the current directory,
///   so it is found whether the app runs from the repo root or a stamped temp workspace. A missing
///   file is a no-op.
///
/// Values are never logged (a `.env` holds secrets).
pub fn load_dotenv() {
    let Some(path) = dotenv_path() else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    for (key, val) in parse_dotenv(&text) {
        // Real env wins: only fill in what isn't already set.
        if std::env::var_os(&key).is_none() {
            std::env::set_var(&key, &val);
        }
    }
}

/// Locate a `.env` file: first next to the running executable, then by walking up from the current
/// directory to the filesystem root. `None` if none is found (or paths can't be resolved).
fn dotenv_path() -> Option<PathBuf> {
    // Next to the exe (covers `cargo run` from target/ and a shipped binary).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(".env");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    // Walk up from cwd (covers running from the repo root or any subdir).
    let mut dir = std::env::current_dir().ok();
    while let Some(d) = dir {
        let candidate = d.join(".env");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent().map(|p| p.to_path_buf());
    }
    None
}

/// Parse `.env` text into `(key, value)` pairs. Pure/host-testable — the file read and env
/// mutation live in [`load_dotenv`]. Skips blanks and `#` comments; tolerates an optional `export`
/// prefix; strips one layer of matching single/double quotes off the value.
fn parse_dotenv(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Allow a leading `export ` (common in shell-sourced .env files).
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((key, val)) = line.split_once('=') else {
            continue; // no '=' ⇒ not a KEY=VALUE line
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        // Strip one layer of matching quotes; an unquoted value keeps its inner `#` as data
        // (we don't attempt inline-comment stripping, which is ambiguous for secrets/URLs).
        let mut val = val.trim();
        let quoted = (val.starts_with('"') && val.ends_with('"') && val.len() >= 2)
            || (val.starts_with('\'') && val.ends_with('\'') && val.len() >= 2);
        if quoted {
            val = &val[1..val.len() - 1];
        }
        out.push((key.to_string(), val.to_string()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::parse_dotenv;

    #[test]
    fn parses_simple_pairs_and_skips_comments_and_blanks() {
        let text = "\
# a comment
GEMINI_API_KEY=abc123

GEMINI_PROJECT_ID=my-project
";
        let pairs = parse_dotenv(text);
        assert_eq!(
            pairs,
            vec![
                ("GEMINI_API_KEY".to_string(), "abc123".to_string()),
                ("GEMINI_PROJECT_ID".to_string(), "my-project".to_string()),
            ]
        );
    }

    #[test]
    fn tolerates_export_prefix_and_surrounding_whitespace() {
        let pairs = parse_dotenv("  export  KEY = value  \n");
        assert_eq!(pairs, vec![("KEY".to_string(), "value".to_string())]);
    }

    #[test]
    fn strips_one_layer_of_matching_quotes() {
        // Quotes are stripped; a `#` inside quotes is preserved (it's part of the secret).
        let pairs = parse_dotenv("A=\"q#uoted\"\nB='single'\nC=bare\n");
        assert_eq!(
            pairs,
            vec![
                ("A".to_string(), "q#uoted".to_string()),
                ("B".to_string(), "single".to_string()),
                ("C".to_string(), "bare".to_string()),
            ]
        );
    }

    #[test]
    fn ignores_lines_without_an_equals_or_key() {
        let pairs = parse_dotenv("not a pair\n=novalue\nGOOD=1\n");
        assert_eq!(pairs, vec![("GOOD".to_string(), "1".to_string())]);
    }
}
