//! The server's state: requests, drafted specs, and credentials.
//!
//! Everything under **one directory**, so a Portainer user mounts one volume and
//! backs up one thing. State split across several paths is a footgun — the backup
//! that misses one looks like it worked.
//!
//! ```text
//!   /data
//!     ├── requests/<id>.json     one file per request
//!     └── credentials.json       device hashes; never a credential
//! ```
//!
//! A file per request rather than one queue file is what makes concurrent access
//! safe without a lock: the browser listing while the daemon claims touches
//! different files, and a torn write costs the *one* request being written rather
//! than the whole queue.
//!
//! **The server holds text and nothing else.** No repository, no model, no path to
//! either. That is not a policy the handlers enforce — there is simply nothing
//! here that could reach them.

use std::path::{Path, PathBuf};

use sc_daemon::IntakeKind;
use sc_proto::{DcError, Result};
use serde::{Deserialize, Serialize};

use crate::auth::Credentials;

/// Where a request stands, from the server's point of view.
///
/// Deliberately close to the daemon's [`TaskState`](sc_daemon::TaskState) but not
/// the same type: the server observes a *lifecycle*, while the daemon owns a
/// *run*. Sharing one enum would force the server to model states it cannot
/// observe (`Drafting` starts and ends on a machine it never sees).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequestState {
    /// Filed, waiting for a daemon to claim it.
    Queued,
    /// A daemon is drafting. Claimed, so no second daemon takes it.
    Claimed,
    /// A spec came back and is waiting for a human.
    AwaitingReview,
    /// Approved. The spec is settled in the repository; nothing was built.
    Ready,
    /// Dropped before approval.
    Discarded,
    /// A daemon reported it could not be drafted.
    Failed,
}

impl RequestState {
    pub fn label(self) -> &'static str {
        match self {
            RequestState::Queued => "queued",
            RequestState::Claimed => "drafting",
            RequestState::AwaitingReview => "awaiting review",
            RequestState::Ready => "ready",
            RequestState::Discarded => "discarded",
            RequestState::Failed => "failed",
        }
    }

    /// What needs a human first, then what is in flight, then the settled.
    pub fn list_order(self) -> u8 {
        match self {
            RequestState::AwaitingReview => 0,
            RequestState::Claimed => 1,
            RequestState::Queued => 2,
            RequestState::Failed => 3,
            RequestState::Ready => 4,
            RequestState::Discarded => 5,
        }
    }
}

/// One filed request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub text: String,
    /// A repository **name**, never a path. The daemon resolves it against its
    /// own configured set; this server never learns where anything lives.
    pub repo: String,
    pub kind: IntakeKind,
    pub state: RequestState,
    pub filed_ms: u64,
    /// The drafted spec, once one has come back.
    #[serde(default)]
    pub spec: Option<String>,
    /// Where it landed in the repository, for the developer to find the file.
    #[serde(default)]
    pub artifact_dir: Option<String>,
    /// Why it failed, or what a reviewer said when sending it back.
    #[serde(default)]
    pub note: Option<String>,
    /// Set when a reviewer sends a draft back; the next claim carries it so the
    /// redraft grounds on the reason rather than repeating itself.
    #[serde(default)]
    pub send_back_note: Option<String>,
    /// When the current draft came back.
    ///
    /// Recorded by the *server*, on receipt — the wire carries no timestamp, and
    /// a clock the daemon controls is one this server cannot check. Reset on each
    /// redraft, because it describes the spec now on the page rather than the
    /// first attempt.
    ///
    /// `Option` because records written before this field existed have none, and
    /// an upgrade must not make a developer's filed requests unreadable.
    #[serde(default)]
    pub drafted_ms: Option<u64>,
}

impl Request {
    pub fn new(
        id: impl Into<String>,
        text: impl Into<String>,
        repo: impl Into<String>,
        kind: IntakeKind,
    ) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            repo: repo.into(),
            kind,
            state: RequestState::Queued,
            filed_ms: now_ms(),
            spec: None,
            artifact_dir: None,
            note: None,
            send_back_note: None,
            drafted_ms: None,
        }
    }

    /// The first line, for a list.
    pub fn summary(&self) -> &str {
        self.text.lines().next().unwrap_or("").trim()
    }

    /// A digest of the drafted spec, or `None` if there is no draft.
    ///
    /// What an approval is *bound to*. Without this, approving settles whatever
    /// text happens to be on disk when the POST lands — so a redraft arriving
    /// while the reviewer reads is approved on the strength of reading the
    /// previous one.
    pub fn spec_digest(&self) -> Option<String> {
        self.spec.as_deref().map(crate::auth::hash)
    }
}

/// The server's state directory.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Open (or create) the store at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Store> {
        let root = root.into();
        std::fs::create_dir_all(root.join("requests"))?;
        Ok(Store { root })
    }

    fn request_path(&self, id: &str) -> PathBuf {
        self.root.join("requests").join(format!("{id}.json"))
    }

    fn credentials_path(&self) -> PathBuf {
        self.root.join("credentials.json")
    }

    /// Write a request.
    pub fn put(&self, req: &Request) -> Result<()> {
        let json = serde_json::to_string_pretty(req).map_err(|e| DcError::Eval(e.to_string()))?;
        write_atomic(&self.request_path(&req.id), json.as_bytes())
    }

    /// Read one request.
    pub fn get(&self, id: &str) -> Result<Option<Request>> {
        match std::fs::read_to_string(self.request_path(id)) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| DcError::Eval(format!("request {id} is unreadable: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn require(&self, id: &str) -> Result<Request> {
        self.get(id)?
            .ok_or_else(|| DcError::Eval(format!("no request {id:?}")))
    }

    /// Every request, oldest first. An unreadable record is skipped rather than
    /// fatal — one bad file must not hide everything else.
    pub fn all(&self) -> Result<Vec<Request>> {
        let dir = self.root.join("requests");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut out: Vec<Request> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .filter_map(|t| serde_json::from_str::<Request>(&t).ok())
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    /// Claim the oldest queued request for a daemon, atomically enough.
    ///
    /// Serialised **per repository**: a repo with something already `Claimed` is
    /// skipped, so two daemons — or one daemon restarted mid-draft — do not both
    /// work the same tree. Skipping rather than stopping means a free repo's work
    /// does not wait behind a busy one.
    pub fn claim_next(&self) -> Result<Option<Request>> {
        let all = self.all()?;
        let busy: Vec<String> = all
            .iter()
            .filter(|r| r.state == RequestState::Claimed)
            .map(|r| r.repo.clone())
            .collect();
        let Some(mut next) = all
            .into_iter()
            .find(|r| r.state == RequestState::Queued && !busy.contains(&r.repo))
        else {
            return Ok(None);
        };
        next.state = RequestState::Claimed;
        self.put(&next)?;
        Ok(Some(next))
    }

    /// Record a drafted spec.
    pub fn record_drafted(&self, id: &str, spec: &str, artifact_dir: &str) -> Result<Request> {
        let mut req = self.require(id)?;
        req.state = RequestState::AwaitingReview;
        req.spec = Some(spec.to_string());
        req.artifact_dir = Some(artifact_dir.to_string());
        req.drafted_ms = Some(now_ms());
        // The note is spent: it grounded the redraft that just happened.
        req.send_back_note = None;
        req.note = None;
        self.put(&req)?;
        Ok(req)
    }

    /// Record that a daemon could not draft it.
    pub fn record_failed(&self, id: &str, reason: &str) -> Result<Request> {
        let mut req = self.require(id)?;
        req.state = RequestState::Failed;
        req.note = Some(reason.to_string());
        self.put(&req)?;
        Ok(req)
    }

    /// Approve a drafted spec, **bound to the exact text the reviewer was shown**.
    ///
    /// The spec is already settled in the repository by the daemon; this records
    /// the decision. **It starts nothing** — `Ready` means the developer picks it
    /// up in their IDE when they choose.
    ///
    /// `expected_digest` is [`Request::spec_digest`] as of the page the reviewer
    /// read. It is **required, not optional**: without it an approval means "settle
    /// whatever is on disk when this POST lands", and a redraft arriving while the
    /// reviewer reads on a train would be approved on the strength of reading the
    /// previous one. Consent has to attach to bytes, not to an id.
    ///
    /// The check lives here rather than in the route so the CLI and desktop gates
    /// inherit the same guarantee rather than each re-deriving it.
    pub fn approve(&self, id: &str, expected_digest: &str) -> Result<Request> {
        let mut req = self.require(id)?;
        if req.state != RequestState::AwaitingReview {
            return Err(DcError::Eval(format!(
                "request {id} is {} — only one awaiting review can be approved",
                req.state.label()
            )));
        }
        let current = req
            .spec_digest()
            .ok_or_else(|| DcError::Eval(format!("request {id} has no drafted spec to approve")))?;
        if current != expected_digest {
            return Err(DcError::Eval(
                "the spec changed after you opened it — a redraft arrived while \
                 you were reading. Read it again before approving; approving now \
                 would sign off text you have not seen."
                    .to_string(),
            ));
        }
        req.state = RequestState::Ready;
        self.put(&req)?;
        Ok(req)
    }

    /// Send a drafted spec back to be redrafted, with a reason.
    pub fn send_back(&self, id: &str, notes: &str) -> Result<Request> {
        let mut req = self.require(id)?;
        if req.state != RequestState::AwaitingReview {
            return Err(DcError::Eval(format!(
                "request {id} is {} — only one awaiting review can be sent back",
                req.state.label()
            )));
        }
        if notes.trim().is_empty() {
            return Err(DcError::Eval(
                "a send-back needs a note saying what to change — without one the \
                 redraft has nothing to go on and will likely produce the same spec"
                    .to_string(),
            ));
        }
        req.state = RequestState::Queued;
        req.send_back_note = Some(notes.trim().to_string());
        req.note = Some(format!("sent back: {}", notes.trim()));
        // The old draft is dropped: the developer rejected it, so showing it
        // again while the redraft is pending would be showing a dead artifact.
        req.spec = None;
        self.put(&req)?;
        Ok(req)
    }

    /// Drop a request **before** it is approved.
    ///
    /// Refuses a settled one. `Ready` means a human read the spec and signed it
    /// off, and it is in the repository — letting a stray tap flip that to
    /// `Discarded` would erase a recorded decision and make the surface disagree
    /// with the working tree. Spec 09's table has no such transition.
    ///
    /// Re-discarding something already discarded is allowed and does nothing: it
    /// is the state the caller asked for, and erroring would be pedantry.
    pub fn discard(&self, id: &str) -> Result<Request> {
        let mut req = self.require(id)?;
        if matches!(req.state, RequestState::Ready) {
            return Err(DcError::Eval(format!(
                "request {id} was approved — its spec is settled in the \
                 repository. Discarding here would erase the decision without \
                 touching the file. Delete the spec in your IDE instead."
            )));
        }
        req.state = RequestState::Discarded;
        self.put(&req)?;
        Ok(req)
    }

    /// Read the credential store.
    pub fn credentials(&self) -> Result<Credentials> {
        match std::fs::read_to_string(self.credentials_path()) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| DcError::Eval(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Credentials::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Write the credential store.
    pub fn put_credentials(&self, creds: &Credentials) -> Result<()> {
        let json = serde_json::to_string_pretty(creds).map_err(|e| DcError::Eval(e.to_string()))?;
        write_atomic(&self.credentials_path(), json.as_bytes())
    }
}

/// Unix milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A fresh request id: time-ordered, with a counter so two in one millisecond
/// differ.
pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    format!("{:013}-{:04}", now_ms(), n % 10_000)
}

/// Write so a reader never sees a partial file. A truncated request record is a
/// request the developer filed and lost.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sc-server-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn store(tag: &str) -> (Store, PathBuf) {
        let dir = temp(tag);
        (Store::open(&dir).unwrap(), dir)
    }

    fn file(s: &Store, id: &str, repo: &str) -> Request {
        let r = Request::new(id, format!("request {id}"), repo, IntakeKind::Feature);
        s.put(&r).unwrap();
        r
    }

    #[test]
    fn all_state_lives_under_one_directory() {
        // One volume to mount, one thing to back up. State scattered across
        // paths is a footgun — the backup that misses one looks like it worked.
        let (s, dir) = store("one-volume");
        file(&s, "r-1", "alpha");
        let mut creds = Credentials::default();
        creds.set_enrol_code("A");
        s.put_credentials(&creds).unwrap();

        assert!(dir.join("requests").join("r-1.json").is_file());
        assert!(dir.join("credentials.json").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_request_survives_a_restart() {
        let (s, dir) = store("durable");
        file(&s, "r-1", "alpha");
        drop(s);

        let reopened = Store::open(&dir).unwrap();
        assert_eq!(reopened.all().unwrap().len(), 1);
        assert_eq!(reopened.require("r-1").unwrap().state, RequestState::Queued);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claiming_takes_the_oldest_and_marks_it_so_nobody_else_does() {
        let (s, dir) = store("claim");
        file(&s, "r-1", "alpha");
        file(&s, "r-2", "alpha");

        let claimed = s.claim_next().unwrap().unwrap();
        assert_eq!(claimed.id, "r-1");
        assert_eq!(claimed.state, RequestState::Claimed);
        // The repo is now busy, so the second waits.
        assert!(s.claim_next().unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_busy_repository_does_not_block_a_free_one() {
        // Work for a free repo must not wait behind work for a busy one.
        let (s, dir) = store("per-repo");
        file(&s, "r-1", "alpha");
        file(&s, "r-2", "alpha");
        file(&s, "r-3", "beta");

        assert_eq!(s.claim_next().unwrap().unwrap().id, "r-1");
        assert_eq!(
            s.claim_next().unwrap().unwrap().id,
            "r-3",
            "alpha is busy, beta is free"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_drafted_spec_comes_back_and_waits_for_a_human() {
        let (s, dir) = store("drafted");
        file(&s, "r-1", "alpha");
        s.claim_next().unwrap();

        let req = s
            .record_drafted("r-1", "# The spec", "specs/the-thing")
            .unwrap();
        assert_eq!(req.state, RequestState::AwaitingReview);
        assert_eq!(req.spec.as_deref(), Some("# The spec"));
        assert_eq!(req.artifact_dir.as_deref(), Some("specs/the-thing"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn approving_settles_the_request_and_starts_nothing() {
        // `Ready` is not "done": nothing was built, and the developer picks it
        // up in their IDE when they choose.
        let (s, dir) = store("approve");
        file(&s, "r-1", "alpha");
        s.claim_next().unwrap();
        s.record_drafted("r-1", "# The spec", "specs/x").unwrap();

        let digest = s.require("r-1").unwrap().spec_digest().unwrap();
        let req = s.approve("r-1", &digest).unwrap();
        assert_eq!(req.state, RequestState::Ready);
        // The spec is still readable after approval — it is the record of what
        // was agreed.
        assert_eq!(req.spec.as_deref(), Some("# The spec"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_approval_of_text_that_has_since_changed_is_refused() {
        // The reviewer opens v1 on a train; a redraft lands while they read.
        // Approving must not settle v2 on the strength of having read v1 —
        // consent attaches to bytes, not to an id.
        let (s, dir) = store("stale-digest");
        file(&s, "r-1", "alpha");
        s.claim_next().unwrap();
        s.record_drafted("r-1", "# Version one", "specs/x").unwrap();
        let read_this = s.require("r-1").unwrap().spec_digest().unwrap();

        // The daemon pushes a redraft under the reviewer.
        s.record_drafted("r-1", "# Version two", "specs/x").unwrap();

        let err = s
            .approve("r-1", &read_this)
            .expect_err("the text changed")
            .to_string();
        assert!(err.contains("changed after you opened it"), "{err}");
        assert!(err.contains("have not seen"), "{err}");
        // And it is left reviewable rather than half-decided.
        assert_eq!(
            s.require("r-1").unwrap().state,
            RequestState::AwaitingReview
        );
        // Reading the new one and approving that works.
        let now = s.require("r-1").unwrap().spec_digest().unwrap();
        assert_eq!(s.approve("r-1", &now).unwrap().state, RequestState::Ready);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_digest_is_over_the_spec_text_so_an_identical_redraft_still_approves() {
        // The binding is to *content*, not to a draft attempt. A redraft that
        // produces byte-identical text is the same artifact, and refusing it
        // would make the reviewer re-read something that did not change.
        let (s, dir) = store("same-digest");
        file(&s, "r-1", "alpha");
        s.claim_next().unwrap();
        s.record_drafted("r-1", "# The spec", "specs/x").unwrap();
        let digest = s.require("r-1").unwrap().spec_digest().unwrap();

        s.record_drafted("r-1", "# The spec", "specs/x").unwrap();
        assert!(s.approve("r-1", &digest).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_request_awaiting_review_can_be_approved_or_sent_back() {
        // Approving a queued request would sign off a spec that does not exist.
        let (s, dir) = store("guards");
        file(&s, "r-1", "alpha");
        assert!(s.approve("r-1", "any-digest").is_err());
        assert!(s.send_back("r-1", "change it").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_draft_is_timestamped_on_receipt_and_restamped_on_redraft() {
        // The server timestamps rather than trusting the wire: a clock the daemon
        // controls is one this server cannot check. And it describes the spec now
        // on the page, not the first attempt.
        let (s, dir) = store("drafted-ms");
        file(&s, "r-1", "alpha");
        assert!(s.require("r-1").unwrap().drafted_ms.is_none());

        s.claim_next().unwrap();
        let first = s
            .record_drafted("r-1", "# v1", "specs/x")
            .unwrap()
            .drafted_ms
            .expect("stamped on receipt");
        s.send_back("r-1", "more detail").unwrap();
        s.claim_next().unwrap();
        let second = s
            .record_drafted("r-1", "# v2", "specs/x")
            .unwrap()
            .drafted_ms
            .expect("restamped");
        assert!(second >= first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_record_written_before_drafted_ms_existed_still_loads() {
        // The data volume outlives any one image tag; an upgrade must not make
        // a developer's filed requests unreadable.
        let json = r#"{"id":"r-1","text":"t","repo":"alpha","kind":"bug",
                       "state":"queued","filed_ms":1}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        assert!(req.drafted_ms.is_none());
        assert!(req.spec_digest().is_none(), "no spec, no digest");
    }

    #[test]
    fn sending_back_requeues_with_the_note_and_drops_the_rejected_draft() {
        // The note grounds the redraft; showing the rejected draft meanwhile
        // would be showing a dead artifact.
        let (s, dir) = store("send-back");
        file(&s, "r-1", "alpha");
        s.claim_next().unwrap();
        s.record_drafted("r-1", "# Too vague", "specs/x").unwrap();

        let req = s.send_back("r-1", "name the actual roles").unwrap();
        assert_eq!(req.state, RequestState::Queued);
        assert_eq!(req.send_back_note.as_deref(), Some("name the actual roles"));
        assert!(req.spec.is_none(), "the rejected draft is dropped");

        // And it is claimable again, carrying its note.
        let again = s.claim_next().unwrap().unwrap();
        assert_eq!(
            again.send_back_note.as_deref(),
            Some("name the actual roles")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_send_back_needs_a_note() {
        let (s, dir) = store("send-back-empty");
        file(&s, "r-1", "alpha");
        s.claim_next().unwrap();
        s.record_drafted("r-1", "# Spec", "specs/x").unwrap();
        assert!(s.send_back("r-1", "  ").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_redraft_clears_the_spent_note() {
        // It grounded the redraft that just happened; carrying it forward would
        // ground the *next* one too, on feedback already acted upon.
        let (s, dir) = store("note-spent");
        file(&s, "r-1", "alpha");
        s.claim_next().unwrap();
        s.record_drafted("r-1", "# v1", "specs/x").unwrap();
        s.send_back("r-1", "more detail").unwrap();
        s.claim_next().unwrap();

        let req = s.record_drafted("r-1", "# v2", "specs/x").unwrap();
        assert!(req.send_back_note.is_none(), "the note is spent");
        assert_eq!(req.spec.as_deref(), Some("# v2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_approved_request_cannot_be_discarded_out_from_under_its_spec() {
        // `Ready` means a human read it and signed it off, and the spec is in
        // the repository. A stray tap flipping that to `Discarded` would erase a
        // recorded decision without touching the file, leaving the surface
        // disagreeing with the working tree. Spec 09 has no such transition.
        let (s, dir) = store("discard-ready");
        file(&s, "r-1", "alpha");
        s.claim_next().unwrap();
        s.record_drafted("r-1", "# The spec", "specs/x").unwrap();
        let digest = s.require("r-1").unwrap().spec_digest().unwrap();
        s.approve("r-1", &digest).unwrap();

        let err = s.discard("r-1").expect_err("already settled").to_string();
        assert!(err.contains("was approved"), "{err}");
        assert_eq!(s.require("r-1").unwrap().state, RequestState::Ready);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn anything_not_yet_approved_can_be_discarded() {
        // Including a failed one — dropping a request that could not be drafted
        // is exactly what the developer wants, and it signs nothing off.
        let (s, dir) = store("discard-ok");
        for (id, prepare) in [("r-1", false), ("r-2", true)] {
            file(&s, id, id); // one repo each, so neither blocks the other
            if prepare {
                s.claim_next().unwrap();
                s.record_drafted(id, "# Spec", "specs/x").unwrap();
            }
            assert_eq!(
                s.discard(id).unwrap().state,
                RequestState::Discarded,
                "{id}"
            );
        }
        // And discarding twice is not an error: it is already the asked-for state.
        assert!(s.discard("r-1").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failure_is_recorded_with_its_reason_and_never_reclaimed() {
        // Spec 19: a failed run stays failed and visible.
        let (s, dir) = store("failed");
        file(&s, "r-1", "alpha");
        s.claim_next().unwrap();

        let req = s
            .record_failed("r-1", "the backend was unreachable")
            .unwrap();
        assert_eq!(req.state, RequestState::Failed);
        assert_eq!(req.note.as_deref(), Some("the backend was unreachable"));
        assert!(s.claim_next().unwrap().is_none(), "not picked back up");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_server_stores_no_path_to_anything() {
        // It holds text. There is no repository path in the record, so there is
        // nothing here that could grow into a filesystem reach.
        let (s, dir) = store("no-paths");
        let r = file(&s, "r-1", "alpha");
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"repo\":\"alpha\""));
        assert!(!json.contains("\"path\""), "{json}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_unreadable_record_does_not_hide_the_others() {
        let (s, dir) = store("corrupt");
        file(&s, "r-1", "alpha");
        std::fs::write(dir.join("requests").join("r-2.json"), "{ truncated").unwrap();
        assert_eq!(s.all().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn credentials_round_trip_and_default_when_absent() {
        let (s, dir) = store("creds");
        assert_eq!(s.credentials().unwrap(), Credentials::default());

        let mut creds = Credentials::default();
        creds.set_enrol_code("ABC-123");
        s.put_credentials(&creds).unwrap();
        assert_eq!(s.credentials().unwrap(), creds);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_list_shows_what_needs_a_human_first() {
        let mut states = [
            RequestState::Ready,
            RequestState::Queued,
            RequestState::AwaitingReview,
            RequestState::Failed,
        ];
        states.sort_by_key(|s| s.list_order());
        assert_eq!(states[0], RequestState::AwaitingReview);
    }
}
