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

use sc_proto::IntakeKind;
use sc_proto::{DcError, Result};
use serde::{Deserialize, Serialize};

use crate::account::{Accounts, Links};

/// How long a claim may stand before the request returns to the queue.
///
/// **Generous on purpose — twenty minutes.** The two failures are not symmetric:
///
/// | too short | too long |
/// |---|---|
/// | a live draft is reclaimed and **two daemons work the same tree** | a dead daemon's repository stays blocked a while longer |
///
/// The left column is the one that corrupts something, so the timeout sits well
/// above any plausible drafting run rather than close to it. A drafting run is a
/// model call against a real repository — usually a minute or two, occasionally
/// much longer on a slow model or a large tree — and the cost of waiting is only
/// latency on a repo whose daemon has already died.
///
/// Not configurable. An operator tuning this down to "make things snappier" is
/// choosing duplicate work without the trade being visible to them.
pub const CLAIM_TIMEOUT_MS: u64 = 20 * 60 * 1000;

/// Shortening the timeout past a plausible drafting run fails the **build**.
///
/// Beside the constant rather than inside `mod tests`: a `const` assertion in a
/// `#[cfg(test)]` module is only evaluated when that module is compiled, so
/// `cargo check` and `cargo build` sail straight past it. Found by shortening
/// the constant to 30s and watching it *not* fire — an inert guard is worse than
/// none, because it reads as protection that is not there.
const _: () = assert!(
    CLAIM_TIMEOUT_MS >= 10 * 60 * 1000,
    "a drafting run is a model call against a real repository, so the claim \
     timeout must stay well clear of one"
);

/// Where a request stands, from the server's point of view.
///
/// Deliberately close to the daemon's own `TaskState` but not the same type: the
/// server observes a *lifecycle*, while the daemon owns a *run*. Sharing one enum
/// would force the server to model states it cannot observe (`Drafting` starts
/// and ends on a machine it never sees) — and would drag the daemon's crate into
/// this one, which is the dependency the public server most needs not to have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequestState {
    /// Filed publicly, waiting to be screened. **Not claimable.**
    ///
    /// Screening runs on a background sweep rather than inline on the filing
    /// request: the server is thread-per-request, so a hung third-party call on
    /// the request path converts directly into thread exhaustion, and the filer
    /// would be left waiting on someone else's API.
    Screening,
    /// Filed, waiting for a daemon to claim it.
    Queued,
    /// A daemon is drafting. Claimed, so no second daemon takes it.
    ///
    /// **Expires.** See [`CLAIM_TIMEOUT_MS`]: a daemon that dies mid-draft would
    /// otherwise hold its repository for ever, because [`Store::claim_next`]
    /// skips any repo with something already claimed.
    Claimed,
    /// A spec came back and is waiting for a human.
    AwaitingReview,
    /// Accepted: read, judged good, and done with here.
    ///
    /// **Nothing was built, and nothing will be by this server.** The spec was
    /// already written into the repository when it was drafted; accepting marks
    /// it settled so it leaves the review list. Building it means opening the
    /// IDE and running the pipeline, on the machine that holds the code.
    ///
    /// `alias = "ready"` because records already on the volume were written
    /// under the older name. Read-compatible, write-forward — no migration pass,
    /// because a migration that runs at startup is one that can fail at startup,
    /// on the one directory the developer cannot lose.
    #[serde(alias = "ready")]
    Accepted,
    /// The screener judged it spam. **Not claimable** — but kept, visible, and
    /// releasable in one click.
    ///
    /// Quarantine rather than deletion is the whole point: a model's opinion may
    /// *withhold* work from the queue, and a human decides whether that was
    /// right. Silently dropping a filing would make the screener the final word
    /// on admission, which is exactly what it must not be.
    Quarantined,
    /// Dropped before approval.
    Discarded,
    /// A daemon reported it could not be drafted.
    Failed,
}

impl RequestState {
    pub fn label(self) -> &'static str {
        match self {
            RequestState::Screening => "screening",
            RequestState::Queued => "queued",
            RequestState::Claimed => "drafting",
            RequestState::AwaitingReview => "awaiting review",
            RequestState::Accepted => "ready",
            RequestState::Quarantined => "quarantined",
            RequestState::Discarded => "discarded",
            RequestState::Failed => "failed",
        }
    }

    /// What needs a human first, then what is in flight, then the settled.
    ///
    /// `Quarantined` sits above `Failed` because it is the one the developer
    /// actually needs to look at — a wrongly-quarantined request is invisible
    /// work, and the whole reason quarantine is not deletion.
    pub fn list_order(self) -> u8 {
        match self {
            RequestState::AwaitingReview => 0,
            RequestState::Claimed => 1,
            RequestState::Queued => 2,
            RequestState::Screening => 3,
            RequestState::Quarantined => 4,
            RequestState::Failed => 5,
            RequestState::Accepted => 6,
            RequestState::Discarded => 7,
        }
    }

    /// Can a daemon claim work in this state?
    ///
    /// **`Queued` and nothing else.** Stated here as a named guarantee rather
    /// than left as an accident of [`Store::claim_next`] happening to filter on
    /// an allowlist — it is what makes "nothing unscreened reaches the
    /// developer's machine" structural.
    pub fn is_claimable(self) -> bool {
        matches!(self, RequestState::Queued)
    }
}

/// Which repositories a daemon will accept work for.
///
/// A type rather than a `&[String]`, because the two cases mean opposite things
/// and an empty slice is exactly what a caller that *forgot* to fill one in
/// produces. [`Anything`](Serves::Anything) can only be written on purpose; a
/// declared-but-empty set is a daemon serving nothing, which claims nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Serves<'a> {
    /// The daemon named no repositories — an older build, which does not know
    /// how to. It gets everything, which is exactly the behaviour before this
    /// existed, so an un-upgraded daemon keeps working.
    Anything,
    /// The daemon named the repositories it has a working tree for. Only those
    /// are handed over.
    These(&'a [String]),
}

impl Serves<'_> {
    /// Would this daemon take work for `repo`?
    ///
    /// Exact and case-sensitive, matching how a daemon resolves a name against
    /// its own configured set: a fuzzy match here would reintroduce the
    /// ambiguity a closed set of names exists to remove.
    pub fn accepts(&self, repo: &str) -> bool {
        match self {
            Serves::Anything => true,
            Serves::These(names) => names.iter().any(|n| n == repo),
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
    /// Which public account filed this, if it came from the public surface.
    ///
    /// `None` means the developer filed it from an enrolled device. This is what
    /// a filer's "my requests" page keys on — never the request id, which is
    /// time-ordered and enumerable in seconds.
    #[serde(default)]
    pub account_id: Option<String>,
    /// When the current claim was taken, for [`CLAIM_TIMEOUT_MS`].
    ///
    /// The **server's** clock, like `drafted_ms` and for the same reason: the
    /// wire carries no timestamp, and a clock the daemon controls is one this
    /// server cannot check — a daemon reporting a fresh claim forever would hold
    /// a repository indefinitely, which is the failure this field exists to end.
    ///
    /// `Option` because records written before this field existed have none.
    /// A `Claimed` request without one is treated as **claimed just now** rather
    /// than as infinitely stale: on upgrade the alternative would reclaim every
    /// in-flight draft at once, and duplicating live work is worse than waiting
    /// one more timeout for a claim that was already stuck.
    #[serde(default)]
    pub claimed_ms: Option<u64>,
    /// Which daemon holds the current claim — the operator's **label** for that
    /// machine, never its key.
    ///
    /// Guarding a late report on state alone leaves a real window: a daemon
    /// whose claim expired, whose work a *second* daemon has since claimed but
    /// not yet finished, finds the request still `Claimed` and its stale draft
    /// is accepted on top of one in progress. Reclaiming work is only safe if
    /// the daemon it was taken from cannot still write, and "still `Claimed`" is
    /// not that guarantee — "still claimed *by you*" is.
    ///
    /// `Option` for the same two reasons as `claimed_ms`: records written before
    /// this field existed have none, and it is dropped alongside it for anything
    /// not `Claimed`, so a stale holder is as unrepresentable as a stale stamp.
    #[serde(default)]
    pub claimed_by: Option<String>,
    /// When this request was handed to a daemon, one entry per hand-off.
    ///
    /// **What a drafting run actually costs is counted here**, because nothing
    /// else records it: a redrafted request looks identical to a first draft,
    /// and `filed_ms` says only when it arrived. Every re-admission — a
    /// send-back, a release — buys another full run against the same record, and
    /// without this there is nothing to count.
    ///
    /// Trimmed to [`crate::config::FILING_WINDOW_MS`] on write, so the vector is
    /// bounded by the cap rather than by the record's age: a request redrafted
    /// daily for a year would otherwise carry 365 entries in a file the review
    /// page reads on every render.
    #[serde(default)]
    pub drafts: Vec<u64>,
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
            account_id: None,
            claimed_ms: None,
            claimed_by: None,
            drafts: Vec::new(),
        }
    }

    /// A request filed from the public surface by a signed-in account.
    ///
    /// A **separate constructor**, not a `state` argument on [`Request::new`]:
    /// an argument is one an edit eventually passes wrongly, whereas this cannot
    /// produce a claimable record at all. Public filings start in
    /// [`RequestState::Screening`], so nothing unscreened can reach the
    /// developer's machine even if every later check were removed.
    /// `now_ms` is **required**, not read from the wall clock.
    ///
    /// The filing cap compares `filed_ms` against the handler's own clock, and a
    /// record stamped from a different source is one the window can never quite
    /// line up with. Taking it as an argument rather than offering a
    /// `.at(now_ms)` afterwards is the same reasoning that makes this a separate
    /// constructor at all: a later filing path can forget to chain a mutator,
    /// and would then silently escape the window. It cannot forget an argument.
    pub fn public(
        id: impl Into<String>,
        text: impl Into<String>,
        repo: impl Into<String>,
        kind: IntakeKind,
        account_id: &str,
        screened: bool,
        now_ms: u64,
    ) -> Self {
        Self {
            // When screening is switched off there is nothing to wait for, and a
            // request parked in `Screening` forever would be worse than one
            // queued honestly.
            state: if screened {
                RequestState::Screening
            } else {
                RequestState::Queued
            },
            account_id: Some(account_id.to_string()),
            filed_ms: now_ms,
            ..Request::new(id, text, repo, kind)
        }
    }

    /// Was this filed by `account_id`?
    ///
    /// `false` for a request the developer filed from an enrolled device, which
    /// carries no account. That is deliberate: the spend ceilings bound what
    /// *strangers* can spend of the developer's budget, not what the developer
    /// spends of their own.
    pub fn filed_by(&self, account_id: &str) -> bool {
        self.account_id.as_deref() == Some(account_id)
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

    /// Public, because [`AccountsCache`](crate::account::AccountsCache) stats
    /// this file on every request and reads it only when it has changed.
    pub fn accounts_path(&self) -> PathBuf {
        self.root.join("accounts.json")
    }

    fn links_path(&self) -> PathBuf {
        self.root.join("links.json")
    }

    fn admin_path(&self) -> PathBuf {
        self.root.join("admin.json")
    }

    /// Public, because [`SettingsCache`](crate::settings::SettingsCache) stats
    /// this file on every request and reads it only when it has changed.
    pub fn settings_path(&self) -> PathBuf {
        self.root.join("settings.json")
    }

    /// Where the roster lives.
    ///
    /// Public, unlike its siblings, because [`RosterCache`](crate::roster::RosterCache)
    /// stats this file on every identification and reads it only when it has
    /// changed. That is the whole mechanism by which revocation takes effect on
    /// the next request, and it needs the path rather than the contents.
    pub fn roster_path(&self) -> PathBuf {
        self.root.join("owners.json")
    }

    /// Write a request.
    ///
    /// **Drops `claimed_ms` on anything not `Claimed`.** There are ten places a
    /// request leaves that state, and "remember to clear the stamp" at each is
    /// the kind of rule that holds until the eleventh is added. Enforcing it at
    /// the one write everything funnels through makes a stale stamp
    /// unrepresentable on disk rather than merely unlikely.
    pub fn put(&self, req: &Request) -> Result<()> {
        let stale = req.state != RequestState::Claimed
            && (req.claimed_ms.is_some() || req.claimed_by.is_some());
        let json = if stale {
            // Both together: a holder left behind on something not `Claimed`
            // says a machine is working on it when none is, which is exactly as
            // misleading as a stale timestamp.
            let mut cleaned = req.clone();
            cleaned.claimed_ms = None;
            cleaned.claimed_by = None;
            serde_json::to_string_pretty(&cleaned)
        } else {
            serde_json::to_string_pretty(req)
        }
        .map_err(|e| DcError::Eval(e.to_string()))?;
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
    ///
    /// **Stale claims are returned to the queue first.** Because a busy repo is
    /// skipped, a daemon that dies mid-draft would otherwise hold its repository
    /// for ever: nothing else for that repo could ever be claimed, and no error
    /// would be reported anywhere. See [`CLAIM_TIMEOUT_MS`].
    pub fn claim_next(&self, serves: Serves<'_>, by: &str) -> Result<Option<Request>> {
        self.reclaim_stale(now_ms())?;

        let all = self.all()?;
        let busy: Vec<String> = all
            .iter()
            .filter(|r| r.state == RequestState::Claimed)
            .map(|r| r.repo.clone())
            .collect();
        // `is_claimable` **first**, and rather than `== Queued`, so the guarantee
        // lives in one named place and reads as the leading condition: a state
        // added later is excluded unless someone deliberately opts it in, and a
        // later edit cannot make the served-repo filter the deciding predicate.
        let Some(mut next) = all
            .into_iter()
            .find(|r| r.state.is_claimable() && serves.accepts(&r.repo) && !busy.contains(&r.repo))
        else {
            return Ok(None);
        };
        let now = now_ms();
        next.state = RequestState::Claimed;
        next.claimed_ms = Some(now);
        next.claimed_by = Some(by.to_string());
        // **The one place a drafting run begins**, so the one place it is
        // counted. Recorded here rather than at the verbs that re-admit work,
        // because those are several and this is one — and a verb added later
        // that reaches `Queued` is counted without anyone remembering to.
        next.drafts.push(now);
        next.drafts
            .retain(|t| now.saturating_sub(*t) <= crate::config::FILING_WINDOW_MS);
        self.put(&next)?;
        Ok(Some(next))
    }

    /// Return claims older than [`CLAIM_TIMEOUT_MS`] to the queue.
    ///
    /// Run from [`claim_next`](Self::claim_next) rather than from a background
    /// thread. A stale claim has no consequence until somebody asks for work, so
    /// checking at that moment costs one scan on a request that already scans,
    /// needs no second thread, and leaves no window in which a sweep and a claim
    /// disagree about who holds a repository.
    ///
    /// A reclaimed request keeps its `send_back_note` — it was never drafted, so
    /// the reason it was sent back still applies to the next attempt.
    ///
    /// `now_ms` is a parameter so a test can age a claim without sleeping.
    pub fn reclaim_stale(&self, now_ms: u64) -> Result<usize> {
        let mut reclaimed = 0;
        for mut req in self.all()? {
            if req.state != RequestState::Claimed {
                continue;
            }
            // A record from before this field existed is treated as claimed now:
            // stamping it starts the clock rather than expiring everything
            // in-flight the moment the server is upgraded.
            let Some(claimed_ms) = req.claimed_ms else {
                req.claimed_ms = Some(now_ms);
                self.put(&req)?;
                continue;
            };
            // `saturating_sub`, so a clock that jumped backwards cannot make a
            // fresh claim look ancient and reclaim work that is genuinely in
            // progress — duplicating a live draft is the harm here.
            if now_ms.saturating_sub(claimed_ms) < CLAIM_TIMEOUT_MS {
                continue;
            }
            req.state = RequestState::Queued;
            req.claimed_ms = None;
            req.claimed_by = None;
            // Said plainly, because the developer's page shows it and "why did
            // this go back to queued" is otherwise unanswerable.
            req.note = Some(
                "the daemon that claimed this stopped responding, so it went back to the queue"
                    .to_string(),
            );
            self.put(&req)?;
            reclaimed += 1;
        }
        Ok(reclaimed)
    }

    /// Record a drafted spec.
    ///
    /// **Refused unless the request is still `Claimed`.** Introducing a claim
    /// timeout introduces a daemon that comes back after its claim expired, and
    /// without this check its late report would overwrite whatever happened
    /// since — a spec another daemon has since drafted, a decision a reviewer
    /// has since made. Reclaiming stale work is only safe if the work it
    /// reclaimed cannot still write.
    pub fn record_drafted(
        &self,
        id: &str,
        by: &str,
        spec: &str,
        artifact_dir: &str,
    ) -> Result<Request> {
        let mut req = self.claimed(id, by)?;
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
    ///
    /// Same guard as [`record_drafted`](Self::record_drafted): a late failure
    /// report must not mark something `Failed` that has already been reclaimed
    /// and successfully drafted by somebody else.
    pub fn record_failed(&self, id: &str, by: &str, reason: &str) -> Result<Request> {
        let mut req = self.claimed(id, by)?;
        req.state = RequestState::Failed;
        req.note = Some(reason.to_string());
        self.put(&req)?;
        Ok(req)
    }

    /// Return claimed work to the queue, undone.
    ///
    /// **Requeues rather than fails**, which is the whole point: a failure is a
    /// statement about the request and is terminal, where this says only that
    /// *this machine* could not do it. Collapsing the two is how a daemon
    /// destroys work it merely could not reach.
    ///
    /// Keeps `send_back_note` for the same reason [`reclaim_stale`] does —
    /// nothing was drafted, so a reviewer's reason for rejecting still applies
    /// to whoever picks it up next.
    ///
    /// The note names who gave it back. Without that, a request that has bounced
    /// looks exactly like one nobody ever claimed, and the misconfiguration
    /// causing it stays invisible.
    ///
    /// There is deliberately **no bounce counter**. It would have to be
    /// persisted and decayed, and the thing it would eventually do is mark the
    /// request `Failed` — the terminal outcome this route exists to avoid. The
    /// cases that could loop each have a better answer: a configuration that
    /// changed is self-healing, because the daemon stops declaring that
    /// repository on its next poll; a path that is not a repository is withdrawn
    /// by the daemon rather than re-offered; and a daemon releasing in a tight
    /// loop is bounded by its own rate budget, which is per-machine.
    ///
    /// [`reclaim_stale`]: Self::reclaim_stale
    pub fn record_released(&self, id: &str, by: &str, reason: &str) -> Result<Request> {
        let mut req = self.claimed(id, by)?;
        req.state = RequestState::Queued;
        // Cleared by `put` for anything not `Claimed`, but set here too so the
        // value returned to the caller matches what landed on disk.
        req.claimed_ms = None;
        req.claimed_by = None;
        req.note = Some(format!("{by} handed this back: {reason}"));
        self.put(&req)?;
        Ok(req)
    }

    /// Load a request, refusing it unless `by` currently holds the claim.
    ///
    /// **State and holder, not state alone.** Checking only the state leaves a
    /// real window: a daemon whose claim expired, whose work a *second* daemon
    /// has since claimed but not yet finished, finds the request still
    /// `Claimed` — so a state-only guard passes and its stale draft lands on top
    /// of one being written right now. Reclaiming work is only safe if the
    /// daemon it was taken from cannot still write, and "still `Claimed`" is not
    /// that guarantee.
    ///
    /// A record written before `claimed_by` existed has no holder, and is
    /// treated as matching **any** daemon: on upgrade the alternative would
    /// reject the in-flight report of a draft that began under the previous
    /// build. The window is one claim timeout.
    ///
    /// The error says *reclaimed* rather than "not found", because the daemon on
    /// the other end did real work and the log it writes is the only place that
    /// will ever explain where the work went.
    fn claimed(&self, id: &str, by: &str) -> Result<Request> {
        let req = self.require(id)?;
        if req.state != RequestState::Claimed {
            return Err(DcError::Eval(format!(
                "request {id:?} is {} rather than claimed — its claim was \
                 reclaimed after {} minutes, and this report arrived too late \
                 to be recorded",
                req.state.label(),
                CLAIM_TIMEOUT_MS / 60_000
            )));
        }
        if let Some(holder) = &req.claimed_by {
            if holder != by {
                return Err(DcError::Eval(format!(
                    "request {id:?} is claimed by {holder:?}, not by {by:?} — \
                     this claim was reclaimed after {} minutes and another daemon \
                     took it, so this report arrived too late to be recorded",
                    CLAIM_TIMEOUT_MS / 60_000
                )));
            }
        }
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
    pub fn accept(&self, id: &str, expected_digest: &str) -> Result<Request> {
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
        req.state = RequestState::Accepted;
        self.put(&req)?;
        Ok(req)
    }

    /// How many requests this account filed in the last `window_ms`.
    ///
    /// Counted from the records themselves rather than a separate tally: a
    /// counter is state that can drift from the thing it counts, and a filer who
    /// discovers the drift is the one who benefits from it. Every state counts,
    /// including `Discarded` and `Quarantined` — the cost being capped is the
    /// *filing*, and letting a discarded request free up budget would make
    /// file-then-discard a way around the limit.
    ///
    /// A rolling window rather than calendar days: "resets at midnight" invites
    /// waiting for midnight, and midnight in whose timezone is a question with
    /// no good answer on a server that holds no locale.
    pub fn filed_since(&self, account_id: &str, since_ms: u64) -> Result<usize> {
        Ok(self
            .all()?
            .iter()
            .filter(|r| r.filed_by(account_id) && r.filed_ms >= since_ms)
            .count())
    }

    /// How many drafting runs `repo` has been sent into since `since_ms`.
    ///
    /// Counted from the records, like [`filed_since`](Self::filed_since) and for
    /// the same reason: a separate tally is state that drifts from the thing it
    /// counts, and the records are already read on this path.
    ///
    /// **Keyed on the repository, not on who asked for the run.** What is being
    /// spent is drafting runs against a project, which is the number the
    /// developer pays for — and it stays true when a second owner is added,
    /// where a per-person cap would simply double.
    pub fn drafts_since(&self, repo: &str, since_ms: u64) -> Result<usize> {
        Ok(self
            .all()?
            .iter()
            .filter(|r| r.repo == repo)
            .map(|r| r.drafts.iter().filter(|t| **t >= since_ms).count())
            .sum())
    }

    /// Every request still waiting to be screened.
    pub fn pending_screening(&self) -> Result<Vec<Request>> {
        Ok(self
            .all()?
            .into_iter()
            .filter(|r| r.state == RequestState::Screening)
            .collect())
    }

    /// Record a screening verdict: `Screening` → `Queued` or `Quarantined`.
    ///
    /// **This is the only writer of that transition, and the only power the
    /// screener has.** It cannot reach any other state, so a model's verdict can
    /// *withhold* work from the queue but never introduce it — admission stays a
    /// decision made by code.
    ///
    /// Refuses anything not currently `Screening`, so a verdict arriving late —
    /// after a human already released the request by hand — cannot re-quarantine
    /// work someone has already looked at.
    pub fn finish_screening(&self, id: &str, quarantine: Option<&str>) -> Result<Request> {
        let mut req = self.require(id)?;
        if req.state != RequestState::Screening {
            return Err(DcError::Eval(format!(
                "request {id} is {} — only one being screened can take a verdict",
                req.state.label()
            )));
        }
        match quarantine {
            Some(reason) => {
                req.state = RequestState::Quarantined;
                req.note = Some(reason.to_string());
            }
            None => req.state = RequestState::Queued,
        }
        self.put(&req)?;
        Ok(req)
    }

    /// Release a quarantined request into the queue.
    ///
    /// Reachable only from a route the gate restricts to an enrolled device, so
    /// the human overrules the screener rather than the other way round. The
    /// note is kept: a released request should still show *why* it was held, or
    /// the developer cannot tell a false positive from a real one next time.
    pub fn release(&self, id: &str) -> Result<Request> {
        let mut req = self.require(id)?;
        if req.state != RequestState::Quarantined {
            return Err(DcError::Eval(format!(
                "request {id} is {} — only a quarantined one can be released",
                req.state.label()
            )));
        }
        req.state = RequestState::Queued;
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
        if matches!(req.state, RequestState::Accepted) {
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

    /// Read the public account store.
    ///
    /// Read **lazily**, only on paths that need it — unlike
    /// [`credentials`](Store::credentials), which is on the hot path. Accounts
    /// are self-serve and unbounded, so parsing them per request would let a
    /// stranger choose how much work every request does.
    pub fn accounts(&self) -> Result<Accounts> {
        read_json(&self.accounts_path())
    }

    pub fn put_accounts(&self, accounts: &Accounts) -> Result<()> {
        let json =
            serde_json::to_string_pretty(accounts).map_err(|e| DcError::Eval(e.to_string()))?;
        write_atomic(&self.accounts_path(), json.as_bytes())
    }

    /// Read outstanding sign-in links.
    pub fn links(&self) -> Result<Links> {
        read_json(&self.links_path())
    }

    pub fn put_links(&self, links: &Links) -> Result<()> {
        let json = serde_json::to_string_pretty(links).map_err(|e| DcError::Eval(e.to_string()))?;
        write_atomic(&self.links_path(), json.as_bytes())
    }

    /// What this server does — the settings that used to be environment
    /// variables.
    ///
    /// Its own file rather than a field beside the roster: this is written by
    /// the administrator from a settings page, the roster from an owners page,
    /// and sharing a file would put two unrelated edits under one rewrite.
    pub fn settings(&self) -> Result<crate::settings::Settings> {
        read_json(&self.settings_path())
    }

    pub fn put_settings(&self, settings: &crate::settings::Settings) -> Result<()> {
        let json =
            serde_json::to_string_pretty(settings).map_err(|e| DcError::Eval(e.to_string()))?;
        write_atomic(&self.settings_path(), json.as_bytes())
    }

    /// Who administers this server.
    ///
    /// A file of its own rather than a field on the roster, for the reason
    /// `oauth-states.json` has one: the roster is written by the administrator
    /// as routine work, and this is written once at setup and almost never
    /// again. Sharing a file would put a claim and a day's owner edits under the
    /// same rewrite — and this is the file somebody deletes to recover a server,
    /// which must not take the owner list with it.
    pub fn admin(&self) -> Result<crate::admin::Admin> {
        read_json(&self.admin_path())
    }

    pub fn put_admin(&self, admin: &crate::admin::Admin) -> Result<()> {
        let json = serde_json::to_string_pretty(admin).map_err(|e| DcError::Eval(e.to_string()))?;
        write_atomic(&self.admin_path(), json.as_bytes())
    }

    /// Read the roster.
    ///
    /// Callers on the request path should go through
    /// [`RosterCache`](crate::roster::RosterCache) instead — this parses
    /// unconditionally, which is right for a write and wasteful for a read.
    pub fn roster(&self) -> Result<crate::roster::Roster> {
        read_json(&self.roster_path())
    }

    pub fn put_roster(&self, roster: &crate::roster::Roster) -> Result<()> {
        let json =
            serde_json::to_string_pretty(roster).map_err(|e| DcError::Eval(e.to_string()))?;
        write_atomic(&self.roster_path(), json.as_bytes())
    }
}

/// Read a JSON file, treating "not there yet" as the default.
///
/// A missing file is the resting state of a fresh install, not an error — but a
/// *malformed* one is loud, because silently returning an empty set would look
/// identical to having been configured with nothing, and the developer would be
/// left wondering where their accounts went.
fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| DcError::Eval(format!("{} is unreadable: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e.into()),
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
        // Random rather than a timestamp: tests run in parallel, and two
        // starting in the same millisecond would share a directory and delete
        // each other's files.
        let d = std::env::temp_dir().join(format!(
            "sc-server-{tag}-{}-{}",
            std::process::id(),
            &crate::auth::mint_secret()[..12]
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
        let mut admin = s.admin().unwrap();
        admin.claim("jamez667@example.test", 1);
        s.put_admin(&admin).unwrap();

        assert!(dir.join("requests").join("r-1.json").is_file());
        assert!(dir.join("admin.json").is_file());
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

        let claimed = s.claim_next(Serves::Anything, "d-test").unwrap().unwrap();
        assert_eq!(claimed.id, "r-1");
        assert_eq!(claimed.state, RequestState::Claimed);
        // The repo is now busy, so the second waits.
        assert!(s.claim_next(Serves::Anything, "d-test").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_busy_repository_does_not_block_a_free_one() {
        // Work for a free repo must not wait behind work for a busy one.
        let (s, dir) = store("per-repo");
        file(&s, "r-1", "alpha");
        file(&s, "r-2", "alpha");
        file(&s, "r-3", "beta");

        assert_eq!(
            s.claim_next(Serves::Anything, "d-test")
                .unwrap()
                .unwrap()
                .id,
            "r-1"
        );
        assert_eq!(
            s.claim_next(Serves::Anything, "d-test")
                .unwrap()
                .unwrap()
                .id,
            "r-3",
            "alpha is busy, beta is free"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- stale claims ---------------------------------------------------------

    #[test]
    fn a_dead_daemon_does_not_block_its_repository_for_ever() {
        // The failure this exists to end, stated as the symptom rather than the
        // mechanism: `claim_next` skips a repo that has anything claimed, so one
        // abandoned claim means *nothing else for that repo is ever claimable*.
        // Silent, permanent, and reported nowhere.
        let (s, dir) = store("stale-blocks");
        file(&s, "r-1", "alpha");
        file(&s, "r-2", "alpha");

        let claimed = s.claim_next(Serves::Anything, "d-test").unwrap().unwrap();
        assert_eq!(claimed.id, "r-1");
        assert!(claimed.claimed_ms.is_some(), "the claim is stamped");
        // The daemon now dies. Nothing else for alpha can be claimed.
        assert!(s.claim_next(Serves::Anything, "d-test").unwrap().is_none());

        // Age the claim past the timeout.
        let mut stuck = s.require("r-1").unwrap();
        stuck.claimed_ms = Some(now_ms() - CLAIM_TIMEOUT_MS - 1);
        s.put(&stuck).unwrap();

        // The next daemon to ask gets r-1 back — the abandoned one, oldest
        // first — rather than finding the repo permanently wedged.
        let again = s.claim_next(Serves::Anything, "d-test").unwrap().unwrap();
        assert_eq!(again.id, "r-1");
        assert_eq!(again.state, RequestState::Claimed);
        assert!(
            again.note.as_deref().is_some_and(|n| n.contains("queue")),
            "the developer is told why it went back: {:?}",
            again.note
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_claim_inside_the_timeout_is_left_alone() {
        // The expensive mistake is the other one: reclaiming a *live* draft puts
        // two daemons on the same tree.
        let (s, dir) = store("stale-fresh");
        file(&s, "r-1", "alpha");
        let claimed = s.claim_next(Serves::Anything, "d-test").unwrap().unwrap();
        // Measured from the **stamp**, not from `now_ms()`: the claim was taken
        // a moment ago, so "now plus the timeout" is already past it.
        let at = claimed.claimed_ms.unwrap();

        assert_eq!(s.reclaim_stale(at).unwrap(), 0);
        // And one millisecond short of the timeout is still inside it.
        assert_eq!(s.reclaim_stale(at + CLAIM_TIMEOUT_MS - 1).unwrap(), 0);
        assert_eq!(s.require("r-1").unwrap().state, RequestState::Claimed);

        // Exactly at the timeout it goes. Pinned on both sides, because a
        // boundary asserted only from the far side passes just as well when the
        // comparison is wrong by a whole timeout.
        assert_eq!(s.reclaim_stale(at + CLAIM_TIMEOUT_MS).unwrap(), 1);
        assert_eq!(s.require("r-1").unwrap().state, RequestState::Queued);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_backwards_clock_cannot_reclaim_live_work() {
        // `saturating_sub`: a clock that jumped back would otherwise make a
        // fresh claim look ancient, and duplicating a running draft is the harm
        // this whole timeout is careful about.
        let (s, dir) = store("stale-clock");
        file(&s, "r-1", "alpha");
        let claimed = s.claim_next(Serves::Anything, "d-test").unwrap().unwrap();

        let long_before = claimed.claimed_ms.unwrap().saturating_sub(86_400_000);
        assert_eq!(s.reclaim_stale(long_before).unwrap(), 0);
        assert_eq!(s.require("r-1").unwrap().state, RequestState::Claimed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_upgraded_record_starts_its_clock_rather_than_expiring_at_once() {
        // Records written before `claimed_ms` existed have none. Treating that
        // as infinitely stale would reclaim every in-flight draft the moment the
        // server is upgraded, which is the one thing worse than a stuck claim.
        let (s, dir) = store("stale-upgrade");
        file(&s, "r-1", "alpha");
        let mut old = s.require("r-1").unwrap();
        old.state = RequestState::Claimed;
        old.claimed_ms = None;
        s.put(&old).unwrap();

        let now = now_ms();
        assert_eq!(s.reclaim_stale(now).unwrap(), 0, "not reclaimed on sight");
        let stamped = s.require("r-1").unwrap();
        assert_eq!(stamped.state, RequestState::Claimed);
        assert_eq!(stamped.claimed_ms, Some(now), "the clock started instead");

        // And it does expire once the timeout has actually elapsed.
        assert_eq!(s.reclaim_stale(now + CLAIM_TIMEOUT_MS).unwrap(), 1);
        assert_eq!(s.require("r-1").unwrap().state, RequestState::Queued);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaving_the_claimed_state_drops_the_stamp() {
        // Enforced in `put` rather than at each of the ten transitions out of
        // `Claimed`, so a stale stamp is unrepresentable on disk instead of
        // merely unlikely. Checked through the states a claim actually reaches.
        let (s, dir) = store("stale-drop");
        for (id, finish) in [("r-1", true), ("r-2", false)] {
            file(&s, id, id); // one repo each, so both are claimable
            s.claim_next(Serves::Anything, "d-test").unwrap();
            assert!(s.require(id).unwrap().claimed_ms.is_some());

            if finish {
                s.record_drafted(id, "d-test", "# Spec", "specs/x").unwrap();
                assert_eq!(s.require(id).unwrap().state, RequestState::AwaitingReview);
            } else {
                s.record_failed(id, "d-test", "could not draft").unwrap();
                assert_eq!(s.require(id).unwrap().state, RequestState::Failed);
            }
            assert_eq!(
                s.require(id).unwrap().claimed_ms,
                None,
                "{id} kept a stale claim stamp"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_daemon_that_comes_back_after_its_claim_expired_cannot_overwrite() {
        // The hazard the timeout introduces. Reclaiming stale work is only safe
        // if the work it reclaimed cannot still write: without this, a resurrected
        // daemon's late report would clobber a spec somebody else has since
        // drafted, or a decision a reviewer has since made.
        let (s, dir) = store("late-report");
        file(&s, "r-1", "alpha");
        s.claim_next(Serves::Anything, "laptop").unwrap();

        // Its claim expires and a second daemon picks the work up and finishes.
        let mut stale = s.require("r-1").unwrap();
        stale.claimed_ms = Some(now_ms() - CLAIM_TIMEOUT_MS - 1);
        s.put(&stale).unwrap();
        s.claim_next(Serves::Anything, "office").unwrap();
        s.record_drafted("r-1", "office", "# The good spec", "specs/x")
            .unwrap();

        // Now the first daemon wakes up and reports. Both verbs are refused.
        let err = s
            .record_drafted("r-1", "laptop", "# The stale spec", "specs/y")
            .expect_err("a late report must not be recorded")
            .to_string();
        assert!(err.contains("too late"), "{err}");
        assert!(err.contains("reclaimed"), "{err}");
        assert!(s.record_failed("r-1", "laptop", "gave up").is_err());

        // The good spec survives untouched, still awaiting its reviewer.
        let req = s.require("r-1").unwrap();
        assert_eq!(req.spec.as_deref(), Some("# The good spec"));
        assert_eq!(req.state, RequestState::AwaitingReview);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn released_work_returns_to_the_queue_and_says_who_gave_it_back() {
        // The difference that matters: a failure is terminal and says something
        // about the request, where a release says only that this machine could
        // not do it — so the work has to survive and be claimable again.
        let (s, dir) = store("released");
        file(&s, "r-1", "alpha");
        s.claim_next(Serves::Anything, "laptop").unwrap();

        let back = s
            .record_released("r-1", "laptop", "no repository named \"alpha\" here")
            .unwrap();
        assert_eq!(back.state, RequestState::Queued);
        assert_eq!(back.claimed_by, None);
        assert_eq!(back.claimed_ms, None);
        let note = back.note.unwrap();
        assert!(note.contains("laptop"), "it names who: {note}");
        assert!(note.contains("alpha"), "and why: {note}");

        // And another daemon can pick it straight up — no waiting out a lease.
        let again = s.claim_next(Serves::Anything, "office").unwrap().unwrap();
        assert_eq!(again.id, "r-1");
        assert_eq!(again.claimed_by.as_deref(), Some("office"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_daemon_holding_a_claim_may_release_it() {
        // Otherwise any daemon could yank work out from under the machine that
        // is drafting it.
        let (s, dir) = store("release-holder");
        file(&s, "r-1", "alpha");
        s.claim_next(Serves::Anything, "laptop").unwrap();

        assert!(s.record_released("r-1", "office", "not mine").is_err());
        assert_eq!(s.require("r-1").unwrap().state, RequestState::Claimed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn releasing_keeps_the_note_a_reviewer_sent_it_back_with() {
        // Nothing was drafted, so the reason it was rejected still applies to
        // whoever picks it up next — the same reasoning as `reclaim_stale`.
        let (s, dir) = store("release-sendback");
        file(&s, "r-1", "alpha");
        let mut r = s.require("r-1").unwrap();
        r.send_back_note = Some("too vague".into());
        s.put(&r).unwrap();
        s.claim_next(Serves::Anything, "laptop").unwrap();

        let back = s.record_released("r-1", "laptop", "wrong machine").unwrap();
        assert_eq!(back.send_back_note.as_deref(), Some("too vague"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_release_does_not_weaken_the_rule_that_a_failure_is_terminal() {
        // Two different verbs reaching two different states. A failure stays
        // final; release is the way to *not* burn a request.
        let (s, dir) = store("release-vs-fail");
        file(&s, "r-1", "alpha");
        s.claim_next(Serves::Anything, "laptop").unwrap();
        s.record_failed("r-1", "laptop", "could not draft").unwrap();

        assert_eq!(s.require("r-1").unwrap().state, RequestState::Failed);
        assert!(s.claim_next(Serves::Anything, "office").unwrap().is_none());
        // And a release cannot resurrect it either — it is not claimed by
        // anyone any more.
        assert!(s
            .record_released("r-1", "laptop", "second thoughts")
            .is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_daemon_cannot_report_on_a_claim_another_daemon_now_holds() {
        // The window a state-only guard leaves open, and the reason a claim has
        // to record *who* holds it.
        //
        // The test above only passes because the second daemon had already
        // FINISHED — the state had moved past `Claimed`, so checking the state
        // was enough. Here the second daemon is still drafting: the request is
        // `Claimed`, a state-only check sees nothing wrong, and the first
        // daemon's stale spec lands on top of work in progress.
        let (s, dir) = store("wrong-holder");
        file(&s, "r-1", "alpha");
        s.claim_next(Serves::Anything, "laptop").unwrap();

        // The laptop's claim expires; the office picks it up and is still
        // working — nothing has been reported yet.
        let mut stale = s.require("r-1").unwrap();
        stale.claimed_ms = Some(now_ms() - CLAIM_TIMEOUT_MS - 1);
        s.put(&stale).unwrap();
        s.claim_next(Serves::Anything, "office").unwrap();
        assert_eq!(s.require("r-1").unwrap().state, RequestState::Claimed);

        let err = s
            .record_drafted("r-1", "laptop", "# Stale", "specs/y")
            .expect_err("the laptop no longer holds this claim")
            .to_string();
        assert!(err.contains("office"), "it names the real holder: {err}");
        assert!(s.record_failed("r-1", "laptop", "gave up").is_err());

        // And the daemon that does hold it is unaffected.
        s.record_drafted("r-1", "office", "# The good spec", "specs/x")
            .unwrap();
        assert_eq!(
            s.require("r-1").unwrap().spec.as_deref(),
            Some("# The good spec")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_request_written_before_the_rename_still_loads() {
        // **The live volume already holds these.** `Ready` became `Accepted`
        // because the old name read as "built", which is the one thing this
        // state does not mean — but every record written under it is still on
        // disk, and losing them to a rename would be the worst possible outcome
        // of a cosmetic change.
        //
        // A serde alias rather than a migration pass: a migration that runs at
        // startup is one that can fail at startup, on the one directory the
        // developer cannot lose.
        let older = r#"{"id":"r-1","text":"a thing","repo":"alpha","kind":"bug",
                        "state":"ready","filed_ms":0}"#;
        let r: Request = serde_json::from_str(older).expect("an older record still loads");
        assert_eq!(r.state, RequestState::Accepted);

        // And writes forward under the new name, so the old spelling ages out
        // of the volume on its own.
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"accepted\""), "{json}");
        assert!(!json.contains("\"ready\""), "{json}");
    }

    #[test]
    fn a_record_written_before_claimed_by_existed_still_loads() {
        // The upgrade case. A claim taken under the previous build has no
        // holder, and rejecting its report would throw away a draft that is
        // legitimately in flight — so an absent holder matches any daemon. The
        // window is one claim timeout.
        let (s, dir) = store("no-holder");
        file(&s, "r-1", "alpha");
        let mut mid_flight = s.require("r-1").unwrap();
        mid_flight.state = RequestState::Claimed;
        mid_flight.claimed_ms = Some(now_ms());
        mid_flight.claimed_by = None;
        s.put(&mid_flight).unwrap();

        s.record_drafted("r-1", "whoever", "# Spec", "specs/x")
            .expect("an in-flight draft from before the upgrade is still recorded");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_claim_records_the_daemon_holding_it_and_lets_go_when_it_ends() {
        let (s, dir) = store("holder-lifecycle");
        file(&s, "r-1", "alpha");
        let claimed = s.claim_next(Serves::Anything, "laptop").unwrap().unwrap();
        assert_eq!(claimed.claimed_by.as_deref(), Some("laptop"));

        // Dropped when the state moves on, so nothing says a machine is working
        // on something when none is.
        s.record_drafted("r-1", "laptop", "# Spec", "specs/x")
            .unwrap();
        assert_eq!(s.require("r-1").unwrap().claimed_by, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reclaimed_request_forgets_who_held_it() {
        // A holder left on a requeued request would name a daemon that is not
        // working on it — as misleading as a stale timestamp, and dropped for
        // the same reason.
        let (s, dir) = store("reclaim-holder");
        file(&s, "r-1", "alpha");
        s.claim_next(Serves::Anything, "laptop").unwrap();

        let mut stale = s.require("r-1").unwrap();
        stale.claimed_ms = Some(now_ms() - CLAIM_TIMEOUT_MS - 1);
        s.put(&stale).unwrap();
        assert_eq!(s.reclaim_stale(now_ms()).unwrap(), 1);

        let req = s.require("r-1").unwrap();
        assert_eq!(req.state, RequestState::Queued);
        assert_eq!(req.claimed_by, None);
        assert_eq!(req.claimed_ms, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_drafted_spec_comes_back_and_waits_for_a_human() {
        let (s, dir) = store("drafted");
        file(&s, "r-1", "alpha");
        s.claim_next(Serves::Anything, "d-test").unwrap();

        let req = s
            .record_drafted("r-1", "d-test", "# The spec", "specs/the-thing")
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
        let (s, dir) = store("accept");
        file(&s, "r-1", "alpha");
        s.claim_next(Serves::Anything, "d-test").unwrap();
        s.record_drafted("r-1", "d-test", "# The spec", "specs/x")
            .unwrap();

        let digest = s.require("r-1").unwrap().spec_digest().unwrap();
        let req = s.accept("r-1", &digest).unwrap();
        assert_eq!(req.state, RequestState::Accepted);
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
        s.claim_next(Serves::Anything, "d-test").unwrap();
        s.record_drafted("r-1", "d-test", "# Version one", "specs/x")
            .unwrap();
        let read_this = s.require("r-1").unwrap().spec_digest().unwrap();

        // A redraft lands under the reviewer. Through the real path — sent back,
        // requeued, claimed again — since a daemon may only report on a claim it
        // currently holds. The reviewer is still holding v1's digest throughout.
        s.send_back("r-1", "redo").unwrap();
        s.claim_next(Serves::Anything, "d-test").unwrap();
        s.record_drafted("r-1", "d-test", "# Version two", "specs/x")
            .unwrap();

        let err = s
            .accept("r-1", &read_this)
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
        assert_eq!(s.accept("r-1", &now).unwrap().state, RequestState::Accepted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_digest_is_over_the_spec_text_so_an_identical_redraft_still_approves() {
        // The binding is to *content*, not to a draft attempt. A redraft that
        // produces byte-identical text is the same artifact, and refusing it
        // would make the reviewer re-read something that did not change.
        let (s, dir) = store("same-digest");
        file(&s, "r-1", "alpha");
        s.claim_next(Serves::Anything, "d-test").unwrap();
        s.record_drafted("r-1", "d-test", "# The spec", "specs/x")
            .unwrap();
        let digest = s.require("r-1").unwrap().spec_digest().unwrap();

        // The real redraft path — sent back, requeued, claimed again — rather
        // than two `record_drafted` calls in a row. A daemon can only report on
        // a claim it currently holds, so the shortcut no longer models anything
        // that can happen.
        s.send_back("r-1", "change it").unwrap();
        s.claim_next(Serves::Anything, "d-test").unwrap();
        s.record_drafted("r-1", "d-test", "# The spec", "specs/x")
            .unwrap();
        assert!(s.accept("r-1", &digest).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_request_awaiting_review_can_be_approved_or_sent_back() {
        // Approving a queued request would sign off a spec that does not exist.
        let (s, dir) = store("guards");
        file(&s, "r-1", "alpha");
        assert!(s.accept("r-1", "any-digest").is_err());
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

        s.claim_next(Serves::Anything, "d-test").unwrap();
        let first = s
            .record_drafted("r-1", "d-test", "# v1", "specs/x")
            .unwrap()
            .drafted_ms
            .expect("stamped on receipt");
        s.send_back("r-1", "more detail").unwrap();
        s.claim_next(Serves::Anything, "d-test").unwrap();
        let second = s
            .record_drafted("r-1", "d-test", "# v2", "specs/x")
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
        s.claim_next(Serves::Anything, "d-test").unwrap();
        s.record_drafted("r-1", "d-test", "# Too vague", "specs/x")
            .unwrap();

        let req = s.send_back("r-1", "name the actual roles").unwrap();
        assert_eq!(req.state, RequestState::Queued);
        assert_eq!(req.send_back_note.as_deref(), Some("name the actual roles"));
        assert!(req.spec.is_none(), "the rejected draft is dropped");

        // And it is claimable again, carrying its note.
        let again = s.claim_next(Serves::Anything, "d-test").unwrap().unwrap();
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
        s.claim_next(Serves::Anything, "d-test").unwrap();
        s.record_drafted("r-1", "d-test", "# Spec", "specs/x")
            .unwrap();
        assert!(s.send_back("r-1", "  ").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_redraft_clears_the_spent_note() {
        // It grounded the redraft that just happened; carrying it forward would
        // ground the *next* one too, on feedback already acted upon.
        let (s, dir) = store("note-spent");
        file(&s, "r-1", "alpha");
        s.claim_next(Serves::Anything, "d-test").unwrap();
        s.record_drafted("r-1", "d-test", "# v1", "specs/x")
            .unwrap();
        s.send_back("r-1", "more detail").unwrap();
        s.claim_next(Serves::Anything, "d-test").unwrap();

        let req = s
            .record_drafted("r-1", "d-test", "# v2", "specs/x")
            .unwrap();
        assert!(req.send_back_note.is_none(), "the note is spent");
        assert_eq!(req.spec.as_deref(), Some("# v2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// File a request already in `state`, the way a public filing or a screener
    /// verdict would leave it.
    fn file_in(s: &Store, id: &str, repo: &str, state: RequestState) -> Request {
        let mut r = Request::new(id, format!("request {id}"), repo, IntakeKind::Feature);
        r.state = state;
        s.put(&r).unwrap();
        r
    }

    #[test]
    fn nothing_unscreened_is_ever_claimable() {
        // The core guarantee of the public surface: a filing reaches the
        // developer's machine only after it has been screened and only via
        // `Queued`. Asserted over every state, so a variant added later that
        // forgets this fails here rather than silently queueing.
        let (s, dir) = store("claimable");
        for (i, state) in [
            RequestState::Screening,
            RequestState::Quarantined,
            RequestState::Claimed,
            RequestState::AwaitingReview,
            RequestState::Accepted,
            RequestState::Discarded,
            RequestState::Failed,
        ]
        .into_iter()
        .enumerate()
        {
            // A repo each, so one cannot mask another by looking busy.
            file_in(&s, &format!("r-{i}"), &format!("repo-{i}"), state);
            assert!(!state.is_claimable(), "{state:?}");
        }
        assert!(
            s.claim_next(Serves::Anything, "d-test").unwrap().is_none(),
            "no daemon may claim any of these"
        );

        // Not even to a daemon that declares every one of those repositories.
        // Asserted with the served-repo filter WIDE OPEN, so an edit that made
        // it the deciding predicate rather than an extra one is caught here
        // rather than in review.
        let every_repo: Vec<String> = (0..7).map(|i| format!("repo-{i}")).collect();
        assert!(
            s.claim_next(Serves::These(&every_repo), "d-test")
                .unwrap()
                .is_none(),
            "declaring a repository widens which queued work you get, never \
             whether unqueued work becomes claimable"
        );

        // And the one state that is claimable still is.
        file_in(&s, "r-ok", "repo-ok", RequestState::Queued);
        assert_eq!(
            s.claim_next(Serves::Anything, "d-test")
                .unwrap()
                .unwrap()
                .id,
            "r-ok"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_daemon_only_gets_work_for_a_repository_it_serves() {
        // The failure this whole change exists to end: with two daemons serving
        // different repositories, whichever polled first used to be handed work
        // it could not do — and reported it as a terminal failure, destroying a
        // request it merely could not reach.
        let (s, dir) = store("serves-filter");
        file(&s, "r-1", "alpha");
        file(&s, "r-2", "beta");

        let beta_only = vec!["beta".to_string()];
        let claimed = s
            .claim_next(Serves::These(&beta_only), "office")
            .unwrap()
            .expect("the beta daemon takes beta work");
        assert_eq!(claimed.id, "r-2");
        assert_eq!(claimed.claimed_by.as_deref(), Some("office"));

        // And it is never offered the one it cannot do, even though that one is
        // older and would otherwise be first.
        assert!(
            s.claim_next(Serves::These(&beta_only), "office")
                .unwrap()
                .is_none(),
            "alpha is not this daemon's to take"
        );

        // The daemon that does serve it still gets it.
        let alpha_only = vec!["alpha".to_string()];
        assert_eq!(
            s.claim_next(Serves::These(&alpha_only), "laptop")
                .unwrap()
                .unwrap()
                .id,
            "r-1"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_daemon_declaring_nothing_gets_everything_so_an_old_one_keeps_working() {
        // `Anything` is what a daemon that does not know how to declare sends,
        // and it must mean "as before" rather than "nothing" — otherwise
        // upgrading the server silently stops every existing daemon.
        let (s, dir) = store("serves-anything");
        file(&s, "r-1", "alpha");
        assert_eq!(
            s.claim_next(Serves::Anything, "old-daemon")
                .unwrap()
                .unwrap()
                .id,
            "r-1"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_daemon_declaring_an_empty_set_claims_nothing() {
        // The other half of why `Serves` is a type: an empty list means a daemon
        // serving nothing, and must not collapse into "everything". `Anything`
        // can only be written deliberately.
        let (s, dir) = store("serves-empty");
        file(&s, "r-1", "alpha");
        assert!(s
            .claim_next(Serves::These(&[]), "serves-nothing")
            .unwrap()
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_served_repository_is_matched_exactly() {
        // The same rule the daemon uses to resolve a name against its own set. A
        // fuzzy match would reintroduce exactly the ambiguity a closed set of
        // names exists to remove.
        let (s, dir) = store("serves-exact");
        file(&s, "r-1", "alpha");
        for near_miss in ["Alpha", "alph", "alphax", " alpha"] {
            let names = vec![near_miss.to_string()];
            assert!(
                s.claim_next(Serves::These(&names), "d-test")
                    .unwrap()
                    .is_none(),
                "{near_miss:?} is not alpha"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_screening_verdict_can_only_withhold_never_admit_something_else() {
        // The screener's entire power. It moves Screening -> Queued or
        // Quarantined and can reach nothing else, so a model's opinion may
        // subtract from the queue but never introduce work.
        let (s, dir) = store("verdict");
        file_in(&s, "r-1", "alpha", RequestState::Screening);
        file_in(&s, "r-2", "beta", RequestState::Screening);

        assert_eq!(
            s.finish_screening("r-1", None).unwrap().state,
            RequestState::Queued
        );
        let held = s.finish_screening("r-2", Some("screened as spam")).unwrap();
        assert_eq!(held.state, RequestState::Quarantined);
        assert_eq!(held.note.as_deref(), Some("screened as spam"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_late_verdict_cannot_requarantine_what_a_human_already_released() {
        // Screening is a background sweep, so a verdict can land after the
        // developer has looked at the request and released it by hand. The human
        // decision must win.
        let (s, dir) = store("late-verdict");
        file_in(&s, "r-1", "alpha", RequestState::Quarantined);
        s.release("r-1").unwrap();

        let err = s
            .finish_screening("r-1", Some("spam"))
            .expect_err("already decided")
            .to_string();
        assert!(err.contains("only one being screened"), "{err}");
        assert_eq!(s.require("r-1").unwrap().state, RequestState::Queued);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_quarantined_request_can_be_released() {
        // Release is the human overruling the screener. Pointing it at anything
        // else would be a second, unguarded way into the claimable queue.
        let (s, dir) = store("release-guard");
        for (i, state) in [
            RequestState::Screening,
            RequestState::Queued,
            RequestState::Claimed,
            RequestState::AwaitingReview,
            RequestState::Accepted,
            RequestState::Discarded,
            RequestState::Failed,
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("r-{i}");
            file_in(&s, &id, "alpha", state);
            assert!(s.release(&id).is_err(), "{state:?} must not be releasable");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn releasing_keeps_the_reason_it_was_held() {
        // Without it the developer cannot tell a false positive from a real one
        // the next time the screener fires on something similar.
        let (s, dir) = store("release-note");
        file_in(&s, "r-1", "alpha", RequestState::Screening);
        s.finish_screening("r-1", Some("screened as spam")).unwrap();

        let released = s.release("r-1").unwrap();
        assert_eq!(released.state, RequestState::Queued);
        assert_eq!(released.note.as_deref(), Some("screened as spam"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_screening_finds_exactly_what_the_sweep_should_look_at() {
        let (s, dir) = store("pending");
        file_in(&s, "r-1", "alpha", RequestState::Screening);
        file_in(&s, "r-2", "beta", RequestState::Queued);
        file_in(&s, "r-3", "gamma", RequestState::Quarantined);

        let pending = s.pending_screening().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "r-1");
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
        s.claim_next(Serves::Anything, "d-test").unwrap();
        s.record_drafted("r-1", "d-test", "# The spec", "specs/x")
            .unwrap();
        let digest = s.require("r-1").unwrap().spec_digest().unwrap();
        s.accept("r-1", &digest).unwrap();

        let err = s.discard("r-1").expect_err("already settled").to_string();
        assert!(err.contains("was approved"), "{err}");
        assert_eq!(s.require("r-1").unwrap().state, RequestState::Accepted);
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
                s.claim_next(Serves::Anything, "d-test").unwrap();
                s.record_drafted(id, "d-test", "# Spec", "specs/x").unwrap();
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
        s.claim_next(Serves::Anything, "d-test").unwrap();

        let req = s
            .record_failed("r-1", "d-test", "the backend was unreachable")
            .unwrap();
        assert_eq!(req.state, RequestState::Failed);
        assert_eq!(req.note.as_deref(), Some("the backend was unreachable"));
        assert!(
            s.claim_next(Serves::Anything, "d-test").unwrap().is_none(),
            "not picked back up"
        );
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
    fn a_list_shows_what_needs_a_human_first() {
        let mut states = [
            RequestState::Accepted,
            RequestState::Queued,
            RequestState::AwaitingReview,
            RequestState::Failed,
        ];
        states.sort_by_key(|s| s.list_order());
        assert_eq!(states[0], RequestState::AwaitingReview);
    }
}
