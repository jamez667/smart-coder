//! The daemon's side of the conversation: dial out, draft, push back.
//!
//! ```text
//!   long-poll ──► work? ──no──► poll again
//!                   │
//!                  yes
//!                   ▼
//!            draft it locally ──► push the spec back
//!                   │
//!                   ├── repository busy or mid-rebase ──► leave it, poll again
//!                   └── could not draft ──► report the failure
//! ```
//!
//! The transport is a trait ([`Transport`]) rather than a concrete HTTP client,
//! for one reason that matters: the *loop* is where the interesting decisions
//! live — what counts as a failure worth reporting, what is merely a deferral,
//! what happens when the server is unreachable — and none of those should need a
//! network to test. [`HttpTransport`] is the real one; tests drive a scripted one.
//!
//! ## What is deliberately not here
//!
//! No retry-with-backoff on a *drafting* failure. Spec 19 is explicit: a failed
//! run stays failed and visible, because an automatic retry hides a reproducible
//! problem behind eventual success. Retrying a *transport* error is different —
//! the server being briefly unreachable says nothing about the request — so the
//! loop simply polls again.

use std::time::Duration;

use sc_model::ModelBackend;
use sc_proto::{DcError, Result};

use crate::config::DaemonConfig;
use crate::queue::Queue;
use crate::runner::{self, Drafted};
use crate::task::Task;
use crate::wire::{self, DraftFailed, DraftedSpec, PollResponse, WorkItem};

/// How the daemon reaches the server.
///
/// Object-safe and synchronous, matching the rest of this workspace — there is no
/// async runtime outside the GUI.
pub trait Transport {
    /// Long-poll for work. The server holds this open until there is something or
    /// its own timeout elapses, so a `None` here is a normal idle tick rather
    /// than an error.
    fn poll(&self) -> Result<Option<WorkItem>>;

    /// Hand back a drafted spec.
    fn push_drafted(&self, drafted: &DraftedSpec) -> Result<()>;

    /// Report that a request could not be drafted, and why.
    fn push_failed(&self, failed: &DraftFailed) -> Result<()>;
}

/// The real transport: outbound HTTPS with a per-daemon API key.
pub struct HttpTransport {
    base_url: String,
    api_key: String,
    agent: ureq::Agent,
}

impl HttpTransport {
    /// Build a transport for `base_url`, authenticating with `api_key`.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            // Outwaits the server's hold on a long poll, so a healthy idle poll is
            // never mistaken for a hung request.
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(wire::POLL_CLIENT_TIMEOUT))
                .build()
                .into(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// A POST whose body is `payload`, authenticated.
    fn post_json<T: serde::Serialize>(&self, path: &str, payload: &T) -> Result<()> {
        self.agent
            .post(self.url(path))
            .header("Authorization", &format!("Bearer {}", self.api_key))
            .send_json(payload)
            .map(|_| ())
            .map_err(|e| DcError::Backend(format!("{path}: {e}")))
    }
}

impl Transport for HttpTransport {
    fn poll(&self) -> Result<Option<WorkItem>> {
        let body = self
            .agent
            .get(self.url(wire::route::WORK))
            .header("Authorization", &format!("Bearer {}", self.api_key))
            .call()
            .map_err(|e| DcError::Backend(format!("poll: {e}")))?
            .body_mut()
            .read_to_string()
            .map_err(|e| DcError::Backend(format!("poll: {e}")))?;

        let response: PollResponse = serde_json::from_str(&body)
            .map_err(|e| DcError::Backend(format!("poll: unreadable response: {e}")))?;
        // Check the version before acting on the payload: a skewed peer should
        // produce a clear message, not a puzzling failure three calls later.
        wire::check_protocol(response.protocol(), "the server").map_err(DcError::Backend)?;

        Ok(match response {
            PollResponse::Work { item, .. } => Some(item),
            PollResponse::Idle { .. } => None,
        })
    }

    fn push_drafted(&self, drafted: &DraftedSpec) -> Result<()> {
        self.post_json(&wire::route::drafted(&drafted.id), drafted)
    }

    fn push_failed(&self, failed: &DraftFailed) -> Result<()> {
        self.post_json(&wire::route::failed(&failed.id), failed)
    }
}

/// What one turn of the loop did — returned so a caller can render progress and
/// tests can assert on the decision rather than on side effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Turn {
    /// Nothing to do.
    Idle,
    /// Drafted and pushed back.
    Drafted { id: String, artifact_dir: String },
    /// The repository was not ready — mid-rebase, or busy. Left for a later poll
    /// rather than reported as a failure: it fixes itself, and a failure would
    /// make the developer requeue by hand for a transient condition.
    Deferred { id: String, reason: String },
    /// Could not be drafted, and the server was told.
    Failed { id: String, reason: String },
    /// The server could not be reached. The loop keeps going — a brief outage
    /// says nothing about any request.
    Unreachable { reason: String },
}

/// Run one turn: poll, and act on whatever came back.
pub fn one_turn(
    transport: &dyn Transport,
    orchestrator: &dyn ModelBackend,
    queue: &Queue,
    cfg: &DaemonConfig,
) -> Turn {
    let item = match transport.poll() {
        Ok(Some(item)) => item,
        Ok(None) => return Turn::Idle,
        Err(e) => {
            return Turn::Unreachable {
                reason: e.to_string(),
            }
        }
    };

    // Mirror the server's work item into the local queue, so the daemon's own
    // record is the same shape whether the request arrived over HTTP or was
    // filed at a terminal. Everything downstream then works unchanged.
    let mut task = Task::of_kind(&item.id, &item.text, &item.repo, item.kind);
    if let Some(note) = &item.send_back_note {
        task.note = Some(format!("sent back: {note}"));
    }
    if let Err(e) = queue.put(&task) {
        // The queue is the daemon's own storage; failing to write it is a local
        // fault, not the request's. Report it so the request is not silently
        // dropped, then carry on.
        let reason = format!("could not record the request locally: {e}");
        let _ = transport.push_failed(&DraftFailed::new(&item.id, &reason));
        return Turn::Failed {
            id: item.id,
            reason,
        };
    }

    // Carry a send-back note into the artifact directory, so the redraft grounds
    // on the developer's reason rather than regenerating the same spec.
    if let Some(note) = &item.send_back_note {
        let _ = runner::send_back_note(cfg, &task, note);
    }

    match runner::draft(orchestrator, queue, cfg, &task) {
        Ok(Drafted::AwaitingReview { artifact_dir }) => {
            let spec = runner::read_spec(cfg, &queue.get(&item.id).ok().flatten().unwrap_or(task))
                .unwrap_or_default();
            let drafted = DraftedSpec::new(&item.id, spec, &artifact_dir);
            match transport.push_drafted(&drafted) {
                Ok(()) => Turn::Drafted {
                    id: item.id,
                    artifact_dir,
                },
                // The spec IS on disk locally; only the hand-back failed. Say so
                // rather than reporting a drafting failure that did not happen.
                Err(e) => Turn::Unreachable {
                    reason: format!("drafted {} but could not hand it back: {e}", item.id),
                },
            }
        }
        Ok(Drafted::Deferred { reason }) => Turn::Deferred {
            id: item.id,
            reason,
        },
        Ok(Drafted::Failed { reason }) => {
            let _ = transport.push_failed(&DraftFailed::new(&item.id, &reason));
            Turn::Failed {
                id: item.id,
                reason,
            }
        }
        Err(e) => {
            let reason = e.to_string();
            let _ = transport.push_failed(&DraftFailed::new(&item.id, &reason));
            Turn::Failed {
                id: item.id,
                reason,
            }
        }
    }
}

/// Poll forever, drafting whatever arrives.
///
/// `should_stop` is checked between turns so a caller can interrupt cleanly —
/// the loop never abandons a draft midway, because a half-written spec helps
/// nobody. `on_turn` renders progress.
///
/// `idle_pause` is the wait after an *unreachable* server; a normal idle poll
/// needs none, because the server already held the request open.
pub fn run_loop(
    transport: &dyn Transport,
    orchestrator: &dyn ModelBackend,
    queue: &Queue,
    cfg: &DaemonConfig,
    should_stop: &dyn Fn() -> bool,
    on_turn: &dyn Fn(&Turn),
    idle_pause: Duration,
) {
    while !should_stop() {
        let turn = one_turn(transport, orchestrator, queue, cfg);
        on_turn(&turn);
        // Back off only when the server is down. Sleeping after ordinary work
        // would add latency the long poll exists to remove.
        if matches!(turn, Turn::Unreachable { .. }) {
            std::thread::sleep(idle_pause);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intake::IntakeKind;
    use crate::task::TaskState;
    use crate::test_support::{interrupt, temp_dir, temp_repo};
    use sc_model::{Capabilities, GenerateRequest, GenerateResponse, ToolCalling};
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// A scripted server. Proves the loop without a socket.
    #[derive(Default)]
    struct Scripted {
        queued: Mutex<Vec<WorkItem>>,
        drafted: Mutex<Vec<DraftedSpec>>,
        failed: Mutex<Vec<DraftFailed>>,
        /// When set, every call fails — a server that is down.
        down: bool,
    }

    impl Scripted {
        fn with(items: Vec<WorkItem>) -> Self {
            Self {
                queued: Mutex::new(items),
                ..Default::default()
            }
        }
        fn down() -> Self {
            Self {
                down: true,
                ..Default::default()
            }
        }
    }

    impl Transport for Scripted {
        fn poll(&self) -> Result<Option<WorkItem>> {
            if self.down {
                return Err(DcError::Backend("connection refused".into()));
            }
            let mut q = self.queued.lock().unwrap();
            Ok(if q.is_empty() {
                None
            } else {
                Some(q.remove(0))
            })
        }
        fn push_drafted(&self, d: &DraftedSpec) -> Result<()> {
            if self.down {
                return Err(DcError::Backend("connection refused".into()));
            }
            self.drafted.lock().unwrap().push(d.clone());
            Ok(())
        }
        fn push_failed(&self, f: &DraftFailed) -> Result<()> {
            if self.down {
                return Err(DcError::Backend("connection refused".into()));
            }
            self.failed.lock().unwrap().push(f.clone());
            Ok(())
        }
    }

    struct Model(String);
    impl ModelBackend for Model {
        fn name(&self) -> &str {
            "scripted"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                max_context_tokens: 8192,
                tool_calling: ToolCalling::None,
                on_device: false,
            }
        }
        fn generate(&self, _r: &GenerateRequest) -> Result<GenerateResponse> {
            Ok(GenerateResponse {
                content: self.0.clone(),
            })
        }
    }

    fn fixture(tag: &str) -> (Queue, DaemonConfig, PathBuf, PathBuf) {
        let qdir = temp_dir(&format!("{tag}-q"));
        let repo = temp_repo(&format!("{tag}-repo"));
        let mut cfg = DaemonConfig::default();
        cfg.add("alpha", &repo).unwrap();
        (Queue::open(&qdir).unwrap(), cfg, qdir, repo)
    }

    fn item(id: &str) -> WorkItem {
        WorkItem {
            id: id.into(),
            text: "Add a health check endpoint".into(),
            repo: "alpha".into(),
            kind: IntakeKind::Feature,
            send_back_note: None,
        }
    }

    fn cleanup(dirs: &[&PathBuf]) {
        for d in dirs {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn an_idle_poll_is_not_an_error() {
        // The server holds the request open, so "nothing to do" is the normal
        // resting state rather than a failure to report.
        let (q, cfg, qdir, repo) = fixture("idle");
        let turn = one_turn(&Scripted::default(), &Model("#".into()), &q, &cfg);
        assert_eq!(turn, Turn::Idle);
        cleanup(&[&qdir, &repo]);
    }

    #[test]
    fn work_is_drafted_locally_and_the_spec_is_handed_back() {
        let (q, cfg, qdir, repo) = fixture("draft");
        let server = Scripted::with(vec![item("srv-1")]);
        let turn = one_turn(&server, &Model("# The spec\n\nBody.".into()), &q, &cfg);

        match &turn {
            Turn::Drafted { id, artifact_dir } => {
                assert_eq!(id, "srv-1");
                assert!(artifact_dir.starts_with("specs/"), "{artifact_dir}");
            }
            other => panic!("expected a draft, got {other:?}"),
        }

        // The spec went back to the server, with its text.
        let pushed = server.drafted.lock().unwrap();
        assert_eq!(pushed.len(), 1);
        assert!(pushed[0].spec.contains("The spec"), "{:?}", pushed[0]);
        // And the file is on the developer's machine, which is where it lives.
        assert!(repo.join(&pushed[0].artifact_dir).join("spec.md").is_file());
        // Locally the task is awaiting review — the server was told, not obeyed.
        assert_eq!(q.require("srv-1").unwrap().state, TaskState::AwaitingReview);
        drop(pushed);
        cleanup(&[&qdir, &repo]);
    }

    #[test]
    fn a_repository_mid_rebase_defers_rather_than_failing() {
        // The tree fixes itself once the developer finishes. Reporting a failure
        // would make them requeue by hand for a transient condition.
        let (q, cfg, qdir, repo) = fixture("deferred");
        interrupt(&repo, "MERGE_HEAD");
        let server = Scripted::with(vec![item("srv-1")]);
        let turn = one_turn(&server, &Model("#".into()), &q, &cfg);

        match &turn {
            Turn::Deferred { id, reason } => {
                assert_eq!(id, "srv-1");
                assert!(reason.contains("merge"), "{reason}");
            }
            other => panic!("expected a deferral, got {other:?}"),
        }
        // Nothing was reported as a failure, and nothing was handed back.
        assert!(server.failed.lock().unwrap().is_empty());
        assert!(server.drafted.lock().unwrap().is_empty());
        cleanup(&[&qdir, &repo]);
    }

    #[test]
    fn a_drafting_failure_is_reported_and_never_silently_retried() {
        // Spec 19: a failed run stays failed and visible. Automatic retry hides a
        // reproducible problem behind eventual success.
        let (q, cfg, qdir, repo) = fixture("failed");
        let server = Scripted::with(vec![item("srv-1")]);
        // An empty reply is a dead backend, which the workflow rejects.
        let turn = one_turn(&server, &Model(String::new()), &q, &cfg);

        assert!(matches!(turn, Turn::Failed { .. }), "{turn:?}");
        let failed = server.failed.lock().unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].id, "srv-1");
        assert!(
            !failed[0].reason.is_empty(),
            "the reason reaches the server"
        );
        drop(failed);
        cleanup(&[&qdir, &repo]);
    }

    #[test]
    fn an_unreachable_server_does_not_fail_any_request() {
        // A brief outage says nothing about a request. Marking one failed for it
        // would make the developer chase a problem that was never theirs.
        let (q, cfg, qdir, repo) = fixture("down");
        let turn = one_turn(&Scripted::down(), &Model("#".into()), &q, &cfg);
        assert!(matches!(turn, Turn::Unreachable { .. }), "{turn:?}");
        assert!(q.all().unwrap().is_empty(), "nothing was recorded locally");
        cleanup(&[&qdir, &repo]);
    }

    #[test]
    fn a_spec_drafted_but_not_handed_back_is_not_reported_as_a_drafting_failure() {
        // The spec IS on disk; only the hand-back failed. Calling that a drafting
        // failure would tell the developer their request could not be specified
        // when it already has been.
        struct DraftsThenDies {
            queued: Mutex<Vec<WorkItem>>,
            failed: Mutex<Vec<DraftFailed>>,
        }
        impl Transport for DraftsThenDies {
            fn poll(&self) -> Result<Option<WorkItem>> {
                let mut q = self.queued.lock().unwrap();
                Ok(if q.is_empty() {
                    None
                } else {
                    Some(q.remove(0))
                })
            }
            fn push_drafted(&self, _d: &DraftedSpec) -> Result<()> {
                Err(DcError::Backend("connection reset".into()))
            }
            fn push_failed(&self, f: &DraftFailed) -> Result<()> {
                self.failed.lock().unwrap().push(f.clone());
                Ok(())
            }
        }

        let (q, cfg, qdir, repo) = fixture("halfway");
        let server = DraftsThenDies {
            queued: Mutex::new(vec![item("srv-1")]),
            failed: Mutex::new(Vec::new()),
        };
        let turn = one_turn(&server, &Model("# The spec".into()), &q, &cfg);

        match &turn {
            Turn::Unreachable { reason } => {
                assert!(reason.contains("could not hand it back"), "{reason}")
            }
            other => panic!("expected Unreachable, got {other:?}"),
        }
        assert!(
            server.failed.lock().unwrap().is_empty(),
            "a transport problem is not a drafting failure"
        );
        // The work is not lost: the spec is on disk and the task awaits review.
        assert_eq!(q.require("srv-1").unwrap().state, TaskState::AwaitingReview);
        cleanup(&[&qdir, &repo]);
    }

    #[test]
    fn a_request_for_an_unconfigured_repository_fails_without_a_model_call() {
        // The daemon resolves the name against ITS OWN set; the server cannot
        // name a repository this machine does not serve.
        let (q, cfg, qdir, repo) = fixture("unknown-repo");
        let mut i = item("srv-1");
        i.repo = "gamma".into();
        let server = Scripted::with(vec![i]);
        let turn = one_turn(&server, &Model("#".into()), &q, &cfg);

        match &turn {
            Turn::Failed { reason, .. } => assert!(reason.contains("gamma"), "{reason}"),
            other => panic!("expected a failure, got {other:?}"),
        }
        assert_eq!(server.failed.lock().unwrap().len(), 1);
        cleanup(&[&qdir, &repo]);
    }

    #[test]
    fn feedback_pushed_as_work_is_refused_rather_than_drafted() {
        // Feedback never becomes a spec. If a server ever hands one over as work,
        // that is a bug on its side — and drafting it anyway would manufacture a
        // work item nobody asked for.
        let (q, cfg, qdir, repo) = fixture("feedback-as-work");
        let mut i = item("srv-1");
        i.kind = IntakeKind::Feedback;
        let server = Scripted::with(vec![i]);
        let turn = one_turn(&server, &Model("#".into()), &q, &cfg);

        match &turn {
            Turn::Failed { reason, .. } => assert!(reason.contains("note"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        cleanup(&[&qdir, &repo]);
    }

    #[test]
    fn the_loop_stops_between_turns_and_never_mid_draft() {
        // A half-written spec helps nobody, so the stop flag is checked between
        // turns rather than inside one.
        let (q, cfg, qdir, repo) = fixture("stop");
        let server = Scripted::with(vec![item("srv-1"), item("srv-2")]);
        let turns: Mutex<Vec<Turn>> = Mutex::new(Vec::new());

        run_loop(
            &server,
            &Model("# The spec".into()),
            &q,
            &cfg,
            // Stop after the first turn.
            &|| !turns.lock().unwrap().is_empty(),
            &|t| turns.lock().unwrap().push(t.clone()),
            Duration::from_millis(1),
        );

        assert_eq!(turns.lock().unwrap().len(), 1, "stopped after one turn");
        // The first was drafted completely; the second was never claimed.
        assert_eq!(server.drafted.lock().unwrap().len(), 1);
        assert!(q.get("srv-2").unwrap().is_none());
        cleanup(&[&qdir, &repo]);
    }

    #[test]
    fn the_loop_drains_the_server_then_idles() {
        let (q, cfg, qdir, repo) = fixture("drain");
        let server = Scripted::with(vec![item("srv-1"), item("srv-2")]);
        let turns: Mutex<Vec<Turn>> = Mutex::new(Vec::new());

        run_loop(
            &server,
            &Model("# The spec".into()),
            &q,
            &cfg,
            // Stop once an idle turn shows the server has nothing left.
            &|| {
                turns
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|t| matches!(t, Turn::Idle))
            },
            &|t| turns.lock().unwrap().push(t.clone()),
            Duration::from_millis(1),
        );

        assert_eq!(server.drafted.lock().unwrap().len(), 2, "both drafted");
        cleanup(&[&qdir, &repo]);
    }
}
