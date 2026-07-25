//! Shared test fixtures. Compiled only under `cfg(test)`.
//!
//! Lint tests are almost all "here is a pack with one check; does exactly the
//! expected lint fire?", so the fixtures are string builders around a minimal
//! valid pack rather than struct literals — that way they exercise the real
//! `Pack::from_toml_str` path including validation.

use sc_comply::scan::TextFile;

use crate::sample::Sample;

/// Wrap one or more `[[controls.checks]]` bodies in a minimal valid pack.
pub fn pack_of(checks_toml: &str) -> String {
    format!(
        r#"
[framework]
id = "test"
name = "Test Framework"
version = "1.0.0"
authority = "None"
scope_note = "a test pack"

[[controls]]
id = "T1"
title = "A test control"
intent = "Something technical that source inspection can settle."
severity = "medium"
{checks_toml}
"#
    )
}

/// A single `[[controls.checks]]` entry from its id and body.
pub fn check_with(id: &str, body: &str) -> String {
    format!(
        r#"
  [[controls.checks]]
  id = "{id}"
  {body}
"#
    )
}

/// A sample workspace containing the given paths, all with empty contents.
pub fn sample_with(paths: &[&str]) -> Sample {
    sample_with_contents(&paths.iter().map(|p| (*p, "")).collect::<Vec<_>>())
}

/// A sample workspace with explicit file contents.
pub fn sample_with_contents(files: &[(&str, &str)]) -> Sample {
    Sample::from_files(
        "/sample",
        files
            .iter()
            .map(|(p, c)| TextFile {
                path: p.to_string(),
                contents: c.to_string(),
                ignored: false,
            })
            .collect(),
    )
}
