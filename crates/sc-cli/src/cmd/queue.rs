//! `queue <action>` — file a request, draft its spec, approve or send it back
//! (spec 19).
//!
//! The vocabulary here is deliberately the same as the public web surface's:
//! **file · list · run · show · approve · send-back · discard**. There is no
//! action that builds. Approving marks a spec `Ready` and writes it into the
//! repository; the developer picks it up in their IDE when they choose.

use std::process::ExitCode;

use sc_cli::{Cli, QueueAction};

/// Dispatch a queue action.
pub fn queue(cli: &Cli, action: &QueueAction) -> ExitCode {
    let cfg = match sc_daemon::config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let q = match sc_daemon::Queue::default_queue() {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error: could not open the queue: {e}");
            return ExitCode::FAILURE;
        }
    };

    let outcome = match action {
        QueueAction::File { text, repo, kind } => file(&q, &cfg, text, repo, *kind),
        QueueAction::List => list(&q),
        QueueAction::Run => return run(cli, &q, &cfg),
        QueueAction::Show { id } => show(&q, &cfg, id),
        QueueAction::Approve { id } => approve(&q, &cfg, id),
        QueueAction::SendBack { id, notes } => send_back(&q, &cfg, id, notes),
        QueueAction::Discard { id } => discard(&q, id),
        QueueAction::Feedback { repo, all } => feedback(&cfg, repo.as_deref(), *all),
        QueueAction::AckFeedback { repo, id } => ack_feedback(repo, id),
        QueueAction::Repos => repos(&cfg),
        QueueAction::AddRepo { name, path } => add_repo(name, path),
        QueueAction::ForgetRepo { name } => forget_repo(name),
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn file(
    q: &sc_daemon::Queue,
    cfg: &sc_daemon::DaemonConfig,
    text: &str,
    repo: &str,
    kind: sc_daemon::IntakeKind,
) -> sc_proto::Result<()> {
    // Resolve the name up front so a typo is caught at the terminal rather than
    // at 3am when the runner claims it.
    cfg.require_repo(repo)?;

    // Feedback never enters the queue: it is a note, not a request, so it costs
    // no model call and touches no repository. Routing it here rather than
    // filtering it later is what keeps that structural.
    if !kind.drafts_a_spec() {
        let store = sc_daemon::FeedbackStore::default_store()?;
        let item = sc_daemon::Feedback::new(sc_daemon::task::new_id(), text, repo);
        store.put(&item)?;
        println!("noted for {repo} ({})", item.id);
        println!("  {}", item.summary());
        println!("\nKept as feedback — no spec, nothing queued.");
        return Ok(());
    }

    let task = sc_daemon::Task::of_kind(sc_daemon::task::new_id(), text, repo, kind);
    q.put(&task)?;
    println!("filed {} against {repo} ({kind})", task.id);
    println!("  {}", task.summary());
    println!("\nRun `smart-coder queue run` to draft it.");
    Ok(())
}

fn feedback(cfg: &sc_daemon::DaemonConfig, repo: Option<&str>, all: bool) -> sc_proto::Result<()> {
    if let Some(r) = repo {
        cfg.require_repo(r)?;
    }
    let store = sc_daemon::FeedbackStore::default_store()?;
    let items = if all {
        store.all(repo)?
    } else {
        store.outstanding(repo)?
    };
    if items.is_empty() {
        println!(
            "no {}feedback{}",
            if all { "" } else { "outstanding " },
            repo.map(|r| format!(" for {r}")).unwrap_or_default()
        );
        return Ok(());
    }
    for f in &items {
        let mark = if f.acknowledged { "·" } else { "◆" };
        println!("{mark} {:<16} {:<8} {}", f.id, f.repo, f.summary());
    }
    if !all {
        println!("\n(`--all` also shows what you have already acknowledged)");
    }
    Ok(())
}

fn ack_feedback(repo: &str, id: &str) -> sc_proto::Result<()> {
    let store = sc_daemon::FeedbackStore::default_store()?;
    let item = store.acknowledge(repo, id)?;
    println!("acknowledged {id} — {}", item.summary());
    Ok(())
}

fn list(q: &sc_daemon::Queue) -> sc_proto::Result<()> {
    let mut tasks = q.all()?;
    if tasks.is_empty() {
        println!("the queue is empty");
        return Ok(());
    }
    // What needs a human first — a queue sorted by id answers no question anyone
    // actually has.
    tasks.sort_by_key(|t| (t.state.list_order(), t.id.clone()));

    for t in &tasks {
        println!(
            "{:<16} {:<10} {:<12} {:<8} {}",
            t.id,
            t.state.label(),
            t.kind.slug(),
            t.repo,
            t.summary()
        );
        if let Some(note) = &t.note {
            println!("{:<16} └ {note}", "");
        }
    }

    // A corrupt record is visible rather than silently absent.
    let bad = q.unreadable()?;
    if !bad.is_empty() {
        eprintln!(
            "\nwarning: {} queue record(s) could not be read: {}",
            bad.len(),
            bad.join(", ")
        );
    }
    Ok(())
}

/// Draft queued tasks until the queue is empty or the process is stopped.
///
/// Foreground and killable: the queue is durable, so Ctrl-C loses nothing and a
/// restart resumes from where it stopped.
fn run(cli: &Cli, q: &sc_daemon::Queue, cfg: &sc_daemon::DaemonConfig) -> ExitCode {
    if cfg.repos.is_empty() {
        eprintln!(
            "error: this daemon serves no repositories. Add one with \
             `smart-coder queue add-repo <name> <path>`."
        );
        return ExitCode::FAILURE;
    }
    let orchestrator = cli.orchestrator();
    if let Err(e) = sc_cli::preflight(&[("orchestrator", &orchestrator)]) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    println!("● queue  drafting specs for {} repo(s)", cfg.repos.len());
    let mut drafted = 0usize;
    loop {
        match sc_daemon::draft_next(&orchestrator, q, cfg) {
            Ok(None) => break,
            Ok(Some((task, outcome))) => {
                drafted += 1;
                match outcome {
                    sc_daemon::Drafted::AwaitingReview { artifact_dir } => {
                        println!("  ◇ [{}] drafted → {artifact_dir}", task.id);
                        println!(
                            "      awaiting review: `smart-coder queue show {}`",
                            task.id
                        );
                    }
                    sc_daemon::Drafted::Deferred { reason } => {
                        println!("  · [{}] deferred — {reason}", task.id);
                        // Left Queued, so stop rather than spin on it forever.
                        break;
                    }
                    sc_daemon::Drafted::Failed { reason } => {
                        eprintln!("  ✗ [{}] failed — {reason}", task.id);
                    }
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    if drafted == 0 {
        println!("nothing to draft");
    }
    ExitCode::SUCCESS
}

fn show(q: &sc_daemon::Queue, cfg: &sc_daemon::DaemonConfig, id: &str) -> sc_proto::Result<()> {
    let task = q.require(id)?;
    println!("{}  {}  [{}]", task.id, task.repo, task.state.label());
    println!("{}\n", task.summary());
    match sc_daemon::runner::read_spec(cfg, &task) {
        Some(spec) => {
            println!("{spec}");
            if task.state == sc_daemon::TaskState::AwaitingReview {
                println!(
                    "\n— approve with `smart-coder queue approve {id}`, or send it back \
                     with `smart-coder queue send-back {id} <what to change>`"
                );
            }
        }
        None => println!("(no spec drafted yet)"),
    }
    Ok(())
}

fn approve(q: &sc_daemon::Queue, cfg: &sc_daemon::DaemonConfig, id: &str) -> sc_proto::Result<()> {
    let task = sc_daemon::approve(q, cfg, id)?;
    println!("approved {id} — {}", task.summary());
    if let Some(dir) = &task.artifact_dir {
        println!("  the spec is in {dir}/spec.md, ready to commit");
    }
    // Say plainly that nothing started, or a user reasonably assumes it did.
    println!("\nNothing has been built. Pick it up in your IDE when you're ready.");
    Ok(())
}

fn send_back(
    q: &sc_daemon::Queue,
    cfg: &sc_daemon::DaemonConfig,
    id: &str,
    notes: &str,
) -> sc_proto::Result<()> {
    sc_daemon::send_back(q, cfg, id, notes)?;
    println!("sent {id} back — it will be redrafted with your note");
    Ok(())
}

fn discard(q: &sc_daemon::Queue, id: &str) -> sc_proto::Result<()> {
    sc_daemon::discard(q, id, None)?;
    println!("discarded {id}");
    Ok(())
}

fn repos(cfg: &sc_daemon::DaemonConfig) -> sc_proto::Result<()> {
    if cfg.repos.is_empty() {
        println!("this daemon serves no repositories");
        println!("add one: `smart-coder queue add-repo <name> <path>`");
        return Ok(());
    }
    for r in &cfg.repos {
        println!("{:<16} {}", r.name, r.path.display());
    }
    Ok(())
}

fn add_repo(name: &str, path: &str) -> sc_proto::Result<()> {
    let mut cfg = sc_daemon::config::load()?;
    cfg.add(name, std::path::Path::new(path))?;
    sc_daemon::config::save(&cfg)?;
    println!(
        "serving {name} → {}",
        cfg.repo(name).unwrap().path.display()
    );
    Ok(())
}

fn forget_repo(name: &str) -> sc_proto::Result<()> {
    let mut cfg = sc_daemon::config::load()?;
    if !cfg.remove(name) {
        println!("no repository named {name}");
        return Ok(());
    }
    sc_daemon::config::save(&cfg)?;
    println!("forgot {name}");
    Ok(())
}
