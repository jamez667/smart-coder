//! Reading the specs.
//!
//! A flat directory of Markdown, read in sorted order so a report is
//! deterministic. This is deliberately *not* `sc_comply::scan_workspace`: that
//! walks the whole repo recursively with gitignore labelling and a size cap, and
//! depending on `sc-comply` for it would pull in packs, TOML and the entire
//! report tree to read one directory. `sc-comply` set exactly this precedent,
//! writing its own walker rather than reusing `sc_index::collect_sources`.

use std::path::Path;

/// One spec document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecDoc {
    /// Workspace-relative, `/`-separated: `docs/specs/17-spec-traceability.md`.
    pub path: String,
    pub contents: String,
}

/// Where specs live, relative to the workspace root.
pub const SPEC_DIR: &str = "docs/specs";

/// Read every `.md` under `docs/specs/`, sorted by path.
///
/// A file that cannot be read is skipped rather than failing the run — one
/// unreadable document must not blind the checker to every other.
pub fn read_specs(root: &Path) -> Vec<SpecDoc> {
    let dir = root.join(SPEC_DIR);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<SpecDoc> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("md"))
        })
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let contents = std::fs::read_to_string(e.path()).ok()?;
            Some(SpecDoc {
                path: format!("{SPEC_DIR}/{name}"),
                contents,
            })
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{repo_root, temp_repo, write};

    #[test]
    fn reads_markdown_in_sorted_order_and_skips_everything_else() {
        let root = temp_repo("scan");
        write(&root, "docs/specs/02-b.md", "b\n");
        write(&root, "docs/specs/01-a.md", "a\n");
        write(&root, "docs/specs/notes.txt", "not a spec\n");
        write(&root, "docs/other.md", "not in specs/\n");

        let specs = read_specs(&root);
        let paths: Vec<&str> = specs.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(paths, vec!["docs/specs/01-a.md", "docs/specs/02-b.md"]);
        assert_eq!(specs[0].contents, "a\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_spec_directory_is_empty_not_a_panic() {
        let root = temp_repo("scan-none");
        assert!(read_specs(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reads_this_repos_real_specs() {
        let specs = read_specs(&repo_root());
        assert!(specs.len() >= 18, "found {} specs", specs.len());
        assert!(specs
            .iter()
            .any(|s| s.path == "docs/specs/17-spec-traceability.md"));
    }
}
