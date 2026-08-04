//! The HTTP layer: bytes off a wire, and the long poll.
//!
//! Everything that decides anything lives in [`crate::routes`] as a pure
//! function. This module reads a request, calls it, and writes the response — so
//! the untested surface is as small as it can be.
//!
//! **No TLS in-process.** A reverse proxy terminates it, which is how this is
//! deployed anyway (Portainer, behind whatever the developer already runs).
//! Certificates, renewal and a private key inside the container are three failure
//! modes solving a problem that is already solved outside it.

use std::sync::{Arc, Mutex};

use sc_proto::wire;
use sc_proto::{DcError, Result};

use crate::config::Config;
use crate::mail::{HttpMailer, Mailer};
use crate::ratelimit::RateLimiter;
use crate::routes::{self, Ctx, Req, Res};
use crate::screen::{HttpScreener, Screener, Verdict};
use crate::store::{now_ms, Store};

/// How long a single idle poll sleeps before re-checking for work.
///
/// Small relative to [`wire::POLL_TIMEOUT`], so a request filed while a daemon is
/// mid-poll is picked up in well under a second rather than waiting out the hold.
const POLL_TICK: std::time::Duration = std::time::Duration::from_millis(250);

/// Everything a request thread needs, assembled once at startup.
///
/// One `Arc` rather than five cloned per request: the set grew with the public
/// surface, and five parallel clones is where one eventually gets forgotten.
struct Shared {
    store: Store,

    limiter: Mutex<RateLimiter>,
    /// Console mail, when the loopback-guarded switch is on. Otherwise the
    /// mailer is built per request from the settings — see `mailer_for`.
    mail_to_console: bool,
    /// Serialises read-modify-write on `accounts.json` and `links.json`.
    write_lock: Mutex<()>,
    /// Who is polling, and for what. Shared like the limiter above.
    seen: Mutex<crate::daemons::Seen>,
    /// Who may review what. Re-read from the volume when the file changes —
    /// see [`crate::roster::RosterCache`] for why not a startup snapshot.
    roster: Mutex<crate::roster::RosterCache>,
    /// What this server does. Same mtime cache, same reasoning.
    settings: Mutex<crate::settings::SettingsCache>,
    /// Who is signed in. On the hot path — see `AccountsCache`.
    accounts: Mutex<crate::account::AccountsCache>,
    seal_key: Option<crate::seal::SealKey>,
}

/// Apply `SC_SERVER_OWNERS` and `SC_SERVER_PUBLIC_REPOS` **once**, the first
/// time this volume is used.
///
/// Both settings are kept so an existing deployment keeps working and a fresh
/// one can be bootstrapped without a browser. They are seeds and not sources of
/// truth: re-applying them every boot would resurrect an owner revoked through
/// the UI, and re-enable a repository the developer turned off — the failure
/// "it takes effect on the next request" exists to prevent, arriving by the
/// back door of a restart.
///
/// The flag is set even when both are empty. Otherwise the first boot *with*
/// something configured would seed a volume that had already been administered,
/// and "I removed the last one and it came back" is the same bug with more
/// steps.
///
/// **`SC_SERVER_PUBLIC_REPOS` is a seed like the rest, on/off half included.**
/// A freshly claimed server has no public surface: the switch lives in
/// [`crate::settings::Settings::public`] and the administrator flips it.
fn seed_roster(store: &Store, cfg: &Config) -> Result<()> {
    let configured: Vec<(String, Vec<String>)> = cfg
        .public
        .as_ref()
        .map(|p| {
            p.owners
                .iter()
                .map(|o| (o.login.clone(), o.repos.clone()))
                .collect()
        })
        .unwrap_or_default();

    let repos: Vec<String> = cfg
        .public
        .as_ref()
        .map(|p| p.repos.names().to_vec())
        .unwrap_or_default();

    let mut roster = store.roster()?;
    if !roster.seed(&configured, &repos, &cfg.daemon_keys, now_ms()) {
        // Said out loud, because a setting that is present and ignored is one
        // somebody will edit expecting an effect. Both are named: an operator
        // adding a repository to the stack and seeing nothing happen needs to
        // be told where it is actually decided.
        if !configured.is_empty() || !repos.is_empty() {
            crate::log::warn("roster settings ignored")
                .with(
                    "note",
                    "the roster on the volume is authoritative; edit owners and \
                     repositories at /owners and /repos",
                )
                .with("owners", configured.len() as u64)
                .with("repos", repos.len() as u64)
                .emit();
        }
        return Ok(());
    }
    store.put_roster(&roster)?;
    crate::log::info("roster seeded")
        .with("owners", configured.len() as u64)
        .with("repos", repos.len() as u64)
        .with("daemons", cfg.daemon_keys.len() as u64)
        .emit();
    Ok(())
}

/// Apply the environment's public settings **once**, on a fresh volume.
///
/// The same seed-not-source-of-truth rule as the roster: without the flag, a
/// redeploy would silently revert every change made at `/settings`, which is
/// the restart back door in a different disguise.
///
/// **Turning the surface on is part of the seed**, because naming a repository
/// in the environment used to be what did it. A volume seeded from a
/// configuration that had a public surface keeps one; a fresh claim starts with
/// it off, and the administrator turns it on deliberately.
fn seed_settings(store: &Store, cfg: &Config) -> Result<()> {
    let mut settings = store.settings()?;
    let p = cfg.public.as_ref();

    let seed = crate::settings::Seed {
        base_url: p.map(|p| p.base_url.as_str()),
        github_id: p
            .and_then(|p| p.github.as_ref())
            .map(|g| g.client_id.as_str()),
        github_secret: p
            .and_then(|p| p.github.as_ref())
            .map(|g| g.client_secret.as_str()),
        site_name: p.map(|p| p.site_name.as_str()),
        public: p.is_some(),
        show_spec: p.map(|p| p.show_spec),
        mail: p.and_then(|p| p.mail.as_ref()),
        screen: p.and_then(|p| p.screen.as_ref()),
        max_daily_filings: p.map(|p| p.max_daily_filings),
        max_daily_drafts: p.map(|p| p.max_daily_drafts),
        max_accounts: p.map(|p| p.max_accounts),
        max_outstanding_links: p.map(|p| p.max_outstanding_links),
    };

    if !settings.seed(seed, cfg.seal_key.as_ref(), now_ms()) {
        if p.is_some() {
            // Said out loud: a setting that is present and inert is one somebody
            // will edit expecting an effect.
            crate::log::warn("public settings ignored")
                .with(
                    "note",
                    "the settings on the volume are authoritative; edit them at                      /settings",
                )
                .emit();
        }
        return Ok(());
    }
    store.put_settings(&settings)?;
    crate::log::info("settings seeded")
        .with("public", settings.public)
        .emit();
    Ok(())
}

/// How often the screening sweep looks for new filings.
///
/// Screening runs here rather than inline on the filing request: this server is
/// thread-per-request, so a hung third-party call on the request path would
/// convert directly into thread exhaustion — and the filer would be left waiting
/// on somebody else's API.
///
/// Two seconds is invisible next to a daemon's own poll interval.
const SCREEN_TICK: std::time::Duration = std::time::Duration::from_secs(2);

/// How many filings one sweep screens.
///
/// A bound so a flood cannot make one tick run for minutes while everything
/// filed after it waits.
const SCREEN_BATCH: usize = 4;

/// Run the server until the process is killed.
pub fn run(cfg: &Config) -> Result<()> {
    let store = Store::open(&cfg.data_dir)?;

    let claim_code = arm_claim(&store)?;

    // **Before anything reads a setting.** A wrong or missing key makes every
    // sealed value unreadable, and without this the server would boot happily
    // and report nothing configured — indistinguishable from a fresh install,
    // and the operator would re-enter secrets that were never lost.
    crate::seal::usable(
        cfg.seal_key.as_ref(),
        Some(&store.settings()?.github_client_secret),
    )
    .map_err(DcError::Eval)?;

    seed_roster(&store, cfg)?;
    seed_settings(&store, cfg)?;

    let shared = Arc::new(Shared {
        store: store.clone(),

        limiter: Mutex::new(RateLimiter::new()),
        mail_to_console: cfg.mail_to_console,
        write_lock: Mutex::new(()),
        seen: Mutex::new(crate::daemons::Seen::default()),
        roster: Mutex::new(crate::roster::RosterCache::default()),
        settings: Mutex::new(crate::settings::SettingsCache::default()),
        accounts: Mutex::new(crate::account::AccountsCache::default()),
        seal_key: cfg.seal_key.clone(),
    });

    let server = tiny_http::Server::http(cfg.addr())
        .map_err(|e| DcError::Eval(format!("could not bind {}: {e}", cfg.addr())))?;

    // One line for one event: the address and the state directory are the same
    // fact about this start, and two lines would only have to be correlated.
    crate::log::info("listening")
        .with("addr", cfg.addr())
        .with("data_dir", cfg.data_dir.display().to_string())
        .emit();
    match &cfg.public {
        Some(p) => {
            crate::log::info("public intake")
                .with("enabled", true)
                .with("repos", p.repos.names().join(","))
                .with("base_url", p.base_url.clone())
                .with("screening", p.screen.is_some())
                .emit();
            if let Some(s) = &p.screen {
                crate::log::info("screening")
                    .with("model", s.model.clone())
                    .with("url", s.url.clone())
                    .emit();
            } else {
                // Its own line, at `warn`, rather than a `false` in the field
                // above. Said plainly: a server that pretends to screen is worse
                // than one that visibly does not — and "visibly" means a line an
                // operator sees, not a value they would have to go looking for.
                crate::log::warn("screening off")
                    .with("note", "filings queue unscreened")
                    .emit();
            }
        }
        None => crate::log::info("public intake")
            .with("enabled", false)
            .emit(),
    }
    // Named, so an operator can see at a glance which machines may claim work —
    // and, on an install still holding a single key, learn that the plural
    // setting exists without being forced onto it.
    crate::log::info("daemon keys")
        .with(
            "labels",
            cfg.daemon_keys
                .iter()
                .map(|d| d.label.clone())
                .collect::<Vec<_>>()
                .join(","),
        )
        .emit();
    if cfg
        .daemon_keys
        .iter()
        .any(|d| d.label == crate::config::DEFAULT_DAEMON_LABEL)
    {
        crate::log::warn("single daemon key")
            .with("setting", crate::config::env::DAEMON_KEY)
            .with("instead", crate::config::env::DAEMON_KEYS)
            .with(
                "note",
                "one key for every daemon means one rate budget, and rotating it \
                 locks out every machine at once",
            )
            .emit();
    }

    if let Some(code) = claim_code {
        // **This line is a credential**, with the same caveat as the enrolment
        // code above: it goes wherever the container log goes, so it expires.
        // Unlike that one it is armed *only while the server is unclaimed*, so a
        // claimed server prints nothing however often it restarts.
        crate::log::warn("claim this server")
            .with("code", code.clone())
            .with("expires_in_s", crate::admin::CLAIM_TTL_MS / 1000)
            .with("note", "open /setup and type it")
            .emit();
    }

    spawn_screening(Arc::clone(&shared));

    for request in server.incoming_requests() {
        let shared = Arc::clone(&shared);
        // A thread per request: this serves one developer and a handful of
        // daemons, so a thread pool would be machinery without a load to justify
        // it. The long poll needs a blocking thread regardless.
        std::thread::spawn(move || {
            // Logged inside `serve_one`, which knows the request id — so the
            // failure can be tied to the access line for the same request.
            let _ = serve_one(request, &shared);
        });
    }
    Ok(())
}

/// The mailer as the settings currently describe it.
///
/// **Built per request rather than once at startup**, because a mail key is now
/// editable from a page — and a key fixed at the moment sign-in is broken is
/// worth nothing if it waits for a redeploy.
///
/// The cost is a fresh `ureq::Agent`, and therefore a fresh TLS handshake, per
/// sign-in. That is affordable precisely here: sending a link is rare,
/// human-paced, and already bounded by `max_outstanding_links`. It would not be
/// affordable on the poll path.
///
/// `mail_to_console` is checked **first** and stays in the environment: it
/// prints sign-in links to the log, and its loopback guard runs at load time
/// where a runtime write could not be covered by it.
fn mailer_for(shared: &Shared, public: Option<&crate::config::PublicConfig>) -> Arc<dyn Mailer> {
    if shared.mail_to_console {
        return Arc::new(crate::mail::Console);
    }
    match public.and_then(|p| p.mail.as_ref()) {
        Some(m) => Arc::new(HttpMailer::new(
            m.provider,
            &m.api_key,
            &m.from,
            &m.from_name,
        )),
        // Nothing configured. Failing loudly beats pretending to have sent.
        None => Arc::new(crate::mail::Unconfigured),
    }
}

/// Start the background screening sweep, if screening is configured.
///
/// **The crate's first background thread**, and deliberately so rather than
/// piggybacking on request dispatch: dispatch runs on every request *and* every
/// 250ms long-poll tick, so a directory scan there would run thousands of times
/// an hour for work that needs doing every couple of seconds.
fn spawn_screening(shared: Arc<Shared>) {
    // **Spawned unconditionally.** It used to return early when screening was
    // off, which made turning it *on* at runtime impossible — there was no loop
    // left to observe the change. The tick costs a `stat` when screening is off.
    std::thread::spawn(move || {
        // The live screener, rebuilt only when the settings that describe it
        // change. Rebuilding every tick would discard its connection pool thirty
        // times a minute for nothing.
        let mut built: Option<(crate::config::ScreenConfig, HttpScreener)> = None;

        loop {
            std::thread::sleep(SCREEN_TICK);

            let wanted = public_now(&shared).and_then(|p| p.screen);
            match wanted {
                None => {
                    // Screening is off. Skip the sweep rather than exiting, or
                    // turning it off would be irreversible. Filings queue
                    // unscreened, which is what `None` has always meant.
                    built = None;
                    continue;
                }
                Some(cfg) => {
                    if built.as_ref().map(|(c, _)| c) != Some(&cfg) {
                        let screener = HttpScreener::new(&cfg.url, &cfg.api_key, &cfg.model);
                        crate::log::info("screening configured")
                            .with("model", cfg.model.clone())
                            .with("url", cfg.url.clone())
                            .emit();
                        built = Some((cfg, screener));
                    }
                }
            }

            let Some((_, screener)) = built.as_ref() else {
                continue;
            };
            if let Err(e) = screen_pending(&shared.store, screener) {
                // Never fatal. A screener that cannot run must not stop the
                // server, and the requests it did not reach stay pending.
                crate::log::error("screening sweep failed")
                    .text("err", e)
                    .emit();
            }
        }
    });
}

/// Screen up to [`SCREEN_BATCH`] pending filings.
fn screen_pending(store: &Store, screener: &dyn Screener) -> Result<()> {
    for request in store.pending_screening()?.into_iter().take(SCREEN_BATCH) {
        let verdict = screener.screen(&request.text);
        let quarantine = match verdict {
            Verdict::Quarantine => Some(Verdict::REASON),
            Verdict::Admit => None,
        };
        // A verdict for a request a human already released is refused by the
        // store, which is the behaviour we want — so the error is ignored rather
        // than logged as a failure.
        let _ = store.finish_screening(&request.id, quarantine);
    }
    Ok(())
}

/// Arm a claim code on a server nobody owns yet.
///
/// Reading this
/// code out of the container log proves you own the deployment, which is the
/// same proof a stack editor stands in for and better evidence than holding a
/// cookie.
///
/// **Armed only while unclaimed.** A claimed server prints nothing however often
/// it restarts — re-arming would leave a standing way to take the server from
/// its administrator, refreshed on every deploy.
fn arm_claim(store: &Store) -> Result<Option<String>> {
    let mut admin = store.admin()?;
    if admin.claimed() {
        return Ok(None);
    }
    let code = crate::admin::mint_claim_code();
    admin.arm(&code, now_ms());
    store.put_admin(&admin)?;
    Ok(Some(code))
}

fn serve_one(mut request: tiny_http::Request, shared: &Shared) -> Result<()> {
    let started = std::time::Instant::now();
    // Minted per request so the access line below and any error it produces can
    // be found together — this server answers several requests at once, and
    // interleaved lines are otherwise impossible to attribute.
    let id = request_id();

    // Logged here rather than propagated silently: this is the one failure that
    // happens *before* there is a request to describe, so the access line below
    // is unreachable for it and nothing else would record that it occurred.
    let req = match read(&mut request) {
        Ok(req) => req,
        Err(e) => {
            crate::log::error("request unreadable")
                .req(&id)
                .text("err", &e)
                .emit();
            return Err(e);
        }
    };
    let is_poll = req.method == "GET" && req.path.split('?').next() == Some(wire::route::WORK);

    let mut res = dispatch(shared, &req, false);

    // The long poll: hold the connection open rather than answering "nothing"
    // immediately, so a request filed on a train is picked up in under a second
    // with almost no idle traffic.
    if is_poll && res.hold_for_work {
        let deadline = std::time::Instant::now() + wire::POLL_TIMEOUT;
        while std::time::Instant::now() < deadline {
            std::thread::sleep(POLL_TICK);
            let again = dispatch(shared, &req, true);
            if !again.hold_for_work {
                res = again;
                break;
            }
        }
    }

    // Read before `write` takes the response.
    let status = res.status;
    let bytes = match &res.binary {
        Some(b) => b.len(),
        None => res.body.len(),
    } as u64;
    let route = route_label(&req.path);

    let outcome = write(request, res);

    // **Once per request, not once per dispatch.** The poll loop above calls
    // `dispatch` every 250ms for up to `POLL_TIMEOUT`, so a line in there would
    // mean roughly four a second for every idle daemon — a log of nothing
    // happening, drowning the log of something happening.
    //
    // After `write`, so `ms` covers writing the response too, and so a client
    // that hangs up mid-body still leaves a record of what was attempted.
    crate::log::info("request")
        .req(&id)
        .with("method", req.method.clone())
        .with("route", route)
        .with("status", status as u64)
        .with("ms", started.elapsed().as_millis() as u64)
        .with("bytes", bytes)
        // A held poll legitimately reads ~30s, which would wreck a latency
        // panel that could not tell it apart from a slow page.
        .with("poll", is_poll)
        .emit();

    if let Err(e) = &outcome {
        crate::log::error("request failed")
            .req(&id)
            .text("err", e)
            .emit();
    }
    outcome
}

/// A short id for one request. Not a credential, so 4 bytes of hex is plenty to
/// tell concurrent requests apart in a log.
fn request_id() -> String {
    let mut bytes = [0u8; 4];
    // Unlike a credential, an id that repeats costs nothing but an ambiguous
    // grep — so this falls back rather than refusing to serve the request.
    if getrandom::fill(&mut bytes).is_err() {
        return "????????".to_string();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Which route a request hit, as a **label** — never the URL it sent.
///
/// The path off the wire is attacker-controlled and carries credentials: a
/// sign-in link's token is a path segment, and that token is a bearer credential
/// good for somebody's account. The query string can carry anything at all.
///
/// So this does not *sanitise* the path, it **classifies** it: every request maps
/// onto one of a fixed set of strings spelled out below, and anything
/// unrecognised becomes `"other"`. A redactor decides what to remove and is
/// wrong the first time it misses something; a classifier decides what to keep
/// and is wrong only by being uninformative.
///
/// The `&'static str` return type is what enforces it. No arm can borrow from
/// `path`, so request data reaching the log is not a bug to be avoided but a
/// thing the signature makes impossible.
///
/// **Deliberately not logged anywhere in the access line**, each because it will
/// be proposed eventually:
///
/// - the **query string**, dropped at the first `?` — it is caller-controlled
///   and free-form;
/// - the **bearer token** and **cookies**, which are credentials;
/// - the **email address**, which only ever arrives in a POST body — a body this
///   line never touches;
/// - the **client IP**. Behind the reverse proxy this deployment assumes it is
///   the proxy's own address, so it is a constant that says nothing. "Fixing"
///   that by reading `X-Forwarded-For` would put an attacker-controlled header
///   carrying somebody's personal data into a log built to be shipped elsewhere;
/// - the **`User-Agent`**, high-cardinality and caller-controlled, and absent
///   from [`Req`] — which holds only what the routes actually use, a property
///   worth more than the field would be.
fn route_label(path: &str) -> &'static str {
    use crate::routes::{private_route, public_route};

    let path = path.split('?').next().unwrap_or("");
    match path {
        public_route::LANDING => "/",
        public_route::FILE => "/public",
        public_route::SIGNIN => "/public/signin",
        public_route::SIGNOUT => "/public/signout",
        public_route::LANGUAGE => "/public/language",
        public_route::SCRIPT => "/public/app.js",
        public_route::FONT_BODY | public_route::FONT_DISPLAY => "/public/font",
        private_route::REVIEW => "/review",
        private_route::SETUP => "/setup",
        private_route::SETUP_GITHUB => "/setup/github",
        p if p.starts_with(public_route::SIGNIN_PREFIX) => "/public/signin/:token",
        p if p.starts_with(public_route::REQUEST_PREFIX) => "/public/request/:id",
        p if p.starts_with(wire::route::WORK) => "/api/v1/work",
        p if p.starts_with(&format!("{}/", private_route::REVIEW)) => "/review/:rest",
        _ => "other",
    }
}

/// Run one request through the routes.
///
/// `rechecking` is set only by the long-poll hold below, which re-runs this
/// every 250ms on a connection it is *already* holding. Those passes must not be
/// charged to the caller's rate budget: they are the server's own polling, and
/// counting them made one held poll cost about 120 requests — enough for an idle
/// daemon to lock itself out of its own server.
/// The public surface as it is configured right now.
///
/// **`None` when the switch is off**, which makes every public route 404 — the
/// surface does not exist rather than existing and refusing. A freshly claimed
/// server is in exactly that state until somebody turns it on.
///
/// The environment is still read at startup and seeded into the settings once;
/// what this reads is the settings, which is where an edit lands.
fn public_now(shared: &Shared) -> Option<crate::config::PublicConfig> {
    let settings = match shared.settings.lock() {
        Ok(mut c) => c.current(&shared.store.settings_path()),
        Err(p) => p.into_inner().current(&shared.store.settings_path()),
    };
    if !settings.public {
        return None;
    }
    let repos = match shared.roster.lock() {
        Ok(mut c) => c.current(&shared.store.roster_path()).enabled(),
        Err(p) => p
            .into_inner()
            .current(&shared.store.roster_path())
            .enabled(),
    };

    let key = shared.seal_key.as_ref();
    Some(crate::config::PublicConfig {
        repos: crate::config::Repos::from(repos),
        // Falls back to the address rather than being required: a masthead is a
        // label, and refusing to serve over one is a worse failure than an ugly
        // heading.
        site_name: if settings.site_name.is_empty() {
            crate::config::host_label(&settings.base_url)
        } else {
            settings.site_name.clone()
        },
        base_url: settings.base_url.clone(),
        // **Derived, never stored.** Whether cookies carry `Secure` follows from
        // the address, so the two cannot drift apart into a surface that thinks
        // it is private while serving over the internet.
        secure_cookies: crate::config::secure_for(&settings.base_url),
        mail: settings.mail(key),
        screen: settings.screen(key),
        show_spec: settings.show_spec.unwrap_or(true),
        max_daily_filings: settings
            .max_daily_filings
            .unwrap_or(crate::config::DEFAULT_MAX_DAILY_FILINGS),
        max_daily_drafts: settings
            .max_daily_drafts
            .unwrap_or(crate::config::DEFAULT_MAX_DAILY_DRAFTS),
        max_accounts: settings
            .max_accounts
            .unwrap_or(crate::config::DEFAULT_MAX_ACCOUNTS),
        max_outstanding_links: settings
            .max_outstanding_links
            .unwrap_or(crate::config::DEFAULT_MAX_OUTSTANDING_LINKS),
        // Read from the roster on the request path; this field is the seed and
        // is no longer consulted to decide anything.
        owners: Vec::new(),
        github: if settings.has_github() {
            settings
                .github_secret(key)
                .map(|client_secret| crate::config::GithubConfig {
                    client_id: settings.github_client_id.clone(),
                    client_secret,
                })
        } else {
            None
        },
    })
}

fn dispatch(shared: &Shared, req: &Req, rechecking: bool) -> Res {
    let now = now_ms();
    let mut guard = match shared.limiter.lock() {
        Ok(g) => g,
        // A poisoned lock means another thread panicked mid-request. Recovering
        // the guard is right here: the limiter is a counter, so the worst a
        // partial update costs is one request counted twice.
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.sweep(now);

    // **The public surface is assembled per request, not frozen at startup.**
    //
    // Every part of it is now editable from `/settings`, and an edit has to take
    // effect on the next request rather than the next restart — a cap raised to
    // stop refusing filings, or a mail key fixed at the moment sign-in is
    // broken, is worth nothing if it waits for a redeploy.
    //
    // Both reads go through mtime caches, so the steady state is two `stat`
    // calls and no parsing.
    let public = public_now(shared);
    let mailer = mailer_for(shared, public.as_ref());
    // **Read per request, like everything else administered from a page.**
    // Revoking a machine has to stop it claiming on its next poll rather
    // than at the next restart — and a poll is the one request that arrives
    // every few seconds, so this goes through the same mtime cache.
    let daemon_keys = match shared.roster.lock() {
        Ok(mut c) => c.current(&shared.store.roster_path()).daemon_keys(),
        Err(p) => p
            .into_inner()
            .current(&shared.store.roster_path())
            .daemon_keys(),
    };

    let mut ctx = Ctx {
        store: &shared.store,
        daemon_keys: &daemon_keys,
        limiter: &mut guard,
        now_ms: now,
        public: public.as_ref(),
        mailer: mailer.as_ref(),
        write_lock: &shared.write_lock,
        seen: &shared.seen,
        roster: &shared.roster,
        settings: &shared.settings,
        accounts: &shared.accounts,
        seal_key: shared.seal_key.as_ref(),
        // Filled in by `handle` before dispatch, beside the caller.
        fresh_auth: false,
        rechecking,
    };
    routes::handle(&mut ctx, req)
}

/// The largest body accepted, before anything is read into memory.
///
/// A drafted spec is the biggest legitimate payload. Reading an unbounded body off
/// the public internet is how a server is killed with one request.
const MAX_BODY: usize = 1024 * 1024;

fn read(request: &mut tiny_http::Request) -> Result<Req> {
    let method = request.method().as_str().to_string();
    let path = request.url().to_string();

    let mut bearer = None;
    let mut cookie_token = None;
    let mut cookie_lang = None;
    let mut accept_language = None;
    for h in request.headers() {
        let name = h.field.as_str().as_str().to_ascii_lowercase();
        let value = h.value.as_str();
        match name.as_str() {
            "authorization" => {
                bearer = value
                    .strip_prefix("Bearer ")
                    .or_else(|| value.strip_prefix("bearer "))
                    .map(|t| t.trim().to_string());
            }
            "cookie" => {
                cookie_token = cookie_value(value, routes::COOKIE);
                cookie_lang = cookie_value(value, routes::LANG_COOKIE);
            }
            // Taken as sent and parsed in `i18n`, which knows what a valid one
            // looks like. Anything unrecognised there falls back to the default,
            // so no validation is owed here.
            "accept-language" => accept_language = Some(value.to_string()),
            _ => {}
        }
    }

    let declared = request.body_length().unwrap_or(0);
    if declared > MAX_BODY {
        // Refuse before reading: the point is not to allocate it.
        return Ok(Req {
            method,
            path,
            bearer,
            cookie_token,
            cookie_lang,
            accept_language,
            body: String::new(),
        });
    }
    let mut body = String::new();
    if declared > 0 {
        use std::io::Read;
        request
            .as_reader()
            .take(MAX_BODY as u64)
            .read_to_string(&mut body)
            .map_err(|e| DcError::Eval(format!("could not read the body: {e}")))?;
    }

    Ok(Req {
        method,
        path,
        bearer,
        cookie_token,
        cookie_lang,
        accept_language,
        body,
    })
}

/// Pull one cookie's value out of a `Cookie:` header.
pub fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|part| {
        let part = part.trim();
        let (k, v) = part.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

fn write(request: tiny_http::Request, res: Res) -> Result<()> {
    let mut headers: Vec<tiny_http::Header> = Vec::new();
    let content_type = format!("Content-Type: {}", res.content_type);
    if let Ok(h) = content_type.parse() {
        headers.push(h);
    }
    // Every response, without exception — a header added per route is a header
    // eventually missing from one. The *policy* rides on the response, decided
    // at the one dispatch site in `handle`; this writer does not know or ask
    // which surface it is serving.
    for (name, value) in routes::security_headers(res.policy) {
        if let Ok(h) = format!("{name}: {value}").parse() {
            headers.push(h);
        }
    }
    if let Some(cookie) = &res.set_cookie {
        if let Ok(h) = format!("Set-Cookie: {cookie}").parse() {
            headers.push(h);
        }
    }

    // A binary body wins where one is set — a font cannot travel as a `String`,
    // and `from_data` is the same writer with a byte slice rather than UTF-8.
    let status = tiny_http::StatusCode(res.status);
    let respond = match res.binary {
        Some(bytes) => {
            let mut response = tiny_http::Response::from_data(bytes).with_status_code(status);
            for h in headers {
                response.add_header(h);
            }
            request.respond(response)
        }
        None => {
            let mut response = tiny_http::Response::from_string(res.body).with_status_code(status);
            for h in headers {
                response.add_header(h);
            }
            request.respond(response)
        }
    };
    respond.map_err(|e| DcError::Eval(format!("could not respond: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every label `route_label` is allowed to produce.
    const LABELS: [&str; 14] = [
        "/",
        "/public",
        "/public/signin",
        "/public/signout",
        "/public/language",
        "/public/app.js",
        "/public/font",
        "/public/signin/:token",
        "/public/request/:id",
        "/api/v1/work",
        "/review",
        "/review/:rest",
        "/setup",
        "other",
    ];

    #[test]
    fn a_route_label_never_carries_request_data() {
        // The property the access log rests on. A sign-in token is a bearer
        // credential and the query string is free-form, so neither may survive
        // into a line that is shipped to a log aggregator.
        const SECRET: &str = "s3cr3ttokenvalue";
        let hostile = [
            format!("/public/signin/{SECRET}"),
            format!("/public/request/{SECRET}"),
            format!("/public?email=someone@example.com&t={SECRET}"),
            format!("/api/v1/work/{SECRET}/drafted"),
            format!("/review/{SECRET}/approve"),
            format!("/{SECRET}"),
            format!("/public/../../{SECRET}"),
            format!("/PUBLIC/SIGNIN/{SECRET}"),
            format!("/public/signin/{}", "A".repeat(4096)),
            String::new(),
            "?".to_string(),
            "/public?".to_string(),
        ];

        for path in &hostile {
            let label = route_label(path);
            assert!(
                LABELS.contains(&label),
                "{path} produced an unknown label {label}"
            );
            assert!(
                !label.contains(SECRET) && !label.contains('@') && !label.contains("AAA"),
                "{path} leaked request data as {label}"
            );
        }
    }

    #[test]
    fn the_known_routes_classify_to_themselves() {
        // The other half: the classifier must actually be informative, or
        // "return `other` always" would pass the test above.
        use crate::routes::{private_route, public_route};

        assert_eq!(route_label(public_route::LANDING), "/");
        assert_eq!(route_label(public_route::FILE), "/public");
        assert_eq!(route_label(public_route::SIGNIN), "/public/signin");
        assert_eq!(route_label(public_route::SIGNOUT), "/public/signout");
        assert_eq!(route_label(public_route::LANGUAGE), "/public/language");
        assert_eq!(route_label(public_route::SCRIPT), "/public/app.js");
        assert_eq!(route_label(public_route::FONT_BODY), "/public/font");
        assert_eq!(route_label(public_route::FONT_DISPLAY), "/public/font");
        assert_eq!(route_label(private_route::REVIEW), "/review");
        assert_eq!(route_label(private_route::SETUP), "/setup");
        assert_eq!(route_label(wire::route::WORK), "/api/v1/work");
        assert_eq!(route_label(&wire::route::drafted("srv-1")), "/api/v1/work");
        assert_eq!(route_label("/public/signin/abc"), "/public/signin/:token");
        assert_eq!(route_label("/public/request/xyz"), "/public/request/:id");
    }

    #[test]
    fn a_query_string_never_survives_classification() {
        // Dropped, not truncated — a truncated query is still caller-controlled
        // content in the log.
        use crate::routes::public_route;
        assert_eq!(route_label("/public?email=a@b.com"), "/public");
        assert_eq!(
            route_label(&format!("{}?next=/review", public_route::SIGNIN)),
            "/public/signin"
        );
    }

    #[test]
    fn an_unknown_path_is_not_reflected() {
        // The catch-all is a constant, so a scanner probing for `/wp-admin`
        // cannot write its own strings into the log.
        assert_eq!(route_label("/wp-admin.php"), "other");
        assert_eq!(route_label("/publicXYZ"), "other");
        assert_eq!(route_label("/../etc/passwd"), "other");
    }

    #[test]
    fn a_request_id_is_short_and_hex() {
        let id = request_id();
        assert_eq!(id.len(), 8, "{id}");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "{id}");
    }

    #[test]
    fn a_cookie_is_found_among_others() {
        let header = "other=1; sc_device=abc123; another=2";
        assert_eq!(
            cookie_value(header, routes::COOKIE).as_deref(),
            Some("abc123")
        );
        assert_eq!(cookie_value(header, "missing"), None);
    }

    #[test]
    fn a_malformed_cookie_header_does_not_panic() {
        // It comes off the public internet; every shape must be survivable.
        for header in ["", ";", "=", "a", "a=;b", "===", "sc_device"] {
            let _ = cookie_value(header, routes::COOKIE);
        }
    }

    #[test]
    fn the_poll_tick_is_well_inside_the_hold() {
        // A tick near the hold would make a request filed mid-poll wait out the
        // whole window, which is the latency long-polling exists to remove.
        assert!(POLL_TICK.as_millis() * 20 < wire::POLL_TIMEOUT.as_millis());
    }

    #[test]
    fn the_body_limit_is_above_a_real_spec_and_far_below_a_denial_of_service() {
        // Reading an unbounded body off the public internet is how a server is
        // killed with one request. Checked at compile time, so a bad edit fails
        // the build rather than waiting for the test run.
        const {
            assert!(MAX_BODY >= 256 * 1024, "a long spec must still fit");
            assert!(MAX_BODY <= 4 * 1024 * 1024, "and no more than that");
        }
    }
}
