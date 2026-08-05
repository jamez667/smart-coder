//! The JSON API the browser surface is built on.
//!
//! Mounted under `/api/v1/ui/`, kept apart from `/api/v1/work` — that one is the
//! daemon's, it is versioned by [`sc_proto::wire`], and a client of one must
//! never be able to reach the other by guessing a path.
//!
//! ## Why this exists beside the pages rather than instead of them
//!
//! Spec 18 records the reversal in full: this surface is becoming a single-page
//! application, and the server-rendered HTML in [`crate::page`] is what it
//! replaces. That replacement is staged, and **nothing here deletes anything**
//! until the thing that supersedes it is proven. Both can answer at once; the
//! pages remain the working surface until the last stage.
//!
//! ## What a JSON API makes tempting, and must not do
//!
//! The HTML surface got several things right by accident of being HTML, and each
//! is easy to lose in translation. They are listed here because the failure mode
//! is a well-meaning "fix" rather than an oversight:
//!
//! - **404, never 403.** Another filer's request, an owner's non-owned
//!   repository, `/setup` after the claim — all answer 404. A 403 confirms the
//!   id is real, which is the fact being withheld.
//! - **One answer for every sign-in outcome.** Unknown address, known address,
//!   revoked account and over-cap all look identical. A JSON body invites a
//!   `reason` field; there must not be one.
//! - **A filer sees a coarse state.** `public_state_label` deliberately blurs
//!   the screening states, and tests assert the words "quarantined" and "spam"
//!   never reach a filer in any locale. The API sends the same coarse label, not
//!   the raw enum.
//! - **Capabilities are the server's answer, not the client's.** [`Me`] says
//!   what this caller may do so the interface can render itself; every route
//!   still checks. A client that hides a button is a courtesy, and a server that
//!   trusts it is a hole.

use serde::Serialize;

use crate::auth::Caller;

/// The path every endpoint here sits under.
///
/// A prefix rather than a set of exact constants, because unlike the page routes
/// these are matched by prefix and dispatched inside this module. It ends in `/`
/// so `starts_with` cannot match `/api/v1/uixyz` — the same rule
/// [`crate::routes::is_public_path`] states for its own prefixes.
pub const PREFIX: &str = "/api/v1/ui/";

/// Who is calling, and what the interface may therefore offer them.
///
/// **The one endpoint a client fetches before anything else.** Everything the
/// SPA renders — which navigation exists, whether a filing form appears, whether
/// the settings link is there at all — comes from here rather than from the
/// client's own idea of who it is.
///
/// This replaces what [`crate::page::shell`] and its account menu did by
/// construction: a filer's menu never named a page they could not open, because
/// the server built the menu. That property has to survive the move to a client
/// that builds its own, and this is how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Me {
    /// `anonymous`, `filer`, `owner` or `administrator`.
    ///
    /// A string rather than a tagged union because it is for display and for
    /// coarse routing; anything that actually gates is a field in `can` below.
    pub role: &'static str,
    /// The login, when there is one. Absent for a filer, who has an address but
    /// no login, and for a stranger.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    /// What this caller may do. Named for the *action*, not the page, so a
    /// rearranged interface does not need new fields.
    pub can: Can,
    /// The repositories this caller may act on, already intersected with what
    /// this surface serves.
    ///
    /// **Pre-intersected, exactly as [`Caller::Owner`] carries it.** The
    /// intersection is done once at identify time so no call site re-derives it
    /// and gets it subtly different — the same reason the enum carries it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,
}

/// The verbs, as booleans.
///
/// Deliberately not a permission bitmask or a role check the client repeats:
/// each field answers one question the interface actually asks. Adding a
/// capability here is a decision; inferring one from `role` on the client is the
/// bug this exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Can {
    /// File a request. Any signed-in account may.
    pub file: bool,
    /// See requests filed by other people — the review list.
    ///
    /// True for an owner (their repositories) and the administrator (all of
    /// them). This is the flag that decides whether the client renders a review
    /// surface at all.
    pub review: bool,
    /// **Accept work.** The administrator alone.
    ///
    /// Separate from `review` because that separation is the entire owner role:
    /// an owner may send back, discard and release, and may not accept. On the
    /// server that is enforced by `Caller::Owner` not matching the admin gate —
    /// no value of the variant reaches an accepting handler. This field only
    /// tells the interface not to draw a button the server would refuse.
    pub accept: bool,
    /// Reach the administrative pages — settings, owners, repositories,
    /// machines, accounts.
    pub administer: bool,
}

impl Me {
    /// What to tell a caller about themselves.
    ///
    /// `None` — no cookie, or one matching nothing — is a stranger rather than
    /// an error. The sign-in surface is reachable signed out, so "who am I"
    /// always has an answer.
    pub fn of(caller: Option<&Caller>) -> Me {
        match caller {
            Some(Caller::Admin { login }) => Me {
                role: "administrator",
                login: Some(login.clone()),
                can: Can {
                    file: true,
                    review: true,
                    accept: true,
                    administer: true,
                },
                repos: Vec::new(),
            },
            Some(Caller::Owner { login, repos }) => Me {
                role: "owner",
                login: Some(login.clone()),
                can: Can {
                    file: true,
                    review: true,
                    accept: false,
                    administer: false,
                },
                repos: repos.clone(),
            },
            Some(Caller::Account { .. }) => Me {
                role: "filer",
                login: None,
                can: Can {
                    file: true,
                    review: false,
                    accept: false,
                    administer: false,
                },
                repos: Vec::new(),
            },
            // **A daemon asking who it is gets `anonymous`, not `daemon`.** It
            // holds a bearer key for `/api/v1/work` and has no business on the
            // browser surface; describing it here would be inventing a browser
            // role for a machine that has none.
            Some(Caller::Daemon { .. }) | None => Me {
                role: "anonymous",
                login: None,
                can: Can {
                    file: false,
                    review: false,
                    accept: false,
                    administer: false,
                },
                repos: Vec::new(),
            },
        }
    }
}

/// A request as its **own filer** may see it.
///
/// **The narrow view, and narrow by construction rather than by filtering.**
/// There is no field here for the repository name, the artifact directory or the
/// daemon's failure note, so no handler can leak one by forgetting to strip it —
/// the same reason [`crate::page::public_detail`] built its HTML from a fixed
/// list rather than from the record.
///
/// The state is the **coarse** label. `Screening`, `Quarantined` and `Queued`
/// all read as "received": a filer learning their request was quarantined learns
/// that this server screens, which is exactly what a spammer would tune against.
/// Tests assert the words never reach a filer in any locale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FiledRequest {
    pub id: String,
    /// The first line, for a list.
    pub summary: String,
    /// A coarse label, already translated. Not the enum.
    pub state: String,
    pub kind: String,
    pub filed_ms: u64,
    /// What they typed. Theirs, so they may read it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The drafted spec — **only when the operator allows it**, which is the
    /// `show_spec` setting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<String>,
}

/// A request as a **reviewer** may see it — an owner, or the administrator.
///
/// Wider than [`FiledRequest`] because a reviewer needs to act on it: the raw
/// state (they decide on the difference between `Quarantined` and `Queued`), the
/// repository, and the daemon's note when something failed.
///
/// **A separate type rather than `Option` fields on one type.** One type with
/// nullable fields is a type where "did I remember to clear that for a filer?"
/// is a question asked at every call site; two types make it a compile error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewRequest {
    pub id: String,
    pub summary: String,
    /// The raw state, lowercased — `queued`, `awaiting-review`, `quarantined`.
    pub state: String,
    pub kind: String,
    pub repo: String,
    pub filed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drafted_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<String>,
    /// Why it failed, or what a previous reviewer said sending it back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Where it landed, for the developer to open the file.
    ///
    /// **Administrator only.** It is a path on their own machine and an owner
    /// has no use for it — see [`ReviewRequest::of`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_dir: Option<String>,
    /// Whether any machine is offering to draft this request's repository.
    ///
    /// `served`, `no-daemon-seen` or `unserved`. **The one diagnostic that
    /// answers "why has nothing happened to this"**, and the three cases send an
    /// operator to three different places: start a daemon, fix a repository name
    /// that does not match what `queue add-repo` was given, or wait.
    ///
    /// It nearly did not survive the move. The rendered page built this reasoning
    /// from the poll record and the API had no field for it, so the test covering
    /// it was deleted as having no subject — which was true, and the right answer
    /// was to give it one rather than to lose the diagnostic.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<&'static str>,
    /// The digest of the spec as sent.
    ///
    /// **This is the accept handshake, not a checksum for its own sake.** The
    /// administrator accepts *these bytes*; if a redraft lands between reading
    /// and accepting, the digest no longer matches and the accept is refused
    /// rather than silently approving text nobody read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec_digest: Option<String>,
}

impl FiledRequest {
    /// Narrow a record to what its filer may see.
    pub fn of(r: &crate::store::Request, show_spec: bool, locale: crate::i18n::Locale) -> Self {
        FiledRequest {
            id: r.id.clone(),
            summary: r.summary().to_string(),
            state: crate::page::public::public_state_label(r.state, locale).to_string(),
            kind: r.kind.slug().to_string(),
            filed_ms: r.filed_ms,
            text: Some(r.text.clone()),
            spec: if show_spec { r.spec.clone() } else { None },
        }
    }
}

impl ReviewRequest {
    /// Widen a record for somebody who may act on it.
    ///
    /// `full` is the administrator. An owner gets everything except the artifact
    /// directory, which is a path on a machine they do not have.
    pub fn of(r: &crate::store::Request, full: bool) -> Self {
        Self::with_coverage(r, full, None)
    }

    /// The same, told whether anything serves this request's repository.
    pub fn with_coverage(
        r: &crate::store::Request,
        full: bool,
        coverage: Option<crate::daemons::Coverage>,
    ) -> Self {
        ReviewRequest {
            id: r.id.clone(),
            summary: r.summary().to_string(),
            state: format!("{:?}", r.state).to_ascii_lowercase(),
            kind: r.kind.slug().to_string(),
            repo: r.repo.clone(),
            filed_ms: r.filed_ms,
            drafted_ms: r.drafted_ms,
            text: Some(r.text.clone()),
            spec: r.spec.clone(),
            note: r.note.clone(),
            artifact_dir: if full { r.artifact_dir.clone() } else { None },
            coverage: coverage.map(|c| match c {
                crate::daemons::Coverage::Served => "served",
                crate::daemons::Coverage::NoDaemonSeen => "no-daemon-seen",
                crate::daemons::Coverage::Unserved => "unserved",
            }),
            spec_digest: r.spec.as_deref().map(crate::auth::hash),
        }
    }
}

/// The settings, as the interface may see them.
///
/// **Presence and a date, never a value.** There is no read path for a stored
/// secret anywhere in this server — the settings page renders whether a key is
/// set and when it was last changed, and this does the same. Making the API
/// return the ciphertext, or worse the plaintext, would create the read path the
/// whole sealing design exists to avoid.
///
/// The address, the site name and the mail settings are **not here at all**:
/// they are environment variables now, so there is nothing on this surface to
/// read or write. See [`crate::settings`] for why they moved and what it cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SettingsView {
    pub public: bool,
    pub show_spec: Option<bool>,
    pub max_daily_filings: Option<usize>,
    pub max_daily_drafts: Option<usize>,
    pub max_accounts: Option<usize>,
    pub max_outstanding_links: Option<usize>,
}

impl SettingsView {
    pub fn of(s: &crate::settings::Settings) -> Self {
        SettingsView {
            public: s.public,
            show_spec: s.show_spec,
            max_daily_filings: s.max_daily_filings,
            max_daily_drafts: s.max_daily_drafts,
            max_accounts: s.max_accounts,
            max_outstanding_links: s.max_outstanding_links,
        }
    }
}

/// An account, for the revoke list.
///
/// The **hint** — `j***@example.com` — and never `email_hash`, never the
/// password hash. Enough to recognise an account you meant to revoke, not enough
/// to be a contact list, which is the line the record itself draws.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccountView {
    pub id: String,
    pub email_hint: String,
    pub created_ms: u64,
    pub revoked: bool,
    /// Does this account hold a password? Not the hash, and not the login.
    pub has_password: bool,
}

impl AccountView {
    pub fn of(a: &crate::account::Account) -> Self {
        AccountView {
            id: a.id.clone(),
            email_hint: a.email_hint.clone(),
            created_ms: a.created_ms,
            revoked: a.revoked,
            has_password: a.password_hash.is_some(),
        }
    }
}

/// The built interface, compiled into the binary.
///
/// **`include_str!`, not a directory on disk**, which is what keeps the runtime
/// container what it has always been: one static binary, nothing to mount, and
/// no file that can go missing between the image being built and being run. The
/// fonts already work this way and this follows them.
///
/// The consequence to know about: **a frontend change is a Rust rebuild.** The
/// dev loop avoids that by serving the interface from Vite and proxying the API,
/// so the two only meet at build time.
pub mod ui {
    /// The document. Every route the interface owns answers this same HTML —
    /// the client reads the path and decides what to draw.
    pub const INDEX: &str = include_str!("../assets/ui/index.html");
    pub const SCRIPT: &str = include_str!("../assets/ui/app.js");
    pub const STYLE: &str = include_str!("../assets/ui/app.css");

    /// Where the built files are served from.
    pub const SCRIPT_PATH: &str = "/ui/app.js";
    pub const STYLE_PATH: &str = "/ui/app.css";
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me(c: Caller) -> Me {
        Me::of(Some(&c))
    }

    #[test]
    fn an_owner_may_review_but_never_accept() {
        // **The owner role, in one assertion.** Sending back, discarding and
        // releasing are theirs; accepting is not. The server enforces this by
        // variant identity — no `Caller::Owner` reaches an accepting handler —
        // and this only stops the interface drawing a button that would 404.
        let m = me(Caller::Owner {
            login: "jo@x.com".into(),
            repos: vec!["intake".into()],
        });
        assert_eq!(m.role, "owner");
        assert!(m.can.review);
        assert!(!m.can.accept, "an owner cannot accept");
        assert!(!m.can.administer);
        assert_eq!(m.repos, vec!["intake".to_string()]);
    }

    #[test]
    fn a_filer_is_told_of_nothing_they_cannot_reach() {
        // The property the server-rendered account menu had by construction: it
        // never named a page the reader could not open. A client builds its own
        // menu now, so this is where that survives.
        let m = me(Caller::Account { id: "a-1".into() });
        assert_eq!(m.role, "filer");
        assert!(m.can.file);
        assert!(!m.can.review);
        assert!(!m.can.accept);
        assert!(!m.can.administer);
        assert!(m.repos.is_empty());
        assert_eq!(m.login, None, "a filer has an address, not a login");
    }

    #[test]
    fn a_stranger_is_an_answer_rather_than_an_error() {
        // Signing in is reachable signed out, so "who am I" must always answer.
        let m = Me::of(None);
        assert_eq!(m.role, "anonymous");
        assert!(!m.can.file);
        assert!(!m.can.review);
    }

    #[test]
    fn a_daemon_has_no_browser_role() {
        // It holds a bearer key for the work API. Giving it a role here would
        // invent a browser identity for a machine that never uses one.
        let m = me(Caller::Daemon {
            label: "laptop".into(),
        });
        assert_eq!(m.role, "anonymous");
        assert!(!m.can.file);
    }

    #[test]
    fn the_administrator_is_the_only_caller_who_may_accept() {
        let m = me(Caller::Admin {
            login: "mail@jcnash.com".into(),
        });
        assert!(m.can.accept);
        assert!(m.can.administer);
        assert!(m.can.review);
    }

    #[test]
    fn no_capability_is_ever_inferred_from_the_role_string() {
        // `role` is for display. If the client could derive `accept` from
        // `role == "administrator"` it would work today and break the moment a
        // role gains a nuance — which is what happened to "the public surface
        // gets script". Every gate is its own field.
        for c in [
            Caller::Admin {
                login: "a@x.com".into(),
            },
            Caller::Owner {
                login: "b@x.com".into(),
                repos: vec![],
            },
            Caller::Account { id: "c".into() },
        ] {
            let m = Me::of(Some(&c));
            let serialised = serde_json::to_string(&m).unwrap();
            assert!(
                serialised.contains("\"can\""),
                "the capabilities travel with the role: {serialised}"
            );
        }
    }
}
