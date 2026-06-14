//! M2 exit-criterion test (spec 07): multi-turn tasks stay coherent on an 8k
//! window **without blowing the budget**, and the assembled prompt is
//! inspectable. We drive a backend that advertises an 8k window through many
//! turns whose observations are huge (whole-file reads of a big file), and assert
//! the assembled prompt never exceeds the hard budget.

use dc_core::{run_agent_with, AgentConfig, ParseRepair};
use dc_model::{Capabilities, GenerateRequest, GenerateResponse, ModelBackend, ToolCalling};
use dc_proto::Result;
use dc_tools::default_registry;

/// An 8k-window backend with no tokenizer (forces the estimator) that always asks
/// to read the same big file — the worst case for context growth.
struct EightKReader {
    script: std::cell::RefCell<Vec<String>>,
}

impl EightKReader {
    fn new(turns: usize) -> Self {
        // Read the big file every turn but the last, then finish.
        let mut script: Vec<String> = (0..turns)
            .map(|_| r#"{"tool":"read_file","path":"big.rs"}"#.to_string())
            .collect();
        script.push(r#"{"tool":"finish"}"#.to_string());
        Self {
            script: std::cell::RefCell::new(script),
        }
    }
}

impl ModelBackend for EightKReader {
    fn name(&self) -> &str {
        "eightk"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_context_tokens: 8_192,
            tool_calling: ToolCalling::None,
            on_device: false,
        }
    }
    fn generate(&self, _req: &GenerateRequest) -> Result<GenerateResponse> {
        let mut s = self.script.borrow_mut();
        let content = if s.is_empty() {
            r#"{"tool":"finish"}"#.to_string()
        } else {
            s.remove(0)
        };
        Ok(GenerateResponse { content })
    }
}

fn temp_repo(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "dc-core-ctx-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn multi_turn_run_stays_under_the_8k_budget() {
    let ws = temp_repo("budget");
    // A big source file: ~2000 lines, far more than fits an 8k window verbatim.
    let big: String = (0..2000)
        .map(|i| format!("fn func_{i}() {{ let x = {i}; }}\n"))
        .collect();
    std::fs::write(ws.join("big.rs"), &big).unwrap();

    let backend = EightKReader::new(8); // 8 big reads, then finish
    let registry = default_registry();
    let report = run_agent_with(
        &backend,
        &registry,
        &ParseRepair,
        "investigate func_1234 in the project",
        &ws,
        &AgentConfig::default(),
    )
    .unwrap();

    assert!(report.finished, "should finish within budget");
    // The whole point of M2: even with huge observations every turn, the assembled
    // prompt never exceeds the hard budget.
    assert!(
        report.peak_prompt_tokens <= report.prompt_budget,
        "prompt {} exceeded budget {}",
        report.peak_prompt_tokens,
        report.prompt_budget
    );
    // And the budget is the effective 8k fraction minus the reserve, not unbounded.
    assert!(report.prompt_budget > 0 && report.prompt_budget < 8_192);

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn find_symbol_tool_resolves_through_the_loop() {
    let ws = temp_repo("findsym");
    std::fs::write(ws.join("lib.rs"), "fn helper() {}\nfn the_target() {}\n").unwrap();

    // Model uses find_symbol, observes the location, then finishes.
    struct FindThenFinish(std::cell::RefCell<Vec<String>>);
    impl ModelBackend for FindThenFinish {
        fn name(&self) -> &str {
            "fsf"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                max_context_tokens: 8_192,
                tool_calling: ToolCalling::None,
                on_device: false,
            }
        }
        fn generate(&self, _r: &GenerateRequest) -> Result<GenerateResponse> {
            Ok(GenerateResponse {
                content: self.0.borrow_mut().remove(0),
            })
        }
    }
    let backend = FindThenFinish(std::cell::RefCell::new(vec![
        r#"{"tool":"find_symbol","name":"the_target"}"#.to_string(),
        r#"{"tool":"finish"}"#.to_string(),
    ]));

    let registry = default_registry();
    let report = run_agent_with(
        &backend,
        &registry,
        &ParseRepair,
        "find the_target",
        &ws,
        &AgentConfig::default(),
    )
    .unwrap();
    assert!(report.finished);
    assert_eq!(report.metrics.invalid, 0);

    let _ = std::fs::remove_dir_all(&ws);
}
