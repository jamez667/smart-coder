//! Workspace scanning: read every text file once, up front.
//!
//! This crate needs its own walker rather than reusing
//! `sc_index::collect_sources`, which filters to `Language::from_path` and so
//! accepts only `.rs`, `.py` and `.cs`. That filter silently drops `.tf`,
//! `.yml`, `Dockerfile`, `.json`, `.gitignore` and `.github/workflows/*` —
//! which is where most real compliance evidence lives. A CC6.6 check built on
//! `collect_sources` would report `Unknown` on every repo that actually
//! configures TLS.
//!
//! The loop is modelled on `search_code` in `sc-tools`: walk everything
//! readable as UTF-8, skip VCS and build noise. `sc_index` is still used, but
//! only inside the symbol collector where its language filter is the point.
//!
//! Scanning happens ONCE per audit and the result is shared by reference. A
//! 40-control pack over a 5k-file repo would otherwise re-walk the tree ~150
//! times.

use std::path::Path;

use crate::evidence::normalize_path;

/// Directories never worth scanning. Mirrors `sc-tools`' `search_code` list,
/// including `.smart-coder` (the agent's own session logs, which would
/// otherwise supply "evidence" quoted from earlier tool output).
const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".smart-coder",
    "__pycache__",
    ".venv",
    "dist",
    "build",
];

/// Files larger than this are not read. Compliance evidence is configuration
/// and source; a 5 MB vendored bundle or lockfile is noise, and reading it
/// costs more than it can ever contribute.
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// A readable text file in the audited workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextFile {
    /// Workspace-relative, forward-slashed.
    pub path: String,
    pub contents: String,
    /// Whether a `.gitignore` rule matches this path.
    ///
    /// A secret in an ignored file is still a real exposure — it sits on
    /// developer machines and backup drives — but it is *not* "committed to
    /// source", and a report that conflates the two overstates its case. The
    /// finding is kept and labelled rather than suppressed.
    pub ignored: bool,
}

impl TextFile {
    /// 1-based `(line_number, line_text)` pairs.
    pub fn lines(&self) -> impl Iterator<Item = (u32, &str)> {
        self.contents
            .lines()
            .enumerate()
            .map(|(i, l)| (i as u32 + 1, l))
    }
}

/// A best-effort `.gitignore` matcher built from the repository root.
///
/// Deliberately partial: it reads only the root `.gitignore`, and supports the
/// common forms (`name`, `*.ext`, `dir/`, `/anchored`, `**/nested`). It does
/// not implement negation (`!`), nested per-directory ignore files, or
/// `.git/info/exclude`.
///
/// That is acceptable because the result is only ever used to *label* evidence,
/// never to suppress it. A missed ignore rule costs a slightly overstated
/// label; it cannot hide a finding. Were this used for filtering, the partial
/// implementation would be a correctness problem and shelling out to
/// `git check-ignore` would be the right call.
#[derive(Debug, Default)]
pub struct IgnoreRules {
    globs: Vec<crate::glob::Glob>,
}

impl IgnoreRules {
    /// Read and compile `<root>/.gitignore`. A missing or unreadable file
    /// yields an empty rule set.
    pub fn load(root: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(root.join(".gitignore")) else {
            return IgnoreRules::default();
        };
        IgnoreRules::parse(&text)
    }

    /// Compile gitignore text into matchable globs.
    pub fn parse(text: &str) -> Self {
        let mut globs = Vec::new();
        for raw in text.lines() {
            let line = raw.trim();
            // Blank, comment, or negation (unsupported — skipping a negation
            // only ever makes the label more conservative).
            if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
                continue;
            }

            let anchored = line.starts_with('/');
            // A trailing `/` means "directory only". We do not track it
            // separately: every pattern set below already includes the
            // `<core>/**/*` form that covers a directory's contents, and the
            // bare `<core>` form is harmless for a directory entry.
            let core = line.trim_start_matches('/').trim_end_matches('/');
            if core.is_empty() {
                continue;
            }

            // A gitignore entry matches at any depth unless it is anchored or
            // already contains a slash.
            let patterns = if anchored || core.contains('/') {
                vec![core.to_string(), format!("{core}/**/*")]
            } else {
                vec![
                    core.to_string(),
                    format!("**/{core}"),
                    format!("{core}/**/*"),
                    format!("**/{core}/**/*"),
                ]
            };

            for p in patterns {
                if let Ok(g) = crate::glob::Glob::new(&p) {
                    globs.push(g);
                }
            }
        }
        IgnoreRules { globs }
    }

    /// Is this workspace-relative, forward-slashed path ignored?
    pub fn is_ignored(&self, path: &str) -> bool {
        self.globs.iter().any(|g| g.is_match(path))
    }
}

/// Walk `root`, returning every readable UTF-8 file under it.
///
/// Unreadable files, binaries and oversized files are skipped silently — the
/// same philosophy as `collect_sources`, which drops files it cannot read
/// rather than failing the whole index. A compliance run must not abort because
/// one file has odd permissions.
///
/// Gitignored files are **included**, flagged via [`TextFile::ignored`]. A
/// secret sitting untracked on a developer machine is a real exposure; it is
/// simply not the same claim as "committed to source", so it is labelled rather
/// than dropped.
///
/// Results are sorted by path so evidence ordering is deterministic across runs
/// and platforms.
pub fn scan_workspace(root: &Path) -> Vec<TextFile> {
    let ignore = IgnoreRules::load(root);
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();

            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                if SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                stack.push(path);
                continue;
            }

            // Skip oversized files without reading them.
            if let Ok(meta) = entry.metadata() {
                if meta.len() > MAX_FILE_BYTES {
                    continue;
                }
            }

            // read_to_string fails on non-UTF-8, which is our binary filter.
            if let Ok(contents) = std::fs::read_to_string(&path) {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                let rel = normalize_path(&rel);
                let ignored = ignore.is_ignored(&rel);
                out.push(TextFile {
                    path: rel,
                    contents,
                    ignored,
                });
            }
        }
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Does a path exist under `root`? Accepts files *and* directories.
///
/// Directories matter: a pack checking for CI evidence names
/// `.github/workflows`, which is a directory, and treating that as "absent"
/// would report a false gap on every GitHub repo.
pub fn path_exists(root: &Path, rel: &str) -> bool {
    root.join(rel).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Dependency-free temp dir, following the `sc-index` convention: pid plus
    /// nanos keeps parallel `cargo test` runs from colliding.
    fn temp_repo(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("sc-comply-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create temp root");
        root
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(p, body).expect("write");
    }

    #[test]
    fn scans_text_files_and_normalizes_paths() {
        let root = temp_repo("scan-basic");
        write(&root, "src/lib.rs", "fn main() {}\n");
        write(&root, ".gitignore", ".env\n");
        write(&root, ".github/workflows/ci.yml", "on: push\n");

        let files = scan_workspace(&root);
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();

        assert!(paths.contains(&".github/workflows/ci.yml"), "{paths:?}");
        assert!(paths.contains(&".gitignore"), "{paths:?}");
        assert!(paths.contains(&"src/lib.rs"), "{paths:?}");
        // Always forward slashes, even on Windows.
        assert!(paths.iter().all(|p| !p.contains('\\')), "{paths:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scans_non_source_extensions_that_collect_sources_would_drop() {
        // The whole reason this walker exists.
        let root = temp_repo("scan-nonsource");
        write(&root, "main.tf", "resource \"aws_s3_bucket\" \"b\" {}\n");
        write(&root, "Dockerfile", "FROM rust:1\n");
        write(&root, "config.json", "{}\n");

        let paths: Vec<String> = scan_workspace(&root).into_iter().map(|f| f.path).collect();
        assert!(paths.contains(&"main.tf".to_string()), "{paths:?}");
        assert!(paths.contains(&"Dockerfile".to_string()), "{paths:?}");
        assert!(paths.contains(&"config.json".to_string()), "{paths:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skips_noise_directories() {
        let root = temp_repo("scan-skip");
        write(&root, "keep.txt", "yes\n");
        write(&root, ".git/config", "secret\n");
        write(&root, "target/debug/build.rs", "no\n");
        write(&root, "node_modules/pkg/index.js", "no\n");
        write(&root, ".smart-coder/sessions/log.txt", "no\n");

        let paths: Vec<String> = scan_workspace(&root).into_iter().map(|f| f.path).collect();
        assert_eq!(paths, vec!["keep.txt".to_string()], "{paths:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skips_binary_files() {
        let root = temp_repo("scan-binary");
        write(&root, "ok.txt", "text\n");
        std::fs::write(root.join("blob.bin"), [0xff_u8, 0xfe, 0x00, 0x01]).expect("write binary");

        let paths: Vec<String> = scan_workspace(&root).into_iter().map(|f| f.path).collect();
        assert!(paths.contains(&"ok.txt".to_string()));
        assert!(!paths.contains(&"blob.bin".to_string()), "{paths:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skips_oversized_files() {
        let root = temp_repo("scan-large");
        write(&root, "small.txt", "ok\n");
        let big = "x".repeat((MAX_FILE_BYTES + 1024) as usize);
        write(&root, "big.txt", &big);

        let paths: Vec<String> = scan_workspace(&root).into_iter().map(|f| f.path).collect();
        assert!(paths.contains(&"small.txt".to_string()));
        assert!(!paths.contains(&"big.txt".to_string()), "{paths:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn results_are_sorted_for_deterministic_evidence() {
        let root = temp_repo("scan-sorted");
        write(&root, "z.txt", "z\n");
        write(&root, "a.txt", "a\n");
        write(&root, "m/n.txt", "n\n");

        let paths: Vec<String> = scan_workspace(&root).into_iter().map(|f| f.path).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn lines_are_one_based() {
        let f = TextFile {
            path: "a.txt".into(),
            contents: "one\ntwo\nthree\n".into(),
            ignored: false,
        };
        let got: Vec<(u32, &str)> = f.lines().collect();
        assert_eq!(got, vec![(1, "one"), (2, "two"), (3, "three")]);
    }

    #[test]
    fn path_exists_accepts_directories() {
        // `.github/workflows` is a directory and must count as present.
        let root = temp_repo("scan-exists");
        write(&root, ".github/workflows/ci.yml", "on: push\n");

        assert!(path_exists(&root, ".github/workflows"));
        assert!(path_exists(&root, ".github/workflows/ci.yml"));
        assert!(!path_exists(&root, "nope.txt"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn gitignored_files_are_scanned_but_flagged() {
        // Deliberately NOT skipped: an untracked `.env` is a real exposure. But
        // it is not "committed to source", so the flag lets the report make the
        // weaker, accurate claim.
        let root = temp_repo("scan-ignored");
        write(&root, ".gitignore", ".env\n*.key\nlogs/\n");
        write(&root, ".env", "SECRET=1\n");
        write(&root, "tls/server.key", "-----BEGIN EC PRIVATE KEY-----\n");
        write(&root, "logs/run.txt", "noise\n");
        write(&root, "src/lib.rs", "fn main() {}\n");

        let files = scan_workspace(&root);
        let by = |p: &str| {
            files
                .iter()
                .find(|f| f.path == p)
                .unwrap_or_else(|| panic!("missing {p} in {files:?}"))
        };

        assert!(by(".env").ignored, "a gitignored file must be flagged");
        assert!(by("tls/server.key").ignored, "*.key matches at any depth");
        assert!(by("logs/run.txt").ignored, "a dir rule covers its contents");
        assert!(!by("src/lib.rs").ignored, "tracked source is not flagged");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_workspace_without_a_gitignore_flags_nothing() {
        let root = temp_repo("scan-nogitignore");
        write(&root, ".env", "SECRET=1\n");

        let files = scan_workspace(&root);
        assert!(files.iter().all(|f| !f.ignored));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ignore_rules_parse_the_common_forms() {
        let r = IgnoreRules::parse(
            "# a comment\n\
             \n\
             .env\n\
             *.key\n\
             /target\n\
             build/\n\
             docs/private/notes.md\n\
             !keepme\n",
        );
        assert!(r.is_ignored(".env"));
        assert!(
            r.is_ignored("nested/.env"),
            "unanchored rules match at depth"
        );
        assert!(r.is_ignored("a/b/c.key"));
        assert!(r.is_ignored("target"));
        assert!(r.is_ignored("target/debug/x.rs"));
        assert!(r.is_ignored("build/out.js"));
        assert!(r.is_ignored("docs/private/notes.md"));

        assert!(!r.is_ignored("src/lib.rs"));
        assert!(!r.is_ignored("keyfile.txt"));
        // Anchored rules do not match at depth.
        assert!(!r.is_ignored("crates/target"));
    }

    #[test]
    fn ignore_rules_skip_negations_conservatively() {
        // Negation is unsupported; skipping it only ever under-flags, which is
        // safe because the flag never suppresses a finding.
        let r = IgnoreRules::parse("!important.env\n");
        assert!(!r.is_ignored("important.env"));
    }

    #[test]
    fn missing_root_yields_no_files_rather_than_panicking() {
        let root = std::env::temp_dir().join("sc-comply-does-not-exist-xyz");
        assert!(scan_workspace(&root).is_empty());
    }
}
