//! The test-writing pass (spec 09, amended): turn the Phase-4 coverage plan into
//! actual test files, written by cheap **worker** models — one worker per test
//! file (file-granularity, spec 08).
//!
//! The worker reasons (writes the test code as text); we write it to disk. This is
//! the same labour split as the implementation swarm: the orchestrator planned
//! *what* to cover, the worker writes the *how*. The resulting files become the
//! frozen contract the implementation workers must satisfy.

use std::path::Path;

use dc_model::{GenerateRequest, Message, ModelBackend};

use crate::coverage::{group_by_file, CoverageItem};

/// A test file produced by a worker, ready to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenTest {
    pub file: String,
    pub content: String,
}

const TEST_WRITER_SYSTEM: &str = "You write a runnable unit-test file. You are given the test \
file name and the behaviors it must cover. Reply with ONLY the file body — no fences, no prose, \
and do NOT repeat the filename. The very first line MUST be the import of the functions under \
test from their module (the module is the test filename without the leading 'test_' and the \
'.py'; e.g. test_mathlib.py → `from mathlib import ...`). Then one test function per behavior. \
The implementation does not exist yet, so the tests must FAIL until it is written correctly.";

/// Ask `worker` to write each test file named by the coverage plan. One model call
/// per file (the cheapest path); the file's coverage items are the worker's brief.
/// Returns the written files (those the worker produced non-empty content for).
pub fn write_tests(worker: &dyn ModelBackend, coverage: &[CoverageItem]) -> Vec<WrittenTest> {
    let mut out = Vec::new();
    for (file, covers) in group_by_file(coverage) {
        let prompt = test_prompt(&file, &covers);
        let req = GenerateRequest::new(vec![
            Message::system(TEST_WRITER_SYSTEM),
            Message::user(prompt),
        ]);
        if let Ok(resp) = worker.generate(&req) {
            let content = clean_test(&file, &resp.content);
            if !content.trim().is_empty() {
                out.push(WrittenTest { file, content });
            }
        }
    }
    out
}

/// Write the produced test files to `workspace`. Returns the relative paths
/// written. (The orchestrator already reviewed the coverage plan; the test files
/// are the worker's output of that plan.)
pub fn persist_tests(workspace: &Path, tests: &[WrittenTest]) -> std::io::Result<Vec<String>> {
    let mut written = Vec::new();
    for t in tests {
        let p = workspace.join(&t.file);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Normalize to a single trailing newline.
        let body = format!("{}\n", t.content.trim_end());
        std::fs::write(&p, body)?;
        written.push(t.file.clone());
    }
    Ok(written)
}

fn test_prompt(file: &str, covers: &[String]) -> String {
    let mut s = format!("Test file: {file}\n\nCover these behaviors, one test each:\n");
    for c in covers {
        s.push_str(&format!("- {c}\n"));
    }
    s.push_str("\nReply with the complete test file.");
    s
}

/// Clean a worker's test output: strip a surrounding ``` fence and a leaked
/// leading line that is just the filename (a small model often echoes the prompt's
/// "Test file: <name>" as the first line of the body).
fn clean_test(file: &str, s: &str) -> String {
    let body = strip_fence(s);
    let mut lines: Vec<&str> = body.lines().collect();
    while let Some(first) = lines.first() {
        let f = first.trim();
        if f.is_empty()
            || f == file
            || f == format!("# {file}")
            || f == format!("Test file: {file}")
        {
            lines.remove(0);
        } else {
            break;
        }
    }
    lines.join("\n").trim_end().to_string()
}

/// Strip a surrounding ``` fence (optional language tag) a model may add.
fn strip_fence(s: &str) -> String {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t.to_string();
    };
    let rest = rest.split_once('\n').map(|(_, r)| r).unwrap_or("");
    rest.trim_end()
        .strip_suffix("```")
        .unwrap_or(rest)
        .trim_end()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dc_model::MockBackend;

    fn temp(tag: &str) -> std::path::PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("dc-wf-tw-{tag}-{n}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn writes_one_file_per_coverage_group() {
        // Two coverage items for one file → one worker call → one written test.
        let backend = MockBackend::new(["```python\ndef test_x():\n    assert f() == 1\n```"]);
        let coverage = vec![
            CoverageItem {
                file: "test_f.py".into(),
                covers: "returns 1".into(),
            },
            CoverageItem {
                file: "test_f.py".into(),
                covers: "handles zero".into(),
            },
        ];
        let written = write_tests(&backend, &coverage);
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].file, "test_f.py");
        assert!(written[0].content.contains("def test_x"));
        // The fence was stripped.
        assert!(!written[0].content.contains("```"));
    }

    #[test]
    fn clean_test_strips_fence_and_leaked_filename() {
        // A small model echoes "test_m.py" as the first body line and wraps in a
        // fence — both are stripped, the real code kept.
        let raw = "```python\ntest_m.py\nfrom m import f\ndef test_f():\n    assert f()\n```";
        let cleaned = clean_test("test_m.py", raw);
        assert!(cleaned.starts_with("from m import f"), "got: {cleaned}");
        assert!(!cleaned.contains("```"));
        assert!(!cleaned.lines().next().unwrap().contains("test_m.py"));
    }

    #[test]
    fn persist_writes_files_with_trailing_newline() {
        let ws = temp("persist");
        let tests = vec![WrittenTest {
            file: "test_a.py".into(),
            content: "def test_a():\n    assert True".into(),
        }];
        let written = persist_tests(&ws, &tests).unwrap();
        assert_eq!(written, vec!["test_a.py"]);
        let body = std::fs::read_to_string(ws.join("test_a.py")).unwrap();
        assert!(body.ends_with("\n"));
        assert!(body.contains("assert True"));
        let _ = std::fs::remove_dir_all(&ws);
    }
}
