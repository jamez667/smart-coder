//! Stamp the commit the binary was BUILT from into the binary.
//!
//! Measurement integrity: `git rev-parse` at RUN time records whatever HEAD
//! happens to be when the binary starts, not the code inside it. Build at 08:26,
//! commit a fix at 08:56, run the old binary, and every row claims a commit whose
//! code it does not contain -- which is exactly how a conclusion ("with every fix
//! applied the model still fails") got recorded against evidence that did not
//! support it.
//!
//! So the stamp is taken here, at compile time, and read back by
//! [`sc_eval::results::current_commit`]. A build from a dirty tree gets a `-dirty`
//! suffix, because an uncommitted tree is no more reproducible than a stale binary.
//!
//! # Why this rebuilds every time
//!
//! Cargo caches a build script's output unless told otherwise, and a cached stamp
//! is the very bug this exists to prevent. `cargo:rerun-if-changed=.git/HEAD` plus
//! the ref it points at covers the common cases, but not all of them (a worktree's
//! `.git` is a file, a packed ref has no loose file to watch, and above all
//! `-dirty` flips on an edit to any tracked file anywhere, which moves neither).
//! Rather than silently cache a wrong value, this also depends on a file it
//! rewrites every run, so the stamp is recomputed on every build. The script runs
//! two short `git` commands; correctness beats the milliseconds.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let repo_root = repo_root();
    let stamp = build_stamp(&repo_root);

    println!("cargo:rustc-env=SC_EVAL_BUILD_COMMIT={stamp}");

    // Watch the obvious inputs, so a plain `git commit` re-stamps for the usual
    // reason and the dependency is legible to anyone reading the build plan...
    if let Some(root) = repo_root.as_deref() {
        let head = root.join(".git").join("HEAD");
        if head.is_file() {
            println!("cargo:rerun-if-changed={}", head.display());
            if let Some(ref_path) = head_ref_path(root, &head) {
                println!("cargo:rerun-if-changed={}", ref_path.display());
            }
        }
    }
    // ...and then defeat the cache anyway, because those inputs do not cover
    // `-dirty`: editing a tracked file changes neither `.git/HEAD` nor the ref, so
    // a clean-stamped binary would keep claiming it was clean.
    //
    // A `rerun-if-changed` on a path that does not exist is treated as "unchanged",
    // not "always rerun" -- measured: it cached happily. The idiom that works is a
    // file whose mtime really does move every build, so write one and depend on it.
    let beacon = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("build-stamp");
    std::fs::write(&beacon, &stamp).expect("write the rerun beacon");
    println!("cargo:rerun-if-changed={}", beacon.display());
}

/// `<short hash>` , or `<short hash>-dirty` when the tree has uncommitted changes,
/// or `"unknown"` outside a checkout / without git.
fn build_stamp(repo_root: &Option<PathBuf>) -> String {
    let Some(hash) = git(repo_root, &["rev-parse", "--short", "HEAD"]) else {
        return "unknown".to_string();
    };
    // A failed `status` is not evidence of a clean tree, but it is also not
    // evidence of a dirty one; leave the hash unqualified rather than lie either way.
    let dirty = git(repo_root, &["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    if dirty {
        format!("{hash}-dirty")
    } else {
        hash
    }
}

/// Run `git` in the repo root (falling back to the manifest dir) and return the
/// trimmed stdout of a successful, non-empty run.
fn git(repo_root: &Option<PathBuf>, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(root) = repo_root.as_deref() {
        cmd.current_dir(root);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// The top level of the checkout containing this crate, if any.
fn repo_root() -> Option<PathBuf> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&manifest)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

/// The loose ref file `.git/HEAD` points at, when it is a symbolic ref and the
/// ref is not packed.
fn head_ref_path(root: &Path, head: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(head).ok()?;
    let target = text.trim().strip_prefix("ref:")?.trim();
    let path = root.join(".git").join(target);
    path.is_file().then_some(path)
}
