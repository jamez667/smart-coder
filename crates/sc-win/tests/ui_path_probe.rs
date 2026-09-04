//! The GUI's EXACT path, driven headlessly.
//!
//! The existing `investigate_probe` calls `session::agent::investigate()` directly, which
//! skips everything the UI does first — the intent classifier call, the `Conversation` with
//! its README/TODO/file-tree context, and the `ChatEvent` drain. That is why it kept
//! producing clean runs while the user's UI failed every time.
//!
//! This goes through `ChatSession::spawn_planning` — what `logic_a.rs` actually calls — and
//! drains the same events the UI drains.
//!
//! Run with:
//!   cargo test -p sc-win --test ui_path_probe -- --ignored --nocapture

use std::fmt::Write as _;
use std::io::Write as _;

const QUESTION: &str = "Can you investigate why on the jump screen the trail behind the \
                        stars it thin before it gets thick? it should be the other way around.";

#[test]
#[ignore]
fn the_gui_path_answers_the_star_trail_question() {
    let ws = std::path::PathBuf::from(r"C:\Users\mail\working\Personal\Games\void-claim");
    assert!(ws.join("crates").is_dir(), "void-claim not found at {ws:?}");

    let cfg = sc_win::config::UiConfig::load();
    println!("backend: {} model: {}", cfg.base_url, cfg.model);

    // Exactly what `App::open_project` + `send_chat` build.
    let readme = std::fs::read_to_string(ws.join("README.md")).unwrap_or_default();
    let todo = std::fs::read_to_string(ws.join("TODO.md")).unwrap_or_default();
    let mut convo = sc_win::chat::Conversation::open(&readme, &todo);
    // EXACTLY what the UI passes: `project_file_paths()` derives from `filetree::full_rows`,
    // NOT from `sc_tools::source_files`. Different filters mean a different file list and a
    // different prompt size -- which is the kind of difference that decides whether a small
    // model has room to finish a thought.
    let tree: Vec<String> = sc_win::filetree::full_rows(&ws)
        .iter()
        .filter(|r| !r.is_dir)
        .map(|r| r.rel.clone())
        .collect();
    println!("file tree rows: {}", tree.len());
    convo.set_file_tree(tree);
    // The UI opens TODO.md in the code view on project open, and passes it as the open file.
    convo.set_open_file(Some(("TODO.md".to_string(), todo.clone())));
    convo.user_turn(QUESTION);

    let session =
        sc_win::chat_session::ChatSession::spawn_planning(cfg, convo, false, Some(ws.clone()));

    let started = std::time::Instant::now();
    let mut streamed = String::new();
    let mut reply = String::new();
    let mut failed: Option<String> = None;
    // Drain like the UI does: `drain()` returns a batch each tick.
    'outer: loop {
        for ev in session.drain() {
            match ev {
                sc_win::chat_session::ChatEvent::Token(t) => {
                    print!("{t}");
                    let _ = std::io::stdout().flush();
                    streamed.push_str(&t);
                }
                sc_win::chat_session::ChatEvent::Reply {
                    text, truncated, ..
                } => {
                    println!(
                        "
[reply: {} chars, truncated={truncated}]",
                        text.len()
                    );
                    reply = text;
                    break 'outer;
                }
                sc_win::chat_session::ChatEvent::Failed(m) => {
                    println!(
                        "
[FAILED] {m}"
                    );
                    failed = Some(m);
                    break 'outer;
                }
            }
        }
        if started.elapsed().as_secs() > 900 {
            failed = Some("timed out after 15 minutes".to_string());
            break 'outer;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let secs = started.elapsed().as_secs();

    let mut log = String::new();
    let _ = writeln!(log, "# GUI-path probe\n");
    let _ = writeln!(log, "Question: {QUESTION}\n");
    let _ = writeln!(log, "- elapsed: {secs}s");
    let _ = writeln!(log, "- failed: {failed:?}\n");
    let _ = writeln!(log, "## Streamed to the panel\n\n```\n{streamed}\n```\n");
    let _ = writeln!(log, "## Final reply\n\n```\n{reply}\n```\n");
    let dir = std::path::Path::new("logs");
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join("ui-path-probe.md");
    std::fs::File::create(&path)
        .and_then(|mut f| f.write_all(log.as_bytes()))
        .expect("write the probe log");
    println!("\nWROTE {} ({secs}s)", path.display());

    // The bar the UI has been failing: a real answer, not the "did not reach a conclusion"
    // fallback.
    assert!(failed.is_none(), "the run failed: {failed:?}");
    assert!(
        !reply.contains("did not reach a conclusion"),
        "the GUI path produced no answer -- this is the failure the user reports every time"
    );
    assert!(
        reply.to_lowercase().contains("starfield"),
        "the answer must name the file: {reply}"
    );
}
