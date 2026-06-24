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

// Keep these SHORT and concrete. A long, instruction-heavy prompt makes the small
// worker model degrade — observed live 2026-06-14: an elaborate version (Flask test-
// client tutorial + library allowlist) made coder-0 reply with just the filename, so
// every test came back empty and NONE were written. One example beats three paragraphs.

/// Python/pytest test-writer (for `test_*.py` files).
const TEST_WRITER_PY: &str = "You write a runnable pytest test file. Reply with ONLY the \
file body — no fences, no prose, do not repeat the filename. Import from the module under test \
(the test filename minus the leading 'test_' and '.py'; e.g. test_app.py → `app`). If it is a \
Flask app, use the test client: `from app import app` / `c = app.test_client()` / \
`r = c.get('/path')`. One test per behavior; the implementation does not exist yet so tests must \
FAIL until written. Write EXACTLY one test function per behavior listed below — do NOT invent \
extra tests (no 'is it importable' / isinstance / smoke tests). Assert EXACTLY, never loosely: \
check `r.status_code == <code>` AND compare the WHOLE parsed body with every field the behavior \
specifies, e.g. `assert r.get_json() == {'name': 'x', 'value': 2}` — NOT `b'2' in r.data` (that \
substring also matches '12' or '200') and NOT a partial dict that drops fields. When the app \
holds state across requests (e.g. an in-memory store), use a DIFFERENT resource name in each \
test so tests don't interfere. /no_think";

/// JavaScript/vitest test-writer (for `*.test.js` files — the plain-JS frontend).
const TEST_WRITER_JS: &str = "You write a runnable vitest test file (plain JavaScript, ES \
modules). Reply with ONLY the file body — no fences, no prose, do not repeat the filename. \
Start with `import { test, expect } from 'vitest'` and import the module under test (the \
test filename minus `.test.js`; e.g. script.test.js → `./script.js`). For DOM behavior, set \
`document.body.innerHTML` and assert on it. One test per behavior; the implementation does not \
exist yet so tests must FAIL until written.";

/// The right test-writer system prompt for a test file, by extension.
fn test_writer_system(file: &str) -> &'static str {
    if file.to_ascii_lowercase().ends_with(".js") {
        TEST_WRITER_JS
    } else {
        TEST_WRITER_PY
    }
}

/// Ask `worker` to write each test file named by the coverage plan. One model call
/// per file (the cheapest path); the file's coverage items are the worker's brief.
/// Returns the written files (those the worker produced non-empty content for).
pub fn write_tests(worker: &dyn ModelBackend, coverage: &[CoverageItem]) -> Vec<WrittenTest> {
    let mut out = Vec::new();
    for (file, covers) in group_by_file(coverage) {
        let prompt = test_prompt(&file, &covers);
        let req = GenerateRequest::new(vec![
            Message::system(test_writer_system(&file).to_string()),
            Message::user(prompt),
        ]);
        // Retry a few times: a tiny thinking model can leak reasoning (rejected by
        // clean_test) or blip, OR drop a required field from the contract (the 8B
        // systematically omits keys). Take the first reply that cleans to real code AND
        // asserts every `expect` key; keep the last non-empty as a fallback so we still
        // produce *a* test if none is perfect (better a slightly loose test than none).
        let mut fallback: Option<String> = None;
        for _ in 0..3 {
            let Ok(resp) = worker.generate(&req) else {
                continue;
            };
            let content = clean_test(&file, &resp.content);
            if content.trim().is_empty() {
                continue;
            }
            if covers_all_expected_keys(&content, &covers) {
                fallback = Some(content);
                break;
            }
            // Cleans to code but drops a contract field — remember it, try again.
            fallback.get_or_insert(content);
        }
        if let Some(content) = fallback {
            out.push(WrittenTest {
                file: file.clone(),
                content,
            });
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

fn test_prompt(file: &str, covers: &[CoverageItem]) -> String {
    let mut s = format!("Test file: {file}\n\nCover these behaviors, one test each:\n");
    for c in covers {
        s.push_str(&format!("- {}", c.covers));
        if let Some(expect) = &c.expect {
            // Hand the writer the exact body to assert so it copies rather than
            // reconstructs (the 8B drops fields when reconstructing — 0/3 kept `name`).
            s.push_str(&format!(
                "  → the response body must equal EXACTLY: {expect} (assert this whole \
                 dict; do not drop any key)"
            ));
        }
        s.push('\n');
    }
    s.push_str("\nReply with the complete test file.");
    s
}

/// The JSON object keys an `expect` literal requires (e.g. `{"name":..,"value":..}` →
/// `["name","value"]`). Empty if `expect` isn't a JSON object.
fn expected_keys(expect: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(expect)
        .ok()
        .and_then(|v| v.as_object().map(|m| m.keys().cloned().collect::<Vec<_>>()))
        .unwrap_or_default()
}

/// Does `test_body` assert every key each `expect` literal requires? The 8B
/// systematically drops fields (asserts `{'value':1}` for a `{name,value}` contract),
/// which under-specifies the frozen contract — so we verify in code, not just by prompt.
/// A test is acceptable if, for every behavior with an `expect` object, all its keys
/// appear somewhere in the generated body.
fn covers_all_expected_keys(test_body: &str, covers: &[CoverageItem]) -> bool {
    for c in covers {
        let Some(expect) = &c.expect else { continue };
        for key in expected_keys(expect) {
            // The key must appear as a string literal the assertion uses.
            let quoted_single = format!("'{key}'");
            let quoted_double = format!("\"{key}\"");
            if !test_body.contains(&quoted_single) && !test_body.contains(&quoted_double) {
                return false;
            }
        }
    }
    true
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
    // The first real line must look like code (Python or JS), not prose. If the model
    // leaked its reasoning ("Okay, let's…"), reject the whole thing (empty → the
    // workflow retries / fails loudly) rather than salvage code buried in a monologue.
    let looks_like_code = lines
        .first()
        .map(|l| {
            let t = l.trim();
            t.starts_with("import ")
                || t.starts_with("from ")
                || t.starts_with("def ")
                || t.starts_with("class ")
                || t.starts_with("const ")
                || t.starts_with("let ")
                || t.starts_with("test(")
                || t.starts_with("describe(")
                || t.starts_with("//")
                || t.starts_with('@')
                || t.starts_with('#')
        })
        .unwrap_or(false);
    if !looks_like_code {
        return String::new();
    }
    let joined = lines.join("\n");
    // The self-import fix is Python-only (test_app.py → from app). JS files keep theirs.
    if file.to_ascii_lowercase().ends_with(".py") {
        fix_self_import(file, &joined).trim_end().to_string()
    } else {
        joined.trim_end().to_string()
    }
}

/// Deterministically correct the module a Python test imports from. A small model
/// often writes `from test_app import ...` (the test file importing from itself) instead
/// of `from app import ...` — a circular/undefined import that can NEVER pass, no matter
/// how correct the implementation (observed live 2026-06-14: a `test_app.py` self-
/// imported, so the coder's correct Flask app still "failed"). The module name is
/// deterministic (`test_<mod>.py` → `<mod>`), so the harness fixes it rather than
/// trusting the model to. No-op for non-`test_*.py` files.
fn fix_self_import(file: &str, body: &str) -> String {
    let Some(module) = file
        .strip_prefix("test_")
        .and_then(|f| f.strip_suffix(".py"))
        .filter(|m| !m.is_empty())
    else {
        return body.to_string();
    };
    body.replace(
        &format!("from test_{module} import"),
        &format!("from {module} import"),
    )
    .replace(
        &format!("import test_{module}"),
        &format!("import {module}"),
    )
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
                expect: None,
            },
            CoverageItem {
                file: "test_f.py".into(),
                covers: "handles zero".into(),
                expect: None,
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
    fn clean_test_fixes_the_self_import_bug() {
        // The model self-imports (`from test_app import ...`) — uncorrectable by the
        // coder, so the harness rewrites it to the real module (`from app import ...`).
        let raw =
            "from test_app import get_restaurants\ndef test_x():\n    assert get_restaurants()";
        let cleaned = clean_test("test_app.py", raw);
        assert!(
            cleaned.starts_with("from app import get_restaurants"),
            "got: {cleaned}"
        );
        assert!(!cleaned.contains("from test_app"));

        // `import test_app` form is fixed too.
        let raw2 = "import test_app\ndef test_y():\n    assert test_app";
        // (the call-site `test_app` won't be rewritten — only the import statement is,
        //  which is the load-bearing line; a correct test refers to the module by its
        //  real name anyway.)
        let cleaned2 = clean_test("test_app.py", raw2);
        assert!(cleaned2.contains("import app"), "got: {cleaned2}");

        // A test that already imports correctly is untouched.
        let ok = "from app import f\ndef test_f():\n    assert f()";
        assert_eq!(clean_test("test_app.py", ok), ok);
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
            expect: None,
        }];
        let written = write_tests(&backend, &coverage);
        assert_eq!(written.len(), 1);
        assert!(written[0].content.starts_with("from m import f"));
    }

    #[test]
    fn rejects_a_test_that_drops_a_contract_field() {
        // The 8B drops fields: a {name,value} contract asserted as {value} only. The
        // first reply omits `name` (rejected); the second includes both (accepted).
        let backend = MockBackend::new([
            "from app import app\ndef test_incr():\n    r = app.test_client().post('/c/x/incr')\n    assert r.get_json() == {'value': 1}",
            "from app import app\ndef test_incr():\n    r = app.test_client().post('/c/x/incr')\n    assert r.get_json() == {'name': 'x', 'value': 1}",
        ]);
        let coverage = vec![CoverageItem {
            file: "test_app.py".into(),
            covers: "incr returns name and value".into(),
            expect: Some(r#"{"name":"x","value":1}"#.to_string()),
        }];
        let written = write_tests(&backend, &coverage);
        assert_eq!(written.len(), 1);
        assert!(
            written[0].content.contains("'name'"),
            "must keep the full contract, got: {}",
            written[0].content
        );
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
