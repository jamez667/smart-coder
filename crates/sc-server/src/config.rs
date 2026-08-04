//! Configuration, from the environment.
//!
//! **Every setting is an environment variable**, because this ships as a Docker
//! image installed in Portainer and a stack editor is where a user configures it.
//! A config file baked into an image is the wrong shape: it cannot be edited
//! without rebuilding, and mounting one to override it makes two sources of truth.
//!
//! **All state lives under one directory** ([`Config::data_dir`]), so a Portainer
//! user has exactly one volume to mount and one thing to back up. State scattered
//! across several paths is a footgun — the backup that misses one of them looks
//! like it worked.

use std::path::PathBuf;

use crate::mail::Provider;

/// How short a secret may be before it is refused.
///
/// A short key is worse than no key: it looks configured while being guessable,
/// which is the failure mode nobody notices.
pub const MIN_SECRET_LEN: usize = 32;

/// How the server is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// What to bind. Defaults to all interfaces, because inside a container
    /// loopback would be unreachable from outside it — the isolation is the
    /// container's, not the bind's.
    pub bind: String,
    pub port: u16,
    /// The one volume: requests, drafted specs, and credentials.
    pub data_dir: PathBuf,
    /// The keys daemons authenticate with, one per machine. **At least one is
    /// required** — the server refuses to start with none rather than running
    /// open, because an unauthenticated intake surface on the public internet is
    /// the failure this whole design exists to prevent.
    ///
    /// A list rather than one key, because a single shared secret collapses
    /// three things that should be per-machine: the rate budget (a runaway
    /// laptop starves the office), revocation (rotating locks out everything at
    /// once), and who holds a claim — a late report from a daemon presumed dead
    /// can otherwise overwrite a draft another machine is still working on.
    pub daemon_keys: Vec<DaemonKey>,
    /// The key the settings on the volume are sealed with.
    ///
    /// **The one secret that must stay in the environment**, because it is what
    /// makes the volume safe to copy. Settings that have to be *replayed* — a
    /// mail key, a screening key — cannot be hashed the way every
    /// credential in [`crate::auth`] is, so they are encrypted instead, and an
    /// encryption key stored beside its ciphertext protects nothing.
    ///
    /// `None` is legal and means nothing can be sealed or opened. A server that
    /// has never been given secrets through the UI does not need one, and
    /// refusing to start without it would demand a key from every deployment
    /// that will never store a secret. What must never happen is *silently
    /// behaving as though nothing were configured* when a sealed file is present
    /// and the key is missing or wrong — see the startup check in
    /// [`crate::serve`].
    pub seal_key: Option<crate::seal::SealKey>,
    /// The public, self-serve filing surface — **absent unless asked for**.
    ///
    /// `Option` rather than a bool plus loose fields, so a route cannot read a
    /// half-configured setup: either every part needed to run a public surface is
    /// present, or the surface does not exist. The same reasoning that makes
    /// `daemon_key` required — the unsafe configuration should be impossible to
    /// express, not merely discouraged.
    pub public: Option<PublicConfig>,
}

/// One daemon's credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonKey {
    /// What the operator calls this machine: `laptop`, `office`.
    ///
    /// Appears in the log and is what a claim records, so a human reading either
    /// can tell which machine did something. **Not a secret** — that is the
    /// point of it being separate from the key.
    pub label: String,
    /// SHA-256 of the key. **The key itself is not kept**, so a `Config` in a
    /// debug print, a panic message or a core dump grants nobody anything.
    pub key_hash: String,
}

/// The label a lone [`env::DAEMON_KEY`] is filed under.
///
/// Named rather than inlined because two places must agree: the fold-in below,
/// and the startup warning that tells the operator what to migrate to.
pub const DEFAULT_DAEMON_LABEL: &str = "default";

/// The repositories a public surface collects for.
///
/// **A type rather than a `Vec<String>`**, for the reason
/// [`Serves`](crate::store::Serves) is one: an empty vector is what a caller who
/// forgot produces, and "did the operator nominate this repository?" gets asked
/// from more than one place. A vector would invite a second `.contains()`
/// somewhere that spelled the comparison slightly differently — and *that* is
/// the comparison a stranger's input is checked against.
///
/// **No longer non-empty by construction.** That invariant was load-bearing
/// while parsing operator input was the only way to build one: an empty set
/// could not arise, so no reader had to decide what it meant.
///
/// A developer who can disable the last repository from the UI makes it real. A
/// type that declares it unrepresentable does not prevent that — it moves the
/// failure from a value a reader must handle to a panic inside `first()`, which
/// is strictly worse. So [`first`](Repos::first) returns an `Option` and the
/// surface serves with nothing on offer, saying why the form cannot take
/// anything. Not refuse-to-boot, which would make the admin page unreachable
/// exactly when it is needed, and not 404, which teaches a filer at a working
/// address nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Repos(Vec<String>);

impl Repos {
    /// Build a set from names already known to be good.
    ///
    /// For tests and for callers holding a validated list. [`public_repos`] is
    /// what enforces uniqueness on operator input; this trusts its caller.
    pub fn new(names: &[&str]) -> Repos {
        Repos(names.iter().map(|n| n.to_string()).collect())
    }

    /// The first one — what a single-repository deployment always gets.
    ///
    /// `None` when nothing is enabled, which the admin page makes reachable.
    pub fn first(&self) -> Option<&str> {
        self.0.first().map(String::as_str)
    }

    /// Is nothing enabled?
    ///
    /// Worth asking directly: the page that says *why* it cannot take a filing
    /// is the whole reason the empty set is allowed to exist.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Did the operator nominate this repository for public intake?
    ///
    /// Exact and case-sensitive, matching the daemon's own resolution of the
    /// same name. A fuzzy match would reintroduce the ambiguity a closed set
    /// exists to remove — and this is the check standing between a stranger's
    /// form submission and a repository nobody nominated.
    pub fn accepts(&self, repo: &str) -> bool {
        self.0.iter().any(|r| r == repo)
    }

    /// Every nominated name, in the order the operator wrote them — which is the
    /// order the picker offers.
    pub fn names(&self) -> &[String] {
        &self.0
    }

    /// Is this a surface for exactly one repository?
    ///
    /// Asked rather than `len() == 1` at the call sites, and still *instead* of
    /// a `len`. The reasoning has shrunk rather than gone: a bare `len` used to
    /// invite an `is_empty` whose only honest answer was a constant `false`,
    /// and now that emptiness is reachable there is a real
    /// [`is_empty`](Repos::is_empty) beside this — which is exactly the
    /// question worth naming, rather than a number every caller re-interprets.
    pub fn is_single(&self) -> bool {
        self.0.len() == 1
    }
}

impl From<Vec<String>> for Repos {
    /// From the roster, which is where the enabled set actually lives.
    ///
    /// No validation, deliberately: the names were checked when they were
    /// enabled, and re-refusing here would mean a record written yesterday could
    /// stop a server booting today — the failure mode a record exists to avoid.
    fn from(names: Vec<String>) -> Repos {
        Repos(names)
    }
}

/// Somebody who may review work for particular repositories — **as seeded from
/// configuration**.
///
/// The authoritative list is [`crate::roster::Roster`] on the volume; this is
/// applied once on a fresh volume and ignored thereafter, so an existing
/// install keeps its owners and a fresh one can start without a browser. What
/// grants anything at runtime is the roster, never this.
///
/// **The allowlist is the whole authorization model**, and deliberately so. The
/// server asks nobody whether a person has anything to do with a repository,
/// so it is the only thing standing between a signed-in account and every
/// drafted spec for a project — and a drafted spec is model output
/// produced by reading the developer's tree.
///
/// That is why every entry is validated at startup and why the failures are
/// refusals rather than empty views: an owner naming a repository this surface
/// does not serve is a typo that would otherwise look applied and grant
/// nothing. A record read at runtime cannot refuse to boot, so the roster
/// intersects instead and the admin page marks what no longer matches.
///
/// **Repository access is not checked anywhere else.** Signing in proves who
/// somebody is and nothing more; what they may see comes from the roster.
/// Adding the check later reads the same field and calls the API *in addition*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    /// The **username**, lowercased. Not the account id.
    ///
    /// The id is the stabler identifier, and that argument does not apply here:
    /// an operator writes this setting by hand from a name they recognise, and
    /// nobody knows their collaborator's numeric id. A login change makes the
    /// entry stop matching — a person who cannot sign in and says so, which is
    /// the safe direction to fail.
    pub login: String,
    /// Which repositories this owner may review.
    ///
    /// Never empty: an entry naming none is refused at startup rather than
    /// producing somebody who signs in successfully and looks at a blank page.
    pub repos: Vec<String>,
}

impl Owner {
    /// May this owner see work for `repo`?
    ///
    /// Exact, like every other repository comparison in this crate — and this
    /// one decides whether one person's project is visible to another.
    pub fn owns(&self, repo: &str) -> bool {
        self.repos.iter().any(|r| r == repo)
    }
}

/// What the public surface needs before it may exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicConfig {
    /// The repositories public filings may be attributed to.
    ///
    /// Names from *this* configuration, never free text from the request body —
    /// a stranger must not be able to aim work at a repository the operator did
    /// not nominate for public intake. What the body may do is *choose among*
    /// these, which is why the form can offer a picker at all.
    ///
    /// **Routing keys, not labels.** The daemon matches each exactly against its
    /// own `queue add-repo` names, so they are not free text — see
    /// [`site_name`](Self::site_name) for what the masthead shows.
    pub repos: Repos,
    /// What the masthead calls this site.
    ///
    /// Separate from the routing keys because the two answer different
    /// questions. A repository name has to equal what the daemon was told,
    /// exactly; a heading wants `jamez667/smart-coder` — the name a person
    /// recognises. Folding them into one field would mean renaming the heading
    /// forces a matching `queue add-repo`, and a mismatch there is a queue that
    /// silently never drains.
    ///
    /// Defaults to the single repository when there is exactly one, and is
    /// **required** when there are several: a masthead reading
    /// `smart-coder · memosy` would appear on the landing and sign-in pages too,
    /// where nothing has been chosen and the join says nothing.
    pub site_name: String,
    /// The absolute base URL sign-in links are built from.
    ///
    /// **Required, and not inferred from the `Host` header**, which is
    /// attacker-controlled: a link built from it would send the filer to whatever
    /// host the request claimed to be. Getting this wrong the other way emails
    /// somebody a link to `localhost`, which is merely useless.
    pub base_url: String,
    /// How to send the sign-in link.
    ///
    /// `None` only when [`Config::mail_to_console`] is on, which is refused
    /// unless the base URL is loopback. An `Option` rather than a placeholder
    /// `MailConfig`: a placeholder holding a real provider name and an empty key
    /// is one refactor away from silently constructing an `HttpMailer` that
    /// authenticates with nothing, and this way that does not compile.
    pub mail: Option<MailConfig>,
    /// The spam screener. `None` means file straight to `Queued` — a server that
    /// pretends to screen is worse than one that plainly does not.
    pub screen: Option<ScreenConfig>,
    /// How many unspent sign-in links may be outstanding at once.
    ///
    /// **The real ceiling on mail spend.** The rate limiter shapes traffic, but
    /// thirty a minute sustained is tens of thousands a day; this refuses before
    /// the mailer is called.
    pub max_outstanding_links: usize,
    /// How many requests one account may file per rolling 24 hours.
    ///
    /// **The ceiling on model spend**, and the only thing standing between a
    /// hostile account and the developer's bill. Every filing that clears the
    /// screener costs a full drafting run on the developer's machine, and the
    /// per-credential rate limit — 240 a minute — is no defence against
    /// something that expensive.
    ///
    /// Generous for a person and tight against a script: a real filer reporting
    /// a morning's worth of bugs does not reach it, and a loop hits it in
    /// seconds.
    pub max_daily_filings: usize,
    /// How many drafting runs one repository may be sent into per rolling 24h.
    ///
    /// Bounds **re-admission**, which [`max_daily_filings`](Self::max_daily_filings)
    /// never did: that one is checked when a request is filed, so a request
    /// already filed re-enters the queue for free however often somebody sends
    /// it back.
    pub max_daily_drafts: usize,
    /// How many accounts may exist at all.
    ///
    /// Without this the per-account cap is weaker than it looks: an id the
    /// attacker cannot *vary* is one they can **re-mint**, and a script with a
    /// hundred disposable addresses holds a hundred budgets. This is the bound
    /// the filing cap is built on.
    ///
    /// Reached, signup stops and the lever is **raising this** — revoking does
    /// *not* make room, because revoked accounts still occupy their slot. A
    /// revoked address can never be re-created, so a freed slot could only be
    /// taken by a different one, and counting only live accounts would let an
    /// attacker's burned identities be swapped one for one under a wall that
    /// looks intact.
    ///
    /// A public form that quietly stops working is bad; one that silently scales
    /// to any number of budgets is worse.
    pub max_accounts: usize,
    /// Should cookies carry the `Secure` attribute?
    ///
    /// **On everywhere except a loopback base URL**, and derived rather than
    /// configured: a browser silently *discards* a `Secure` cookie sent over
    /// plain HTTP, so on `http://localhost` sign-in and the language switcher
    /// would both appear to do nothing at all — the request succeeds, the cookie
    /// vanishes, and the next page has forgotten. The symptom looks like a bug
    /// in the feature rather than a property of the cookie.
    ///
    /// Derived from the same check that already governs `http://` in
    /// [`PublicConfig::base_url`], so there is no setting to get wrong: a
    /// deployed server cannot be talked into dropping `Secure`, because its base
    /// URL must be `https://` to start at all.
    pub secure_cookies: bool,
    /// Does a filer get to read the spec drafted from their request?
    ///
    /// Defaults **on**, which is a deliberate choice worth understanding: the
    /// drafted spec is model output produced by reading the developer's
    /// repository, and the filer wrote the prompt that produced it. Turn it off
    /// where the contents should not be described to strangers.
    ///
    /// **Per surface, not per repository** — the phrasing here used to say "for
    /// a repository", which is no longer accurate and was never quite
    /// implementable: this is read on the landing page and the list, which show
    /// several repositories at once and have no single answer.
    pub show_spec: bool,
    /// Who may review work, and for which repositories.
    ///
    /// Empty means the owner role does not exist on this surface — the resting
    /// state, and what every deployment has until an operator names somebody.
    pub owners: Vec<Owner>,
}

/// How the sign-in link is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailConfig {
    pub provider: Provider,
    pub api_key: String,
    pub from: String,
    pub from_name: String,
}

/// How filings are screened before they may be claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenConfig {
    pub api_key: String,
    pub url: String,
    pub model: String,
}

/// Where Gemini's OpenAI-compatible endpoint lives.
///
/// The default because it is what the rest of this project already points at for
/// a hosted planner, and its cheapest model is more than enough to sort spam
/// from a bug report.
pub const DEFAULT_SCREEN_URL: &str = "https://generativelanguage.googleapis.com/v1beta/openai";

/// Cheapest and fastest of the Gemini family.
/// What a sign-in email is signed by, when nobody says otherwise.
///
/// Named rather than inline, because the settings page defaults to the same
/// thing and two copies of a default drift.
pub const DEFAULT_MAIL_FROM_NAME: &str = "Smart Coder";

pub const DEFAULT_SCREEN_MODEL: &str = "gemini-2.5-flash-lite";

/// The default ceiling on unspent sign-in links.
pub const DEFAULT_MAX_OUTSTANDING_LINKS: usize = 200;

/// The default per-account daily filing cap.
///
/// Twenty is a lot of genuine bug reports from one person in a day and nothing
/// at all to a script.
pub const DEFAULT_MAX_DAILY_FILINGS: usize = 20;

/// The default ceiling on how many accounts may exist.
///
/// High enough that a real audience never notices, low enough that a signup
/// flood stops somewhere the developer can see rather than growing until the
/// volume fills.
pub const DEFAULT_MAX_ACCOUNTS: usize = 1_000;

/// The default ceiling on drafting runs per repository per day.
///
/// **The only cap that bounds what is actually spent.**
/// [`DEFAULT_MAX_DAILY_FILINGS`] bounds what a stranger may *file*, which was
/// the whole threat model while filing was the only way work reached the queue.
/// It never bounded *re-admission*: a request already filed has paid its filing,
/// and every later send-back or release buys another full drafting run against
/// the same record for nothing.
///
/// Sixty is generous for a project under real review — a redraft or two on each
/// of a day's requests — and stops a send-back loop within minutes.
///
/// Note what the filing cap really allowed, which makes this the first genuine
/// bound: twenty filings *per account*, against a default of a thousand
/// accounts.
pub const DEFAULT_MAX_DAILY_DRAFTS: usize = 60;

/// The window the filing cap is measured over.
///
/// Rolling rather than calendar: "resets at midnight" invites waiting for
/// midnight, and midnight in whose timezone has no good answer on a server that
/// holds no locale.
pub const FILING_WINDOW_MS: u64 = 24 * 60 * 60 * 1000;

/// The environment variables, named once so the error messages and the
/// documentation cannot disagree.
pub mod env {
    pub const BIND: &str = "SC_SERVER_BIND";
    pub const PORT: &str = "SC_SERVER_PORT";
    pub const DATA_DIR: &str = "SC_SERVER_DATA";
    /// One key per daemon, as `label:key` pairs: `laptop:0123…,office:89ab…`.
    pub const DAEMON_KEYS: &str = "SC_SERVER_DAEMON_KEYS";
    /// A single daemon's key. Superseded by [`DAEMON_KEYS`], and still read: an
    /// install that predates the plural keeps working without a stack edit.
    pub const DAEMON_KEY: &str = "SC_SERVER_DAEMON_KEY";
    /// The key the settings on the volume are sealed with.
    ///
    /// Stays in the environment on purpose: it is what makes a copied
    /// volume inert, and a key stored beside its own ciphertext protects
    /// nothing.
    pub const SECRET_KEY: &str = "SC_SERVER_SECRET_KEY";

    /// Set to turn the public surface on. Everything below is then required.
    /// The GitHub OAuth application's client id. Public — it appears in the URL
    /// The GitHub OAuth application's client secret. **Never leaves this
    /// Who may review work, and for which repositories:
    /// `jamez667:smart-coder|memosy,someone:memosy`.
    ///
    /// **This list is the authorization model.** Nothing is checked anywhere
    /// else, so an entry here is the only thing granting sight of a project's
    /// drafted specs.
    pub const OWNERS: &str = "SC_SERVER_OWNERS";
    /// The repositories the public surface collects for, comma-separated:
    /// `smart-coder,memosy`. Setting this or [`PUBLIC_REPO`] turns the surface
    /// on.
    pub const PUBLIC_REPOS: &str = "SC_SERVER_PUBLIC_REPOS";
    /// A single repository. Superseded by [`PUBLIC_REPOS`], and still read: an
    /// install that predates the plural keeps working without a stack edit, and
    /// setting both is how a second repository is added before this one is
    /// tidied away.
    pub const PUBLIC_REPO: &str = "SC_SERVER_PUBLIC_REPO";
    pub const PUBLIC_BASE_URL: &str = "SC_SERVER_PUBLIC_BASE_URL";
    /// What the masthead calls this site. Defaults to `PUBLIC_REPO`.
    ///
    /// Separate from it because that one is a **routing key** the daemon matches
    /// exactly — a heading wants `owner/repo`, and renaming a heading should not
    /// force a matching `queue add-repo`.
    pub const PUBLIC_SITE_NAME: &str = "SC_SERVER_PUBLIC_SITE_NAME";
    pub const PUBLIC_MAX_LINKS: &str = "SC_SERVER_PUBLIC_MAX_LINKS";
    pub const PUBLIC_SHOW_SPEC: &str = "SC_SERVER_PUBLIC_SHOW_SPEC";
    /// Requests one account may file per rolling 24h — the model-spend ceiling.
    pub const PUBLIC_MAX_DAILY: &str = "SC_SERVER_PUBLIC_MAX_DAILY";
    /// Drafting runs one repository may be sent into per rolling 24h — the cap
    /// on what re-admitting work costs.
    pub const PUBLIC_MAX_DRAFTS: &str = "SC_SERVER_PUBLIC_MAX_DRAFTS";
    /// How many accounts may exist — what the per-account cap rests on.
    pub const PUBLIC_MAX_ACCOUNTS: &str = "SC_SERVER_PUBLIC_MAX_ACCOUNTS";

    pub const MAIL_PROVIDER: &str = "SC_SERVER_MAIL_PROVIDER";
    pub const MAIL_KEY: &str = "SC_SERVER_MAIL_KEY";
    pub const MAIL_FROM: &str = "SC_SERVER_MAIL_FROM";
    pub const MAIL_FROM_NAME: &str = "SC_SERVER_MAIL_FROM_NAME";

    /// Optional. Absent means filings are not screened at all.
    pub const SCREEN_KEY: &str = "SC_SERVER_SCREEN_KEY";
    pub const SCREEN_URL: &str = "SC_SERVER_SCREEN_URL";
    pub const SCREEN_MODEL: &str = "SC_SERVER_SCREEN_MODEL";
}

impl Config {
    /// Read the configuration from the process environment.
    pub fn from_env() -> std::result::Result<Config, String> {
        Config::from_vars(|k| std::env::var(k).ok())
    }

    /// Read from an arbitrary lookup — the seam every test uses, so no test has
    /// to mutate the process environment and race every other test.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> std::result::Result<Config, String> {
        let daemon_keys = daemon_keys(&get)?;

        // `opt` for the same reason as `bind` and `data_dir` below: a stack
        // editor passes unconfigured settings through as empty strings, and an
        // empty port is "not set", not a parse error.
        let port = match opt(&get, env::PORT) {
            Some(p) => p
                .trim()
                .parse::<u16>()
                .map_err(|_| format!("{} must be a port number 1-65535, got {p:?}", env::PORT))?,
            None => 8420,
        };
        if port == 0 {
            return Err(format!("{} must not be 0", env::PORT));
        }

        Ok(Config {
            // `opt`, not `get`: **an empty value must mean "unset"**, not
            // "this is the value". A Compose file written for a stack editor
            // passes every setting through as `${NAME:-}`, so unconfigured ones
            // arrive as empty strings rather than as absent — and an empty
            // `SC_SERVER_DATA` overrides the image's own `/data` default, after
            // which the server tries to write to `/` and dies with a bare
            // "Permission denied" that names nothing.
            bind: opt(&get, env::BIND).unwrap_or_else(|| "0.0.0.0".to_string()),
            port,
            data_dir: opt(&get, env::DATA_DIR)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/data")),
            daemon_keys,
            // Parsed here rather than where it is used, so a malformed key
            // is one startup error naming the setting instead of a failure
            // to open every secret, which reads as "nothing is configured".
            seal_key: match opt(&get, env::SECRET_KEY) {
                Some(raw) => Some(
                    crate::seal::SealKey::parse(&raw)
                        .map_err(|e| format!("{}: {e}", env::SECRET_KEY))?,
                ),
                None => None,
            },
            // Read again here rather than threaded out of `public_from`, which
            // returns `None` when the public surface is off — and the switch is
            // meaningless in that case anyway, since nothing sends mail.
            public: public_from(&get)?,
        })
    }

    /// The address to bind.
    pub fn addr(&self) -> String {
        format!("{}:{}", self.bind, self.port)
    }
}

/// Read a numeric limit, or its default.
///
/// **Zero is refused.** A cap of nought is a public surface that silently
/// accepts nothing, which reads to the operator as a broken feature rather than
/// a setting — and "turn it off" is expressed by not setting `PUBLIC_REPO` at
/// all, which turns the whole surface off honestly.
fn count(
    get: &impl Fn(&str) -> Option<String>,
    key: &str,
    default: usize,
) -> std::result::Result<usize, String> {
    let Some(raw) = opt(get, key) else {
        return Ok(default);
    };
    let n: usize = raw
        .parse()
        .map_err(|_| format!("{key} must be a whole number, got {raw:?}"))?;
    if n == 0 {
        return Err(format!(
            "{key} must not be 0 — that would accept nothing while looking \
             configured. To turn the public surface off, leave both {} and {} \
             unset.",
            env::PUBLIC_REPOS,
            env::PUBLIC_REPO
        ));
    }
    Ok(n)
}

/// The daemon credentials, from either setting.
///
/// **Both are read, and that is the migration.** A deployment holding only
/// [`env::DAEMON_KEY`] keeps working — its key is folded in under
/// [`DEFAULT_DAEMON_LABEL`] — so upgrading the server never requires touching
/// every daemon on the same afternoon. Setting both is legal and useful: it is
/// how a second machine is added before the first one is migrated.
///
/// Neither is the same refusal to start as before. Running open is not a
/// degraded mode; it is the failure this whole design exists to prevent.
fn daemon_keys(
    get: &impl Fn(&str) -> Option<String>,
) -> std::result::Result<Vec<DaemonKey>, String> {
    let mut keys: Vec<(String, String)> = Vec::new();

    if let Some(raw) = opt(get, env::DAEMON_KEYS) {
        for pair in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            // `split_once`, not `split`: a key is opaque and may itself contain
            // a colon, so only the *first* one separates.
            let Some((label, key)) = pair.split_once(':') else {
                return Err(format!(
                    "{} entries are `label:key`, but {pair:?} has no colon. \
                     For example: laptop:0123…,office:89ab…",
                    env::DAEMON_KEYS
                ));
            };
            keys.push((label.trim().to_string(), key.trim().to_string()));
        }
    }

    if let Some(key) = opt(get, env::DAEMON_KEY) {
        keys.push((DEFAULT_DAEMON_LABEL.to_string(), key.trim().to_string()));
    }

    if keys.is_empty() {
        return Err(format!(
            "{} is required — set it to `label:key` pairs, one per daemon \
             (for example laptop:0123…,office:89ab…), with keys of 32+ random \
             characters. {} still works for a single daemon. Without either, the \
             server would accept work from anyone.",
            env::DAEMON_KEYS,
            env::DAEMON_KEY
        ));
    }

    let mut out: Vec<DaemonKey> = Vec::new();
    for (label, key) in keys {
        if label.is_empty() {
            return Err(format!(
                "a {} entry has an empty label. Name the machine, so the log and \
                 the review page can say which one it was.",
                env::DAEMON_KEYS
            ));
        }
        // A short key is worse than no key: it looks configured while being
        // guessable, which is the failure mode nobody notices.
        if key.len() < MIN_SECRET_LEN {
            return Err(format!(
                "the key for {label:?} is only {} characters. Use at least \
                 {MIN_SECRET_LEN} — a short key looks configured while being \
                 guessable.",
                key.len()
            ));
        }
        // Refused rather than resolved. A duplicate label makes a claim's holder
        // ambiguous, and deleting one entry to revoke a machine would silently
        // leave the other working — the exact property per-daemon keys exist to
        // provide.
        if out.iter().any(|d| d.label == label) {
            return Err(format!(
                "two daemons are both labelled {label:?}. Labels have to be \
                 unique, or revoking one machine would not revoke it."
            ));
        }
        let key_hash = crate::auth::hash(&key);
        if let Some(other) = out.iter().find(|d| d.key_hash == key_hash) {
            return Err(format!(
                "{:?} and {label:?} share one key. Give each machine its own, or \
                 revoking either leaves the other able to claim work.",
                other.label
            ));
        }
        out.push(DaemonKey { label, key_hash });
    }
    Ok(out)
}

/// The longest repository name accepted.
///
/// The daemon matches these byte-for-byte against its own `queue add-repo`
/// names, so a name nobody could plausibly have typed there is a typo — and a
/// long one would also be rendered into a picker on every page.
///
/// Shared with the admin route that enables one, rather than copied: the two
/// write into the same set, and a bound enforced in one place is a bound the
/// other can be used to get around.
pub const MAX_REPO_NAME: usize = 128;

/// The repositories the public surface collects for, from either setting.
///
/// **Both are read, and that is the migration** — the same shape as
/// [`daemon_keys`]. A deployment holding only [`env::PUBLIC_REPO`] keeps
/// working, so adding a second repository never requires rewriting the first
/// setting, and setting both is how one is added before the stack entry is
/// tidied away.
///
/// `None` means the public surface is off, which is the switch this setting has
/// always been — now with two spellings.
///
/// Duplicates are **refused, not deduplicated**. A repeated name is a typo or a
/// misunderstanding about what the picker will show, and quietly collapsing it
/// renders a form whose option list is shorter than what the operator wrote —
/// which reads as the setting having been ignored.
fn public_repos(
    get: &impl Fn(&str) -> Option<String>,
) -> std::result::Result<Option<Repos>, String> {
    let plural: Vec<String> = opt(get, env::PUBLIC_REPOS)
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let mut out: Vec<String> = Vec::new();
    for name in plural {
        if name.len() > MAX_REPO_NAME {
            return Err(format!(
                "a {} entry is {} characters. The daemon matches this name \
                 exactly against its own `queue add-repo` names, so this is a \
                 typo rather than a repository.",
                env::PUBLIC_REPOS,
                name.len()
            ));
        }
        // **Within this one setting only.** A name written twice in the same
        // list is a typo, and collapsing it silently would render a picker
        // shorter than what the operator wrote.
        if out.contains(&name) {
            return Err(format!(
                "{name:?} is listed twice in {}. Repository names have to be \
                 unique, or the picker would offer the same one twice.",
                env::PUBLIC_REPOS
            ));
        }
        out.push(name);
    }

    // The singular setting folds in, and **a name already in the plural is not
    // an error** — it is the migration working. Setting both is how a second
    // repository is added before the first entry is tidied away, which this
    // file recommends; refusing the overlap would refuse exactly the state that
    // advice produces. It cost a live server a start-up before it was noticed.
    if let Some(one) = opt(get, env::PUBLIC_REPO) {
        let one = one.trim().to_string();
        if !one.is_empty() && !out.contains(&one) {
            out.push(one);
        }
    }

    if out.is_empty() {
        return Ok(None);
    }
    Ok(Some(Repos(out)))
}

/// Who may review work, and for what.
///
/// `login:repo|repo`, comma-separated between owners:
///
/// ```text
/// SC_SERVER_OWNERS=jamez667:smart-coder|memosy,someone:memosy
/// ```
///
/// **`|` inside an entry and `,` between them**, rather than commas throughout.
/// With one separator doing both jobs a parser would have to guess whether the
/// next token started a new owner by looking for a colon — exactly the ambiguity
/// `split_once(':')` was chosen to avoid.
///
/// Every failure here is a refusal to start, because this list *is* the
/// authorization model: a setting that looks applied and grants the wrong thing
/// is worse than a server that will not boot.
fn owners(
    get: &impl Fn(&str) -> Option<String>,
    repos: &Repos,
) -> std::result::Result<Vec<Owner>, String> {
    let Some(raw) = opt(get, env::OWNERS) else {
        return Ok(Vec::new());
    };

    let mut out: Vec<Owner> = Vec::new();
    for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let Some((login, names)) = entry.split_once(':') else {
            return Err(format!(
                "{} entries are `login:repo|repo`, but {entry:?} has no colon. \
                 For example: jamez667:smart-coder|memosy",
                env::OWNERS
            ));
        };
        // Lowercased on the way in so a setting written `Jamez667` matches a
        // name registered as `jamez667`, rather than silently granting nothing.
        let login = login.trim().to_ascii_lowercase();
        if login.is_empty() {
            return Err(format!(
                "a {} entry has no login before its colon: {entry:?}",
                env::OWNERS
            ));
        }

        let owned: Vec<String> = names
            .split('|')
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(str::to_string)
            .collect();
        if owned.is_empty() {
            return Err(format!(
                "{login:?} is an owner of no repositories. Name at least one \
                 after the colon, or leave them out of {} — an owner who may see \
                 nothing signs in successfully and looks at a blank page.",
                env::OWNERS
            ));
        }
        for repo in &owned {
            if !repos.accepts(repo) {
                return Err(format!(
                    "{login:?} is an owner of {repo:?}, which this surface does \
                     not serve. Add it to {} or fix the spelling — otherwise the \
                     setting looks applied and grants nothing.",
                    env::PUBLIC_REPOS
                ));
            }
        }
        // Refused rather than merged. Two entries for one person is a mistake,
        // and merging them would mean deleting one to revoke somebody left them
        // with access from the other.
        if out.iter().any(|o| o.login == login) {
            return Err(format!(
                "{login:?} is listed twice in {}. One entry per owner, or \
                 removing one would not remove them.",
                env::OWNERS
            ));
        }
        out.push(Owner {
            login,
            repos: owned,
        });
    }
    Ok(out)
}

/// Read a trimmed, non-blank setting. Blank is *absent*, not present-and-empty.
fn opt(get: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    get(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Build the public surface's configuration, or `None` if it was not asked for.
///
/// **Refuses to start half-public.** `SC_SERVER_PUBLIC_REPO` is the switch; once
/// it is set, everything the surface needs must be present. A server that
/// accepted filings but could not send a sign-in link would look configured
/// while being broken, which is the failure nobody notices until a stranger
/// reports that nothing arrived.
fn public_from(
    get: &impl Fn(&str) -> Option<String>,
) -> std::result::Result<Option<PublicConfig>, String> {
    let Some(repos) = public_repos(get)? else {
        // Not asked for. Every other public setting is ignored rather than
        // half-applied.
        return Ok(None);
    };

    let base_url = opt(get, env::PUBLIC_BASE_URL).ok_or_else(|| {
        format!(
            "{} is required when {} is set: a sign-in link needs an absolute URL, \
             and it must not be inferred from the Host header — that is \
             attacker-controlled, so a link built from it would send the filer to \
             whatever host the request claimed to be.",
            env::PUBLIC_BASE_URL,
            env::PUBLIC_REPOS
        )
    })?;
    check_base_url(&base_url).map_err(|e| format!("{}: {e}", env::PUBLIC_BASE_URL))?;

    // Read before the struct so the GitHub application can be checked against
    // it: owners with no way to sign in is a setting that looks applied and
    // does nothing.
    let owned = owners(get, &repos)?;

    Ok(Some(PublicConfig {
        // Defaults to the routing key, so an operator who does not care sees a
        // sensible heading without configuring a second thing.
        // Defaulted only when there is one repository to default *to*. With
        // several, a joined heading would read `smart-coder · memosy` on the
        // landing and sign-in pages as well, where nothing has been chosen and
        // the join says nothing — so the operator is made to name their site.
        site_name: match opt(get, env::PUBLIC_SITE_NAME) {
            Some(name) => name,
            // "Is there exactly one" and "here it is" folded into a single
            // match on the `Option`, so there is no branch left where the two
            // could disagree and panic.
            None if repos.is_single() => match repos.first() {
                Some(only) => only.to_string(),
                None => String::new(),
            },
            None => {
                return Err(format!(
                    "{} is required when {} names more than one repository: the \
                     masthead appears on every page, including the ones reached \
                     before a repository is chosen, so it cannot be derived from \
                     the set.",
                    env::PUBLIC_SITE_NAME,
                    env::PUBLIC_REPOS,
                ))
            }
        },
        // Validated against the set below, so an owner of a repository this
        // surface does not serve is caught here rather than at their first
        // sign-in. Read before `repos` is moved into the struct.
        owners: owned.clone(),
        repos,
        // Computed before `base_url` is moved, and from the same value the
        // `https://` check above ran on.
        secure_cookies: !is_private_host(&base_url),
        base_url: base_url.trim_end_matches('/').to_string(),
        // **Optional, where it used to be required.** A surface with no mail
        // provider serves and says sign-in is unavailable, the same way one
        // with no repositories enabled says it cannot take a filing.
        mail: mail_from(get)?,
        screen: screen_from(get)?,
        max_outstanding_links: count(get, env::PUBLIC_MAX_LINKS, DEFAULT_MAX_OUTSTANDING_LINKS)?,
        max_daily_filings: count(get, env::PUBLIC_MAX_DAILY, DEFAULT_MAX_DAILY_FILINGS)?,
        max_daily_drafts: count(get, env::PUBLIC_MAX_DRAFTS, DEFAULT_MAX_DAILY_DRAFTS)?,
        max_accounts: count(get, env::PUBLIC_MAX_ACCOUNTS, DEFAULT_MAX_ACCOUNTS)?,
        // On unless explicitly turned off.
        show_spec: opt(get, env::PUBLIC_SHOW_SPEC)
            .map(|v| {
                !matches!(
                    v.to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
            })
            .unwrap_or(true),
    }))
}

fn mail_from(
    get: &impl Fn(&str) -> Option<String>,
) -> std::result::Result<Option<MailConfig>, String> {
    // **No provider is not an error.** It seeds a surface that cannot send
    // sign-in links yet and says so on the page, the same way one with no
    // repositories enabled says it cannot take a filing. Refusing to start
    // would put the settings page that fixes it out of reach — and it is
    // reachable, because the administrator signs in with a password rather
    // than by email.
    let Some(raw) = opt(get, env::MAIL_PROVIDER) else {
        return Ok(None);
    };
    let provider = Provider::parse(&raw).ok_or_else(|| {
        format!(
            "unknown mail provider {raw:?}. One of: {}.",
            Provider::ALL
                .iter()
                .map(|p| p.slug())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;

    let api_key = opt(get, env::MAIL_KEY)
        .ok_or_else(|| format!("{} is required to send sign-in links", env::MAIL_KEY))?;
    let from = opt(get, env::MAIL_FROM).ok_or_else(|| {
        format!(
            "{} is required: mail needs a sender address",
            env::MAIL_FROM
        )
    })?;

    Ok(Some(MailConfig {
        provider,
        api_key,
        from,
        from_name: opt(get, env::MAIL_FROM_NAME)
            .unwrap_or_else(|| DEFAULT_MAIL_FROM_NAME.to_string()),
    }))
}

fn screen_from(
    get: &impl Fn(&str) -> Option<String>,
) -> std::result::Result<Option<ScreenConfig>, String> {
    // Absent is a legitimate choice: file straight to the queue and rely on the
    // account gate. Pretending to screen would be worse.
    let Some(api_key) = opt(get, env::SCREEN_KEY) else {
        return Ok(None);
    };
    if api_key.len() < MIN_SECRET_LEN {
        return Err(format!(
            "{} is only {} characters. Use at least {MIN_SECRET_LEN} — a short key \
             looks configured while being guessable.",
            env::SCREEN_KEY,
            api_key.len()
        ));
    }
    Ok(Some(ScreenConfig {
        api_key,
        url: opt(get, env::SCREEN_URL)
            .unwrap_or_else(|| DEFAULT_SCREEN_URL.to_string())
            .trim_end_matches('/')
            .to_string(),
        model: opt(get, env::SCREEN_MODEL).unwrap_or_else(|| DEFAULT_SCREEN_MODEL.to_string()),
    }))
}

/// The host part of a URL, without scheme, port or path.
///
/// Handles the bracketed form (`http://[::1]:8420`), because splitting an IPv6
/// authority on `:` otherwise returns an empty host and quietly fails open.
/// Is this an address sign-in links can safely be built from?
///
/// **One function, two callers**: this runs at startup on the environment and
/// again on the setup form, so the rule cannot hold in one place and not the
/// other. A base URL entered through a page is the same credential-bearing
/// address as one set in a stack.
pub fn check_base_url(base_url: &str) -> std::result::Result<(), String> {
    let base_url = base_url.trim();
    if base_url.is_empty() {
        return Err("an absolute address is required - a sign-in link cannot be                     built from the Host header, which is attacker-controlled"
            .to_string());
    }
    if !base_url.starts_with("https://") && !base_url.starts_with("http://") {
        return Err("an absolute address, starting https://".to_string());
    }
    if !base_url.starts_with("https://") && !is_private_host(base_url) {
        return Err(
            "must be https:// - a sign-in link is a credential in a URL, and plain              HTTP puts it in the clear. (http://localhost is allowed for trying it              locally.)"
                .to_string(),
        );
    }
    Ok(())
}

/// Will cookies carry `Secure` for this address?
///
/// Derived, never configured. "Is this a private network" is a question people
/// answer wrong, and answering it wrong drops `Secure` from every session
/// cookie silently. Exposed so the setup form can *show* what it decided rather
/// than asking.
pub fn secure_for(base_url: &str) -> bool {
    !is_private_host(base_url.trim())
}

/// A masthead for an address nobody has named.
///
/// The host, stripped of scheme and port. A label is cosmetic, so falling back
/// to something plain beats refusing to serve — which is what the environment
/// path did when several repositories made the old default ambiguous.
pub fn host_label(base_url: &str) -> String {
    host_of(base_url.trim()).to_string()
}

fn host_of(url: &str) -> &str {
    let authority = url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or("");
    match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    }
}

/// Is this URL somewhere a plain-HTTP sign-in link cannot leak from?
///
/// **Loopback, or a private network address.** The rule this relaxes is that a
/// sign-in link is a credential in a URL, so plain HTTP puts it in the clear —
/// true on the internet, and not true on a link that cannot be routed off the
/// network it was issued on. Somebody running this on their own LAN to try it
/// is not exposing anything to anyone who was not already on that LAN.
///
/// The ranges are the private ones from RFC 1918 and RFC 4193, plus link-local:
/// `10/8`, `172.16/12`, `192.168/16`, `169.254/16`, and IPv6 `fc00::/7` and
/// `fe80::/10`. Anything else — a public IP, a hostname, a DNS name that happens
/// to resolve privately — is treated as the internet, because this function only
/// sees a string and a name is not a promise about where it points.
fn is_private_host(url: &str) -> bool {
    let host = host_of(url);
    if matches!(host, "localhost" | "127.0.0.1" | "::1") {
        return true;
    }

    // IPv4 private ranges. Parsed rather than prefix-matched: "10.0.0.1" is
    // private and "100.0.0.1" is not, and `starts_with("10.")` gets that right
    // only by accident of the dot.
    let octets: Vec<u8> = host
        .split('.')
        .filter_map(|o| o.parse::<u8>().ok())
        .collect();
    if octets.len() == 4 && host.split('.').count() == 4 {
        return match (octets[0], octets[1]) {
            (10, _) => true,
            (172, b) => (16..=31).contains(&b),
            (192, 168) => true,
            // Link-local, which is what a machine gives itself with no DHCP.
            (169, 254) => true,
            _ => false,
        };
    }

    // IPv6 unique-local (fc00::/7) and link-local (fe80::/10).
    let lower = host.to_ascii_lowercase();
    lower.starts_with("fc") || lower.starts_with("fd") || lower.starts_with("fe80:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn load(pairs: &[(&str, &str)]) -> std::result::Result<Config, String> {
        let map = vars(pairs);
        Config::from_vars(|k| map.get(k).cloned())
    }

    const GOOD_KEY: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn the_defaults_are_what_a_container_needs() {
        let cfg = load(&[(env::DAEMON_KEY, GOOD_KEY)]).unwrap();
        // All interfaces: inside a container, loopback is unreachable from
        // outside it, so binding loopback would make the image useless.
        assert_eq!(cfg.bind, "0.0.0.0");
        assert_eq!(cfg.port, 8420);
        // One directory, which is the one volume a Portainer user mounts.
        assert_eq!(cfg.data_dir, PathBuf::from("/data"));
        assert_eq!(cfg.addr(), "0.0.0.0:8420");
    }

    #[test]
    fn the_server_refuses_to_start_without_a_daemon_key() {
        // Running open is not a degraded mode, it is the failure this design
        // exists to prevent. `sc-web`'s `--no-token` has no equivalent here.
        let err = load(&[]).unwrap_err();
        assert!(err.contains(env::DAEMON_KEY), "{err}");
        assert!(err.contains("accept work from anyone"), "{err}");

        // Blank and whitespace are absent, not present-but-empty.
        assert!(load(&[(env::DAEMON_KEY, "")]).is_err());
        assert!(load(&[(env::DAEMON_KEY, "   ")]).is_err());
    }

    #[test]
    fn a_short_key_is_refused_rather_than_accepted_quietly() {
        // Worse than no key: it looks configured while being guessable, and
        // nobody notices until it matters.
        let err = load(&[(env::DAEMON_KEY, "hunter2")]).unwrap_err();
        assert!(err.contains("at least 32"), "{err}");
    }

    const OTHER_KEY: &str = "fedcba9876543210fedcba9876543210";

    /// The public settings, with the repositories named by the plural setting.
    fn public_vars_multi() -> Vec<(&'static str, &'static str)> {
        vec![
            (env::DAEMON_KEY, GOOD_KEY),
            (env::PUBLIC_REPOS, "intake,memosy"),
            (env::PUBLIC_SITE_NAME, "two things"),
            (env::PUBLIC_BASE_URL, "https://specs.example.com"),
            (env::MAIL_PROVIDER, "brevo"),
            (env::MAIL_KEY, GOOD_KEY),
            (env::MAIL_FROM, "noreply@example.com"),
        ]
    }

    #[test]
    fn several_repositories_are_read_from_one_setting() {
        let cfg = load(&public_vars_multi()).unwrap();
        let p = cfg.public.expect("configured");
        assert_eq!(p.repos.names(), ["intake", "memosy"]);
        assert!(!p.repos.is_single());
        // The operator's order, because it is the order the picker offers.
        assert_eq!(p.repos.first(), Some("intake"));
    }

    #[test]
    fn an_old_deployment_naming_one_repository_still_works() {
        // The migration: an install predating the plural keeps working
        // untouched, or upgrading the server means editing every stack on the
        // same afternoon.
        let cfg = load(&public_vars()).unwrap();
        let p = cfg.public.expect("configured");
        assert_eq!(p.repos.names(), ["intake"]);
        // And the masthead still defaults to it, since there is one to take.
        assert_eq!(p.site_name, "intake");
    }

    #[test]
    fn both_repository_settings_together_are_a_union() {
        // How a second repository is added before the old setting is tidied
        // away — the same migration shape as the daemon keys.
        let mut vars = public_vars();
        vars.push((env::PUBLIC_REPOS, "memosy"));
        vars.push((env::PUBLIC_SITE_NAME, "two things"));
        let cfg = load(&vars).unwrap();
        assert_eq!(
            cfg.public.unwrap().repos.names(),
            ["memosy", "intake"],
            "the plural is read first, then the singular folds in"
        );
    }

    #[test]
    fn only_a_nominated_repository_is_accepted() {
        // The check a stranger's form submission is measured against. Exact and
        // case-sensitive, matching how the daemon resolves the same name.
        let repos = Repos::new(&["intake", "memosy"]);
        assert!(repos.accepts("intake"));
        assert!(repos.accepts("memosy"));
        for near_miss in ["Intake", "intak", "intakex", " intake", "secret-repo"] {
            assert!(!repos.accepts(near_miss), "{near_miss:?} is not nominated");
        }
    }

    #[test]
    fn the_singular_setting_overlapping_the_plural_is_the_migration_working() {
        // **This took a live server down.** The migration advice is to set both
        // while moving across — and the duplicate check then refused exactly
        // the state that advice produces, so the server would not boot on a
        // configuration its own documentation recommends.
        //
        // An overlap between the two settings is not a typo. It is one name
        // written in two places by somebody following the instructions.
        let mut vars = public_vars_multi();
        vars[1] = (env::PUBLIC_REPOS, "intake,memosy");
        vars.push((env::PUBLIC_REPO, "intake"));

        let p = load(&vars).unwrap().public.unwrap();
        assert_eq!(
            p.repos.names(),
            ["intake", "memosy"],
            "the overlap collapses rather than refusing"
        );
    }

    #[test]
    fn the_singular_setting_still_adds_a_repository_the_plural_omits() {
        // The other half of the union, which must keep working: a name only in
        // the singular is still served.
        let mut vars = public_vars_multi();
        vars[1] = (env::PUBLIC_REPOS, "memosy");
        vars.push((env::PUBLIC_REPO, "intake"));
        let p = load(&vars).unwrap().public.unwrap();
        assert_eq!(p.repos.names(), ["memosy", "intake"]);
    }

    #[test]
    fn a_repository_listed_twice_is_refused() {
        // Refused rather than deduplicated: quietly collapsing it renders a
        // picker shorter than what the operator wrote, which reads as the
        // setting having been ignored.
        let mut vars = public_vars_multi();
        vars[1] = (env::PUBLIC_REPOS, "intake,intake");
        let err = load(&vars).unwrap_err();
        assert!(err.contains("twice"), "{err}");
    }

    #[test]
    fn blank_entries_between_commas_are_skipped() {
        let mut vars = public_vars_multi();
        vars[1] = (env::PUBLIC_REPOS, "intake, ,memosy,");
        let cfg = load(&vars).unwrap();
        assert_eq!(cfg.public.unwrap().repos.names(), ["intake", "memosy"]);
    }

    #[test]
    fn naming_no_repository_leaves_the_public_surface_off() {
        // Blank is absent, not present-and-empty — a stack editor passes
        // unconfigured settings through as empty strings.
        let mut vars = public_vars_multi();
        vars[1] = (env::PUBLIC_REPOS, "  ");
        assert!(load(&vars).unwrap().public.is_none());
    }

    #[test]
    fn several_repositories_require_the_site_to_be_named() {
        // The masthead appears on pages reached before any repository is
        // chosen, so it cannot be derived from the set.
        let mut vars = public_vars_multi();
        vars.retain(|(k, _)| *k != env::PUBLIC_SITE_NAME);
        let err = load(&vars).unwrap_err();
        assert!(err.contains(env::PUBLIC_SITE_NAME), "{err}");
    }

    #[test]
    fn a_repository_name_nobody_could_have_typed_is_refused() {
        let entry = format!("intake,{}", "a".repeat(129));
        let mut vars = public_vars_multi();
        vars[1] = (env::PUBLIC_REPOS, Box::leak(entry.into_boxed_str()));
        let err = load(&vars).unwrap_err();
        assert!(err.contains("typo"), "{err}");
    }

    #[test]
    fn owners_are_read_with_the_repositories_they_own() {
        let mut vars = public_vars_multi();
        vars.push((env::OWNERS, "jamez667:intake|memosy,someone:memosy"));
        let p = load(&vars).unwrap().public.unwrap();

        assert_eq!(p.owners.len(), 2);
        assert_eq!(p.owners[0].login, "jamez667");
        assert_eq!(p.owners[0].repos, ["intake", "memosy"]);
        assert!(p.owners[1].owns("memosy"));
        assert!(!p.owners[1].owns("intake"), "not theirs");
    }

    #[test]
    fn an_owners_login_is_lowercased_so_the_setting_matches_the_account() {
        // A setting is written the way a person types a name; the account was
        // registered however its holder typed it. The two must still match, or
        // the setting grants nothing while looking applied.
        let mut vars = public_vars_multi();
        vars.push((env::OWNERS, "JameZ667:intake"));
        let p = load(&vars).unwrap().public.unwrap();
        assert_eq!(p.owners[0].login, "jamez667");
    }

    #[test]
    fn an_owner_of_a_repository_this_surface_does_not_serve_is_refused() {
        // The allowlist IS the authorization model, so a typo in it must not be
        // a silently-empty view somebody investigates weeks later.
        let mut vars = public_vars_multi();
        vars.push((env::OWNERS, "jamez667:intake|typo-repo"));
        let err = load(&vars).unwrap_err();
        assert!(err.contains("does not serve"), "{err}");
        assert!(
            err.contains("typo-repo"),
            "it names the one at fault: {err}"
        );
    }

    #[test]
    fn an_owner_of_no_repositories_is_refused() {
        // Fails closed: somebody who signs in successfully and sees a blank page
        // has no way to tell that from a bug.
        let mut vars = public_vars_multi();
        vars.push((env::OWNERS, "jamez667:"));
        let err = load(&vars).unwrap_err();
        assert!(err.contains("no repositories"), "{err}");
    }

    #[test]
    fn an_owner_listed_twice_is_refused() {
        // Merging them would mean deleting one entry to revoke somebody leaves
        // them with access from the other.
        let mut vars = public_vars_multi();
        vars.push((env::OWNERS, "jamez667:intake,jamez667:memosy"));
        let err = load(&vars).unwrap_err();
        assert!(err.contains("twice"), "{err}");
    }

    #[test]
    fn an_owner_entry_without_a_colon_says_what_the_shape_is() {
        let mut vars = public_vars_multi();
        vars.push((env::OWNERS, "jamez667"));
        let err = load(&vars).unwrap_err();
        assert!(err.contains("login:repo"), "{err}");
    }

    #[test]
    fn no_owners_is_the_resting_state() {
        // Every deployment until an operator names somebody, and the surface
        // works exactly as it did before the role existed.
        let p = load(&public_vars()).unwrap().public.unwrap();
        assert!(p.owners.is_empty());
    }

    #[test]
    fn several_daemon_keys_are_read_from_one_setting() {
        let cfg = load(&[(
            env::DAEMON_KEYS,
            &format!("laptop:{GOOD_KEY},office:{OTHER_KEY}"),
        )])
        .unwrap();

        let labels: Vec<&str> = cfg.daemon_keys.iter().map(|d| d.label.as_str()).collect();
        assert_eq!(labels, ["laptop", "office"]);
        // Hashed on the way in, so the key itself is not sitting in the config
        // for a debug print or a panic message to hand out.
        assert_eq!(cfg.daemon_keys[0].key_hash, crate::auth::hash(GOOD_KEY));
        let debug = format!("{cfg:?}");
        assert!(!debug.contains(GOOD_KEY), "the key is in {debug}");
    }

    #[test]
    fn the_single_setting_still_works_and_is_labelled_default() {
        // The migration: an install that predates the plural must upgrade
        // without a stack edit, or a server upgrade means touching every daemon
        // on the same afternoon.
        let cfg = load(&[(env::DAEMON_KEY, GOOD_KEY)]).unwrap();
        assert_eq!(cfg.daemon_keys.len(), 1);
        assert_eq!(cfg.daemon_keys[0].label, DEFAULT_DAEMON_LABEL);
    }

    #[test]
    fn both_settings_together_are_a_union_so_a_daemon_can_be_added_first() {
        // How the migration is actually performed: add the new machine under
        // the plural setting while the old one still runs on the singular.
        let cfg = load(&[
            (env::DAEMON_KEYS, &format!("office:{OTHER_KEY}")),
            (env::DAEMON_KEY, GOOD_KEY),
        ])
        .unwrap();
        let labels: Vec<&str> = cfg.daemon_keys.iter().map(|d| d.label.as_str()).collect();
        assert_eq!(labels, ["office", DEFAULT_DAEMON_LABEL]);
    }

    #[test]
    fn a_duplicate_label_is_refused_because_revocation_would_be_ambiguous() {
        // Two machines under one name means deleting the entry to revoke one
        // silently leaves the other working — the exact property per-daemon
        // keys exist to provide.
        let err = load(&[(
            env::DAEMON_KEYS,
            &format!("laptop:{GOOD_KEY},laptop:{OTHER_KEY}"),
        )])
        .unwrap_err();
        assert!(err.contains("unique"), "{err}");
    }

    #[test]
    fn two_labels_sharing_one_key_is_refused() {
        // Revoking either would leave the other able to claim work, so the two
        // labels would be a fiction.
        let err = load(&[(
            env::DAEMON_KEYS,
            &format!("laptop:{GOOD_KEY},office:{GOOD_KEY}"),
        )])
        .unwrap_err();
        assert!(err.contains("share one key"), "{err}");
    }

    #[test]
    fn a_short_key_in_the_list_is_refused_like_a_short_singular_one() {
        let err = load(&[(env::DAEMON_KEYS, "laptop:hunter2")]).unwrap_err();
        assert!(err.contains("at least 32"), "{err}");
        assert!(err.contains("laptop"), "the machine is named: {err}");
    }

    #[test]
    fn an_entry_without_a_colon_says_what_the_shape_is() {
        let err = load(&[(env::DAEMON_KEYS, GOOD_KEY)]).unwrap_err();
        assert!(err.contains("label:key"), "{err}");
    }

    #[test]
    fn a_key_may_contain_a_colon_because_only_the_first_one_separates() {
        // The key is opaque. Splitting on every colon would corrupt a perfectly
        // good credential rather than refusing it, which is worse.
        let key = format!("{GOOD_KEY}:with:colons");
        let cfg = load(&[(env::DAEMON_KEYS, &format!("laptop:{key}"))]).unwrap();
        assert_eq!(cfg.daemon_keys[0].key_hash, crate::auth::hash(&key));
    }

    #[test]
    fn neither_setting_is_still_a_refusal_to_start() {
        // Unchanged by the plural: with no key at all the server must not run.
        let err = load(&[(env::DAEMON_KEYS, "")]).unwrap_err();
        assert!(err.contains("accept work from anyone"), "{err}");
    }

    #[test]
    fn every_setting_is_overridable_from_the_environment() {
        // A Portainer stack editor sets environment variables; anything not
        // settable that way is not configurable in practice.
        let cfg = load(&[
            (env::DAEMON_KEY, GOOD_KEY),
            (env::BIND, "127.0.0.1"),
            (env::PORT, "9000"),
            (env::DATA_DIR, "/srv/state"),
        ])
        .unwrap();
        assert_eq!(cfg.addr(), "127.0.0.1:9000");
        assert_eq!(cfg.data_dir, PathBuf::from("/srv/state"));
    }

    #[test]
    fn a_bad_port_is_a_clear_error_not_a_silent_default() {
        // Falling back to the default would leave the container listening
        // somewhere the user did not ask for, which they discover by the service
        // being unreachable.
        //
        // **Blank is not in this list**, and the distinction is the point: a
        // *typo* is a value that was meant to be a port and is not, while an
        // empty string is an unfilled box in a stack editor. Compose passes
        // every unconfigured setting through as `${NAME:-}`, so refusing blanks
        // here would make a bare deploy fail to start. See
        // `an_empty_value_means_unset_for_every_setting`.
        for bad in ["http", "70000", "-1", "8420 8421", "84.20"] {
            let err = load(&[(env::DAEMON_KEY, GOOD_KEY), (env::PORT, bad)]).unwrap_err();
            assert!(err.contains(env::PORT), "{bad:?}: {err}");
        }
        // Whitespace is blank too — a box somebody typed a space into.
        assert_eq!(
            load(&[(env::DAEMON_KEY, GOOD_KEY), (env::PORT, "  ")])
                .expect("whitespace is unset")
                .port,
            8420
        );
    }

    /// The minimum a working public surface needs.
    fn public_vars() -> Vec<(&'static str, &'static str)> {
        vec![
            (env::DAEMON_KEY, GOOD_KEY),
            (env::PUBLIC_REPO, "intake"),
            (env::PUBLIC_BASE_URL, "https://specs.example.com"),
            (env::MAIL_PROVIDER, "brevo"),
            (env::MAIL_KEY, GOOD_KEY),
            (env::MAIL_FROM, "noreply@example.com"),
        ]
    }

    #[test]
    fn the_portainer_stack_file_documents_every_setting() {
        // The deployment file is how a Portainer user learns this server exists
        // at all — it is the only documentation most installs will ever get.
        // Nothing else notices when it falls behind: the server starts fine
        // without a setting being *mentioned*, so the drift is silent and the
        // symptom is somebody not knowing a cap exists until it bites.
        //
        // Checked **both ways**. A missing setting is undocumented; a setting
        // named in the file but unknown to the server is worse, because a user
        // will paste it, see no error, and believe it took effect.
        // **Both deployment files.** They are separate because Compose and
        // Swarm want different shapes, which means a setting can be added to
        // one and forgotten in the other — and an operator on the Swarm stack
        // would then have no box for a cap that exists.
        let files = [
            (
                "deploy/sc-server.stack.yml",
                include_str!("../../../deploy/sc-server.stack.yml"),
            ),
            (
                "deploy/sc-server.swarm.yml",
                include_str!("../../../deploy/sc-server.swarm.yml"),
            ),
        ];
        let source = include_str!("config.rs");
        let block = source
            .split("pub mod env {")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("the env module is declared in this file");

        let declared: Vec<&str> = block
            .split("pub const ")
            .skip(1)
            .filter_map(|s| s.split('"').nth(1))
            .collect();
        assert!(
            declared.len() > 10,
            "the env names did not parse: {declared:?}"
        );

        for (path, stack) in files {
            for name in &declared {
                assert!(stack.contains(name), "{name} is not mentioned in {path}");
            }
        }

        // Consumed by **Compose**, not by the server: it substitutes the image
        // tag before the container exists. Named individually rather than
        // matched by a pattern, so the next addition has to be a deliberate
        // entry here instead of quietly slipping through a prefix rule.
        const NOT_THE_SERVERS: [&str; 1] = ["SC_SERVER_TAG"];

        // The reverse direction, by scanning each file for anything that looks
        // like one of ours.
        for (path, stack) in files {
            for word in stack.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
                if word.starts_with("SC_SERVER_") && !NOT_THE_SERVERS.contains(&word) {
                    assert!(
                        declared.contains(&word),
                        "{word} is in {path} but the server never reads it"
                    );
                }
            }
        }

        // And the exception list itself must stay honest: an entry that *is* a
        // real setting would silently exempt it from the check above.
        for name in NOT_THE_SERVERS {
            assert!(
                !declared.contains(&name),
                "{name} is a real setting, so it does not belong in NOT_THE_SERVERS"
            );
        }
    }

    // -- console mail, the local-only escape hatch ---------------------------

    #[test]
    fn an_empty_value_means_unset_for_every_setting() {
        // **How a stack editor actually passes configuration.** A Compose file
        // written for Portainer lists every setting as `${NAME:-}` so the box
        // for it exists in the UI, which means unconfigured settings arrive as
        // empty strings rather than as absent.
        //
        // Getting this wrong was not theoretical: an empty SC_SERVER_DATA
        // overrode the image's own `/data`, and the server tried to write to `/`
        // and died with a bare "Permission denied" naming nothing. The others
        // fail more loudly but just as wrongly — an empty port is a parse error.
        let empties: Vec<(&str, &str)> = vec![
            (env::DAEMON_KEY, GOOD_KEY),
            (env::BIND, ""),
            (env::PORT, ""),
            (env::DATA_DIR, ""),
            (env::PUBLIC_REPO, ""),
            (env::PUBLIC_BASE_URL, ""),
            (env::MAIL_PROVIDER, ""),
            (env::MAIL_KEY, ""),
            (env::MAIL_FROM, ""),
            (env::SCREEN_KEY, ""),
            (env::PUBLIC_MAX_DAILY, ""),
            (env::PUBLIC_MAX_ACCOUNTS, ""),
            (env::PUBLIC_MAX_LINKS, ""),
            (env::PUBLIC_SHOW_SPEC, ""),
        ];
        let cfg = load(&empties).expect("every blank is treated as unset");

        // Each falls back to what it would have been with the variable absent.
        assert_eq!(cfg.bind, "0.0.0.0");
        assert_eq!(cfg.port, 8420);
        assert_eq!(cfg.data_dir, PathBuf::from("/data"));
        // And a blank PUBLIC_REPO leaves the public surface off rather than
        // turning it on with an empty repository name.
        assert!(cfg.public.is_none());
    }

    #[test]
    fn a_private_network_address_is_told_apart_from_the_internet() {
        // The rule being relaxed is "plain HTTP puts a sign-in link in the
        // clear", which is true on the internet and not true of a link that
        // cannot be routed off the network it was issued on. So the boundary
        // has to be exact: an address one digit outside a private range is the
        // internet, and treating it as private would leak credentials.
        for private in [
            "http://localhost:8420",
            "http://127.0.0.1",
            "http://[::1]:8420",
            "http://10.0.0.1:8420",
            "http://10.255.255.255",
            "http://172.16.0.1",
            "http://172.31.255.1",
            "http://192.168.0.100:8420",
            "http://169.254.1.1",
            "http://[fd00::1]:8420",
            "http://[fe80::1]",
        ] {
            assert!(is_private_host(private), "{private} should be private");
        }

        for public in [
            "https://specs.example.com",
            "http://8.8.8.8",
            // Just outside 172.16/12 on both sides — the range a prefix match
            // gets wrong.
            "http://172.15.0.1",
            "http://172.32.0.1",
            // `starts_with("10.")` says private; parsing says otherwise.
            "http://100.0.0.1",
            // Not 192.168.
            "http://192.169.0.1",
            // A name is not a promise about where it resolves.
            "http://internal.example.com",
            // Craftable lookalikes.
            "http://10.0.0.1.example.com",
            "http://192.168.0.1.attacker.test",
        ] {
            assert!(!is_private_host(public), "{public} should NOT be private");
        }
    }

    #[test]
    fn a_lan_deployment_may_serve_the_public_surface_over_plain_http() {
        // Running it on your own LAN to try it out is a real case, and neither
        // localhost nor the internet. Refusing it forced a choice between not
        // trying the feature and putting a self-signed certificate in front.
        let cfg = load(&[
            (env::DAEMON_KEY, GOOD_KEY),
            (env::PUBLIC_REPO, "intake"),
            (env::PUBLIC_BASE_URL, "http://192.168.0.100:8420"),
        ])
        .expect("a private address may serve plain HTTP");
        let public = cfg.public.unwrap();
        // And cookies drop `Secure` there for the same reason they do on
        // localhost: a browser discards a `Secure` cookie sent over plain HTTP,
        // so keeping it would make sign-in appear to silently do nothing.
        assert!(!public.secure_cookies);
    }

    #[test]
    fn plain_http_on_a_public_address_is_still_refused() {
        // The guard that must not have been weakened by the above.
        let err = load(&[
            (env::DAEMON_KEY, GOOD_KEY),
            (env::PUBLIC_REPO, "intake"),
            (env::PUBLIC_BASE_URL, "http://specs.example.com"),
        ])
        .unwrap_err();
        assert!(err.contains("https://"), "{err}");
        assert!(err.contains("in the clear"), "{err}");
    }

    #[test]
    fn the_public_surface_is_off_unless_asked_for() {
        // A fresh container must not be an open intake form by accident.
        let cfg = load(&[(env::DAEMON_KEY, GOOD_KEY)]).unwrap();
        assert!(cfg.public.is_none());
    }

    #[test]
    fn the_server_refuses_to_start_half_public() {
        // **HALF a mail provider is the failure**, not the absence of one.
        // Naming a provider without a key looks configured while being broken —
        // the failure nobody notices until a stranger reports that nothing
        // arrived. Naming none at all is a legal, visible state: the page says
        // sign-in is unavailable, and `/settings` fixes it without a restart.
        //
        // `PUBLIC_BASE_URL` stays required because the sign-in links sent to
        // filers are built from it.
        for missing in [env::PUBLIC_BASE_URL, env::MAIL_KEY, env::MAIL_FROM] {
            let vars: Vec<_> = public_vars()
                .into_iter()
                .filter(|(k, _)| *k != missing)
                .collect();
            let err = load(&vars).unwrap_err();
            assert!(err.contains(missing), "missing {missing}: {err}");
        }
    }

    #[test]
    fn a_fully_configured_public_surface_loads() {
        let cfg = load(&public_vars()).unwrap();
        let p = cfg.public.expect("configured");
        assert_eq!(p.repos.names(), ["intake"]);
        assert_eq!(p.base_url, "https://specs.example.com");
        let mail = p.mail.as_ref().expect("a provider is configured");
        assert_eq!(mail.provider, crate::mail::Provider::Brevo);
        assert_eq!(mail.from_name, "Smart Coder", "a sensible default");
        assert_eq!(p.max_outstanding_links, DEFAULT_MAX_OUTSTANDING_LINKS);
        // Screening is absent unless a key is given, and that is a legitimate
        // choice rather than a broken one.
        assert!(p.screen.is_none());
    }

    #[test]
    fn a_base_url_over_plain_http_is_refused_because_the_link_is_a_credential() {
        let mut vars = public_vars();
        vars.retain(|(k, _)| *k != env::PUBLIC_BASE_URL);
        vars.push((env::PUBLIC_BASE_URL, "http://specs.example.com"));
        let err = load(&vars).unwrap_err();
        assert!(err.contains("https"), "{err}");
        assert!(err.contains("in the clear"), "{err}");
    }

    #[test]
    fn a_loopback_base_url_over_http_is_allowed_for_trying_it_locally() {
        for url in ["http://localhost:8420", "http://127.0.0.1:8420"] {
            let mut vars = public_vars();
            vars.retain(|(k, _)| *k != env::PUBLIC_BASE_URL);
            vars.push((env::PUBLIC_BASE_URL, url));
            assert!(load(&vars).is_ok(), "{url}");
        }
    }

    #[test]
    fn an_unknown_mail_provider_names_the_ones_that_exist() {
        // "unknown provider" alone leaves the operator guessing at spellings.
        let mut vars = public_vars();
        vars.retain(|(k, _)| *k != env::MAIL_PROVIDER);
        vars.push((env::MAIL_PROVIDER, "sendgrid"));
        let err = load(&vars).unwrap_err();
        assert!(err.contains("brevo"), "{err}");
        assert!(err.contains("resend"), "{err}");
    }

    #[test]
    fn screening_is_configured_or_absent_but_never_half_present() {
        let mut vars = public_vars();
        vars.push((env::SCREEN_KEY, GOOD_KEY));
        let p = load(&vars).unwrap().public.unwrap();
        let s = p.screen.expect("configured");
        // Sensible defaults, so only the key is mandatory.
        assert_eq!(s.url, DEFAULT_SCREEN_URL);
        assert_eq!(s.model, DEFAULT_SCREEN_MODEL);

        // And a short key is refused rather than quietly accepted.
        let mut short = public_vars();
        short.push((env::SCREEN_KEY, "hunter2"));
        assert!(load(&short).unwrap_err().contains("at least"));
    }

    #[test]
    fn the_spend_ceilings_have_defaults_and_are_overridable() {
        let p = load(&public_vars()).unwrap().public.unwrap();
        assert_eq!(p.max_daily_filings, DEFAULT_MAX_DAILY_FILINGS);
        assert_eq!(p.max_accounts, DEFAULT_MAX_ACCOUNTS);

        let mut vars = public_vars();
        vars.push((env::PUBLIC_MAX_DAILY, "5"));
        vars.push((env::PUBLIC_MAX_ACCOUNTS, "50"));
        let p = load(&vars).unwrap().public.unwrap();
        assert_eq!(p.max_daily_filings, 5);
        assert_eq!(p.max_accounts, 50);
    }

    #[test]
    fn a_cap_of_zero_is_refused_rather_than_silently_accepting_nothing() {
        // A public surface that accepts nothing reads as a broken feature, not a
        // setting — and "off" is expressed by leaving PUBLIC_REPO unset, which
        // turns the whole surface off honestly.
        for key in [
            env::PUBLIC_MAX_DAILY,
            env::PUBLIC_MAX_ACCOUNTS,
            env::PUBLIC_MAX_LINKS,
        ] {
            let mut vars = public_vars();
            vars.push((key, "0"));
            let err = load(&vars).unwrap_err();
            assert!(err.contains(key), "{err}");
            assert!(err.contains("accept nothing"), "{err}");
        }
    }

    #[test]
    fn a_non_numeric_cap_is_a_clear_error() {
        let mut vars = public_vars();
        vars.push((env::PUBLIC_MAX_DAILY, "lots"));
        let err = load(&vars).unwrap_err();
        assert!(err.contains("whole number"), "{err}");
        assert!(err.contains("lots"), "names what was given: {err}");
    }

    #[test]
    fn showing_the_spec_defaults_on_and_can_be_turned_off() {
        // On by default is a deliberate choice: the drafted spec describes the
        // developer's repository, and a filer wrote the prompt that produced it.
        assert!(load(&public_vars()).unwrap().public.unwrap().show_spec);

        for off in ["0", "false", "no", "OFF"] {
            let mut vars = public_vars();
            vars.push((env::PUBLIC_SHOW_SPEC, off));
            assert!(
                !load(&vars).unwrap().public.unwrap().show_spec,
                "{off} should turn it off"
            );
        }
    }

    #[test]
    fn public_settings_are_ignored_entirely_when_the_surface_is_off() {
        // Half-applied public settings on a private server would be a surprise;
        // the switch is one variable and everything else hangs off it.
        let cfg = load(&[
            (env::DAEMON_KEY, GOOD_KEY),
            (env::MAIL_PROVIDER, "nonsense"),
            (env::PUBLIC_BASE_URL, "http://not-https.example.com"),
        ])
        .expect("no public repo means no public surface, and no validation of it");
        assert!(cfg.public.is_none());
    }

    #[test]
    fn a_public_surface_with_no_mail_provider_still_loads() {
        // **Replaces the console-mail waiver.** Looking at the surface locally
        // must not require an API key for a third party, and refusing to start
        // would put the settings page that configures mail out of reach. The
        // page says sign-in is unavailable instead.
        let cfg = load(&[
            (env::DAEMON_KEY, GOOD_KEY),
            (env::PUBLIC_REPO, "intake"),
            (env::PUBLIC_BASE_URL, "http://localhost:8420"),
        ])
        .unwrap();
        let public = cfg.public.expect("the surface exists");
        // `None`, not a placeholder: no later branch can quietly construct an
        // `HttpMailer` with an empty key.
        assert!(public.mail.is_none());
    }
}
