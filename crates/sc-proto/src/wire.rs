//! The wire protocol between the daemon and the hosted server (spec 18).
//!
//! **One definition, used by both ends.** The server depends on this crate rather
//! than restating these shapes, so the two cannot drift — the failure spec 17
//! exists to catch, prevented here by construction instead of detected later.
//!
//! ## The shape of the conversation
//!
//! The daemon **dials out**; the server never calls it. Three exchanges:
//!
//! ```text
//!   GET  /api/v1/work        long-poll — is there anything for me?
//!   POST /api/v1/work/:id/drafted    here is the spec I drafted
//!   POST /api/v1/work/:id/failed     I could not draft it, and why
//! ```
//!
//! That is the whole daemon-facing API. It carries **text only** — a request in,
//! a drafted spec out. The server has no path to a repository, no model, and no
//! filesystem access to anything the daemon owns, so there is nothing here that
//! could grow into an execution path.
//!
//! ## Why long-poll
//!
//! The server holds `/work` open until there is work or [`POLL_TIMEOUT`] elapses.
//! That gives near-instant pickup with almost no idle traffic, in one ordinary
//! HTTP call each way — no persistent connection to keep alive, no reconnect
//! logic, and no async runtime, which this workspace has nowhere outside the GUI.
//!
//! Fixed-interval polling would force a choice between latency and wasted
//! requests: a request filed on a train would wait an interval for no reason.
//!
//! ## Versioning
//!
//! Every payload carries [`PROTOCOL_VERSION`]. A daemon and a server are deployed
//! separately and *will* skew — the developer updates one and forgets the other —
//! so a mismatch must be a clear message rather than a confusing deserialisation
//! error deep in a handler.

use serde::{Deserialize, Serialize};

use crate::IntakeKind;

/// The protocol this build speaks.
///
/// Bumped only for a breaking change. Both ends check it, because a daemon and a
/// server are separately deployed and skew is the normal case rather than the
/// exception.
pub const PROTOCOL_VERSION: u32 = 1;

/// How long the server holds a `/work` poll open before answering "nothing".
///
/// Long enough that an idle daemon makes two requests a minute; short enough to
/// sit comfortably inside the default timeouts of proxies and load balancers,
/// which commonly cut idle connections at 60s.
pub const POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The client-side ceiling on a poll, slightly above [`POLL_TIMEOUT`].
///
/// The margin absorbs network latency so a healthy long-poll is never mistaken
/// for a hung one — a client timeout *below* the server's would make every
/// successful idle poll look like a failure.
pub const POLL_CLIENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// Work the server is handing to a daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    /// The server's id for this request. The daemon echoes it back and does not
    /// interpret it — the two sides keep their own identifiers.
    pub id: String,
    /// The request, verbatim as filed.
    pub text: String,
    /// Which repository, **by name**. The daemon resolves it against its own
    /// configured set and refuses anything absent — the server never learns a
    /// path, and cannot name one (spec 18).
    pub repo: String,
    /// What kind of request, which shapes the drafting prompt.
    pub kind: IntakeKind,
    /// A note from a previous send-back, if this is a redraft. The regeneration
    /// grounds on it, so the developer's reason for rejecting reaches the model.
    #[serde(default)]
    pub send_back_note: Option<String>,
}

/// The server's answer to a poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum PollResponse {
    /// Draft this.
    Work { protocol: u32, item: WorkItem },
    /// Nothing to do. The daemon polls again immediately; the *server* provided
    /// the delay by holding the request open.
    Idle { protocol: u32 },
}

impl PollResponse {
    pub fn work(item: WorkItem) -> Self {
        PollResponse::Work {
            protocol: PROTOCOL_VERSION,
            item,
        }
    }

    pub fn idle() -> Self {
        PollResponse::Idle {
            protocol: PROTOCOL_VERSION,
        }
    }

    /// The protocol version this payload claims.
    pub fn protocol(&self) -> u32 {
        match self {
            PollResponse::Work { protocol, .. } | PollResponse::Idle { protocol } => *protocol,
        }
    }
}

/// A drafted spec, going back to the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftedSpec {
    pub protocol: u32,
    /// The server's id, echoed.
    pub id: String,
    /// The spec, as Markdown.
    ///
    /// The server stores and renders this but **must treat it as untrusted**: a
    /// model wrote it, and a drafted spec containing a remote image reference is
    /// an exfiltration path through the `Referer` header (spec 18).
    pub spec: String,
    /// Where it landed in the repository, workspace-relative — so the developer
    /// can find the file after approving.
    pub artifact_dir: String,
}

impl DraftedSpec {
    pub fn new(
        id: impl Into<String>,
        spec: impl Into<String>,
        artifact_dir: impl Into<String>,
    ) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            id: id.into(),
            spec: spec.into(),
            artifact_dir: artifact_dir.into(),
        }
    }
}

/// A drafting attempt that did not produce a spec.
///
/// Distinct from a *deferral*: a failure is reported so the developer sees it,
/// where a deferral (a tree mid-rebase) simply leaves the work unclaimed for the
/// next poll. Reporting a transient local condition as a failure would make the
/// developer requeue by hand for something that fixes itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftFailed {
    pub protocol: u32,
    pub id: String,
    /// Why, in terms the developer can act on.
    pub reason: String,
}

impl DraftFailed {
    pub fn new(id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            id: id.into(),
            reason: reason.into(),
        }
    }
}

/// What went wrong with a request, as the server reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    pub error: String,
}

impl WireError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self { error: msg.into() }
    }
}

/// Check a payload's protocol version against this build's.
///
/// Returns a message naming both versions, because "protocol mismatch" alone
/// leaves a developer with two separately-deployed components and no idea which
/// to update.
pub fn check_protocol(theirs: u32, peer: &str) -> std::result::Result<(), String> {
    if theirs == PROTOCOL_VERSION {
        return Ok(());
    }
    let direction = if theirs > PROTOCOL_VERSION {
        "this daemon is older"
    } else {
        "the server is older"
    };
    Err(format!(
        "protocol mismatch: {peer} speaks v{theirs}, this build speaks \
         v{PROTOCOL_VERSION} — {direction}. Update it and retry."
    ))
}

/// The daemon-facing routes. Held here so both ends agree on the strings.
pub mod route {
    /// Long-poll for work.
    pub const WORK: &str = "/api/v1/work";

    /// Where a drafted spec is posted, for the work item `id`.
    pub fn drafted(id: &str) -> String {
        format!("/api/v1/work/{id}/drafted")
    }

    /// Where a failure is posted, for the work item `id`.
    pub fn failed(id: &str) -> String {
        format!("/api/v1/work/{id}/failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> WorkItem {
        WorkItem {
            id: "srv-1".into(),
            text: "Add a health check endpoint".into(),
            repo: "alpha".into(),
            kind: IntakeKind::Feature,
            send_back_note: None,
        }
    }

    #[test]
    fn every_payload_round_trips_through_json() {
        // Both ends deserialize what the other serialized; a shape that does not
        // survive the trip is a protocol that does not work.
        let poll = PollResponse::work(item());
        assert_eq!(
            serde_json::from_str::<PollResponse>(&serde_json::to_string(&poll).unwrap()).unwrap(),
            poll
        );

        let idle = PollResponse::idle();
        assert_eq!(
            serde_json::from_str::<PollResponse>(&serde_json::to_string(&idle).unwrap()).unwrap(),
            idle
        );

        let drafted = DraftedSpec::new("srv-1", "# Spec", "specs/add-a-health-check");
        assert_eq!(
            serde_json::from_str::<DraftedSpec>(&serde_json::to_string(&drafted).unwrap()).unwrap(),
            drafted
        );

        let failed = DraftFailed::new("srv-1", "the backend was unreachable");
        assert_eq!(
            serde_json::from_str::<DraftFailed>(&serde_json::to_string(&failed).unwrap()).unwrap(),
            failed
        );
    }

    #[test]
    fn a_poll_response_is_tagged_so_idle_and_work_are_unambiguous() {
        // Untagged, an empty `Work` and an `Idle` would be indistinguishable, and
        // a daemon would try to draft nothing.
        let json = serde_json::to_string(&PollResponse::idle()).unwrap();
        assert!(json.contains("\"type\":\"idle\""), "{json}");
        let json = serde_json::to_string(&PollResponse::work(item())).unwrap();
        assert!(json.contains("\"type\":\"work\""), "{json}");
    }

    #[test]
    fn a_work_item_carries_a_repo_name_and_never_a_path() {
        // The server cannot name a path even if it wanted to: there is no field
        // for one. That is what makes traversal unreachable rather than
        // mitigated (spec 18).
        let json = serde_json::to_string(&item()).unwrap();
        assert!(json.contains("\"repo\":\"alpha\""), "{json}");
        for path_ish in ["\"path\"", "\"dir\"", "\"workspace\""] {
            assert!(
                !json.contains(path_ish),
                "the wire format must have no path field: {json}"
            );
        }
    }

    #[test]
    fn a_send_back_note_reaches_the_redraft() {
        // Without it the regeneration has nothing to go on and likely produces
        // the same spec — which reads to the developer as being ignored.
        let mut i = item();
        i.send_back_note = Some("name the actual roles".into());
        let back: WorkItem = serde_json::from_str(&serde_json::to_string(&i).unwrap()).unwrap();
        assert_eq!(
            back.send_back_note.as_deref(),
            Some("name the actual roles")
        );
    }

    #[test]
    fn a_payload_from_an_older_peer_still_parses() {
        // The daemon and server are deployed separately and WILL skew. A missing
        // optional field must not make the payload unreadable, or the version
        // check never gets a chance to produce its clear message.
        let json = r#"{"id":"srv-1","text":"t","repo":"alpha","kind":"bug"}"#;
        let item: WorkItem = serde_json::from_str(json).unwrap();
        assert_eq!(item.kind, IntakeKind::Bug);
        assert!(item.send_back_note.is_none());
    }

    #[test]
    fn a_protocol_mismatch_says_which_side_is_behind() {
        // "Protocol mismatch" alone leaves a developer with two separately
        // deployed components and no idea which to update.
        let newer = check_protocol(PROTOCOL_VERSION + 1, "the server").unwrap_err();
        assert!(newer.contains("this daemon is older"), "{newer}");
        assert!(
            newer.contains(&format!("v{}", PROTOCOL_VERSION + 1)),
            "{newer}"
        );

        let older = check_protocol(PROTOCOL_VERSION - 1, "the server").unwrap_err();
        assert!(older.contains("the server is older"), "{older}");

        assert!(check_protocol(PROTOCOL_VERSION, "the server").is_ok());
    }

    #[test]
    fn the_client_timeout_exceeds_the_server_hold() {
        // A client timeout at or below the server's would make every successful
        // idle poll look like a hung request.
        assert!(
            POLL_CLIENT_TIMEOUT > POLL_TIMEOUT,
            "the client must outwait the server's hold"
        );
        // And the server's hold stays inside the ~60s idle cut common to proxies.
        assert!(POLL_TIMEOUT.as_secs() < 60);
    }

    #[test]
    fn routes_are_shared_so_the_two_ends_cannot_disagree() {
        assert_eq!(route::WORK, "/api/v1/work");
        assert_eq!(route::drafted("srv-1"), "/api/v1/work/srv-1/drafted");
        assert_eq!(route::failed("srv-1"), "/api/v1/work/srv-1/failed");
    }
}
