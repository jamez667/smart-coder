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
no explanation, and do NOT repeat the filename. The very first line MUST be the import of the \
functions under test from their module (the module is the test filename without the leading \
'test_' and the '.py'; e.g. test_mathlib.py → `from mathlib import ...`). Then one test function \
per behavior. The implementation does not exist yet, so the tests must FAIL until it is written \
correctly. /no_think";

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
        // Retry a few times: a tiny thinking model can leak reasoning (rejected by
        // clean_test) or blip. Take the first reply that cleans to real code.
        for _ in 0..3 {
            let Ok(resp) = worker.generate(&req) else {
                continue;
            };
            let content = clean_test(&file, &resp.content);
            if !content.trim().is_empty() {
                out.push(WrittenTest {
                    file: file.clone(),
                    content,
                });
                break;
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

/// Clean a worker's test output into a runnable file, or return empty if it isn't
/// one. Strips a ``` fence and any leaked preamble (a thinking model dumps its
/// reasoning — "Okay, let's understand the requirements…" — before/instead of
/// code), then keeps from the first real Python line onward. If no code line
/// exists (pure prose), returns empty so the workflow rejects it loudly rather
/// than writing a file that won't even parse.
fn clean_test(file: &str, s: &str) -> String {
    let body = strip_fence(s);
    // Drop leading blanks and an echoed filename line.
    let mut lines: Vec<&str> = body.lines().collect();
    while let Some(first) = lines.first() {
        let t = first.trim();
        if t.is_empty() || t == file || t == format!("# {file}") {
            lines.remove(0);
        } else {
            break;
        }
    }
    // The first real line must look like Python (import/from/def/class/@/comment).
    // If it's a prose sentence, the model leaked its reasoning instead of code —
    // reject the whole thing (empty → the workflow retries / fails loudly) rather
    // than salvage code buried in a monologue.
    let looks_like_code = lines
        .first()
        .map(|l| {
            let t = l.trim();
            t.starts_with("import ")
                || t.starts_with("from ")
                || t.starts_with("def ")
                || t.starts_with("class ")
                || t.starts_with('@')
                || t.starts_with('#')
        })
        .unwrap_or(false);
    if looks_like_code {
        lines.join("\n").trim_end().to_string()
    } else {
        String::new()
    }
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
    fn clean_test_rejects_leaked_reasoning() {
        // A thinking model dumps its monologue instead of code → rejected (empty),
        // even though a `def` appears later inside the prose.
        let raw = "Okay, let's understand the requirements. The user wants tests.\n\
                   def test_x():\n    assert f()";
        assert_eq!(clean_test("test_x.py", raw), "");
    }

    #[test]
    fn write_tests_retries_past_a_leaked_reasoning_reply() {
        // First reply is leaked prose (rejected), second is real code → recovered.
        let backend = MockBackend::new([
            "Okay so I need to think about this carefully and write some tests...",
            "from m import f\ndef test_f():\n    assert f()",
        ]);
        let coverage = vec![CoverageItem {
            file: "test_m.py".into(),
            covers: "f works".into(),
        }];
        let written = write_tests(&backend, &coverage);
        assert_eq!(written.len(), 1);
        assert!(written[0].content.starts_with("from m import f"));
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
