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
        QueueAction::Serve => return serve(cli, &q, &cfg),
        QueueAction::Link { url, key } => link(url, key),
        QueueAction::LinkStatus => link_status(&cfg),
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
    // Same reasoning as `serve`: a task left `Drafting` by a killed run holds its
    // repository, and every later task for it is skipped without explanation.
    reclaim_abandoned(q);
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
                    sc_daemon::Drafted::Released { reason } => {
                        // Reachable from the *local* queue too, where there is
                        // no server to hand it back to — so it stops rather
                        // than spinning, same as a deferral.
                        eprintln!("  ↩ [{}] not for this machine — {reason}", task.id);
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

/// Link this daemon to a hosted server.
fn link(url: &str, key: &str) -> sc_proto::Result<()> {
    let mut cfg = sc_daemon::config::load()?;
    cfg.link(url, key)?;
    sc_daemon::config::save(&cfg)?;

    let link = cfg.require_server()?;
    println!("linked to {}", link.url);
    println!(
        "\nThe daemon dials OUT to it and accepts no connections, so nothing on \
         this machine is exposed."
    );
    println!("Run `smart-coder queue serve` to start drafting what it hands over.");
    Ok(())
}

/// Say where this daemon points, without printing the key.
fn link_status(cfg: &sc_daemon::DaemonConfig) -> sc_proto::Result<()> {
    match &cfg.server {
        // Never the key itself: this is the command a developer runs while
        // screen-sharing or pasting output into a bug report.
        Some(link) => {
            println!("linked to {}", link.url);
            println!("key: set ({} characters, not shown)", link.key.len());
        }
        None => {
            println!("not linked to a server");
            println!(
                "\n`smart-coder queue run` drafts from the local queue and needs no \
                 server.\nTo dial out to one: `smart-coder queue link <url> --key <key>`"
            );
        }
    }
    Ok(())
}

/// Dial the linked server and draft whatever it hands over.
fn serve(cli: &Cli, q: &sc_daemon::Queue, cfg: &sc_daemon::DaemonConfig) -> ExitCode {
    let link = match cfg.require_server() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if cfg.repos.is_empty() {
        eprintln!(
            "error: this daemon serves no repositories, so it can draft nothing the \
             server hands over. Add one with `smart-coder queue add-repo <name> <path>`."
        );
        return ExitCode::FAILURE;
    }
    let orchestrator = cli.orchestrator();
    // Fail at the terminal rather than on the first work item at 3am.
    if let Err(e) = sc_cli::preflight(&[("orchestrator", &orchestrator)]) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    // Anything still `Drafting` is a corpse from a previous run — this process
    // has claimed nothing yet. Left alone it would hold its repository forever
    // and every later request for it would be skipped in silence.
    reclaim_abandoned(q);

    // The names are what the server routes on: it hands this daemon only work
    // for repositories it declared, so a second machine serving a different set
    // is never offered work it cannot do.
    let served = cfg.names();
    let transport = sc_daemon::HttpTransport::new(&link.url, &link.key, &served);
    println!("● serve  {}", link.url);
    println!(
        "         {} repo(s) · specs only · Ctrl-C to stop",
        cfg.repos.len()
    );
    if served.is_empty() {
        // Otherwise this waits for ever, quietly, looking exactly like a server
        // with nothing to hand out.
        println!("         ⚠ no repositories configured — this daemon will be offered nothing");
        println!("           add one with `smart-coder queue add-repo <name> <path>`");
    } else {
        println!("         serving {}", served.join(", "));
    }

    // Ctrl-C terminates the process outright — there is no signal handler here,
    // because a dependency for one buys little: the queue is durable, so an
    // abrupt stop loses the in-flight model call and nothing else, and the next
    // start reclaims the task above. The stop flag `run_loop` takes exists for
    // programmatic callers and tests.
    sc_daemon::run_loop(
        &transport,
        &orchestrator,
        q,
        cfg,
        &|| false,
        &report_turn,
        // Only used after an *unreachable* server; an ordinary idle poll is
        // already paced by the server holding the request open.
        std::time::Duration::from_secs(10),
    );

    println!("stopped");
    ExitCode::SUCCESS
}

/// Requeue anything a previous run left mid-draft, and say so.
///
/// Not fatal if it fails: the daemon can still work on repositories that are not
/// blocked, and refusing to start over a housekeeping error would be worse than
/// the blockage it fixes.
fn reclaim_abandoned(q: &sc_daemon::Queue) {
    match q.requeue_abandoned() {
        Ok(ids) if ids.is_empty() => {}
        Ok(ids) => {
            println!(
                "  ↺ requeued {} task(s) left mid-draft by a previous run: {}",
                ids.len(),
                ids.join(", ")
            );
        }
        Err(e) => eprintln!("warning: could not check for abandoned tasks: {e}"),
    }
}

/// Render one turn of the serve loop.
fn report_turn(turn: &sc_daemon::Turn) {
    match turn {
        // An idle poll is the resting state, not news. Printing it would scroll
        // the interesting lines away at two per minute, forever.
        sc_daemon::Turn::Idle => {}
        sc_daemon::Turn::Drafted { id, artifact_dir } => {
            println!("  ◇ [{id}] drafted → {artifact_dir}");
            println!("      the reviewer reads it on the web surface");
        }
        sc_daemon::Turn::Deferred { id, reason } => {
            println!("  · [{id}] deferred — {reason}");
        }
        sc_daemon::Turn::Released { id, reason } => {
            // Not an error: the work went back to the queue for a machine that
            // can do it. Worth printing, because the usual cause is a repository
            // this daemon was expected to serve and does not.
            println!("  ↩ [{id}] handed back — {reason}");
        }
        sc_daemon::Turn::Failed { id, reason } => {
            eprintln!("  ✗ [{id}] failed — {reason}");
        }
        sc_daemon::Turn::Unreachable { reason } => {
            eprintln!("  ⚠ server unreachable — {reason}");
        }
    }
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
