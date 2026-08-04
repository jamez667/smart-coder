//! The routes, as a pure function over a request.
//!
//! [`handle`] takes a described request and returns a described response. No
//! socket, no thread, no clock — so every route, every refusal and every header is
//! testable at unit speed, and the HTTP layer in [`serve`](crate::serve) is left
//! with nothing but reading bytes off a wire and writing them back.
//!
//! ## The vocabulary is deliberately small
//!
//! **file a request · watch it draft · read the spec · approve or send back.**
//!
//! There is no route that builds, no route that reaches a later phase, and no
//! route that names a path. That is not a policy the handlers enforce — the
//! [`Store`] has no field for a path and the server has no model and no repository,
//! so there is nothing here that *could* grow into an execution path (spec 18).

use sc_proto::wire::{
    self, DraftFailed, DraftedSpec, PollResponse, WireError, WorkItem, WorkReleased,
};
use sc_proto::IntakeKind;

use std::sync::Mutex;

use crate::account;
use crate::auth::{self, Caller};
use crate::config::PublicConfig;
use crate::i18n::Locale;
use crate::mail::Mailer;
use crate::ratelimit::{Bucket, RateLimiter};
use crate::store::{new_id, Request, RequestState, Serves, Store};

/// The cookie a browser carries once enrolled.
pub const COOKIE: &str = "sc_device";

/// The cookie carrying a half-finished setup.
///
/// **Its own name, not [`COOKIE`].** A setup token is not a session and grants
/// nothing once the server is claimed; sharing a name would mean `identify`
/// having to tell two unrelated things apart on every request, and "both
/// present" is what an attacker constructs.
pub const SETUP_COOKIE: &str = "sc_setup";

/// The `; Secure` a cookie carries, or nothing on a loopback server.
///
/// A browser **discards** a `Secure` cookie arriving over plain HTTP, so on
/// `http://localhost` this attribute makes sign-in and the language switcher both
/// appear to do nothing: the request succeeds, the cookie is dropped, and the
/// next page has forgotten. That reads as a bug in the feature.
///
/// Derived from the base URL rather than configured — a deployed server's base
/// URL must be `https://` before it will start, so there is nothing to get wrong
/// and no setting an operator can talk into dropping it. When no public surface
/// is configured the answer is `Secure`, which is the safe direction: the only
/// cookie in play then is the developer's own device credential.
fn secure_attr(ctx: &Ctx<'_>) -> &'static str {
    match ctx.public {
        Some(p) if !p.secure_cookies => "",
        _ => "; Secure",
    }
}

/// The cookie remembering the reader's chosen language.
///
/// Separate from [`COOKIE`] and **not** `HttpOnly`: it holds a preference, not a
/// credential, and the public surface's script may want to read it. Nothing
/// authenticates on it, and its value is parsed by
/// [`Locale::parse`](crate::i18n::Locale::parse), which accepts only codes this
/// server has a catalogue for — so a hostile value selects nothing.
pub const LANG_COOKIE: &str = "sc_lang";

/// The public, unauthenticated surface's paths.
///
/// Held as constants in one place so the rate-limit classifier and the route
/// matcher cannot disagree about which paths are public — a path public to one
/// and private to the other is either a leak or a lockout.
///
/// **Matched by exact equality, never by prefix.** `starts_with("/public")` would
/// also match `/publicXYZ`, and on the private surface a loose prefix fails
/// *closed* (401) while here it fails *open*.
pub mod public_route {
    /// The filing form (`GET`) and the POST that files.
    pub const FILE: &str = "/public";
    /// Ask for a sign-in link.
    pub const SIGNIN: &str = "/public/signin";
    /// `/public/signin/<token>` — `GET` renders and **changes nothing**;
    /// `POST` spends the link.
    pub const SIGNIN_PREFIX: &str = "/public/signin/";
    /// Start a GitHub sign-in — renders a page with a link, and **changes
    /// nothing a GET should not**: it mints a state token, which is the one
    /// write, and is what makes the link single-use.
    pub const AUTH_GITHUB: &str = "/public/auth/github";
    /// Where GitHub sends a reader back, with `?code=…&state=…`.
    pub const AUTH_GITHUB_CALLBACK: &str = "/public/auth/github/callback";
    /// End a session.
    pub const SIGNOUT: &str = "/public/signout";
    /// `/public/request/<id>` — one of the filer's own requests.
    pub const REQUEST_PREFIX: &str = "/public/request/";
    /// Choose a language. `POST`, because it sets a cookie — and reachable
    /// **signed out**, since somebody who cannot read the sign-in page is
    /// precisely who needs it.
    pub const LANGUAGE: &str = "/public/language";
    /// The surface's own script. A served file rather than an inline block,
    /// because the policy is `script-src 'self'` and never `'unsafe-inline'`.
    pub const SCRIPT: &str = "/public/app.js";
    /// The body face, served from this origin.
    pub const FONT_BODY: &str = "/public/dm-sans.woff2";
    /// The display face, served from this origin.
    pub const FONT_DISPLAY: &str = "/public/fraunces.woff2";
    /// The landing page — what `/` is, and the first thing a stranger sees.
    pub const LANDING: &str = "/";
}

/// The developer's own paths.
///
/// Named here for the same reason the public ones are: the route matcher and the
/// enrolment gate must agree about which address shows the code box, and two
/// string literals in two functions eventually will not.
pub mod private_route {
    /// The review surface. **Moved off `/`**, which is now the landing page.
    pub const REVIEW: &str = "/review";
    /// Who may file, and the switch that stops them.
    pub const ACCOUNTS: &str = "/accounts";
    /// Who may review, and for what.
    ///
    /// **Device-only by virtue of living past the gate** — which is the whole
    /// argument that an owner cannot promote an owner. It is not a check inside
    /// the handler that somebody could forget to write.
    pub const OWNERS: &str = "/owners";
    /// Which repositories the public surface collects for.
    ///
    /// Admin-only for the same structural reason. **Turning the surface itself
    /// on moved here too**: the only caller who can reach it proved they can
    /// read this container's log, so the posture is preserved by *who* rather
    /// than by *where*. See [`crate::settings::Settings::public`].
    pub const REPOS: &str = "/repos";
    /// Claim an unclaimed server. **Exists only while unclaimed.**
    pub const SETUP: &str = "/setup";
    /// Step two of setup: the GitHub application.
    pub const SETUP_GITHUB: &str = "/setup/github";
    /// What this server does. Device-only, like every other admin page.
    pub const SETTINGS: &str = "/settings";
    /// Which machines may claim work.
    pub const DAEMONS: &str = "/daemons";
    /// The address and the three secrets. **Needs a fresh sign-in** — see
    /// [`SENSITIVE_VERBS`](super::SENSITIVE_VERBS).
    pub const SETTINGS_SECRET: &str = "/settings/secret";
    pub const SETTINGS_PUBLIC: &str = "/settings/public";
    pub const SETTINGS_SITE: &str = "/settings/site";
    pub const SETTINGS_MAIL: &str = "/settings/mail";
    pub const SETTINGS_SCREEN: &str = "/settings/screen";
    pub const SETTINGS_CAPS: &str = "/settings/caps";
}

/// The verbs that decide a request's fate.
///
/// Named once so the test proving an account cannot reach any of them iterates
/// this list rather than a hand-written copy that goes stale the moment a verb
/// is added.
pub const REVIEW_VERBS: [&str; 5] = [
    "accept",
    "accept/confirm",
    "send-back",
    "discard",
    "release",
];

/// A request, reduced to what the routes actually use.
#[derive(Debug, Clone)]
pub struct Req {
    pub method: String,
    pub path: String,
    /// The `Authorization: Bearer …` value, if any — how a daemon authenticates.
    pub bearer: Option<String>,
    /// The device token from the cookie, if any — how a browser authenticates.
    pub cookie_token: Option<String>,
    /// The setup token, from a browser part-way through claiming this server.
    ///
    /// A separate field rather than sharing [`cookie_token`](Req::cookie_token):
    /// the two authenticate unrelated things, and one name for both would mean
    /// deciding which was meant on every request.
    pub cookie_setup: Option<String>,
    /// The reader's chosen language, from the `lang` cookie.
    ///
    /// Named fields rather than a header map, so `Req` keeps its property of
    /// being **only what the routes actually use** — a bag invites reading
    /// whatever happens to be in it.
    pub cookie_lang: Option<String>,
    /// The `Accept-Language` header, as sent.
    pub accept_language: Option<String>,
    pub body: String,
}

impl Req {
    pub fn get(path: &str) -> Req {
        Req {
            method: "GET".into(),
            path: path.into(),
            bearer: None,
            cookie_token: None,
            cookie_setup: None,
            cookie_lang: None,
            accept_language: None,
            body: String::new(),
        }
    }

    pub fn post(path: &str, body: &str) -> Req {
        Req {
            method: "POST".into(),
            path: path.into(),
            bearer: None,
            cookie_token: None,
            cookie_setup: None,
            cookie_lang: None,
            accept_language: None,
            body: body.into(),
        }
    }

    /// The language this request is rendered in.
    ///
    /// Computed rather than stored, so there is no way to have a `Req` whose
    /// locale disagrees with the signals it was built from.
    pub fn locale(&self) -> Locale {
        crate::i18n::negotiate(self.cookie_lang.as_deref(), self.accept_language.as_deref())
    }

    #[cfg(test)]
    pub fn with_lang(mut self, cookie: Option<&str>, accept: Option<&str>) -> Req {
        self.cookie_lang = cookie.map(str::to_string);
        self.accept_language = accept.map(str::to_string);
        self
    }

    pub fn with_bearer(mut self, token: &str) -> Req {
        self.bearer = Some(token.to_string());
        self
    }

    pub fn with_cookie(mut self, token: &str) -> Req {
        self.cookie_token = Some(token.to_string());
        self
    }

    /// Carry a half-finished setup, as the browser doing it would.
    pub fn with_setup(mut self, token: &str) -> Req {
        self.cookie_setup = Some(token.to_string());
        self
    }
}

/// A response, ready to be written.
#[derive(Debug, Clone)]
pub struct Res {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
    /// Set instead of `body` for a response that is not text — a font.
    ///
    /// A separate field rather than making `body` a `Vec<u8>`: every other
    /// handler and every test builds and asserts on a `String`, and converting
    /// them all to bytes would be a large diff in service of two routes. `None`
    /// on everything but those, and the writer prefers this when it is set.
    ///
    /// `&'static [u8]` because the only sources are `include_bytes!` — there is
    /// no path from a request to a file on disk, and this type is what keeps it
    /// that way.
    pub binary: Option<&'static [u8]>,
    /// A `Set-Cookie` value, used exactly once: at enrolment.
    pub set_cookie: Option<String>,
    /// Set when the handler wants the caller to hold the connection open — the
    /// long-poll. The HTTP layer waits, then calls back.
    pub hold_for_work: bool,
    /// What this is served with. Defaults to [`Policy::Strict`]; the public
    /// surface is stamped in one place, in [`handle`], rather than by each
    /// handler remembering to.
    pub policy: Policy,
}

impl Res {
    pub fn json(status: u16, body: impl Into<String>) -> Res {
        Res {
            status,
            content_type: "application/json",
            body: body.into(),
            binary: None,
            set_cookie: None,
            hold_for_work: false,
            policy: Policy::Strict,
        }
    }

    pub fn html(status: u16, body: impl Into<String>) -> Res {
        Res {
            status,
            content_type: "text/html; charset=utf-8",
            body: body.into(),
            binary: None,
            set_cookie: None,
            hold_for_work: false,
            policy: Policy::Strict,
        }
    }

    /// Serve this on the public surface's policy.
    ///
    /// Called **once**, on everything `public_route` returns. Applied at the
    /// dispatch site rather than at each `Res::html` inside the public handlers,
    /// because there are twenty of those and one of them would eventually be
    /// added without it.
    fn with_policy(mut self, policy: Policy) -> Res {
        self.policy = policy;
        self
    }

    fn ok_json<T: serde::Serialize>(value: &T) -> Res {
        match serde_json::to_string(value) {
            Ok(json) => Res::json(200, json),
            Err(e) => error(500, &format!("could not encode the response: {e}")),
        }
    }
}

/// A JSON error body. Every failure looks the same shape, so a client has one
/// thing to parse rather than guessing per route.
fn error(status: u16, msg: &str) -> Res {
    let body = serde_json::to_string(&WireError::new(msg))
        .unwrap_or_else(|_| "{\"error\":\"unknown\"}".to_string());
    Res::json(status, body)
}

/// Which surface a response came from, and therefore what it is served with.
///
/// The two surfaces differ in exactly one way — whether the page may run script
/// — and this is where that difference lives. It is carried **on the response**
/// rather than decided in `serve.rs`, because by the time a response reaches the
/// socket writer the only thing left to distinguish the surfaces by is the path,
/// and matching the path a second time is a second copy of the routing table.
///
/// [`Strict`](Policy::Strict) is the `Default`. A handler that forgets to say
/// which surface it is on gets the *tighter* policy, so the failure is a public
/// page whose script does not run — visible immediately — rather than a private
/// page that quietly permits one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Policy {
    /// No script at all. The private surface, and everything not on the public
    /// one: JSON for the daemon, errors, the enrolment page.
    #[default]
    Strict,
    /// Script from this origin only. A filer's own pages.
    ///
    /// **The dividing line is how many distinct authors' model output one page
    /// renders**, not which surface it is on. A filer's pages show *their own*
    /// requests, so a script that went wrong reaches only its author's data —
    /// they already control the input and can already read the output. A page
    /// showing several filers' specs at once has no such argument, whichever
    /// surface it lives on.
    ///
    /// Stated this way because the first reading — "the public surface gets
    /// script" — was true only while the public surface had one kind of reader.
    /// The owner pages made it false: they sit on public paths and render every
    /// filer's spec for a repository, and were served with script until the
    /// policy started being chosen by *caller* rather than by path.
    PublicScript,
}

impl Policy {
    /// The `Content-Security-Policy` value.
    ///
    /// `default-src 'none'` on both, so a remote subresource is unreachable
    /// either way. **That is the directive doing the security work**, and every
    /// other entry here is a narrow re-permission of something same-origin.
    ///
    /// The distinction worth holding on to: the exfiltration argument is about
    /// *remote origins*, not about subresources as such. A font or a `fetch()`
    /// that never leaves this server tells nobody that a page was viewed, so
    /// permitting those costs nothing the argument was protecting. Permitting a
    /// remote one would cost all of it.
    pub fn csp(self) -> &'static str {
        match self {
            Policy::Strict => {
                "default-src 'none'; style-src 'unsafe-inline'; font-src 'self'; \
                 form-action 'self'; base-uri 'none'; frame-ancestors 'none'"
            }
            // `'self'` and not `'unsafe-inline'`: an inline-script allowance is
            // also what a successful injection needs, and the public surface is
            // the one rendering model-authored text. Script here must be a
            // served file.
            //
            // `connect-src 'self'` lets that script call this server — a live
            // status on a filed request without a reload — while leaving a call
            // to anywhere else refused. That is the shape that matters: script
            // able to *reach a third party* is what turns a rendered spec into
            // an exfiltration channel, and this grants none of it.
            Policy::PublicScript => {
                "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; \
                 font-src 'self'; connect-src 'self'; form-action 'self'; \
                 base-uri 'none'; frame-ancestors 'none'"
            }
        }
    }
}

/// The five headers on **every** response, without exception.
///
/// Rendering model-authored Markdown is an exfiltration path: one hallucinated
/// remote image leaks the page URL — which identifies the request — through the
/// `Referer` header. `sc-web` sends none of these today; spec 18 says this path
/// must not inherit that.
///
/// They are returned from one function rather than added per route, because a
/// header added per route is a header eventually missing from one.
pub fn security_headers(policy: Policy) -> [(&'static str, &'static str); 5] {
    [
        // No `Referer` anywhere, so a remote subresource cannot leak the URL.
        ("Referrer-Policy", "no-referrer"),
        // A drafted spec is not something to leave in a proxy or a browser cache.
        ("Cache-Control", "no-store"),
        // No remote subresources at all: the CSP is what makes the exfiltration
        // path unreachable rather than merely unreferred.
        ("Content-Security-Policy", policy.csp()),
        ("X-Content-Type-Options", "nosniff"),
        ("X-Frame-Options", "DENY"),
    ]
}

/// Everything a handler needs.
pub struct Ctx<'a> {
    pub store: &'a Store,
    pub daemon_keys: &'a [crate::config::DaemonKey],
    pub limiter: &'a mut RateLimiter,
    pub now_ms: u64,
    /// The public surface, when one is configured. `None` makes every public
    /// route **404** — the surface does not exist rather than existing and
    /// refusing, so a half-configured server cannot leak one.
    pub public: Option<&'a PublicConfig>,
    /// How sign-in links are sent.
    pub mailer: &'a dyn Mailer,
    /// Guards read-modify-write on `accounts.json` and `links.json`.
    ///
    /// `write_atomic` prevents a *torn* file but not a lost update: two
    /// concurrent signups both read, both append, and one silently vanishes.
    /// `credentials.json` has always had this and it never mattered — the
    /// developer enrolling two devices at once is not a real event. Self-serve
    /// public signup makes it one.
    pub write_lock: &'a Mutex<()>,
    /// Which daemons have polled recently, and what they offered to serve.
    ///
    /// Read by the review page to say *why* a request is not moving, and written
    /// by the poll. In memory and shared, like the rate limiter beside it —
    /// see [`crate::daemons`] for why it is not on disk.
    pub seen: &'a Mutex<crate::daemons::Seen>,
    /// Who may review what, re-read from the volume when it changes.
    ///
    /// A `Mutex` around a cache rather than a snapshot passed in: **revocation
    /// has to take effect on the request after it**, which is the property that
    /// made owners-in-configuration right and had to survive the move off it.
    /// See [`crate::roster::RosterCache`].
    pub roster: &'a Mutex<crate::roster::RosterCache>,
    /// What this server does, re-read from the volume when it changes.
    pub settings: &'a Mutex<crate::settings::SettingsCache>,
    /// Who is signed in, re-read from the volume when it changes.
    ///
    /// **On the hot path**, unlike the roster: this is the only credential store,
    /// so every cookie-bearing request consults it — including one carrying a
    /// cookie that matches nothing, resolved before the rate limiter runs. See
    /// [`crate::account::AccountsCache`].
    pub accounts: &'a Mutex<crate::account::AccountsCache>,
    /// The key sealed settings are read with, when one is configured.
    ///
    /// `None` means nothing can be sealed or opened — see [`crate::seal`]. A
    /// server given a wrong key refuses to boot rather than arriving here, so
    /// `None` here really is "no key", not "the wrong one".
    pub seal_key: Option<&'a crate::seal::SealKey>,
    /// Set when this request's session proved itself against GitHub within
    /// [`FRESH_AUTH_MS`](crate::account::FRESH_AUTH_MS).
    ///
    /// **Not a field on `Caller::Owner`**, and not a second caller variant. Two
    /// variants for one identity would multiply the gate; a boolean on the
    /// variant is a check somebody has to remember. Kept here, read only by the
    /// handlers that change a secret, and ignored by every other route.
    pub fresh_auth: bool,
    /// Set when the HTTP layer is re-checking a long poll it already holds.
    ///
    /// The hold re-runs [`handle`] every 250ms looking for work that may have
    /// arrived. Those passes are the *server's* own polling, not the caller's,
    /// and charging them to the caller's budget is what made an idle daemon
    /// rate-limit itself.
    pub rechecking: bool,
}

/// Route one request.
pub fn handle(ctx: &mut Ctx<'_>, req: &Req) -> Res {
    // The masthead names the repository this surface collects for. Set here,
    // once, rather than passed through ten renderers that have no other reason
    // to know it — and **cleared on the way out**, because a thread serves many
    // requests and a name left behind would appear on the next one.
    match ctx.public {
        Some(p) => crate::page::site::set(&p.site_name),
        None => crate::page::site::clear(),
    }
    let res = handle_inner(ctx, req);
    crate::page::site::clear();
    res
}

fn handle_inner(ctx: &mut Ctx<'_>, req: &Req) -> Res {
    let caller = identify(ctx, req);
    // Resolved once, beside the caller, so a handler cannot forget to ask and
    // cannot ask a second time and get a different answer within one request.
    ctx.fresh_auth = fresh_auth(ctx, req);

    let path = req.path.split('?').next().unwrap_or("").to_string();
    let method = req.method.as_str();

    // Rate limit before anything reads a *request record*, so a guessing or
    // filing loop costs one hash rather than a disk scan per attempt. The
    // credential store above is the one unavoidable read.
    //
    // The bucket depends on the path, which is why this sits after the split
    // rather than at the top: an anonymous caller on the public surface must not
    // share a budget with one guessing enrolment codes, or either can lock the
    // other out.
    //
    // **Skipped when the HTTP layer is re-checking a poll it is already
    // holding.** A held poll re-runs this function every 250ms for up to
    // `POLL_TIMEOUT`, so counting each pass charged one *request* about 120
    // times — an idle daemon burnt half its minute's budget every thirty
    // seconds, and two overlapping polls locked it out of its own server. The
    // caller has already been admitted once; the re-checks are the server's own
    // doing, not new traffic.
    if !ctx.rechecking && !ctx.limiter.allow(bucket_for(&caller, &path), ctx.now_ms) {
        return error(429, "too many requests — wait a minute and try again");
    }

    // The daemon-facing API. Its routes are shared constants, so the two ends
    // cannot disagree about the strings.
    if path.starts_with("/api/v1/work") {
        // The label is taken from the *resolved* caller, so what a daemon claims
        // and reports under is the machine whose key it presented — never
        // anything it sent.
        let Some(Caller::Daemon { label }) = &caller else {
            return error(401, "unauthorized");
        };
        let label = label.clone();
        return daemon_route(ctx, req, method, &path, &label);
    }

    // Setup is the one route reachable without a credential — it is how the
    // first one is obtained. Guarded by the single-use claim code instead.
    //
    // **It stops existing the moment the server is claimed.** Not "exists and
    // refuses" — a 404 means a stranger cannot tell a claimed server from one
    // that never had setup, and there is no page for them to keep trying.
    if path == private_route::SETUP || path == private_route::SETUP_GITHUB {
        return setup_route(ctx, req, method, &path);
    }

    // The public surface, matched **before** the device gate below — that gate is
    // what makes everything past it private, so anything reachable without a
    // device must be handled here or not at all.
    //
    // Skipped entirely when no public surface is configured, so the routes do not
    // exist rather than existing and refusing.
    //
    // This is also **the one place the public policy is applied**. Everything
    // `public_route` returns is stamped here, so a public handler cannot be
    // written without it and a private one cannot accidentally acquire it — the
    // two properties a per-handler `.with_policy()` call would each fail at.
    //
    // **Chosen by who the caller turned out to be, not by which path matched.**
    // The rule was never "the public surface gets script" — see [`Policy`]: it is
    // that a page rendering *one* author's model output can afford script, and a
    // page rendering *many* authors' cannot. An owner's pages are on a public
    // path and show every filer's spec for a repository, which is the second kind
    // and was being served as the first.
    if is_public_path(&path) {
        return match ctx.public {
            Some(_) => {
                let policy = match &caller {
                    Some(Caller::Owner { .. }) => Policy::Strict,
                    _ => Policy::PublicScript,
                };
                public_route(ctx, req, method, &path, &caller).with_policy(policy)
            }
            // No public surface configured: this 404 is not *on* that surface, so
            // it is served strict like every other non-public response.
            None => Res::html(404, crate::page::not_found()),
        };
    }

    // Everything else is the developer's own surface.
    //
    // **This pattern is what makes the owner role safe.** An owner may decline
    // work and may not accept it, and that is not enforced by a check inside
    // the accept handler — it is enforced here, by `Caller::Owner` not being
    // `Caller::Admin`. Every accepting verb lives past this line, so there is
    // no value of that variant which reaches one. An owner's own surface is
    // public-side, above.
    //
    // Both variants now arrive as a GitHub session, so the whole burden of
    // telling them apart sits in `identify` — one function, one branch, checked
    // before the roster is consulted. That is the right place for it and the
    // only place it happens.
    let Some(Caller::Admin { .. }) = caller else {
        // **Not found** rather than unauthorized: a 401 on `/review` tells a
        // stranger the address is real. That now covers a signed-in owner and a
        // signed-in filer too, which is the same answer for the same reason.
        //
        // The one accommodation is for the developer holding a cookie that used
        // to work: a bare 404 is a confusing answer to somebody who was enrolled
        // yesterday, so the page names the way back in when there is one.
        if method == "GET" {
            return Res::html(
                404,
                if has_github(ctx) {
                    crate::page::not_found_for_admin()
                } else {
                    crate::page::not_found()
                },
            );
        }
        return error(401, "unauthorized");
    };
    browser_route(ctx, req, method, &path)
}

/// Begin a GitHub sign-in: mint a state token and offer the link.
///
/// **A page with a link, not a redirect.** [`Res`] carries no `Location` header
/// and giving it one means changing the response writer for a single route —
/// and `set_language` records the standing objection to redirects on a surface
/// anyone can reach. A link also stays inside the CSP as written, where a form
/// posting to github.com would be refused by `form-action 'self'`.
/// The GitHub application, wherever it is configured.
///
/// **Settings first, then the environment.** An application entered through
/// setup lives on the volume; one from the stack is the older path and stays
/// readable so an existing deployment keeps working. Asked in one place so the
/// two cannot disagree about which is in force.
fn github_app(ctx: &Ctx<'_>) -> Option<(String, String)> {
    let settings = match ctx.settings.lock() {
        Ok(mut cache) => cache.current(&ctx.store.settings_path()),
        Err(p) => p.into_inner().current(&ctx.store.settings_path()),
    };
    if settings.has_github() {
        if let Some(secret) = settings.github_secret(ctx.seal_key) {
            return Some((settings.github_client_id.clone(), secret));
        }
    }
    ctx.public
        .and_then(|p| p.github.as_ref())
        .map(|g| (g.client_id.clone(), g.client_secret.clone()))
}

/// Begin the sign-in that will claim this server.
///
/// The same hop as an ordinary sign-in, marked on the state so the callback
/// knows the login it resolves should own the server. Marked there rather than
/// on a cookie because the state is already single-use, already expires, and is
/// already what the callback validates.
fn start_github_claim(ctx: &mut Ctx<'_>, locale: Locale) -> Res {
    start_github_inner(ctx, locale, true)
}

fn start_github(ctx: &mut Ctx<'_>, locale: Locale) -> Res {
    start_github_inner(ctx, locale, false)
}

fn start_github_inner(ctx: &mut Ctx<'_>, locale: Locale, claiming: bool) -> Res {
    let Some((client_id, client_secret)) = github_app(ctx) else {
        // No application configured: the route does not exist, rather than
        // existing and failing after a trip to GitHub.
        return Res::html(404, crate::page::public_not_found(locale));
    };
    let client = crate::oauth::HttpGithub::new(&client_id, &client_secret);

    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut states = match ctx.store.oauth_states() {
        Ok(s) => s,
        Err(e) => return error(500, &e.to_string()),
    };
    states.sweep(ctx.now_ms);
    // Bounded before minting, so the file cannot be grown without limit by
    // anyone who can reach the start of the flow.
    if states.outstanding(ctx.now_ms) >= crate::oauth::MAX_OUTSTANDING_STATES {
        return Res::html(
            429,
            crate::page::public_message(locale.strings().github_busy, locale),
        );
    }
    let state = if claiming {
        states.issue_claim(ctx.now_ms)
    } else {
        states.issue(ctx.now_ms)
    };
    if let Err(e) = ctx.store.put_oauth_states(&states) {
        return error(500, &e.to_string());
    }
    drop(_guard);

    Res::html(
        200,
        crate::page::github_start_page(&client.authorize_url(&state), locale),
    )
}

/// Come back from GitHub: spend the state, exchange the code, open a session.
///
/// Every failure renders the same page. The reader can only act on "it did not
/// work, try again", and distinguishing a forged state from an expired one for
/// them would tell an attacker which half they got right.
fn finish_github(ctx: &mut Ctx<'_>, req: &Req, locale: Locale) -> Res {
    let Some((client_id, client_secret)) = github_app(ctx) else {
        return Res::html(404, crate::page::public_not_found(locale));
    };
    let client = crate::oauth::HttpGithub::new(&client_id, &client_secret);
    finish_github_with(ctx, req, locale, &client)
}

/// The callback, against any [`Github`](crate::oauth::Github) client.
///
/// Split out so a test can drive the whole path with a stub — the alternative is
/// a test that talks to GitHub, which is not a test.
fn finish_github_with(
    ctx: &mut Ctx<'_>,
    req: &Req,
    locale: Locale,
    client: &dyn crate::oauth::Github,
) -> Res {
    let failed = || {
        Res::html(
            400,
            crate::page::public_message(locale.strings().github_failed, locale),
        )
    };

    // Set from the state below, which is the only place that knows. A claim
    // rides the hop rather than a cookie because the state is already single-use
    // and already validated here.
    let claiming;

    let query = crate::query::CallbackQuery::parse(&req.path);
    let (Some(code), Some(state)) = (query.code, query.state) else {
        // GitHub sends `?error=access_denied` when somebody declines, and that
        // is not a failure worth a different page: they chose it.
        return failed();
    };

    // The state is spent **before** the code is exchanged. A code that survives
    // a failed exchange is worth nothing; a state that survives one could be
    // replayed.
    {
        let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut states = match ctx.store.oauth_states() {
            Ok(s) => s,
            Err(e) => return error(500, &e.to_string()),
        };
        match states.consume(&state, ctx.now_ms) {
            Ok(was_claiming) => claiming = was_claiming,
            Err(_) => return failed(),
        }
        if let Err(e) = ctx.store.put_oauth_states(&states) {
            return error(500, &e.to_string());
        }
    }

    let login = match client.login_for(&code) {
        Ok(login) => login,
        Err(e) => {
            // Logged for the operator, never shown: the page says only that it
            // did not work. The error names GitHub's complaint, which is the
            // operator's business and not the reader's.
            crate::log::warn("github sign-in failed")
                .text("err", e)
                .emit();
            return failed();
        }
    };

    // **The second half of the claim.** The code proved whoever started this can
    // read the container's logs; GitHub has now proved who they are. Written
    // before the account, so a server that fails to record the account is still
    // owned rather than left claimable by the next person to read the log.
    //
    // Re-checked here rather than trusted from `/setup`: `Admin::claim` is
    // reached only when the server is *still* unclaimed, so two people who both
    // spent a code cannot both end up owning it — the first to finish wins and
    // the second is an ordinary sign-in.
    if claiming {
        let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut admin = match ctx.store.admin() {
            Ok(a) => a,
            Err(e) => return error(500, &e.to_string()),
        };
        if !admin.claimed() {
            admin.claim(&login, ctx.now_ms);
            if let Err(e) = ctx.store.put_admin(&admin) {
                return error(500, &e.to_string());
            }
            crate::log::warn("server claimed")
                .with("login", login.to_ascii_lowercase())
                .with("note", "this account now administers this server")
                .emit();
        }
    }

    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut accounts = match ctx.store.accounts() {
        Ok(a) => a,
        Err(e) => return error(500, &e.to_string()),
    };
    // **The administrator is exempt from the signup cap.** Otherwise a flood
    // that reaches the ceiling locks out the one person who can stop it — and
    // the surface that stops it is behind their sign-in.
    let is_admin = ctx.store.admin().map(|a| a.is(&login)).unwrap_or(false);

    // Keyed on the login rather than on an email: a GitHub profile's address is
    // often absent and can be several, so it is not an identity this can rely
    // on. The hash keeps the shape every other account has.
    let email_hash = auth::hash(&format!("github:{login}"));
    let existing = accounts.any_by_email(&email_hash).cloned();
    let id = match existing {
        Some(account) if account.revoked => {
            // Revoked stays revoked, and says nothing about why — the same
            // silence a revoked magic-link account gets.
            return failed();
        }
        Some(account) => account.id,
        None => {
            let cap = ctx.public.map(|p| p.max_accounts).unwrap_or(0);
            if !is_admin && accounts.accounts.len() >= cap {
                crate::log::warn("signup refused")
                    .with("accounts", accounts.accounts.len() as u64)
                    .with("cap", cap as u64)
                    .with("via", "github")
                    .emit();
                return failed();
            }
            let account = accounts.create(&email_hash, &login, ctx.now_ms);
            // What makes them recognisable as an owner later — and only that:
            // whether this login *is* one comes from the configuration, checked
            // on every request.
            let idx = accounts.accounts.len() - 1;
            accounts.accounts[idx].login = Some(login.clone());
            account.id
        }
    };

    // **A re-authentication lands on the browser's existing session** rather
    // than opening a second one. Otherwise proving yourself again would mean
    // signing out and back in, and the old session would stay live and stale
    // beside the new one — two credentials where the point was to refresh one.
    let session = match req
        .cookie_token
        .as_deref()
        .filter(|t| accounts.refresh_session(t, ctx.now_ms))
    {
        Some(existing) => existing.to_string(),
        None => accounts.open_session(&id, ctx.now_ms),
    };
    if let Err(e) = ctx.store.put_accounts(&accounts) {
        return error(500, &e.to_string());
    }
    invalidate_accounts(ctx);
    let secure = secure_attr(ctx);
    drop(_guard);

    let Some(repos) = ctx.public.map(|p| p.repos.clone()) else {
        return Res::html(404, crate::page::public_not_found(locale));
    };
    let mut res = Res::html(
        200,
        crate::page::public_file_page(&[], &repos, false, locale),
    );
    res.set_cookie = Some(format!(
        "{COOKIE}={session}; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age=31536000"
    ));
    res
}

/// Which budget this request is counted against.
///
/// An authenticated caller is keyed on **who it turned out to be** — a device
/// id, an account id, or a daemon's label — never on anything the caller
/// chooses, since a per-email or per-`X-Forwarded-For` bucket lets an attacker
/// mint a fresh budget per value, which is no limit at all. An anonymous one is
/// keyed on the route class.
///
/// All three identities are hashed before they become a bucket key, so the
/// limiter's map holds nothing that names a person or a machine.
fn bucket_for(caller: &Option<Caller>, path: &str) -> Bucket {
    match caller {
        // Keyed on the label, so each machine has its own budget: a daemon stuck
        // in a retry loop on one host cannot exhaust the allowance of another.
        Some(Caller::Daemon { label }) => Bucket::Credential(auth::hash(label)),
        // Keyed on the login, so the budget is per person: an administrator
        // on a phone and a laptop is one human, and the per-device budget
        // died with per-device credentials.
        Some(Caller::Admin { login }) => Bucket::Credential(auth::hash(login)),
        // A signed-in filer gets their own budget. Safe to key on, unlike an
        // email or a forwarded header, because an account id is minted by this
        // server and costs a confirmed mailbox to obtain — the caller cannot vary
        // it to mint fresh budgets.
        Some(Caller::Account { id }) => Bucket::Credential(auth::hash(id)),
        // Keyed on the login, so one owner's browser cannot spend another's
        // budget. Safe to key on for the same reason an account id is: it was
        // resolved by this server from configuration, not sent by the caller.
        Some(Caller::Owner { login, .. }) => Bucket::Credential(auth::hash(login)),
        None if is_public_path(path) => {
            // Asking for a link costs an email; spending one costs a disk write.
            // Everything else on the public surface is a page render, and
            // starving those would itself be the denial of service.
            if path == public_route::SIGNIN || path.starts_with(public_route::SIGNIN_PREFIX) {
                Bucket::PublicWrite
            } else {
                Bucket::PublicRead
            }
        }
        None => Bucket::AnonPrivate,
    }
}

/// Is this one of the public surface's paths?
///
/// Exact equality for fixed paths and `starts_with` only on prefixes ending in
/// `/`, so `/publicXYZ` cannot match — on the private surface a loose prefix
/// fails *closed* (401), but here it fails **open**.
fn is_public_path(path: &str) -> bool {
    // The landing page is public: it is what a stranger arriving at the bare
    // address sees, and it must render for somebody with no account at all.
    path == public_route::LANDING
        || path == public_route::FILE
        || path == public_route::SIGNIN
        // Both halves of the GitHub flow. Absent from this list they would fall
        // through to the private device gate and 404, which is a sign-in button
        // that leads nowhere.
        || path == public_route::AUTH_GITHUB
        || path == public_route::AUTH_GITHUB_CALLBACK
        || path == public_route::SIGNOUT
        || path == public_route::LANGUAGE
        || path == public_route::SCRIPT
        || path == public_route::FONT_BODY
        || path == public_route::FONT_DISPLAY
        || path.starts_with(public_route::SIGNIN_PREFIX)
        || path.starts_with(public_route::REQUEST_PREFIX)
}

/// Who is calling, if anyone.
///
/// One cookie name serves both a device and an account. Two names would force a
/// choice when both were present — and "both present" is what an attacker
/// constructs. Which thing a token authenticates is decided by which store it
/// matches, and the **device store is checked first**, so the developer's own
/// browser never pays for reading the account file.
/// The accounts as they are right now.
///
/// Through the cache, so the steady state is a `stat` rather than a parse of a
/// file a stranger can make grow. A poisoned lock recovers the guard rather than
/// failing the request: this is a read, and the worst a partial update costs is
/// one stale answer.
fn accounts_now(ctx: &Ctx<'_>) -> std::sync::Arc<crate::account::Accounts> {
    let path = ctx.store.accounts_path();
    match ctx.accounts.lock() {
        Ok(mut cache) => cache.current(&path),
        Err(p) => p.into_inner().current(&path),
    }
}

/// Make the next request re-read the accounts.
///
/// **Called after every write**, because revocation has to take effect on the
/// request after it — the same property the roster keeps, and the reason the
/// mtime alone is not trusted.
fn invalidate_accounts(ctx: &Ctx<'_>) {
    if let Ok(mut cache) = ctx.accounts.lock() {
        cache.invalidate();
    }
}

/// Was this request's session proved against GitHub recently enough to change a
/// secret?
///
/// Read from the session rather than the caller, because it is a property of
/// *this browser's* proof and not of the person holding it.
fn fresh_auth(ctx: &Ctx<'_>, req: &Req) -> bool {
    let Some(token) = req.cookie_token.as_deref() else {
        return false;
    };
    accounts_now(ctx).session_fresh(token, ctx.now_ms)
}

fn identify(ctx: &Ctx<'_>, req: &Req) -> Option<Caller> {
    if let Some(bearer) = &req.bearer {
        // Walked rather than looked up: `auth::matches` is constant-time over
        // fixed-width hashes, so the only thing this leaks is *how many* keys
        // are configured — a count, not a credential.
        if let Some(daemon) = ctx
            .daemon_keys
            .iter()
            .find(|d| auth::matches(bearer, &d.key_hash))
        {
            return Some(Caller::Daemon {
                label: daemon.label.clone(),
            });
        }
    }
    let token = req.cookie_token.as_deref()?;

    // **The account store is now read on every cookie-bearing request**, not
    // lazily behind a public surface. It has to be: it is the only credential
    // store left, so an administrator with no public surface must still be
    // recognised on their own private one.
    //
    // The ordering argument that used to protect it — check the small
    // developer-sized credentials file first, read the attacker-sized account
    // file only where signup exists — is therefore gone. `max_accounts` is what
    // bounds this now, and it does double duty: it caps signup *and* caps what
    // one request can be made to parse.
    let accounts = accounts_now(ctx);
    let account = accounts.session_for(token)?;

    // A GitHub login is required for anything above a filer. A magic-link
    // account has none and is an `Account` whatever any record says — so a
    // claim naming an email address grants nothing rather than escalating.
    //
    // **Not `?`.** Returning `None` here would make a signed-in filer
    // *anonymous* rather than an account, which silently drops them out of
    // every per-account cap.
    let Some(login) = account.login.as_deref() else {
        return Some(Caller::Account {
            id: account.id.clone(),
        });
    };

    // **The administrator, checked before the roster, returning immediately.**
    //
    // The early return is load-bearing. An administrator who *also* appears in
    // `owners.json` is easy to arrange — the seed may have put them there — and
    // without this they would match `owner_for` first and be identified as an
    // owner, losing their own server to a file they can edit from the UI.
    //
    // The claim lives on the volume and its only writer is past the gate, so an
    // owner cannot promote themselves into it. The old guarantee holds with the
    // words swapped: an administrator is claimed once, never self-appointed.
    let admin = ctx.store.admin().ok()?;
    if admin.is(login) {
        return Some(Caller::Admin {
            login: login.to_ascii_lowercase(),
        });
    }

    // An account whose identity is a GitHub login **the roster names** is an
    // owner. Reached only after the administrator branch, and only where a
    // public surface exists — an owner reviews public filings, so without one
    // there is nothing for the role to mean.
    //
    // The roster costs a `stat` rather than a parse on requests that get here.
    if let Some(public) = ctx.public {
        let roster = {
            let mut cache = ctx.roster.lock().ok()?;
            cache.current(&ctx.store.roster_path())
        };
        if let Some(owner) = roster.owner_for(login) {
            // **Intersected with what this surface actually serves.** The
            // roster and the repository list are separately editable, so a
            // record can name something no longer collected here. Granting it
            // would be a permission that looks applied and reaches nothing.
            let repos: Vec<String> = owner
                .repos
                .iter()
                .filter(|r| public.repos.accepts(r))
                .cloned()
                .collect();
            if !repos.is_empty() {
                return Some(Caller::Owner {
                    login: owner.login.clone(),
                    repos,
                });
            }
        }
    }
    Some(Caller::Account {
        id: account.id.clone(),
    })
}

// ---------------------------------------------------------------------------
// The daemon side
// ---------------------------------------------------------------------------

fn daemon_route(ctx: &mut Ctx<'_>, req: &Req, method: &str, path: &str, by: &str) -> Res {
    if method == "GET" && path == wire::route::WORK {
        // Parsed from the **raw** path: `path` arrives already split on `?`,
        // and the declaration is the part that was cut off.
        let declared = crate::query::PollQuery::parse(&req.path);
        // A daemon that named nothing is an older build, and gets what it always
        // got. `These` only when it actually said something, so "declared
        // nothing" and "serves nothing" stay different answers.
        let serves = if declared.repos.is_empty() {
            Serves::Anything
        } else {
            Serves::These(&declared.repos)
        };

        // Recorded before the claim, so a poll that finds nothing still counts
        // as evidence this daemon is alive and offering these repositories —
        // which is exactly the case the review page needs it for.
        if let Ok(mut seen) = ctx.seen.lock() {
            seen.saw(by, &declared.repos, !declared.repos.is_empty(), ctx.now_ms);
        }

        return match ctx.store.claim_next(serves, by) {
            Ok(Some(r)) => Res::ok_json(&PollResponse::work(work_item(&r))),
            // Nothing right now. The HTTP layer holds the connection open and
            // asks again; this body is what it sends if the hold expires.
            Ok(None) => {
                let mut res = Res::ok_json(&PollResponse::idle());
                res.hold_for_work = true;
                res
            }
            Err(e) => error(500, &e.to_string()),
        };
    }

    // /api/v1/work/<id>/<verb>
    let rest = path.strip_prefix("/api/v1/work/").unwrap_or("");
    let mut parts = rest.splitn(2, '/');
    let id = parts.next().unwrap_or("");
    let verb = parts.next().unwrap_or("");

    if method != "POST" || id.is_empty() {
        return error(404, "no such route");
    }

    match verb {
        "drafted" => {
            let payload: DraftedSpec = match serde_json::from_str(&req.body) {
                Ok(p) => p,
                Err(e) => return error(400, &format!("unreadable payload: {e}")),
            };
            if let Err(msg) = wire::check_protocol(payload.protocol, "the daemon") {
                return error(400, &msg);
            }
            match ctx
                .store
                .record_drafted(id, by, &payload.spec, &payload.artifact_dir)
            {
                Ok(_) => Res::json(200, "{\"ok\":true}"),
                Err(e) => error(404, &e.to_string()),
            }
        }
        "failed" => {
            let payload: DraftFailed = match serde_json::from_str(&req.body) {
                Ok(p) => p,
                Err(e) => return error(400, &format!("unreadable payload: {e}")),
            };
            if let Err(msg) = wire::check_protocol(payload.protocol, "the daemon") {
                return error(400, &msg);
            }
            match ctx.store.record_failed(id, by, &payload.reason) {
                Ok(_) => Res::json(200, "{\"ok\":true}"),
                Err(e) => error(404, &e.to_string()),
            }
        }
        "released" => {
            let payload: WorkReleased = match serde_json::from_str(&req.body) {
                Ok(p) => p,
                Err(e) => return error(400, &format!("unreadable payload: {e}")),
            };
            if let Err(msg) = wire::check_protocol(payload.protocol, "the daemon") {
                return error(400, &msg);
            }
            match ctx.store.record_released(id, by, &payload.reason) {
                Ok(_) => Res::json(200, "{\"ok\":true}"),
                Err(e) => error(404, &e.to_string()),
            }
        }
        _ => error(404, "no such route"),
    }
}

fn work_item(r: &Request) -> WorkItem {
    WorkItem {
        id: r.id.clone(),
        text: r.text.clone(),
        repo: r.repo.clone(),
        kind: r.kind,
        send_back_note: r.send_back_note.clone(),
    }
}

// ---------------------------------------------------------------------------
// The browser side
// ---------------------------------------------------------------------------

fn browser_route(ctx: &mut Ctx<'_>, req: &Req, method: &str, path: &str) -> Res {
    match (method, path) {
        // The review list. `/` used to be here and is now the landing page, so
        // an enrolled device arriving there is sent on rather than left looking
        // at a page meant for strangers.
        ("GET", private_route::REVIEW) | ("GET", "/index.html") => match ctx.store.all() {
            Ok(all) => Res::html(200, crate::page::index(&all)),
            Err(e) => error(500, &e.to_string()),
        },

        ("POST", "/file") => {
            let form = form_fields(&req.body);
            let text = form.get("text").cloned().unwrap_or_default();
            let repo = form.get("repo").cloned().unwrap_or_default();
            // `IntakeKind::parse` is the one parser, shared with the CLI — a
            // second one here would drift, and the short forms it accepts are
            // exactly what someone types on a phone.
            let kind = form
                .get("kind")
                .and_then(|k| IntakeKind::parse(k))
                .unwrap_or_default();
            file_request(ctx, &text, &repo, kind)
        }

        // Who can file, and the switch that stops them. Device-only by virtue of
        // living past the gate.
        ("GET", "/accounts") => match ctx.store.accounts() {
            Ok(a) => Res::html(200, crate::page::accounts_page(&a)),
            Err(e) => error(500, &e.to_string()),
        },

        ("POST", p) if p.starts_with("/accounts/") && p.ends_with("/revoke") => {
            let id = p
                .trim_start_matches("/accounts/")
                .trim_end_matches("/revoke");
            revoke_account(ctx, id)
        }

        // Who may review, and for what. **Device-only by virtue of living
        // here**, past the gate — which is what makes "an owner cannot promote
        // an owner" structural rather than a check somebody has to remember.
        ("GET", private_route::OWNERS) => owners_page(ctx),

        ("POST", private_route::OWNERS) => {
            let form = form_fields(&req.body);
            let login = form.get("login").map(|l| l.trim()).unwrap_or_default();
            set_owner(ctx, login, &form_values(&req.body, "repos"))
        }

        ("POST", p) if p.starts_with("/owners/") && p.ends_with("/revoke") => {
            let login = p.trim_start_matches("/owners/").trim_end_matches("/revoke");
            revoke_owner(ctx, login)
        }

        // Which machines may claim work.
        ("GET", private_route::DAEMONS) => daemons_page(ctx, None),

        ("POST", private_route::DAEMONS) => {
            let form = form_fields(&req.body);
            mint_daemon_key(ctx, form.get("label").map(|l| l.trim()).unwrap_or_default())
        }

        ("POST", p) if p.starts_with("/daemons/") && p.ends_with("/revoke") => {
            let label = p
                .trim_start_matches("/daemons/")
                .trim_end_matches("/revoke");
            revoke_daemon(ctx, label)
        }

        // What this server does.
        ("GET", private_route::SETTINGS) => show_settings(ctx, None, None),
        ("POST", p) if p.starts_with("/settings") => settings_write(ctx, req, p),

        // Which repositories collect publicly.
        ("GET", private_route::REPOS) => repos_page(ctx, None),

        ("POST", private_route::REPOS) => {
            let form = form_fields(&req.body);
            let name = form.get("name").map(|n| n.trim()).unwrap_or_default();
            // The override the refusal page offers. A separate field rather
            // than a second route, so the record of *which* case it was is
            // written by the same handler that decided it.
            let anyway = form.get("anyway").is_some_and(|v| v == "yes");
            enable_repo(ctx, name, anyway)
        }

        ("POST", p) if p.starts_with("/repos/") && p.ends_with("/disable") => {
            let name = p.trim_start_matches("/repos/").trim_end_matches("/disable");
            disable_repo(ctx, name)
        }

        ("GET", p) if p.starts_with("/request/") => {
            let id = p.trim_start_matches("/request/");
            match ctx.store.get(id) {
                Ok(Some(r)) => {
                    let who = who_serves(ctx, &r.repo);
                    Res::html(200, crate::page::detail(&r, &who))
                }
                Ok(None) => Res::html(404, crate::page::not_found()),
                Err(e) => error(500, &e.to_string()),
            }
        }

        ("POST", p) if p.starts_with("/request/") => {
            let rest = p.trim_start_matches("/request/");
            // `splitn(2, …)` leaves the remainder whole, so a two-segment verb
            // like `approve/confirm` arrives intact.
            let mut parts = rest.splitn(2, '/');
            let id = parts.next().unwrap_or("");
            let verb = parts.next().unwrap_or("");
            let form = form_fields(&req.body);

            // Asking is not deciding: this renders the confirmation and changes
            // nothing. Handled apart from the others because it alone returns a
            // page rather than a settled request.
            if verb == "accept" {
                return ask_to_accept(ctx, id);
            }

            // The two verbs that put a request back in the claimable queue cost
            // a drafting run each. Checked before the store is touched, and
            // against the request's own repository — which has to be read first,
            // because the budget is the repository's rather than the caller's.
            if matches!(verb, "send-back" | "release") {
                if let Ok(Some(r)) = ctx.store.get(id) {
                    if let Err(res) = drafting_budget(ctx, &r.repo) {
                        return res;
                    }
                }
            }

            let outcome = match verb {
                "accept/confirm" => {
                    let digest = form.get("digest").cloned().unwrap_or_default();
                    ctx.store.accept(id, &digest)
                }
                "send-back" => {
                    let notes = form.get("notes").cloned().unwrap_or_default();
                    ctx.store.send_back(id, &notes)
                }
                "discard" => ctx.store.discard(id),
                // The developer overruling the screener. Device-only by virtue
                // of living here, past the gate.
                "release" => ctx.store.release(id),
                _ => return error(404, "no such route"),
            };
            match outcome {
                Ok(r) => {
                    let who = who_serves(ctx, &r.repo);
                    Res::html(200, crate::page::detail(&r, &who))
                }
                Err(e) => Res::html(400, crate::page::message(&e.to_string())),
            }
        }

        _ => Res::html(404, crate::page::not_found()),
    }
}

/// What the register says about who could draft work for `repo`.
///
/// A poisoned lock means another thread panicked mid-poll. Recovered rather than
/// propagated, for the same reason the rate limiter recovers its own: this is a
/// hint shown on a page, and refusing to render the page because the hint is
/// unavailable would be a worse answer than rendering it without one.
fn who_serves(ctx: &Ctx<'_>, repo: &str) -> crate::page::Who {
    let seen = match ctx.seen.lock() {
        Ok(s) => s,
        Err(poisoned) => poisoned.into_inner(),
    };
    crate::page::Who {
        coverage: seen.coverage(repo, ctx.now_ms),
        offered: seen.offered(ctx.now_ms),
    }
}

// ---------------------------------------------------------------------------
// The public surface
//
// A sibling of `browser_route`, not a caller of it. The review verbs live in
// that function and this one never reaches them — unreachable by structure
// rather than by a check somebody has to remember.
// ---------------------------------------------------------------------------

fn public_route(
    ctx: &mut Ctx<'_>,
    req: &Req,
    method: &str,
    path: &str,
    caller: &Option<Caller>,
) -> Res {
    // A device is the developer, who has their own surface; an account is a
    // filer. Anyone else is signed out.
    let account_id = match caller {
        Some(Caller::Account { id }) => Some(id.clone()),
        _ => None,
    };
    // An owner reaches this surface too, and sees something different on it.
    // Cloned because `ctx` is taken mutably below.
    let owner = match caller {
        Some(Caller::Owner { login, repos }) => Some((login.clone(), repos.clone())),
        _ => None,
    };

    // Decided once here and passed down, rather than re-derived per page. Every
    // response from this surface is in the same language as every other, which
    // is not true if each renderer negotiates for itself.
    let locale = req.locale();

    // An owner's surface, before the filer routes below. Reached with the same
    // cookie on the same paths — what differs is who the caller turned out to
    // be, which is decided in `identify` from the configuration.
    if let Some((login, repos)) = owner {
        return owner_route(ctx, req, method, path, &login, &repos, locale);
    }

    match (method, path) {
        // The landing page. What the bare address is, and the only page here
        // that says what any of this is for.
        //
        // A signed-in filer is sent to their own page instead: they have already
        // read the pitch, and the thing they came back for is what they filed.
        ("GET", public_route::LANDING) => match &account_id {
            Some(id) => match mine(ctx, id) {
                Ok(list) => match ctx.public {
                    Some(p) => Res::html(
                        200,
                        crate::page::public_file_page(&list, &p.repos, p.show_spec, locale),
                    ),
                    // Unreachable past `is_public_path`, which only matches when
                    // a surface exists — but the surface is what holds the set,
                    // so this asks rather than unwrapping.
                    None => Res::html(404, crate::page::public_not_found(locale)),
                },
                Err(e) => error(500, &e.to_string()),
            },
            None => Res::html(200, crate::page::landing_page(locale)),
        },

        // Ask for a link. Reachable signed-out — it is how one signs in.
        ("GET", public_route::SIGNIN) => Res::html(
            200,
            crate::page::signin_page_full(locale, has_github(ctx), has_mail(ctx)),
        ),

        // Start a GitHub sign-in, and come back from one. Both reachable
        // signed-out for the same reason.
        ("GET", public_route::AUTH_GITHUB) => start_github(ctx, locale),
        ("GET", public_route::AUTH_GITHUB_CALLBACK) => finish_github(ctx, req, locale),
        ("POST", public_route::SIGNIN) => request_sign_in(ctx, req),

        ("POST", public_route::SIGNOUT) => sign_out(ctx, req),

        // Choosing a language. Signed out on purpose: somebody who cannot read
        // the sign-in page is exactly who needs this, and requiring an account
        // first would mean reading a page in a language they do not have.
        ("POST", public_route::LANGUAGE) => set_language(ctx, req),

        // The surface's script. Static, identical for everyone, and reachable
        // signed out — the sign-in page carries the language switcher it
        // enhances.
        ("GET", public_route::SCRIPT) => Res {
            content_type: "text/javascript; charset=utf-8",
            ..Res::html(200, crate::page::PUBLIC_SCRIPT)
        },

        // The two faces, compiled into the binary. Same-origin by construction:
        // there is no path from a request to a file on disk here, so no request
        // can name one.
        ("GET", public_route::FONT_BODY) => font(crate::page::FONT_BODY),
        ("GET", public_route::FONT_DISPLAY) => font(crate::page::FONT_DISPLAY),

        // The landing page a link opens. **Changes nothing** — mail scanners
        // fetch every URL in a message, and a GET that spent the token would
        // burn it before the human saw it.
        ("GET", p) if p.starts_with(public_route::SIGNIN_PREFIX) => {
            let token = p.trim_start_matches(public_route::SIGNIN_PREFIX);
            // Rendered whether or not the token is real: a 404 on an invalid one
            // would be a free validity oracle, cheaper than the POST it guards.
            Res::html(200, crate::page::signin_confirm_page(token, locale))
        }
        ("POST", p) if p.starts_with(public_route::SIGNIN_PREFIX) => {
            let token = p.trim_start_matches(public_route::SIGNIN_PREFIX);
            complete_sign_in(ctx, token, locale)
        }

        // Everything below needs a signed-in filer.
        _ => match account_id {
            Some(id) => signed_in_route(ctx, req, method, path, &id, locale),
            None => Res::html(
                200,
                crate::page::signin_page_full(locale, has_github(ctx), has_mail(ctx)),
            ),
        },
    }
}

fn signed_in_route(
    ctx: &mut Ctx<'_>,
    req: &Req,
    method: &str,
    path: &str,
    account_id: &str,
    locale: Locale,
) -> Res {
    let show_spec = ctx.public.map(|p| p.show_spec).unwrap_or(false);
    // Cloned rather than borrowed: `ctx` is taken mutably by the handlers below,
    // and a set of a few short names is cheaper than restructuring the match
    // around a borrow that only one arm needs.
    let repos = ctx.public.map(|p| p.repos.clone());

    match (method, path) {
        ("GET", public_route::FILE) => match (mine(ctx, account_id), repos) {
            (Ok(list), Some(repos)) => Res::html(
                200,
                crate::page::public_file_page(&list, &repos, show_spec, locale),
            ),
            (Ok(_), None) => Res::html(404, crate::page::public_not_found(locale)),
            (Err(e), _) => error(500, &e.to_string()),
        },
        ("POST", public_route::FILE) => file_publicly(ctx, req, account_id),

        ("GET", p) if p.starts_with(public_route::REQUEST_PREFIX) => {
            let id = p.trim_start_matches(public_route::REQUEST_PREFIX);
            match ctx.store.get(id) {
                // `filed_by`, never the id alone: ids are time-ordered and
                // enumerable in seconds, so keying on one would let any signed-in
                // filer read every other filer's requests — and the developer's.
                Ok(Some(r)) if r.filed_by(account_id) => {
                    Res::html(200, crate::page::public_detail(&r, show_spec, locale))
                }
                // Somebody else's request is *not found*, not forbidden:
                // "forbidden" would confirm the id exists.
                Ok(_) => Res::html(404, crate::page::public_not_found(locale)),
                Err(e) => error(500, &e.to_string()),
            }
        }

        _ => Res::html(404, crate::page::public_not_found(locale)),
    }
}

/// Stop an account filing.
///
/// **The lever that makes self-serve signup acceptable.** Without a route it
/// would mean editing `accounts.json` on the volume by hand, which is not a
/// backstop anyone reaches for at the moment they need it.
///
/// Every session dies at once, because liveness is derived from the account
/// rather than copied onto each session.
fn revoke_account(ctx: &mut Ctx<'_>, id: &str) -> Res {
    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut accounts = match ctx.store.accounts() {
        Ok(a) => a,
        Err(e) => return error(500, &e.to_string()),
    };
    if !accounts.revoke(id) {
        // Already revoked, or never existed. Not an error worth a page: the
        // caller asked for a state that now holds.
        return match ctx.store.accounts() {
            Ok(a) => Res::html(200, crate::page::accounts_page(&a)),
            Err(e) => error(500, &e.to_string()),
        };
    }
    if let Err(e) = ctx.store.put_accounts(&accounts) {
        return error(500, &e.to_string());
    }
    invalidate_accounts(ctx);
    Res::html(200, crate::page::accounts_page(&accounts))
}

/// Render the roster.
///
/// **404 with no public surface**, like every other page that only means
/// something when one exists: an owner reviews public filings, so a
/// private-only server has nothing for this page to administer.
fn owners_page(ctx: &Ctx<'_>) -> Res {
    // **Serves with the public surface off**, where it used to 404. That
    // guard made sense while the switch was an environment variable and this
    // page could not turn it on; now it can, so 404ing here would hide the
    // page exactly when somebody came to fix the thing it is about. An owner
    // reviews public filings, so with no surface the list is simply empty and
    // the page says why.
    let served = ctx.public.map(|p| p.repos.clone()).unwrap_or_default();
    match ctx.store.roster() {
        Ok(r) => Res::html(200, crate::page::owners_page(&r, &served)),
        Err(e) => error(500, &e.to_string()),
    }
}

/// Promote somebody, or change what they own.
///
/// **Repository names are matched against what this surface serves**, never
/// taken on trust — the same rule as a public filing, for the same reason. A
/// name that matches nothing would be a permission that looks applied and
/// grants nothing, which is precisely what the configuration used to refuse to
/// boot on and a record cannot.
fn set_owner(ctx: &mut Ctx<'_>, login: &str, repos: &[String]) -> Res {
    let Some(public) = ctx.public else {
        return Res::html(404, crate::page::not_found());
    };
    if login.is_empty() || login.len() > 64 {
        return error(400, "a GitHub username is required");
    }
    // GitHub's own rule, so a typo is refused here rather than becoming a
    // record that can never match anybody.
    if !login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return error(400, "that is not a GitHub username");
    }
    let known: Vec<String> = repos
        .iter()
        .map(|r| r.trim().to_string())
        .filter(|r| public.repos.accepts(r))
        .collect();
    if known.is_empty() {
        // Not silently written as an owner of nothing: that reads on the page
        // as promoted and grants nothing at all.
        return error(400, "pick at least one repository this server serves");
    }

    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut roster = match ctx.store.roster() {
        Ok(r) => r,
        Err(e) => return error(500, &e.to_string()),
    };
    roster.set_owner(login, &known, ctx.now_ms);
    // Seeding is a first-use thing, and this volume has now been administered.
    // Without it a restart would re-apply the setting over a hand-made roster.
    roster.seeded = true;
    if let Err(e) = ctx.store.put_roster(&roster) {
        return error(500, &e.to_string());
    }
    invalidate_roster(ctx);
    Res::html(200, crate::page::owners_page(&roster, &public.repos))
}

/// Demote somebody.
fn revoke_owner(ctx: &mut Ctx<'_>, login: &str) -> Res {
    let Some(public) = ctx.public else {
        return Res::html(404, crate::page::not_found());
    };
    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut roster = match ctx.store.roster() {
        Ok(r) => r,
        Err(e) => return error(500, &e.to_string()),
    };
    // Already revoked, or never there. Not an error worth a page: the caller
    // asked for a state that now holds.
    if roster.revoke(login) {
        if let Err(e) = ctx.store.put_roster(&roster) {
            return error(500, &e.to_string());
        }
        invalidate_roster(ctx);
    }
    Res::html(200, crate::page::owners_page(&roster, &public.repos))
}

/// Which repositories collect publicly.
///
/// `unconfirmed` carries the name a daemon could not be found for, so the page
/// can offer the override instead of a bare error.
fn repos_page(ctx: &Ctx<'_>, unconfirmed: Option<&str>) -> Res {
    // Serves with the public surface off, for the reason `owners_page` gives:
    // enabling a repository is part of turning one on.
    let offered = match ctx.seen.lock() {
        Ok(seen) => seen.offered(ctx.now_ms),
        Err(p) => p.into_inner().offered(ctx.now_ms),
    };
    match ctx.store.roster() {
        Ok(r) => Res::html(200, crate::page::repos_page(&r, &offered, unconfirmed)),
        Err(e) => error(500, &e.to_string()),
    }
}

/// Collect publicly for a repository.
///
/// **The daemon check is a typo-catcher, not a security gate**, and is built as
/// one. Enabling `smrt-coder` writes a name nothing will ever claim: filings
/// pile up against a repository that does not exist, and the surface looks
/// broken rather than misconfigured. Asking a machine that is polling right now
/// catches that at the moment it is cheapest to fix.
///
/// It cannot be a refusal, because a `None` is not proof of a typo:
/// [`Seen`](crate::daemons::Seen) is empty for the first half-minute after a
/// restart, and a daemon on an older build declares nothing at all. So an
/// unconfirmed name is *questioned* — the page names the case, lists what is
/// actually on offer, and offers to proceed. Taking the override records
/// `served_by: None`, so a repository enabled without confirmation stays
/// visibly distinguishable from one a machine vouched for.
fn enable_repo(ctx: &mut Ctx<'_>, name: &str, anyway: bool) -> Res {
    if name.is_empty() || name.len() > crate::config::MAX_REPO_NAME {
        return error(400, "a repository name is required");
    }

    let served_by = match ctx.seen.lock() {
        Ok(seen) => seen.declared_by(name, ctx.now_ms),
        Err(p) => p.into_inner().declared_by(name, ctx.now_ms),
    };
    if served_by.is_none() && !anyway {
        return repos_page(ctx, Some(name));
    }

    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut roster = match ctx.store.roster() {
        Ok(r) => r,
        Err(e) => return error(500, &e.to_string()),
    };
    roster.enable(name, served_by, ctx.now_ms);
    // This volume has now been administered; see `set_owner`.
    roster.seeded = true;
    if let Err(e) = ctx.store.put_roster(&roster) {
        return error(500, &e.to_string());
    }
    invalidate_roster(ctx);
    repos_page(ctx, None)
}

/// Stop collecting for a repository.
///
/// **Nothing already filed is touched.** Disabling closes the door; it does not
/// discard what came through it, and the developer's own review surface still
/// shows every request. Deleting them would make this button destructive in a
/// way its name does not say.
fn disable_repo(ctx: &mut Ctx<'_>, name: &str) -> Res {
    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut roster = match ctx.store.roster() {
        Ok(r) => r,
        Err(e) => return error(500, &e.to_string()),
    };
    if roster.disable(name) {
        if let Err(e) = ctx.store.put_roster(&roster) {
            return error(500, &e.to_string());
        }
        invalidate_roster(ctx);
    }
    repos_page(ctx, None)
}

/// Make the next identification re-read the roster.
///
/// The mtime would usually catch it anyway. A filesystem with coarse timestamps
/// can record this write inside the same tick as the read before it — and a
/// developer who revokes somebody, reloads, and sees no change would reasonably
/// conclude it had not worked.
fn invalidate_roster(ctx: &Ctx<'_>) {
    if let Ok(mut cache) = ctx.roster.lock() {
        cache.invalidate();
    }
}

/// This filer's own requests, newest first.
fn mine(ctx: &Ctx<'_>, account_id: &str) -> sc_proto::Result<Vec<Request>> {
    Ok(ctx
        .store
        .all()?
        .into_iter()
        .filter(|r| r.filed_by(account_id))
        .collect())
}

/// Refuse a verb that would buy another drafting run when the repository has
/// already spent its day.
///
/// **Called by every verb that re-admits work**, and a function rather than
/// three copies for exactly that reason: `send-back` and `release` both move a
/// request back to `Queued`, and a fourth added later either calls this or
/// visibly does not.
///
/// The gap this closes was open from the moment a request could be re-admitted
/// at all. `max_daily_filings` is checked when something is *filed*, keyed on
/// the filer — so a request already filed re-enters the queue for nothing,
/// however often. Each re-entry is a full drafting run on the developer's
/// machine.
///
/// **The developer's own verbs are bounded too**, deliberately. The cap states
/// what one project may cost in a day, and a send-back loop from a redraft that
/// keeps coming back wrong is as easy for the developer to cause as for an
/// owner. Reaching it should read as information about the day, not as a
/// refusal of authority.
fn drafting_budget(ctx: &Ctx<'_>, repo: &str) -> std::result::Result<(), Res> {
    let Some(public) = ctx.public else {
        return Ok(());
    };
    let since = ctx.now_ms.saturating_sub(crate::config::FILING_WINDOW_MS);
    match ctx.store.drafts_since(repo, since) {
        Ok(n) if n >= public.max_daily_drafts => {
            crate::log::warn("drafting budget reached")
                .with("repo", repo.to_string())
                .with("drafts", n as u64)
                .with("cap", public.max_daily_drafts as u64)
                .emit();
            Err(error(
                429,
                &format!(
                    "{repo} has been drafted {n} times today, which is the limit. \
                     Every send-back and release buys another full drafting run on \
                     the developer's machine, so the cap is what stops one \
                     repository spending a day's budget on one request."
                ),
            ))
        }
        Ok(_) => Ok(()),
        // A store that cannot be read is not a budget decision. Refusing here
        // would turn a transient read failure into a refusal that looks like a
        // spend limit.
        Err(_) => Ok(()),
    }
}

/// The verbs an owner may reach.
///
/// `send-back` and `discard` decide *against* work, and their failure mode is
/// lost work — visible on the page, and the filer can file again.
///
/// **`release` is the exception, and knowingly so.** It moves a quarantined
/// request to `Queued`, so a daemon will draft it: the one owner power that
/// reaches the developer's machine. It is here because an owner who can see
/// their repository's queue and not unblock it has to ask the developer for
/// every screening false positive, which makes the role decorative. What makes
/// it affordable is [`drafting_budget`] — the cost is bounded per repository,
/// so the worst case is a day's drafting runs and not an unbounded loop.
///
/// **`accept` is the verb no owner has**, and the reason is different from
/// cost. Accepting settles a request; building it means opening the IDE and
/// running the pipeline, which is the developer's machine and the developer's
/// call. That is not enforced here but structurally: every accepting route
/// lives behind the `Caller::Admin` match on the private surface, which no
/// `Caller::Owner` satisfies.
///
/// Named beside [`REVIEW_VERBS`] so the two lists can be compared at a glance,
/// and so a verb added there is *absent* here until somebody decides otherwise —
/// which is the safe direction for that omission to fall.
pub const OWNER_VERBS: [&str; 3] = ["send-back", "discard", "release"];

/// Everything filed against the repositories an owner owns.
///
/// **Every state**, not just what awaits a decision: an owner asking "why has
/// nothing happened to this" needs to see it sitting queued or failed, and a
/// page that showed only `AwaitingReview` could not answer them.
///
/// Filtered on the caller's own repository set — resolved once when they were
/// identified — so the question asked here is "is this request's repository in
/// their set", never "who are they, and what does the configuration say". A
/// site that re-derived it could forget to.
fn owned(ctx: &Ctx<'_>, repos: &[String]) -> sc_proto::Result<Vec<Request>> {
    Ok(ctx
        .store
        .all()?
        .into_iter()
        .filter(|r| repos.contains(&r.repo))
        .collect())
}

/// What an owner may do: read their repositories' work, decline it, and unblock
/// it. Never accept it — see [`OWNER_VERBS`].
///
/// Every route here re-checks that the request's repository is one of theirs.
/// The check is against the set carried on the caller, resolved once at
/// identification — so a request for somebody else's repository is **not
/// found**, not forbidden: a 403 would confirm the id is real to somebody with
/// no business knowing it, which is the same reasoning that makes another
/// filer's request 404 rather than 401.
fn owner_route(
    ctx: &mut Ctx<'_>,
    req: &Req,
    method: &str,
    path: &str,
    login: &str,
    repos: &[String],
    locale: Locale,
) -> Res {
    let not_found = || Res::html(404, crate::page::public_not_found(locale));

    match (method, path) {
        // Their list, wherever they land.
        ("GET", public_route::LANDING) | ("GET", public_route::FILE) => match owned(ctx, repos) {
            Ok(list) => Res::html(200, crate::page::owner_page(&list, login, repos, locale)),
            Err(e) => error(500, &e.to_string()),
        },

        ("GET", p) if p.starts_with(public_route::REQUEST_PREFIX) => {
            let id = p.trim_start_matches(public_route::REQUEST_PREFIX);
            match ctx.store.get(id) {
                Ok(Some(r)) if repos.contains(&r.repo) => {
                    Res::html(200, crate::page::owner_detail(&r, locale))
                }
                // Theirs or not, the answer to a repository they do not own is
                // the same as to one that does not exist.
                Ok(_) => not_found(),
                Err(e) => error(500, &e.to_string()),
            }
        }

        ("POST", p) if p.starts_with(public_route::REQUEST_PREFIX) => {
            let rest = p.trim_start_matches(public_route::REQUEST_PREFIX);
            let mut parts = rest.splitn(2, '/');
            let id = parts.next().unwrap_or("");
            let verb = parts.next().unwrap_or("");

            // **The allowlist, checked before anything is read.** An owner
            // reaching `approve` here would be refused even if the private gate
            // somehow let them through — belt and braces on the property that
            // matters most in this file.
            if !OWNER_VERBS.contains(&verb) {
                return not_found();
            }

            // The repository is carried out of this read rather than fetched
            // again: the ownership check and the drafting budget ask about the
            // same record, and two reads could disagree.
            let repo = match ctx.store.get(id) {
                Ok(Some(r)) if repos.contains(&r.repo) => r.repo,
                Ok(_) => return not_found(),
                Err(e) => return error(500, &e.to_string()),
            };

            // `send-back` puts the request back in the claimable queue, so it
            // costs a drafting run on the developer's machine — bounded per
            // repository, not per owner.
            if matches!(verb, "send-back" | "release") {
                if let Err(res) = drafting_budget(ctx, &repo) {
                    return res;
                }
            }

            let form = form_fields(&req.body);
            let outcome = match verb {
                "send-back" => {
                    let note = form.get("note").cloned().unwrap_or_default();
                    ctx.store.send_back(id, note.trim())
                }
                "discard" => ctx.store.discard(id),
                // The one owner verb that admits work. Screening is a model
                // judging a stranger's text, so it has false positives — and an
                // owner who can see their repository's queue but not unblock it
                // has to ask the developer about every one of them. Bounded by
                // the budget taken above.
                "release" => ctx.store.release(id),
                // Unreachable past the allowlist above; refused rather than
                // unwrapped so a later edit to `OWNER_VERBS` cannot open a path
                // through here by accident.
                _ => return not_found(),
            };
            match outcome {
                Ok(r) => {
                    crate::log::info("owner decided")
                        .with("owner", login.to_string())
                        .with("verb", verb.to_string())
                        .with("repo", r.repo.clone())
                        .emit();
                    Res::html(200, crate::page::owner_detail(&r, locale))
                }
                Err(e) => Res::html(400, crate::page::public_message(&e.to_string(), locale)),
            }
        }

        // Signing out is the same act for everybody.
        ("POST", public_route::SIGNOUT) => sign_out(ctx, req),
        ("POST", public_route::LANGUAGE) => set_language(ctx, req),

        _ => not_found(),
    }
}

/// Send a sign-in link, or quietly do nothing.
///
/// **The response is identical in every case** — unknown address, existing
/// account, revoked account, malformed input, over the outstanding cap. Only
/// what gets *sent* differs, so this cannot be used to discover whether an
/// address has an account.
fn request_sign_in(ctx: &mut Ctx<'_>, req: &Req) -> Res {
    // **Refused outright when nothing can send.** Every other failure here is
    // deliberately indistinguishable — the page looks the same whether or not
    // mail went out, so it cannot be used to test whether an address has an
    // account. That argument does not reach this one: "this server has no mail
    // provider" is not a fact about any person, and silently accepting an
    // address nobody will ever act on is the worse answer.
    //
    // It is reachable because the masthead dialog is rendered by the shell,
    // which does not know what is configured. Rather than thread that through
    // fourteen callers, the honest refusal lives here, where the answer is
    // known.
    if !has_mail(ctx) {
        return Res::html(
            503,
            crate::page::public_message(req.locale().strings().signin_no_mail, req.locale()),
        )
        .with_policy(Policy::PublicScript);
    }

    let form = form_fields(&req.body);
    let raw = form.get("email").cloned().unwrap_or_default();
    let sent = try_send_link(ctx, &raw);
    if let Err(e) = sent {
        // Logged for the operator, never shown: the page must look the same
        // whether or not mail went out.
        //
        // `warn`, not `error`: the common cause is the outstanding-links cap,
        // which is the design working.
        //
        // The error text is the only part that varies, and none of the paths
        // that produce it put the address in: `try_send_link` refuses with a
        // fixed string, `Unconfigured` with another, and `HttpMailer` formats
        // only its provider slug and the transport error — deliberately, for
        // this reason (see `mail.rs`).
        crate::log::warn("sign-in link not sent")
            .text("err", e)
            .emit();
    }
    Res::html(200, crate::page::signin_sent_page(req.locale()))
}

/// Everything that might refuse, kept apart from the response so the response
/// cannot accidentally depend on it.
fn try_send_link(ctx: &mut Ctx<'_>, raw_email: &str) -> sc_proto::Result<()> {
    let Some(public) = ctx.public else {
        return Ok(());
    };
    if !account::valid_email(raw_email) {
        return Ok(());
    }
    let email = account::normalize_email(raw_email);
    let email_hash = auth::hash(&email);

    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());

    // A revoked account is sent **nothing at all**. A "your account was revoked"
    // mail is one an attacker can trigger at a victim's address, and the
    // revocation was a decision that does not need re-litigating by email.
    let accounts = ctx.store.accounts()?;
    if let Some(existing) = accounts.any_by_email(&email_hash) {
        if existing.revoked {
            return Ok(());
        }
    }

    let mut links = ctx.store.links()?;
    links.sweep(ctx.now_ms);
    // The real ceiling on mail spend: refused *before* the mailer is called.
    if links.outstanding(ctx.now_ms) >= public.max_outstanding_links {
        return Err(sc_proto::DcError::Eval(
            "too many sign-in links are outstanding".to_string(),
        ));
    }

    let token = links.issue(&email_hash, &account::email_hint(&email), ctx.now_ms);
    ctx.store.put_links(&links)?;
    drop(_guard);

    let url = format!(
        "{}{}{}",
        public.base_url,
        public_route::SIGNIN_PREFIX,
        token
    );
    ctx.mailer.send(
        &email,
        crate::mail::SUBJECT,
        &crate::mail::sign_in_body(&url),
    )
}

/// Spend a link: create the account if new, and open a session.
fn complete_sign_in(ctx: &mut Ctx<'_>, token: &str, locale: Locale) -> Res {
    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());

    let mut links = match ctx.store.links() {
        Ok(l) => l,
        Err(e) => return error(500, &e.to_string()),
    };
    let (email_hash, email_hint) = match links.consume(token, ctx.now_ms) {
        Ok(v) => v,
        Err(account::LinkError::AlreadyUsed) => {
            return Res::html(200, crate::page::signin_failed_page(true, locale))
        }
        Err(account::LinkError::Invalid) => {
            return Res::html(200, crate::page::signin_failed_page(false, locale))
        }
    };
    if let Err(e) = ctx.store.put_links(&links) {
        return error(500, &e.to_string());
    }

    let mut accounts = match ctx.store.accounts() {
        Ok(a) => a,
        Err(e) => return error(500, &e.to_string()),
    };
    // A revoked account must not be handed back by signing in again, or
    // revocation would mean nothing. The link was already spent, so this is not
    // a retry loop.
    if let Some(existing) = accounts.any_by_email(&email_hash) {
        if existing.revoked {
            return Res::html(200, crate::page::signin_failed_page(false, locale));
        }
    }
    let id = match accounts.by_email(&email_hash) {
        // Signing in again as somebody who already has an account creates
        // nothing, so the ceiling below never blocks an existing filer.
        Some(a) => a.id.clone(),
        None => {
            // What the per-account filing cap rests on. An id an attacker cannot
            // *vary* is one they can **re-mint**: a script with a hundred
            // disposable addresses would otherwise hold a hundred budgets.
            let cap = ctx.public.map(|p| p.max_accounts).unwrap_or(0);
            if accounts.accounts.len() >= cap {
                // Logged for the operator, because a signup wall is something
                // they need to know they have hit — the page below says only
                // that it did not work.
                crate::log::warn("signup refused")
                    .with("accounts", accounts.accounts.len() as u64)
                    .with("cap", cap as u64)
                    .emit();
                return Res::html(200, crate::page::signin_failed_page(false, locale));
            }
            accounts.create(&email_hash, &email_hint, ctx.now_ms).id
        }
    };
    let session = accounts.open_session(&id, ctx.now_ms);
    if let Err(e) = ctx.store.put_accounts(&accounts) {
        return error(500, &e.to_string());
    }
    invalidate_accounts(ctx);
    let secure = secure_attr(ctx);
    let Some(repos) = ctx.public.map(|p| p.repos.clone()) else {
        // Unreachable: signing in is a public route and only exists when a
        // surface does.
        return Res::html(404, crate::page::public_not_found(locale));
    };

    let mut res = Res::html(
        200,
        crate::page::public_file_page(&[], &repos, false, locale),
    );
    res.set_cookie = Some(format!(
        "{COOKIE}={session}; Path=/; HttpOnly; SameSite=Strict{secure}; Max-Age=31536000"
    ));
    res
}

/// End this session.
fn sign_out(ctx: &mut Ctx<'_>, req: &Req) -> Res {
    if let Some(token) = &req.cookie_token {
        let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
        if let Ok(mut accounts) = ctx.store.accounts() {
            let hashed = auth::hash(token);
            if let Some(s) = accounts
                .sessions
                .iter_mut()
                .find(|s| s.token_hash == hashed)
            {
                s.revoked = true;
                let _ = ctx.store.put_accounts(&accounts);
                invalidate_accounts(ctx);
            }
        }
    }
    let secure = secure_attr(ctx);
    let mut res = Res::html(
        200,
        crate::page::signin_page_full(req.locale(), has_github(ctx), has_mail(ctx)),
    );
    // Max-Age=0 so the browser drops it rather than carrying a dead token.
    res.set_cookie = Some(format!(
        "{COOKIE}=; Path=/; HttpOnly; SameSite=Strict{secure}; Max-Age=0"
    ));
    res
}

/// One of the two faces, as bytes.
///
/// **Still `no-store`, like every other response.** A font could safely be
/// cached for a year, and re-fetching 100KB per page is a real cost on the bad
/// connection this surface is designed for. It is not done here because the
/// `Cache-Control` header is returned from one function for *every* response —
/// which is what stops a drafted spec being left in a proxy — and carving an
/// exception into that would make "no response is cached" a claim with an
/// asterisk rather than a fact. The browser's own connection reuse absorbs most
/// of it; if the cost ever shows up in practice, the fix is a per-response
/// cache policy carried on `Res`, not a special case in the header function.
fn font(bytes: &'static [u8]) -> Res {
    Res {
        status: 200,
        content_type: "font/woff2",
        body: String::new(),
        binary: Some(bytes),
        set_cookie: None,
        hold_for_work: false,
        policy: Policy::PublicScript,
    }
}

/// Remember the reader's language.
///
/// Takes no session and touches no store: this sets a preference cookie and
/// re-renders. It is the one public write that costs nothing to serve, which is
/// why it is safe to leave reachable signed out.
///
/// **There is no `next=` parameter and no redirect.** A "return to where you
/// were" field on a route reachable by anyone is an open redirect waiting to be
/// found, and this surface is small enough that landing on the sign-in page —
/// now in the chosen language — is no real loss.
fn set_language(ctx: &Ctx<'_>, req: &Req) -> Res {
    let fields = form_fields(&req.body);
    // An unknown code selects the default rather than erroring. The value is
    // matched against the catalogues this server actually has, so nothing a
    // caller writes here reaches a page except by choosing among them.
    let locale = fields
        .get("lang")
        .and_then(|v| Locale::parse(v))
        .unwrap_or_default();

    let secure = secure_attr(ctx);
    let mut res = Res::html(
        200,
        crate::page::signin_page_full(locale, has_github(ctx), has_mail(ctx)),
    );
    // Not `HttpOnly`: this is a preference, not a credential, and the public
    // surface's script may read it. `SameSite=Lax` rather than `Strict` so that
    // arriving from an external link — which is how somebody reaches a filing
    // page — still shows the language they chose.
    res.set_cookie = Some(format!(
        "{LANG_COOKIE}={}; Path=/; SameSite=Lax{secure}; Max-Age=31536000",
        locale.code()
    ));
    res
}

/// Is there a GitHub application for an owner to sign in with?
///
/// **The same condition [`start_github`] exists on**, asked in one place so the
/// button and the route cannot drift apart. They already had: the flow was
/// built and reachable and no page linked to it, so the only way in was to know
/// the URL. Offering a link where the route 404s is the same bug pointing the
/// other way.
/// Can this server send a sign-in link?
///
/// Rendered on, so a surface with no provider says so rather than offering a
/// form that accepts an address and sends nothing.
fn has_mail(ctx: &Ctx<'_>) -> bool {
    ctx.public.is_some_and(|p| p.mail.is_some())
}

fn has_github(ctx: &Ctx<'_>) -> bool {
    github_app(ctx).is_some()
}

/// Claim an unclaimed server.
///
/// **Every arm 404s once claimed**, so setup stops existing rather than existing
/// and refusing — a stranger cannot tell a claimed server from one that never
/// had this route, and there is nothing to keep trying.
fn setup_route(ctx: &mut Ctx<'_>, req: &Req, method: &str, path: &str) -> Res {
    let gone = || Res::html(404, crate::page::not_found());

    let admin = match ctx.store.admin() {
        Ok(a) => a,
        Err(e) => return error(500, &e.to_string()),
    };
    if admin.claimed() {
        return gone();
    }
    let settings = match ctx.store.settings() {
        Ok(s) => s,
        Err(e) => return error(500, &e.to_string()),
    };

    // **Everything past step one belongs to the browser that spent the code.**
    //
    // Without this the later steps are guarded only by the server being
    // unclaimed, so a half-finished setup on a public hostname is open to
    // whoever arrives next — and they would supply their own GitHub
    // application, sign in, and own the server.
    //
    // It bit on a *migrated* volume rather than a fresh one: seeding fills in
    // the address, so "step one is already done" was true for everybody from
    // the first boot.
    let mine = admin.setting_up(req.cookie_setup.as_deref(), ctx.now_ms);

    match (method, path) {
        ("GET", private_route::SETUP) => {
            // Step two only for the browser that reached it. Anybody else is
            // sent back to the code box — which they cannot pass without
            // reading the container's log.
            if mine && !settings.base_url.is_empty() {
                return Res::html(
                    200,
                    crate::page::setup_github_page(&settings.base_url, None),
                );
            }
            Res::html(200, crate::page::setup_page("", None))
        }

        ("POST", private_route::SETUP) => {
            let form = form_fields(&req.body);
            let base_url = form.get("base_url").map(|b| b.trim()).unwrap_or_default();
            let code = form.get("code").map(|c| c.trim()).unwrap_or_default();

            // **The address is checked before the code is spent.** A typo in the
            // address must not burn the one code the operator has, leaving them
            // to restart the container to get another.
            if let Err(e) = crate::config::check_base_url(base_url) {
                return Res::html(400, crate::page::setup_page(base_url, Some(&e)));
            }

            let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
            let mut admin = match ctx.store.admin() {
                Ok(a) => a,
                Err(e) => return error(500, &e.to_string()),
            };
            let Some(setup_token) = admin.spend(code, ctx.now_ms) else {
                // One message for every failure — wrong, expired, already spent.
                // Distinguishing them tells a guesser which half they got right.
                return Res::html(
                    400,
                    crate::page::setup_page(
                        base_url,
                        Some(
                            "That code did not work. It is printed in the                              container's log at startup, is single-use, and                              expires after thirty minutes — restart the server                              for a fresh one.",
                        ),
                    ),
                );
            };
            if let Err(e) = ctx.store.put_admin(&admin) {
                return error(500, &e.to_string());
            }

            let mut settings = match ctx.store.settings() {
                Ok(s) => s,
                Err(e) => return error(500, &e.to_string()),
            };
            settings.base_url = base_url.to_string();
            // This volume has now been administered, so the environment must not
            // seed over it on the next boot.
            settings.seeded = true;
            if let Err(e) = ctx.store.put_settings(&settings) {
                return error(500, &e.to_string());
            }
            invalidate_settings(ctx);

            // **The rest of the wizard belongs to this browser now.** Without
            // it, every step after this one is guarded only by the server being
            // unclaimed, and somebody arriving mid-way could supply their own
            // GitHub application and take the server.
            let secure = secure_attr(ctx);
            let mut res = Res::html(200, crate::page::setup_github_page(base_url, None));
            res.set_cookie = Some(format!(
                "{SETUP_COOKIE}={setup_token}; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age={}",
                crate::admin::CLAIM_TTL_MS / 1000
            ));
            res
        }

        ("POST", private_route::SETUP_GITHUB) => {
            // **The step that would hand the server over.** Naming a GitHub
            // application decides which account can finish the claim, so this
            // is the one an interloper wants.
            if !mine {
                return Res::html(
                    400,
                    crate::page::setup_page(
                        "",
                        Some(
                            "Start again from the claim code. Setting this                              server up has to be finished in the browser that                              started it.",
                        ),
                    ),
                );
            }
            if settings.base_url.is_empty() {
                return Res::html(400, crate::page::setup_page("", None));
            }
            let form = form_fields(&req.body);
            let id = form.get("client_id").map(|v| v.trim()).unwrap_or_default();
            let secret = form
                .get("client_secret")
                .map(|v| v.trim())
                .unwrap_or_default();
            if id.is_empty() || secret.is_empty() {
                return Res::html(
                    400,
                    crate::page::setup_github_page(
                        &settings.base_url,
                        Some("Both halves are needed: an id without a secret is a                               sign-in button that always fails."),
                    ),
                );
            }
            let Some(key) = ctx.seal_key else {
                return Res::html(
                    400,
                    crate::page::setup_github_page(
                        &settings.base_url,
                        Some("This server has no sealing key, so it cannot store a                               secret. Set SC_SERVER_SECRET_KEY and restart."),
                    ),
                );
            };

            let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
            let mut settings = match ctx.store.settings() {
                Ok(s) => s,
                Err(e) => return error(500, &e.to_string()),
            };
            settings.set_github(key, id, secret, ctx.now_ms);
            settings.seeded = true;
            if let Err(e) = ctx.store.put_settings(&settings) {
                return error(500, &e.to_string());
            }
            invalidate_settings(ctx);
            drop(_guard);

            // Straight into the sign-in that decides who owns this. An
            // interstitial with a link rather than a redirect, for the reason
            // `oauth` gives.
            start_github_claim(ctx, req.locale())
        }

        _ => gone(),
    }
}

/// Which machines may claim work.
fn daemons_page(ctx: &Ctx<'_>, minted: Option<(&str, &str)>) -> Res {
    match ctx.store.roster() {
        Ok(r) => Res::html(200, crate::page::daemons_page(&r, minted)),
        Err(e) => error(500, &e.to_string()),
    }
}

/// Mint a credential for a machine.
///
/// **The server generates it and shows it once.** That is strictly better than
/// an environment variable, which sits in plaintext in a stack editor for as
/// long as the deployment lives: this is unrecoverable rather than permanently
/// readable, and only its hash reaches the volume.
///
/// Minting for a label that already has a key **replaces** it, which is how a
/// key is rotated. That machine stops being able to claim until it is updated,
/// and the page says so — true of a stack edit too, but a button does not look
/// like a deploy.
fn mint_daemon_key(ctx: &mut Ctx<'_>, label: &str) -> Res {
    if label.is_empty() || label.len() > 64 {
        return error(400, "a machine needs a name");
    }
    // The label lands in a URL for revocation and in every log line about this
    // machine, so it is kept to what reads back unambiguously.
    if !label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return error(400, "letters, numbers, dashes and underscores");
    }

    let key = auth::mint_secret();
    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut roster = match ctx.store.roster() {
        Ok(r) => r,
        Err(e) => return error(500, &e.to_string()),
    };
    roster.set_daemon(label, &auth::hash(&key), ctx.now_ms);
    roster.seeded = true;
    if let Err(e) = ctx.store.put_roster(&roster) {
        return error(500, &e.to_string());
    }
    invalidate_roster(ctx);
    crate::log::info("daemon key minted")
        .with("label", label.to_string())
        .emit();
    Res::html(200, crate::page::daemons_page(&roster, Some((label, &key))))
}

/// Stop trusting a machine.
///
/// Takes effect on that daemon's **next poll** rather than at the next restart,
/// because the poll path reads the roster through its cache.
fn revoke_daemon(ctx: &mut Ctx<'_>, label: &str) -> Res {
    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut roster = match ctx.store.roster() {
        Ok(r) => r,
        Err(e) => return error(500, &e.to_string()),
    };
    if roster.revoke_daemon(label) {
        if let Err(e) = ctx.store.put_roster(&roster) {
            return error(500, &e.to_string());
        }
        invalidate_roster(ctx);
        crate::log::warn("daemon key revoked")
            .with("label", label.to_string())
            .emit();
    }
    Res::html(200, crate::page::daemons_page(&roster, None))
}

/// The routes that change a secret, and therefore need a fresh sign-in.
///
/// Named once so the test proving each one refuses a stale session iterates this
/// list rather than a copy that goes stale when a second is added. A route added
/// here without a `require_fresh` call fails that test, which is the safe
/// direction for the omission to fall.
pub const SENSITIVE_VERBS: [&str; 1] = [private_route::SETTINGS_SECRET];

/// Refuse a change made on a session that has not proved itself recently.
///
/// **This is what a secret costs on top of a session.** An administrator's
/// session already reaches accept, discard and owner promotion; that is the same
/// blast radius the device cookie had. Secrets are where it stops being
/// acceptable — somebody holding a stolen cookie must not be able to rotate the
/// mail key and redirect every sign-in link — so this asks for GitHub *at that
/// moment* rather than at some point in the past.
fn require_fresh(ctx: &Ctx<'_>) -> std::result::Result<(), Res> {
    if ctx.fresh_auth {
        return Ok(());
    }
    Err(show_settings(
        ctx,
        None,
        Some(
            "Changing a secret needs a fresh sign-in, and it has been more than \
             five minutes since this browser last proved itself. Sign in again, \
             then change it.",
        ),
    ))
}

/// What this server does.
fn show_settings(ctx: &Ctx<'_>, saved: Option<&str>, problem: Option<&str>) -> Res {
    match ctx.store.settings() {
        Ok(s) => Res::html(
            if problem.is_some() { 400 } else { 200 },
            crate::page::settings_page(&s, ctx.fresh_auth, saved, problem),
        ),
        Err(e) => error(500, &e.to_string()),
    }
}

/// Change something.
///
/// One handler for every settings form, because they share a shape: read under
/// the lock, mutate, write, invalidate, re-render. Splitting them would be six
/// copies of that sequence and six chances to forget the invalidate.
fn settings_write(ctx: &mut Ctx<'_>, req: &Req, path: &str) -> Res {
    // **The gate, before anything is read.** Only the secret form needs it, and
    // it is checked here rather than inside that arm so the list above is the
    // single place the requirement is stated.
    if SENSITIVE_VERBS.contains(&path) {
        if let Err(res) = require_fresh(ctx) {
            return res;
        }
    }

    let form = form_fields(&req.body);
    let field = |name: &str| form.get(name).map(|v| v.trim().to_string());
    // A blank number is "use the default", not zero — the page says so.
    let cap = |name: &str| -> Option<Option<usize>> {
        match field(name) {
            None => Some(None),
            Some(v) if v.is_empty() => Some(None),
            Some(v) => v.parse::<usize>().ok().map(Some),
        }
    };

    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut s = match ctx.store.settings() {
        Ok(s) => s,
        Err(e) => return error(500, &e.to_string()),
    };

    let saved = match path {
        private_route::SETTINGS_PUBLIC => {
            // A toggle rather than a checkbox: an unticked box submits nothing,
            // so a form that could turn it *on* could never turn it off.
            s.public = !s.public;
            // Names the direction, so the confirmation is not the same sentence
            // whichever way it went.
            if s.public {
                "The public site is now on, and that"
            } else {
                "The public site is now off, and that"
            }
        }
        private_route::SETTINGS_SITE => {
            s.site_name = field("site_name").unwrap_or_default();
            s.show_spec = Some(field("show_spec").is_some());
            "The site"
        }
        private_route::SETTINGS_MAIL => {
            let provider = field("mail_provider").unwrap_or_default();
            if !provider.is_empty() && crate::mail::Provider::parse(&provider).is_none() {
                return show_settings(
                    ctx,
                    None,
                    Some("That is not a provider this server can send with. One of: brevo, resend, postmark."),
                );
            }
            s.mail_provider = provider;
            s.mail_from = field("mail_from").unwrap_or_default();
            s.mail_from_name = field("mail_from_name").unwrap_or_default();
            "Email"
        }
        private_route::SETTINGS_SCREEN => {
            s.screen_url = field("screen_url").unwrap_or_default();
            s.screen_model = field("screen_model").unwrap_or_default();
            "Screening"
        }
        private_route::SETTINGS_CAPS => {
            let (Some(f), Some(d), Some(a), Some(l)) = (
                cap("max_daily_filings"),
                cap("max_daily_drafts"),
                cap("max_accounts"),
                cap("max_outstanding_links"),
            ) else {
                return show_settings(ctx, None, Some("Those have to be whole numbers, or blank."));
            };
            s.max_daily_filings = f;
            s.max_daily_drafts = d;
            s.max_accounts = a;
            s.max_outstanding_links = l;
            "The ceilings"
        }
        private_route::SETTINGS_SECRET => {
            if let Some(base) = field("base_url") {
                if let Err(e) = crate::config::check_base_url(&base) {
                    return show_settings(ctx, None, Some(&format!("That address {e}")));
                }
                s.base_url = base;
            }
            let Some(key) = ctx.seal_key else {
                return show_settings(
                    ctx,
                    None,
                    Some(
                        "This server has no sealing key, so it cannot store a \
                         secret. Set SC_SERVER_SECRET_KEY and restart.",
                    ),
                );
            };
            // **Blank means keep.** A form that cleared a key every time it was
            // submitted for another reason would be a trap.
            for (name, slot) in [
                ("mail_key", &mut s.mail_key),
                ("github_client_secret", &mut s.github_client_secret),
                ("screen_key", &mut s.screen_key),
            ] {
                if let Some(v) = form.get(name).map(|v| v.trim()).filter(|v| !v.is_empty()) {
                    *slot = crate::seal::seal(key, v, ctx.now_ms);
                }
            }
            "The address and secrets"
        }
        _ => return Res::html(404, crate::page::not_found()),
    };

    // Administered, so the environment must not seed over it on the next boot.
    s.seeded = true;
    if let Err(e) = ctx.store.put_settings(&s) {
        return error(500, &e.to_string());
    }
    invalidate_settings(ctx);
    drop(_guard);
    show_settings(ctx, Some(saved), None)
}

/// Make the next request re-read the settings.
fn invalidate_settings(ctx: &Ctx<'_>) {
    if let Ok(mut cache) = ctx.settings.lock() {
        cache.invalidate();
    }
}

/// A filing that named no repository this surface collects for.
///
/// Deliberately says nothing about which ones it *does* — the picker already
/// lists those to anyone who reached the form honestly.
fn refuse_repo(locale: Locale) -> Res {
    Res::html(
        400,
        crate::page::public_message(locale.strings().file_repo_unknown, locale),
    )
    .with_policy(Policy::PublicScript)
}

/// File a request from the public surface.
///
/// The repository is **chosen from the configured set, never taken from the
/// body on trust** — so a stranger cannot aim work at a repository the operator
/// did not nominate for public intake.
///
/// A surface serving several repositories has to let the filer say which, so
/// the body does carry a name. What keeps the guarantee is that the name is
/// only ever *matched* against the configured set: it selects, it does not
/// introduce. One that matches nothing is refused rather than defaulted, for
/// the reason set out at the check itself.
fn file_publicly(ctx: &mut Ctx<'_>, req: &Req, account_id: &str) -> Res {
    let locale = req.locale();
    let Some(public) = ctx.public else {
        return Res::html(404, crate::page::public_not_found(locale));
    };
    let screened = public.screen.is_some();

    let form = form_fields(&req.body);

    // **Checked against the configured set, never trusted.** The picker renders
    // from that same set, so an honest filer always sends one of these; anything
    // else was hand-crafted, and the answer is to refuse rather than to fall
    // back on a default.
    //
    // Falling back would file the request against a repository the filer did not
    // choose, and nothing on the page would say so — the work would simply land
    // somewhere else. A refusal is the honest failure, and it keeps this the one
    // place a repository name is decided.
    //
    // A surface serving one repository renders no field, so an absent name is
    // normal there and takes the only one. With several, absent means the form
    // was not the thing that sent this.
    //
    // With **none** enabled, `first()` is `None` and this falls through to the
    // refusal below — which is right: there is nothing to file against, and the
    // page a filer arrived on already says so.
    let repo = match form.get("repo").map(|r| r.trim()) {
        None | Some("") if public.repos.is_single() => match public.repos.first() {
            Some(only) => only.to_string(),
            None => return refuse_repo(locale),
        },
        Some(named) if public.repos.accepts(named) => named.to_string(),
        _ => return refuse_repo(locale),
    };
    let text = form.get("text").cloned().unwrap_or_default();
    let text = text.trim();
    let kind = form
        .get("kind")
        .and_then(|k| IntakeKind::parse(k))
        .unwrap_or_default();

    if text.is_empty() {
        return Res::html(
            400,
            crate::page::public_message(locale.strings().error_empty, locale),
        );
    }
    if let Err(msg) = check_length(text) {
        return Res::html(400, crate::page::public_message(&msg, locale));
    }

    // The ceiling on model spend. Every filing that clears the screener costs a
    // full drafting run on the developer's machine, and the per-credential rate
    // limit is no defence against something that expensive.
    //
    // Counted and written **under the same lock** the account paths hold.
    // Without it the count-then-write is a race: a filer holding two sessions,
    // or one script issuing parallel POSTs, would have every request read the
    // same pre-write total and every one of them pass — an overshoot bounded by
    // concurrency rather than by the cap.
    //
    // Checked *before* the record is written, so a refused filing costs a
    // directory read and nothing else.
    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());

    let since = ctx.now_ms.saturating_sub(crate::config::FILING_WINDOW_MS);
    match ctx.store.filed_since(account_id, since) {
        Ok(n) if n >= public.max_daily_filings => {
            return Res::html(
                429,
                crate::page::public_message(
                    &format!(
                        "That is {n} requests in a day, which is the limit. Each one is \
                     written up by hand on someone's machine, so the cap is there to \
                     keep that manageable — try again tomorrow, or say the rest in a \
                     request you have already filed."
                    ),
                    locale,
                ),
            );
        }
        Ok(_) => {}
        Err(e) => return error(500, &e.to_string()),
    }

    // Stamped from the handler's clock, which is the one the cap above measures
    // against — two clock sources in one decision is a window that never quite
    // lines up with the records it counts.
    let request = Request::public(
        new_id(),
        text,
        &repo,
        kind,
        account_id,
        screened,
        ctx.now_ms,
    );
    match ctx.store.put(&request) {
        Ok(()) => Res::html(200, crate::page::public_filed(&request, locale)),
        Err(e) => error(500, &e.to_string()),
    }
}

/// Render the confirmation for an approval. **Changes nothing.**
///
/// The first of two deliberate steps (spec 20). It restates what is being
/// approved and carries a digest of the exact text shown, which
/// [`Store::approve`](crate::store::Store::approve) re-checks on submit — so the
/// approval binds to bytes the reviewer saw rather than to whatever is on disk
/// when the second POST lands.
fn ask_to_accept(ctx: &mut Ctx<'_>, id: &str) -> Res {
    let req = match ctx.store.get(id) {
        Ok(Some(r)) => r,
        Ok(None) => return Res::html(404, crate::page::not_found()),
        Err(e) => return error(500, &e.to_string()),
    };
    if req.state != RequestState::AwaitingReview {
        return Res::html(
            400,
            crate::page::message(&format!(
                "This request is {} — only one awaiting review can be approved.",
                req.state.label()
            )),
        );
    }
    let (Some(spec), Some(digest)) = (req.spec.clone(), req.spec_digest()) else {
        return Res::html(
            400,
            crate::page::message("There is no drafted spec to approve yet."),
        );
    };
    Res::html(200, crate::page::confirm_accept(&req, &spec, &digest))
}

/// The longest a request may be.
///
/// Words rather than bytes, because it is a limit a person can hold in their head
/// while typing — "500 words" means something, "16 KB" does not.
///
/// Deliberately short. Three things fall out of it beyond the obvious volume cap:
/// the screener sees the **whole** request rather than a truncation, so spam
/// cannot be hidden past the cut; the text stays readable on a phone, which is
/// the review surface's whole premise; and a request this size is a *request*
/// rather than a specification — the spec is drafted from it, not copied from it.
pub const MAX_WORDS: usize = 500;

/// A hard byte ceiling behind the word count.
///
/// 500 "words" of pathological input — one enormous token with no whitespace —
/// is unbounded otherwise. Generous enough that no honest 500-word request hits
/// it first.
pub const MAX_BYTES: usize = 8 * 1024;

/// Is this request text within the limits?
///
/// Shared by every filing path so the public and enrolled surfaces cannot drift
/// to different limits — which would make the screener's "sees the whole text"
/// property true on one and false on the other.
pub fn check_length(text: &str) -> std::result::Result<(), String> {
    let words = text.split_whitespace().count();
    if words > MAX_WORDS {
        return Err(format!(
            "That is {words} words; the limit is {MAX_WORDS}. Say the essential \
             part — a spec is drafted from your request, not copied from it."
        ));
    }
    if text.len() > MAX_BYTES {
        return Err(format!(
            "That is {} characters, which is over the {MAX_BYTES} limit even \
             though it is under {MAX_WORDS} words.",
            text.len()
        ));
    }
    Ok(())
}

/// File a request.
///
/// The repository is a **name**, taken from a fixed list the page renders. There
/// is no field here for a path, so traversal is unreachable rather than
/// mitigated — the daemon resolves the name against its own configured set and
/// refuses anything absent (spec 18).
fn file_request(ctx: &mut Ctx<'_>, text: &str, repo: &str, kind: IntakeKind) -> Res {
    let text = text.trim();
    if text.is_empty() {
        return Res::html(400, crate::page::message("A request needs some text."));
    }
    if repo.trim().is_empty() {
        return Res::html(
            400,
            crate::page::message("Choose which repository this is about."),
        );
    }
    if let Err(msg) = check_length(text) {
        return Res::html(400, crate::page::message(&msg));
    }

    let req = Request::new(new_id(), text, repo.trim(), kind);
    match ctx.store.put(&req) {
        Ok(()) => Res::html(200, crate::page::filed(&req)),
        Err(e) => error(500, &e.to_string()),
    }
}

/// Parse `a=1&b=2` with percent-decoding.
fn form_fields(body: &str) -> std::collections::HashMap<String, String> {
    body.split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|pair| {
            let mut kv = pair.splitn(2, '=');
            let k = kv.next()?;
            let v = kv.next().unwrap_or("");
            Some((percent_decode(k), percent_decode(v)))
        })
        .collect()
}

/// Every value submitted under one name, in order.
///
/// [`form_fields`] collects into a map and so keeps only the last of a repeated
/// name. That is right for every field that is a single input and wrong for a
/// checkbox group, where "repos=a&repos=b" is one answer with two parts —
/// silently keeping `b` would grant an owner one repository of the two ticked.
fn form_values(body: &str, name: &str) -> Vec<String> {
    body.split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|pair| {
            let mut kv = pair.splitn(2, '=');
            let k = kv.next()?;
            let v = kv.next().unwrap_or("");
            (percent_decode(k) == name).then(|| percent_decode(v))
        })
        .filter(|v| !v.trim().is_empty())
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The states a listing shows, in the order a human cares about them.
pub fn listing_order(mut all: Vec<Request>) -> Vec<Request> {
    all.sort_by(|a, b| {
        a.state
            .list_order()
            .cmp(&b.state.list_order())
            .then(b.filed_ms.cmp(&a.filed_ms))
    });
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const KEY: &str = "0123456789abcdef0123456789abcdef";
    /// A second daemon's key, for the tests that tell machines apart.
    const OTHER_KEY: &str = "fedcba9876543210fedcba9876543210";

    struct Fixture {
        store: Store,
        limiter: RateLimiter,
        dir: PathBuf,
        /// `None` unless a test turns the public surface on, so every existing
        /// test keeps exercising a private-only server.
        public: Option<PublicConfig>,
        mailer: crate::mail::testing::Recording,
        write_lock: Mutex<()>,
        seen: Mutex<crate::daemons::Seen>,
        roster: Mutex<crate::roster::RosterCache>,
        settings: Mutex<crate::settings::SettingsCache>,
        accounts: Mutex<crate::account::AccountsCache>,
        seal_key: Option<crate::seal::SealKey>,
        /// Advanced by tests that need a link to expire.
        now_ms: u64,
        /// One entry by default, so most tests read as a single-daemon server.
        /// A test that cares about telling machines apart replaces it.
        daemon_keys: Vec<crate::config::DaemonKey>,
    }

    /// The daemon credentials a fixture starts with.
    fn one_key(label: &str, key: &str) -> Vec<crate::config::DaemonKey> {
        vec![crate::config::DaemonKey {
            label: label.to_string(),
            key_hash: auth::hash(key),
        }]
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Fixture {
        fn new(tag: &str) -> Fixture {
            // A random suffix, not a timestamp: tests run in parallel and two
            // starting in the same millisecond would otherwise share a directory
            // and delete each other's files — a flake that only shows up under
            // load, which is the worst kind.
            let dir = std::env::temp_dir().join(format!(
                "sc-routes-{tag}-{}-{}",
                std::process::id(),
                &crate::auth::mint_secret()[..12]
            ));
            let store = Store::open(&dir).unwrap();
            Fixture {
                store,
                limiter: RateLimiter::new(),
                dir,
                public: None,
                mailer: crate::mail::testing::Recording::default(),
                write_lock: Mutex::new(()),
                seen: Mutex::new(crate::daemons::Seen::default()),
                roster: Mutex::new(crate::roster::RosterCache::default()),
                settings: Mutex::new(crate::settings::SettingsCache::default()),
                accounts: Mutex::new(crate::account::AccountsCache::default()),
                // Every fixture can seal, so a test never has to think about
                // it unless it is the thing under test.
                seal_key: crate::seal::SealKey::parse(&crate::auth::mint_secret()).ok(),
                now_ms: 1_000,
                daemon_keys: one_key("test-daemon", KEY),
            }
        }

        /// Turn the public surface on, as a configured deployment would.
        fn with_public(mut self, screened: bool) -> Fixture {
            self.public = Some(PublicConfig {
                repos: crate::config::Repos::new(&["intake"]),
                site_name: "intake".into(),
                base_url: "https://specs.example.test".into(),
                // Matching the base URL above, which is what the real
                // configuration derives this from.
                secure_cookies: true,
                mail: Some(crate::config::MailConfig {
                    provider: crate::mail::Provider::Brevo,
                    api_key: KEY.into(),
                    from: "noreply@example.test".into(),
                    from_name: "Smart Coder".into(),
                }),
                screen: screened.then(|| crate::config::ScreenConfig {
                    api_key: KEY.into(),
                    url: "https://screen.example.test".into(),
                    model: "test-model".into(),
                }),
                max_outstanding_links: 200,
                max_daily_filings: crate::config::DEFAULT_MAX_DAILY_FILINGS,
                max_daily_drafts: crate::config::DEFAULT_MAX_DAILY_DRAFTS,
                max_accounts: crate::config::DEFAULT_MAX_ACCOUNTS,
                show_spec: true,
                // No owner role unless a test asks for one, which is the
                // resting state of every deployment.
                owners: Vec::new(),
                github: None,
            });
            self
        }

        /// Configure a GitHub application, as an operator would.
        fn with_github(mut self) -> Fixture {
            if let Some(p) = self.public.as_mut() {
                p.github = Some(crate::config::GithubConfig {
                    client_id: "test-client-id".into(),
                    client_secret: "test-client-secret".into(),
                });
            }
            self
        }

        /// Promote an owner, as the developer would from the admin page.
        ///
        /// Onto the **roster on the volume**, which is where the answer now
        /// comes from. A test that wrote to the configuration instead would
        /// pass while granting nothing in production.
        fn with_owner(self, login: &str, repos: &[&str]) -> Fixture {
            let mut roster = self.store.roster().unwrap();
            let repos: Vec<String> = repos.iter().map(|r| r.to_string()).collect();
            roster.set_owner(login, &repos, self.now_ms);
            roster.seeded = true;
            self.store.put_roster(&roster).unwrap();
            self
        }

        /// Serve a second repository, so the picker and its validation are
        /// reachable.
        ///
        /// Writes **both** the roster and the resolved set on `PublicConfig`.
        /// In production `serve.rs` resolves the second from the first on every
        /// request; this fixture builds `Ctx` directly, so it does that step by
        /// hand — and writing the roster too keeps the admin page's view and
        /// the request path's view from disagreeing inside one test.
        fn with_repos(mut self, names: &[&str]) -> Fixture {
            let mut roster = self.store.roster().unwrap();
            for name in names {
                roster.enable(name, Some("test-daemon".into()), self.now_ms);
            }
            roster.seeded = true;
            self.store.put_roster(&roster).unwrap();
            if let Some(p) = self.public.as_mut() {
                p.repos = crate::config::Repos::new(names);
            }
            self
        }

        /// Run as a loopback server, which is what drops `Secure` from cookies.
        fn on_loopback(mut self) -> Fixture {
            if let Some(p) = self.public.as_mut() {
                p.base_url = "http://localhost:8420".into();
                p.secure_cookies = false;
            }
            self
        }

        /// Tighten the caps, so a test can reach them without filing twenty.
        fn with_caps(mut self, daily: usize, accounts: usize) -> Fixture {
            if let Some(p) = self.public.as_mut() {
                p.max_daily_filings = daily;
                p.max_accounts = accounts;
            }
            self
        }

        /// Tighten the drafting budget, so a test can reach it without
        /// re-admitting sixty times.
        fn with_draft_cap(mut self, drafts: usize) -> Fixture {
            if let Some(p) = self.public.as_mut() {
                p.max_daily_drafts = drafts;
            }
            self
        }

        fn go(&mut self, req: &Req) -> Res {
            // **Mirrors `dispatch`.** Production reads the live keys from the
            // roster on every request; this fixture builds `Ctx` by hand, so it
            // does that step here. Falling back to the configured list keeps
            // every test that predates minting working unchanged.
            let minted = self.store.roster().unwrap().daemon_keys();
            let daemon_keys = if minted.is_empty() {
                self.daemon_keys.clone()
            } else {
                minted
            };
            let mut ctx = Ctx {
                store: &self.store,
                daemon_keys: &daemon_keys,
                limiter: &mut self.limiter,
                now_ms: self.now_ms,
                public: self.public.as_ref(),
                mailer: &self.mailer,
                write_lock: &self.write_lock,
                seen: &self.seen,
                roster: &self.roster,
                settings: &self.settings,
                accounts: &self.accounts,
                seal_key: self.seal_key.as_ref(),
                // Filled in by `handle` before dispatch, beside the caller.
                fresh_auth: false,
                rechecking: false,
            };
            handle(&mut ctx, req)
        }

        /// As the HTTP layer re-checks a poll it is already holding.
        fn go_rechecking(&mut self, req: &Req) -> Res {
            let mut ctx = Ctx {
                store: &self.store,
                daemon_keys: &self.daemon_keys,
                limiter: &mut self.limiter,
                now_ms: self.now_ms,
                public: self.public.as_ref(),
                mailer: &self.mailer,
                write_lock: &self.write_lock,
                seen: &self.seen,
                roster: &self.roster,
                settings: &self.settings,
                accounts: &self.accounts,
                seal_key: self.seal_key.as_ref(),
                fresh_auth: false,
                rechecking: true,
            };
            handle(&mut ctx, req)
        }

        /// Sign in as a filer, returning the session cookie.
        /// Sign in as somebody who authenticated with GitHub, returning the
        /// session cookie.
        ///
        /// Builds the account directly because the OAuth routes do not exist
        /// yet — this is precisely what their callback will do, so the *rest*
        /// of the owner behaviour can be proven before the flow that produces
        /// it. Whether the login is an owner still comes from the configuration,
        /// which is the property under test.
        fn signed_in_as_github(&mut self, login: &str) -> String {
            let mut accounts = self.store.accounts().unwrap();
            let account =
                accounts.create(&auth::hash(&format!("github:{login}")), login, self.now_ms);
            let idx = accounts.accounts.len() - 1;
            accounts.accounts[idx].login = Some(login.to_ascii_lowercase());
            let session = accounts.open_session(&account.id, self.now_ms);
            self.store.put_accounts(&accounts).unwrap();
            session
        }

        fn signed_in(&mut self, email: &str) -> String {
            let asked = self.go(&Req::post(
                public_route::SIGNIN,
                &format!("email={}", email.replace('@', "%40")),
            ));
            assert_eq!(asked.status, 200, "{}", asked.body);

            // The token only ever exists in the emailed body — it is stored
            // hashed, exactly as a credential should be.
            let body = self.mailer.last_body().expect("a link was emailed");
            let token = body
                .split(public_route::SIGNIN_PREFIX)
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .expect("the body carries a link")
                .to_string();

            let res = self.go(&Req::post(
                &format!("{}{token}", public_route::SIGNIN_PREFIX),
                "",
            ));
            assert_eq!(res.status, 200, "{}", res.body);
            cookie_token(&res).expect("a session was opened")
        }
    }

    /// Pull the session token out of a `Set-Cookie`.
    fn cookie_token(res: &Res) -> Option<String> {
        let raw = res.set_cookie.as_ref()?;
        let value = raw
            .trim_start_matches(&format!("{COOKIE}="))
            .split(';')
            .next()?;
        (!value.is_empty()).then(|| value.to_string())
    }

    impl Fixture {
        /// Enrol a browser and return its cookie token.
        /// Claim this server and sign in as its administrator.
        ///
        /// Writes the claim straight to the volume rather than walking `/setup`:
        /// the wizard has its own tests, and every other test wants an
        /// administrator rather than a re-run of how one is made.
        fn as_admin(&mut self) -> String {
            let mut admin = self.store.admin().unwrap();
            admin.claim("jamez667", self.now_ms);
            self.store.put_admin(&admin).unwrap();
            self.signed_in_as_github("jamez667")
        }

        fn file(&mut self, token: &str, text: &str, repo: &str) -> String {
            let res = self.go(&Req::post(
                "/file",
                &format!("text={text}&repo={repo}&kind=feature"),
            )
            .with_cookie(token));
            assert_eq!(res.status, 200, "{}", res.body);
            self.store.all().unwrap().last().unwrap().id.clone()
        }
    }

    // -- the daemon side ----------------------------------------------------

    #[test]
    fn a_daemon_polls_and_gets_work() {
        let mut f = Fixture::new("poll");
        let token = f.as_admin();
        f.file(&token, "add+a+health+check", "alpha");

        let res = f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        assert_eq!(res.status, 200);
        let poll: PollResponse = serde_json::from_str(&res.body).unwrap();
        match poll {
            PollResponse::Work { item, .. } => {
                assert_eq!(item.repo, "alpha");
                assert_eq!(item.text, "add a health check");
            }
            other => panic!("expected work, got {other:?}"),
        }
    }

    #[test]
    fn an_idle_poll_asks_the_layer_above_to_hold_the_connection() {
        // The delay belongs to the server, not to a client-side sleep: fixed
        // polling would force a choice between latency and wasted requests.
        let mut f = Fixture::new("idle");
        let res = f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        assert_eq!(res.status, 200);
        assert!(res.hold_for_work, "the handler asks for a long poll");
        let poll: PollResponse = serde_json::from_str(&res.body).unwrap();
        assert!(matches!(poll, PollResponse::Idle { .. }));
    }

    #[test]
    fn the_daemon_api_is_closed_to_everyone_but_the_daemon() {
        // An unauthenticated intake surface on the public internet is the exact
        // failure this design exists to prevent.
        let mut f = Fixture::new("daemon-closed");
        let token = f.as_admin();

        for req in [
            Req::get(wire::route::WORK),
            Req::get(wire::route::WORK).with_bearer("wrong-key"),
            // Even an enrolled *browser* cannot claim work: a device credential
            // is a person, not a runner.
            Req::get(wire::route::WORK).with_cookie(&token),
        ] {
            assert_eq!(f.go(&req).status, 401, "{:?}", req.path);
        }
    }

    #[test]
    fn each_daemon_authenticates_with_its_own_key() {
        // The point of per-machine keys: two daemons, two credentials, both
        // admitted — and each recognised as *itself* rather than as "a daemon".
        let mut f = Fixture::new("two-keys");
        f.daemon_keys = vec![
            crate::config::DaemonKey {
                label: "laptop".into(),
                key_hash: auth::hash(KEY),
            },
            crate::config::DaemonKey {
                label: "office".into(),
                key_hash: auth::hash(OTHER_KEY),
            },
        ];

        for key in [KEY, OTHER_KEY] {
            let res = f.go(&Req::get(wire::route::WORK).with_bearer(key));
            assert_ne!(res.status, 401, "{key} should be admitted");
        }
        assert_eq!(
            f.go(&Req::get(wire::route::WORK).with_bearer("neither"))
                .status,
            401
        );
    }

    #[test]
    fn revoking_one_daemons_key_leaves_the_other_working() {
        // What a shared key cannot do. Removing one machine's entry — the whole
        // operation "revoke that laptop" consists of — must not disturb the
        // others, or an operator has to choose between a lost machine keeping
        // access and every machine losing it at once.
        let mut f = Fixture::new("revoke-one");
        f.daemon_keys = vec![
            crate::config::DaemonKey {
                label: "laptop".into(),
                key_hash: auth::hash(KEY),
            },
            crate::config::DaemonKey {
                label: "office".into(),
                key_hash: auth::hash(OTHER_KEY),
            },
        ];

        // The operator deletes the laptop's pair and redeploys.
        f.daemon_keys.retain(|d| d.label != "laptop");

        assert_eq!(
            f.go(&Req::get(wire::route::WORK).with_bearer(KEY)).status,
            401,
            "the revoked machine is out"
        );
        assert_ne!(
            f.go(&Req::get(wire::route::WORK).with_bearer(OTHER_KEY))
                .status,
            401,
            "the other machine is untouched"
        );
    }

    #[test]
    fn each_daemon_has_its_own_rate_budget() {
        // A shared key means one budget: a daemon stuck in a retry loop on one
        // host exhausts the allowance of every other machine. Keyed on the
        // label, the blast radius is the machine that misbehaved.
        let laptop = bucket_for(
            &Some(Caller::Daemon {
                label: "laptop".into(),
            }),
            wire::route::WORK,
        );
        let office = bucket_for(
            &Some(Caller::Daemon {
                label: "office".into(),
            }),
            wire::route::WORK,
        );
        assert_ne!(laptop, office);
        // And the bucket holds a hash, not the machine's name.
        assert_eq!(
            laptop,
            Bucket::Credential(auth::hash("laptop")),
            "the label is hashed before it becomes a key"
        );
    }

    #[test]
    fn holding_a_poll_open_does_not_spend_the_daemons_rate_budget() {
        // The bug this exists for: the HTTP layer re-runs `handle` every 250ms
        // for the length of a hold, and each pass used to be charged to the
        // caller. One 30s poll therefore cost ~120 requests out of 240 a minute,
        // so an idle daemon rate-limited *itself* off its own server within two
        // polls — and the symptom was a 429 with no traffic to explain it.
        //
        // Driven through the fixture rather than by calling the limiter, because
        // the defect was in who gets charged, not in the counting.
        let mut f = Fixture::new("poll-budget");

        // Far more re-checks than one hold performs.
        for i in 0..500 {
            let res = f.go_rechecking(&Req::get(wire::route::WORK).with_bearer(KEY));
            assert_ne!(res.status, 429, "re-check {i} was charged to the caller");
        }

        // And a genuinely new poll is still counted, so the budget still exists.
        let mut over = 0;
        for _ in 0..400 {
            if f.go(&Req::get(wire::route::WORK).with_bearer(KEY)).status == 429 {
                over += 1;
            }
        }
        assert!(over > 0, "a real request must still be rate limited");
    }

    #[test]
    fn released_work_is_requeued_rather_than_failed() {
        // The route a daemon uses to say "wrong machine" instead of burning the
        // request with a failure.
        let mut f = Fixture::new("released-route");
        let token = f.as_admin();
        let id = f.file(&token, "something", "alpha");
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));

        let payload = serde_json::to_string(&WorkReleased::new(&id, "no such repo here")).unwrap();
        let res = f.go(&Req::post(&wire::route::released(&id), &payload).with_bearer(KEY));
        assert_eq!(res.status, 200);

        let r = f.store.get(&id).unwrap().unwrap();
        assert_eq!(r.state, crate::store::RequestState::Queued);
        assert!(r.note.unwrap().contains("no such repo here"));
    }

    #[test]
    fn two_daemons_serving_different_repositories_both_get_work() {
        // The arrangement this whole change exists for: a machine with the code
        // for one repository and a machine with the code for another, polling
        // the same server. Before, whichever asked first was handed work it
        // could not do — and reported it as a terminal failure, destroying a
        // request it merely could not reach.
        let mut f = Fixture::new("two-daemons");
        f.daemon_keys = vec![
            crate::config::DaemonKey {
                label: "laptop".into(),
                key_hash: auth::hash(KEY),
            },
            crate::config::DaemonKey {
                label: "office".into(),
                key_hash: auth::hash(OTHER_KEY),
            },
        ];
        let token = f.as_admin();
        let alpha = f.file(&token, "something+for+alpha", "alpha");
        let beta = f.file(&token, "something+for+beta", "beta");

        let claim = |f: &mut Fixture, key: &str, repo: &str| -> Option<String> {
            let res =
                f.go(&Req::get(&format!("{}?repo={repo}", wire::route::WORK)).with_bearer(key));
            let parsed: PollResponse = serde_json::from_str(&res.body).unwrap();
            match parsed {
                PollResponse::Work { item, .. } => Some(item.id),
                PollResponse::Idle { .. } => None,
            }
        };

        // Each takes its own, and neither is offered the other's.
        assert_eq!(claim(&mut f, OTHER_KEY, "beta").as_deref(), Some(&beta[..]));
        assert_eq!(
            claim(&mut f, KEY, "alpha").as_deref(),
            Some(&alpha[..]),
            "alpha was still there for the daemon that serves it"
        );
    }

    #[test]
    fn a_daemon_is_never_handed_a_repository_it_did_not_declare() {
        let mut f = Fixture::new("undeclared");
        let token = f.as_admin();
        f.file(&token, "something+for+alpha", "alpha");

        let res = f.go(&Req::get(&format!("{}?repo=beta", wire::route::WORK)).with_bearer(KEY));
        let parsed: PollResponse = serde_json::from_str(&res.body).unwrap();
        assert!(
            matches!(parsed, PollResponse::Idle { .. }),
            "a daemon serving only beta must not be given alpha"
        );
        // And the request is untouched, still waiting for a daemon that can.
        assert_eq!(
            f.store.all().unwrap()[0].state,
            crate::store::RequestState::Queued
        );
    }

    #[test]
    fn a_daemon_that_declares_nothing_still_gets_work() {
        // An older daemon does not know how to declare, and upgrading the server
        // must not silently stop it.
        let mut f = Fixture::new("declares-nothing");
        let token = f.as_admin();
        let id = f.file(&token, "anything", "alpha");

        let res = f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        let parsed: PollResponse = serde_json::from_str(&res.body).unwrap();
        match parsed {
            PollResponse::Work { item, .. } => assert_eq!(item.id, id),
            PollResponse::Idle { .. } => panic!("an un-upgraded daemon must keep working"),
        }
    }

    #[test]
    fn a_request_nothing_serves_says_so_rather_than_waiting_silently() {
        // "Waiting for a daemon to pick it up" is true and useless for a request
        // that will never move. The page has to distinguish the two cases,
        // because they send the operator to different places.
        let mut f = Fixture::new("unserved-page");
        let token = f.as_admin();
        let id = f.file(&token, "something+for+alpha", "alpha");

        // Nothing has polled at all.
        let html = f
            .go(&Req::get(&format!("/request/{id}")).with_cookie(&token))
            .body;
        assert!(html.contains("No daemon has connected"), "{html}");
        assert!(html.contains("queue serve"), "it names the fix: {html}");

        // Now a daemon polls, but serves something else entirely.
        f.go(&Req::get(&format!("{}?repo=beta", wire::route::WORK)).with_bearer(KEY));
        let html = f
            .go(&Req::get(&format!("/request/{id}")).with_cookie(&token))
            .body;
        assert!(html.contains("No connected daemon serves"), "{html}");
        assert!(html.contains("add-repo alpha"), "it names the fix: {html}");
        assert!(
            html.contains("<code>beta</code>"),
            "and what is on offer: {html}"
        );

        // And once something serves it, the ordinary message comes back.
        f.go(&Req::get(&format!("{}?repo=alpha", wire::route::WORK)).with_bearer(KEY));
        let id2 = f.file(&token, "another+alpha+thing", "alpha");
        let html = f
            .go(&Req::get(&format!("/request/{id2}")).with_cookie(&token))
            .body;
        assert!(html.contains("Waiting for a daemon"), "{html}");
    }

    #[test]
    fn a_drafted_spec_comes_back_and_the_request_awaits_review() {
        let mut f = Fixture::new("drafted");
        let token = f.as_admin();
        let id = f.file(&token, "add+a+health+check", "alpha");
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));

        let payload =
            serde_json::to_string(&DraftedSpec::new(&id, "# Spec", "specs/health")).unwrap();
        let res = f.go(&Req::post(&wire::route::drafted(&id), &payload).with_bearer(KEY));
        assert_eq!(res.status, 200, "{}", res.body);

        let req = f.store.require(&id).unwrap();
        assert_eq!(req.state, RequestState::AwaitingReview);
        assert_eq!(req.spec.as_deref(), Some("# Spec"));
    }

    #[test]
    fn a_failure_is_recorded_with_its_reason() {
        let mut f = Fixture::new("failed");
        let token = f.as_admin();
        let id = f.file(&token, "something", "alpha");
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));

        let payload =
            serde_json::to_string(&DraftFailed::new(&id, "the backend was unreachable")).unwrap();
        let res = f.go(&Req::post(&wire::route::failed(&id), &payload).with_bearer(KEY));
        assert_eq!(res.status, 200, "{}", res.body);
        assert_eq!(f.store.require(&id).unwrap().state, RequestState::Failed);
    }

    #[test]
    fn a_protocol_mismatch_is_a_clear_message_not_a_parse_error() {
        // The daemon and server are deployed separately and will skew; the
        // developer needs to be told which one to update.
        let mut f = Fixture::new("skew");
        let token = f.as_admin();
        let id = f.file(&token, "something", "alpha");

        let payload = format!(
            "{{\"protocol\":99,\"id\":{id:?},\"spec\":\"# S\",\"artifact_dir\":\"specs/x\"}}"
        );
        let res = f.go(&Req::post(&wire::route::drafted(&id), &payload).with_bearer(KEY));
        assert_eq!(res.status, 400);
        assert!(res.body.contains("protocol mismatch"), "{}", res.body);
        assert!(res.body.contains("older"), "{}", res.body);
    }

    // -- the browser side ---------------------------------------------------

    #[test]
    fn revoking_a_filer_stops_them_on_the_very_next_request() {
        // Revocation is the lever that makes self-serve signup acceptable, and
        // a cache must not blunt it. Driven through the real routes, so a
        // missed `invalidate_accounts` at any write site would show up here.
        //
        // **Note what this does and does not prove.** On a filesystem with fine
        // timestamps the mtime alone would catch the write, so this passes even
        // with invalidation disabled — the direct test in `account.rs` is what
        // pins the cache's own behaviour. This one pins the route.
        let mut f = Fixture::new("revoke-cached").with_public(false);
        let session = f.signed_in("jo@x.com");
        let admin = f.as_admin();

        assert_eq!(
            f.go(&Req::get(public_route::FILE).with_cookie(&session))
                .status,
            200
        );

        let id = f
            .store
            .accounts()
            .unwrap()
            .live()
            .iter()
            .find(|a| a.email_hint.contains("jo"))
            .expect("the filer")
            .id
            .clone();
        let res = f.go(&Req::post(&format!("/accounts/{id}/revoke"), "").with_cookie(&admin));
        assert_eq!(res.status, 200, "{}", res.body);

        let after = f.go(&Req::get(public_route::FILE).with_cookie(&session));
        assert!(
            after.body.contains("Sign in"),
            "a revoked filer was still signed in: {}",
            after.body
        );
    }

    #[test]
    fn a_session_opened_this_instant_works_immediately() {
        // A signup and its first request can land inside one filesystem
        // timestamp tick, on a filesystem whose timestamps are coarse enough —
        // and then the mtime has not moved and only `invalidate_accounts` makes
        // this work.
        //
        // **Honest about what it proves.** On the filesystems this suite runs
        // on the mtime does move, so this passes with invalidation disabled: it
        // pins the route's behaviour, not the invalidation. What makes the
        // invalidation non-negotiable is that its absence is a bug that appears
        // only on somebody else's disk.
        let mut f = Fixture::new("signup-cached").with_public(false);
        let session = f.signed_in("new@x.com");
        assert_eq!(
            f.go(&Req::get(public_route::FILE).with_cookie(&session))
                .status,
            200,
            "a session opened this instant did not work"
        );
    }

    #[test]
    fn a_minted_key_is_shown_once_and_then_only_its_hash_is_kept() {
        // **The one place this server prints a secret.** Shown on the page that
        // made it and never again, which is strictly better than an environment
        // variable sitting in a stack editor for the life of the deployment.
        let mut f = Fixture::new("daemon-mint").with_public(false);
        let admin = f.as_admin();

        let res = f.go(&Req::post(private_route::DAEMONS, "label=laptop").with_cookie(&admin));
        assert_eq!(res.status, 200, "{}", res.body);

        // The key is in the page exactly once, and the page says so.
        let roster = f.store.roster().unwrap();
        let record = roster
            .daemons
            .iter()
            .find(|d| d.label == "laptop")
            .expect("minted");
        assert!(!record.revoked);
        assert!(
            !res.body.contains(&record.key_hash),
            "the hash is not shown"
        );
        assert!(
            res.body.contains("only time it is shown"),
            "the page does not warn: {}",
            res.body
        );

        // Reloading the page does not show it again.
        let again = f.go(&Req::get(private_route::DAEMONS).with_cookie(&admin));
        assert!(
            !again.body.contains("only time it is shown"),
            "the key was shown a second time"
        );

        // And nothing reversible reached the volume.
        let raw = std::fs::read_to_string(f.store.roster_path()).unwrap();
        assert!(raw.contains(&record.key_hash), "the hash is stored");
    }

    #[test]
    fn a_minted_key_lets_that_machine_claim_and_a_revoked_one_cannot() {
        // Driven through the real poll route, because "the key works" is the
        // only claim that matters and the store shape is incidental to it.
        let mut f = Fixture::new("daemon-claim").with_public(false);
        let admin = f.as_admin();
        // The fixture starts with a configured key; clear it so only the minted
        // one can possibly be what answers.
        f.daemon_keys = Vec::new();

        let res = f.go(&Req::post(private_route::DAEMONS, "label=laptop").with_cookie(&admin));
        let key = res
            .body
            .split("<pre>")
            .nth(1)
            .and_then(|rest| rest.split("</pre>").next())
            .expect("the page carries the key")
            .to_string();

        assert_eq!(
            f.go(&Req::get(wire::route::WORK).with_bearer(&key)).status,
            200,
            "a freshly minted key could not claim"
        );

        let res = f.go(&Req::post("/daemons/laptop/revoke", "").with_cookie(&admin));
        assert_eq!(res.status, 200, "{}", res.body);

        // **The next poll**, not the next restart.
        assert_eq!(
            f.go(&Req::get(wire::route::WORK).with_bearer(&key)).status,
            401,
            "a revoked machine could still claim"
        );
    }

    #[test]
    fn re_minting_rotates_rather_than_adding_a_second_credential() {
        // Two live keys for one machine would make "revoke that machine"
        // ambiguous, and revoking one would leave the other working.
        let mut f = Fixture::new("daemon-rotate").with_public(false);
        let admin = f.as_admin();
        f.daemon_keys = Vec::new();

        let first = f
            .go(&Req::post(private_route::DAEMONS, "label=laptop").with_cookie(&admin))
            .body
            .split("<pre>")
            .nth(1)
            .and_then(|r| r.split("</pre>").next())
            .unwrap()
            .to_string();
        let second = f
            .go(&Req::post(private_route::DAEMONS, "label=laptop").with_cookie(&admin))
            .body
            .split("<pre>")
            .nth(1)
            .and_then(|r| r.split("</pre>").next())
            .unwrap()
            .to_string();
        assert_ne!(first, second);

        let roster = f.store.roster().unwrap();
        assert_eq!(roster.daemons.len(), 1, "two records for one machine");
        assert_eq!(
            f.go(&Req::get(wire::route::WORK).with_bearer(&second))
                .status,
            200
        );
        assert_eq!(
            f.go(&Req::get(wire::route::WORK).with_bearer(&first))
                .status,
            401,
            "the rotated-away key still worked"
        );
    }

    #[test]
    fn an_owner_cannot_mint_or_revoke_a_machine() {
        // A daemon key claims work on the developer's machine. Past the gate,
        // so no value of `Caller::Owner` reaches it.
        let (mut f, session, _mine, _theirs) = owner_fixture("daemon-owner");
        assert_eq!(
            f.go(&Req::get(private_route::DAEMONS).with_cookie(&session))
                .status,
            404
        );
        for (path, body) in [
            (private_route::DAEMONS, "label=theirs"),
            ("/daemons/laptop/revoke", ""),
        ] {
            let res = f.go(&Req::post(path, body).with_cookie(&session));
            assert_eq!(res.status, 401, "an owner reached {path}: {}", res.body);
        }
    }

    #[test]
    fn a_machine_name_that_would_not_read_back_is_refused() {
        // The label lands in a revocation URL and in every log line about this
        // machine, so it is kept to what reads back unambiguously.
        let mut f = Fixture::new("daemon-label").with_public(false);
        let admin = f.as_admin();
        for bad in ["", "has spaces", "slash/es", "../..", &"x".repeat(65)] {
            let res =
                f.go(
                    &Req::post(private_route::DAEMONS, &format!("label={bad}")).with_cookie(&admin)
                );
            assert_eq!(res.status, 400, "accepted {bad:?}: {}", res.body);
        }
        assert!(f.store.roster().unwrap().daemons.is_empty());
    }

    #[test]
    fn a_surface_with_no_mail_provider_says_so_rather_than_taking_an_address() {
        // **Replaces the console mailer.** Printing sign-in links to the log was
        // how you looked at this surface without a Brevo account; a link is a
        // credential, so that is gone. What is left is the honest failure: the
        // page says it cannot send, rather than accepting an address and
        // silently dropping it.
        let mut f = Fixture::new("no-mail").with_public(false);
        if let Some(p) = f.public.as_mut() {
            p.mail = None;
        }

        let res = f.go(&Req::get(public_route::SIGNIN));
        assert_eq!(res.status, 200, "the page still serves");
        assert!(res.body.contains("cannot send"), "{}", res.body);
        // The page's OWN form is gone. The masthead dialog is rendered by the
        // shell and still carries one — it is on every page and knows nothing
        // about what is configured — which is why the POST refuses below.
        assert!(
            !res.body.contains("id=\"email\""),
            "a form that sends nothing was offered: {}",
            res.body
        );

        // **And the POST refuses.** The masthead dialog is rendered by the
        // shell, which does not know what is configured, so its form is still
        // on the page — the refusal has to be where the answer is known.
        let res = f.go(&Req::post(public_route::SIGNIN, "email=jo%40x.com"));
        assert_eq!(res.status, 503, "{}", res.body);
        assert!(
            f.store.links().unwrap().links.is_empty(),
            "a link was minted"
        );

        // And with a provider it is an ordinary sign-in page again.
        let mut f = Fixture::new("with-mail").with_public(false);
        let res = f.go(&Req::get(public_route::SIGNIN));
        assert!(res.body.contains("id=\"email\""), "{}", res.body);
    }

    #[test]
    fn the_public_surface_is_turned_on_from_the_settings_page() {
        // The switch that used to be an environment variable. A freshly claimed
        // server has it off, and turning it on is deliberate rather than a side
        // effect of naming a repository.
        let mut f = Fixture::new("settings-public").with_public(false);
        let admin = f.as_admin();

        // The fixture's `with_public` writes the *config*, which is only a seed
        // now; the volume decides.
        assert!(!f.store.settings().unwrap().public);

        let res = f.go(&Req::post(private_route::SETTINGS_PUBLIC, "").with_cookie(&admin));
        assert_eq!(res.status, 200, "{}", res.body);
        assert!(f.store.settings().unwrap().public);

        // And it is a toggle, so it can be turned off again. A checkbox could
        // not: an unticked box submits nothing.
        f.go(&Req::post(private_route::SETTINGS_PUBLIC, "").with_cookie(&admin));
        assert!(!f.store.settings().unwrap().public);
    }

    #[test]
    fn a_secret_is_saved_sealed_and_never_rendered_back() {
        // **The claim the whole settings surface rests on.** Asserted against
        // the response body *and* the file, because those are the two places a
        // secret could escape to.
        let mut f = Fixture::new("settings-secret").with_public(false);
        let admin = f.as_admin();

        let res = f.go(&Req::post(
            private_route::SETTINGS_SECRET,
            "base_url=https%3A%2F%2Fspecs.example.test&mail_key=xkeysib-very-secret",
        )
        .with_cookie(&admin));
        assert_eq!(res.status, 200, "{}", res.body);
        assert!(!res.body.contains("xkeysib-very-secret"), "{}", res.body);
        // The page says it is set, and when.
        assert!(res.body.contains("set"), "{}", res.body);

        let raw = std::fs::read_to_string(f.store.settings_path()).unwrap();
        assert!(!raw.contains("xkeysib-very-secret"), "{raw}");

        let settings = f.store.settings().unwrap();
        assert!(settings.mail_key.is_set());
        assert_eq!(
            crate::seal::open(f.seal_key.as_ref().unwrap(), &settings.mail_key).as_deref(),
            Some("xkeysib-very-secret"),
            "and it can still be read back by the server"
        );
    }

    #[test]
    fn a_blank_secret_keeps_the_one_that_is_there() {
        // A form submitted for another reason must not clear a key. That would
        // be a trap, and the failure would be silent until a sign-in bounced.
        let mut f = Fixture::new("settings-blank").with_public(false);
        let admin = f.as_admin();

        f.go(&Req::post(private_route::SETTINGS_SECRET, "mail_key=first-key").with_cookie(&admin));
        let before = f.store.settings().unwrap().mail_key.clone();
        assert!(before.is_set(), "the first write landed");

        f.go(&Req::post(
            private_route::SETTINGS_SECRET,
            "base_url=https%3A%2F%2Fspecs.example.test&mail_key=",
        )
        .with_cookie(&admin));
        assert_eq!(
            f.store.settings().unwrap().mail_key,
            before,
            "a blank field cleared a key"
        );
    }

    #[test]
    fn every_sensitive_verb_refuses_a_stale_session() {
        // **What a secret costs on top of a session.** Iterates the shared
        // constant, so a verb added later is covered without anyone remembering
        // to extend this list.
        let mut f = Fixture::new("settings-stale").with_public(false);
        let admin = f.as_admin();

        // Long enough that the session is no longer freshly proved.
        f.now_ms += crate::account::FRESH_AUTH_MS * 4;

        for path in SENSITIVE_VERBS {
            let res = f.go(&Req::post(path, "mail_key=sneaky").with_cookie(&admin));
            assert_eq!(res.status, 400, "{path} took a stale session: {}", res.body);
            assert!(res.body.contains("fresh sign-in"), "{}", res.body);
        }
        assert!(
            !f.store.settings().unwrap().mail_key.is_set(),
            "a stale session wrote a secret"
        );

        // The non-sensitive forms still work: only secrets need the hop.
        let res = f.go(&Req::post(private_route::SETTINGS_PUBLIC, "").with_cookie(&admin));
        assert_eq!(res.status, 200, "{}", res.body);
    }

    #[test]
    fn a_bad_address_is_refused_by_the_same_rule_the_environment_used() {
        // One function, two callers — so the rule cannot hold at boot and not
        // here. Plain http on a public address puts a sign-in link in the clear.
        let mut f = Fixture::new("settings-bad-url").with_public(false);
        let admin = f.as_admin();

        let res = f.go(&Req::post(
            private_route::SETTINGS_SECRET,
            "base_url=http%3A%2F%2Fspecs.example.test",
        )
        .with_cookie(&admin));
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(f.store.settings().unwrap().base_url.is_empty());
    }

    #[test]
    fn a_cap_can_be_raised_without_a_restart() {
        // The point of moving these off the environment: a ceiling raised to
        // stop refusing filings is worth nothing if it waits for a redeploy.
        let mut f = Fixture::new("settings-caps").with_public(false);
        let admin = f.as_admin();

        let res = f.go(
            &Req::post(private_route::SETTINGS_CAPS, "max_daily_filings=7").with_cookie(&admin),
        );
        assert_eq!(res.status, 200, "{}", res.body);
        assert_eq!(f.store.settings().unwrap().max_daily_filings, Some(7));

        // Blank is "the built-in default", not zero — which would be a surface
        // that accepts nothing and reads as broken.
        f.go(&Req::post(private_route::SETTINGS_CAPS, "max_daily_filings=").with_cookie(&admin));
        assert_eq!(f.store.settings().unwrap().max_daily_filings, None);

        // And nonsense is refused rather than silently defaulted.
        let res = f.go(
            &Req::post(private_route::SETTINGS_CAPS, "max_daily_filings=lots").with_cookie(&admin),
        );
        assert_eq!(res.status, 400, "{}", res.body);
    }

    #[test]
    fn a_provider_this_server_cannot_send_with_is_refused() {
        // Caught here rather than at the next sign-in, where the failure would
        // be somebody else's link never arriving.
        let mut f = Fixture::new("settings-provider").with_public(false);
        let admin = f.as_admin();

        let res = f.go(
            &Req::post(private_route::SETTINGS_MAIL, "mail_provider=sendgrid").with_cookie(&admin),
        );
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(
            res.body.contains("brevo"),
            "and it says what does work: {}",
            res.body
        );
        assert!(f.store.settings().unwrap().mail_provider.is_empty());
    }

    #[test]
    fn an_owner_cannot_reach_the_settings_page() {
        // The same structural argument as the roster: past the gate, so no
        // value of `Caller::Owner` gets here.
        let (mut f, session, _mine, _theirs) = owner_fixture("settings-owner");
        assert_eq!(
            f.go(&Req::get(private_route::SETTINGS).with_cookie(&session))
                .status,
            404
        );
        for path in [
            private_route::SETTINGS_PUBLIC,
            private_route::SETTINGS_SECRET,
        ] {
            let res = f.go(&Req::post(path, "mail_key=x").with_cookie(&session));
            assert_eq!(res.status, 401, "an owner reached {path}: {}", res.body);
        }
    }

    #[test]
    fn an_owner_who_is_also_the_administrator_is_the_administrator() {
        // **The ordering property, and the reason `identify` returns early.**
        // An administrator who also appears in `owners.json` is easy to arrange
        // — the seed may have put them there — and without the early return
        // they would match `owner_for` first and be identified as an owner,
        // losing their own server to a file they can edit from the UI.
        let mut f = Fixture::new("admin-also-owner")
            .with_public(false)
            .with_repos(&["intake"])
            .with_owner("jamez667", &["intake"]);
        let session = f.as_admin();

        assert_eq!(
            f.go(&Req::get(private_route::REVIEW).with_cookie(&session))
                .status,
            200,
            "the administrator was demoted by the roster"
        );
    }

    #[test]
    fn a_magic_link_account_named_as_the_administrator_grants_nothing() {
        // A claim naming an email address must not escalate: a GitHub login is
        // required for anything above a filer, so the claim never matches.
        let mut f = Fixture::new("admin-magic-link").with_public(false);
        let mut admin = f.store.admin().unwrap();
        admin.claim("jo@x.com", 1);
        f.store.put_admin(&admin).unwrap();

        let session = f.signed_in("jo@x.com");
        assert_eq!(
            f.go(&Req::get(private_route::REVIEW).with_cookie(&session))
                .status,
            404
        );
        // And they are still an ordinary filer, not anonymous — otherwise they
        // would silently drop out of every per-account cap.
        assert_eq!(
            f.go(&Req::get(public_route::FILE).with_cookie(&session))
                .status,
            200
        );
    }

    #[test]
    fn the_administrator_reaches_their_surface_with_no_public_one_configured() {
        // `identify` used to bail out before the account lookup when
        // `ctx.public` was `None`. With the account store as the only
        // credential store that would lock the administrator out of a
        // private-only server — their own machine, with nothing public on it.
        let mut f = Fixture::new("admin-no-public");
        assert!(f.public.is_none(), "no public surface at all");
        let session = f.as_admin();

        assert_eq!(
            f.go(&Req::get(private_route::REVIEW).with_cookie(&session))
                .status,
            200
        );
    }

    #[test]
    fn a_stranger_signing_in_with_github_is_not_the_administrator() {
        // The claim is one login. Anybody else who signs in is an account.
        let mut f = Fixture::new("gh-stranger").with_public(false);
        let mut admin = f.store.admin().unwrap();
        admin.claim("jamez667", 1);
        f.store.put_admin(&admin).unwrap();

        let session = f.signed_in_as_github("somebody-else");
        assert_eq!(
            f.go(&Req::get(private_route::REVIEW).with_cookie(&session))
                .status,
            404
        );
    }

    #[test]
    fn an_unclaimed_server_has_no_administrator_at_all() {
        // Not "everybody" and not "the first person": until the claim is made,
        // the private surface belongs to nobody.
        let mut f = Fixture::new("unclaimed").with_public(false);
        let session = f.signed_in_as_github("jamez667");
        assert_eq!(
            f.go(&Req::get(private_route::REVIEW).with_cookie(&session))
                .status,
            404
        );
    }

    #[test]
    fn the_private_surface_is_not_found_to_anybody_who_is_not_the_administrator() {
        // **Replaces the enrolment-page test**, whose premise went with the code
        // box. The property that mattered survives: a 401 on `/review` would
        // tell a stranger the address is real, so everything private is *not
        // found* to everyone else.
        let mut f = Fixture::new("private-404");
        for path in [
            private_route::REVIEW,
            "/accounts",
            "/owners",
            "/repos",
            "/request/anything",
        ] {
            let res = f.go(&Req::get(path));
            assert_eq!(res.status, 404, "{path}: {}", res.body);
        }
        // And a POST is 401 rather than 404: it is not a page anybody browses
        // to, so there is no address to confirm.
        assert_eq!(f.go(&Req::post("/owners", "login=x&repos=y")).status, 401);
    }

    #[test]
    fn a_dead_cookie_is_told_where_to_sign_in() {
        // The developer whose enrolled browser stopped working is the one person
        // most likely to hit this, and a bare "there is nothing here" is a
        // confusing answer at an address that worked yesterday. It leaks
        // nothing — the sign-in link is on the public landing page already.
        let mut f = Fixture::new("dead-cookie").with_public(false).with_github();
        let res =
            f.go(&Req::get(private_route::REVIEW).with_cookie("a-token-that-matches-nothing"));
        assert_eq!(res.status, 404);
        assert!(res.body.contains(public_route::AUTH_GITHUB), "{}", res.body);

        // With no application there is nothing to point at, so it says nothing.
        let mut f = Fixture::new("dead-cookie-no-gh");
        let res = f.go(&Req::get(private_route::REVIEW).with_cookie("nothing"));
        assert_eq!(res.status, 404);
        assert!(
            !res.body.contains(public_route::AUTH_GITHUB),
            "{}",
            res.body
        );
    }

    #[test]
    fn filing_a_request_names_a_repository_and_never_a_path() {
        // The form has no field for a path, so traversal is unreachable rather
        // than mitigated (spec 18).
        let mut f = Fixture::new("file");
        let token = f.as_admin();
        let id = f.file(&token, "add+a+health+check", "alpha");

        let req = f.store.require(&id).unwrap();
        assert_eq!(req.repo, "alpha");
        assert_eq!(req.state, RequestState::Queued);
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("\"path\""), "{json}");
    }

    #[test]
    fn an_empty_or_oversized_request_is_refused_with_a_reason() {
        let mut f = Fixture::new("file-bad");
        let token = f.as_admin();

        let empty = f.go(&Req::post("/file", "text=+++&repo=alpha").with_cookie(&token));
        assert_eq!(empty.status, 400);

        // Too many words, each of them tiny.
        let wordy = format!("text={}&repo=alpha", "word+".repeat(MAX_WORDS + 10));
        let over = f.go(&Req::post("/file", &wordy).with_cookie(&token));
        assert_eq!(over.status, 400);
        assert!(over.body.contains("words"), "{}", over.body);

        // And one enormous token, which the word count alone would wave through.
        let huge = format!("text={}&repo=alpha", "x".repeat(MAX_BYTES + 1));
        let over = f.go(&Req::post("/file", &huge).with_cookie(&token));
        assert_eq!(over.status, 400);
        assert!(over.body.contains("characters"), "{}", over.body);
    }

    #[test]
    fn the_length_limit_is_the_same_on_every_filing_path() {
        // The public and enrolled surfaces must not drift to different limits:
        // the screener's "sees the whole request" property is only true if the
        // text it screens is the text that was accepted.
        assert!(check_length("a short request").is_ok());
        assert!(check_length(&"word ".repeat(MAX_WORDS)).is_ok());
        assert!(check_length(&"word ".repeat(MAX_WORDS + 1)).is_err());
        assert!(check_length(&"x".repeat(MAX_BYTES + 1)).is_err());

        // 500 words is comfortably under the byte ceiling for real prose, so an
        // honest request never hits the second limit first.
        let realistic =
            "the health check returns 200 while the database is down ".repeat(MAX_WORDS / 10);
        assert!(
            check_length(&realistic).is_ok(),
            "{} bytes",
            realistic.len()
        );
    }

    #[test]
    fn every_intake_kind_is_accepted_and_an_unknown_one_defaults() {
        let mut f = Fixture::new("kinds");
        let token = f.as_admin();
        for (form, expected) in [
            ("bug", IntakeKind::Bug),
            ("feature", IntakeKind::Feature),
            ("improvement", IntakeKind::Improvement),
            ("feedback", IntakeKind::Feedback),
            ("nonsense", IntakeKind::Feature),
        ] {
            let body = format!("text=a+thing&repo=alpha&kind={form}");
            f.go(&Req::post("/file", &body).with_cookie(&token));
            let last = f.store.all().unwrap().last().unwrap().clone();
            assert_eq!(last.kind, expected, "{form}");
        }
    }

    /// Pull the digest out of a confirmation page's hidden field.
    fn digest_from(html: &str) -> String {
        let at = html
            .find("name=\"digest\" value=\"")
            .expect("the confirmation carries a digest");
        let rest = &html[at + "name=\"digest\" value=\"".len()..];
        rest[..rest.find('"').unwrap()].to_string()
    }

    #[test]
    fn approving_takes_two_deliberate_posts_and_the_first_decides_nothing() {
        // Spec 20: approve is a deliberate action taken below the full artifact.
        // The first POST asks; only the second settles.
        let mut f = Fixture::new("accept");
        let token = f.as_admin();
        let id = f.file(&token, "a+thing", "alpha");
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        let payload = serde_json::to_string(&DraftedSpec::new(&id, "# Spec", "specs/x")).unwrap();
        f.go(&Req::post(&wire::route::drafted(&id), &payload).with_bearer(KEY));

        let asked = f.go(&Req::post(&format!("/request/{id}/accept"), "").with_cookie(&token));
        assert_eq!(asked.status, 200, "{}", asked.body);
        assert_eq!(
            f.store.require(&id).unwrap().state,
            RequestState::AwaitingReview,
            "the first post asks, it does not decide"
        );
        assert!(asked.body.contains("/accept/confirm"), "{}", asked.body);

        let digest = digest_from(&asked.body);
        let settled = f.go(&Req::post(
            &format!("/request/{id}/accept/confirm"),
            &format!("digest={digest}"),
        )
        .with_cookie(&token));
        assert_eq!(settled.status, 200, "{}", settled.body);
        // `Ready` is not `Done`: nothing was built, and the developer picks it up
        // in their IDE on their own schedule.
        assert_eq!(f.store.require(&id).unwrap().state, RequestState::Accepted);
    }

    #[test]
    fn an_approval_of_text_that_changed_under_the_reviewer_is_refused() {
        // The reviewer opens v1 on a train; `queue serve` pushes a redraft while
        // they read. Confirming must not settle v2 on the strength of having read
        // v1 — consent attaches to bytes, not to an id.
        let mut f = Fixture::new("stale");
        let token = f.as_admin();
        let id = f.file(&token, "a+thing", "alpha");
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        let v1 = serde_json::to_string(&DraftedSpec::new(&id, "# Version one", "specs/x")).unwrap();
        f.go(&Req::post(&wire::route::drafted(&id), &v1).with_bearer(KEY));

        let asked = f.go(&Req::post(&format!("/request/{id}/accept"), "").with_cookie(&token));
        let stale = digest_from(&asked.body);

        // The daemon redrafts under them. Through the real path — sent back,
        // requeued, claimed again — because a daemon may now only report on a
        // claim it currently holds.
        f.go(&Req::post(&format!("/request/{id}/send-back"), "notes=redo").with_cookie(&token));
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        let v2 = serde_json::to_string(&DraftedSpec::new(&id, "# Version two", "specs/x")).unwrap();
        f.go(&Req::post(&wire::route::drafted(&id), &v2).with_bearer(KEY));

        let refused = f.go(&Req::post(
            &format!("/request/{id}/accept/confirm"),
            &format!("digest={stale}"),
        )
        .with_cookie(&token));
        assert_eq!(refused.status, 400, "{}", refused.body);
        assert!(refused.body.contains("changed"), "{}", refused.body);
        assert_eq!(
            f.store.require(&id).unwrap().state,
            RequestState::AwaitingReview,
            "left reviewable rather than half-decided"
        );
    }

    #[test]
    fn a_confirm_with_no_digest_at_all_is_refused() {
        // The obvious bypass: skip the confirmation page and POST the committing
        // route directly. It must not succeed by omission.
        let mut f = Fixture::new("no-digest");
        let token = f.as_admin();
        let id = f.file(&token, "a+thing", "alpha");
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        let payload = serde_json::to_string(&DraftedSpec::new(&id, "# Spec", "specs/x")).unwrap();
        f.go(&Req::post(&wire::route::drafted(&id), &payload).with_bearer(KEY));

        for body in ["", "digest=", "digest=nonsense"] {
            let res = f
                .go(&Req::post(&format!("/request/{id}/accept/confirm"), body).with_cookie(&token));
            assert_eq!(res.status, 400, "{body:?}: {}", res.body);
        }
        assert_eq!(
            f.store.require(&id).unwrap().state,
            RequestState::AwaitingReview
        );
    }

    #[test]
    fn asking_to_approve_something_with_no_draft_is_refused() {
        // Approving a queued request would be signing off a spec that does not
        // exist yet.
        let mut f = Fixture::new("ask-nodraft");
        let token = f.as_admin();
        let id = f.file(&token, "a+thing", "alpha");

        let res = f.go(&Req::post(&format!("/request/{id}/accept"), "").with_cookie(&token));
        assert_eq!(res.status, 400, "{}", res.body);
        let missing = f.go(&Req::post("/request/nope/accept", "").with_cookie(&token));
        assert_eq!(missing.status, 404);
    }

    #[test]
    fn sending_back_requeues_it_with_the_note() {
        let mut f = Fixture::new("send-back");
        let token = f.as_admin();
        let id = f.file(&token, "a+thing", "alpha");
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        let payload = serde_json::to_string(&DraftedSpec::new(&id, "# Vague", "specs/x")).unwrap();
        f.go(&Req::post(&wire::route::drafted(&id), &payload).with_bearer(KEY));

        let res = f.go(&Req::post(
            &format!("/request/{id}/send-back"),
            "notes=name+the+actual+roles",
        )
        .with_cookie(&token));
        assert_eq!(res.status, 200, "{}", res.body);
        let req = f.store.require(&id).unwrap();
        assert_eq!(req.state, RequestState::Queued);
        assert_eq!(req.send_back_note.as_deref(), Some("name the actual roles"));

        // And the note reaches the daemon on the next claim, so the redraft
        // grounds on the reason rather than repeating itself.
        let res = f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        let poll: PollResponse = serde_json::from_str(&res.body).unwrap();
        match poll {
            PollResponse::Work { item, .. } => assert_eq!(
                item.send_back_note.as_deref(),
                Some("name the actual roles")
            ),
            other => panic!("expected work, got {other:?}"),
        }
    }

    #[test]
    fn the_browser_cannot_reach_a_route_that_builds_because_there_is_none() {
        // The surface's whole vocabulary is: file · watch · read · approve or send
        // back. Spec 19's "no writing code" anti-goal, satisfied structurally.
        let mut f = Fixture::new("no-build");
        let token = f.as_admin();
        let id = f.file(&token, "a+thing", "alpha");

        for path in [
            format!("/request/{id}/build"),
            format!("/request/{id}/run"),
            format!("/request/{id}/implement"),
            "/build".to_string(),
            "/run".to_string(),
        ] {
            let res = f.go(&Req::post(&path, "").with_cookie(&token));
            assert_eq!(res.status, 404, "{path} must not exist");
        }
    }

    // -- headers and limits -------------------------------------------------

    /// Both policies, so a property asserted here is asserted for the whole
    /// server rather than for whichever one the test happened to pick.
    const POLICIES: [Policy; 2] = [Policy::Strict, Policy::PublicScript];

    #[test]
    fn the_three_headers_spec_18_names_are_all_present() {
        // Rendering model-authored Markdown is an exfiltration path: one
        // hallucinated remote image leaks the page URL via `Referer`.
        for policy in POLICIES {
            let headers = security_headers(policy);
            let named: Vec<&str> = headers.iter().map(|(k, _)| *k).collect();
            for required in [
                "Referrer-Policy",
                "Cache-Control",
                "Content-Security-Policy",
            ] {
                assert!(named.contains(&required), "{policy:?} missing {required}");
            }
            let referrer = headers
                .iter()
                .find(|(k, _)| *k == "Referrer-Policy")
                .unwrap();
            assert_eq!(referrer.1, "no-referrer", "{policy:?}");
            let cache = headers.iter().find(|(k, _)| *k == "Cache-Control").unwrap();
            assert_eq!(cache.1, "no-store", "{policy:?}");
        }
    }

    #[test]
    fn no_policy_permits_a_remote_subresource() {
        // This is what makes the exfiltration path unreachable rather than
        // merely unreferred — and it is the part the public/private split does
        // **not** relax. Permitting script is not permitting a remote origin.
        for policy in POLICIES {
            let csp = policy.csp();
            assert!(csp.starts_with("default-src 'none'"), "{policy:?}: {csp}");
            assert!(csp.contains("frame-ancestors 'none'"), "{policy:?}: {csp}");
            assert!(!csp.contains("https:"), "{policy:?} allows a remote: {csp}");
            assert!(!csp.contains("http:"), "{policy:?} allows a remote: {csp}");
            assert!(!csp.contains('*'), "{policy:?} allows a wildcard: {csp}");

            // The invariant behind those greps, checked directly rather than by
            // naming the ways it could be broken: **every source in every
            // directive is same-origin or nothing.** A bare domain, a `data:`
            // URI, a `blob:` — none carry the strings above, and each would be a
            // way out of this server. Written as an allowlist so a source nobody
            // anticipated is refused rather than merely unlisted.
            for directive in csp.split(';') {
                let directive = directive.trim();
                let Some((name, sources)) = directive.split_once(' ') else {
                    continue;
                };
                for source in sources.split_whitespace() {
                    assert!(
                        matches!(source, "'self'" | "'none'" | "'unsafe-inline'"),
                        "{policy:?}: {name} permits {source:?}, which is not same-origin"
                    );
                }
            }
            // And `'unsafe-inline'` is tolerated above only for styles — the
            // stylesheet ships inside the page. Anywhere else it is an injection
            // vector, and `no_policy_permits_an_inline_script` pins the script
            // case specifically.
            for directive in csp.split(';') {
                let directive = directive.trim();
                if directive.contains("'unsafe-inline'") {
                    assert!(
                        directive.starts_with("style-src"),
                        "{policy:?}: {directive:?} is inline-permitting and is not style-src"
                    );
                }
            }
        }
    }

    #[test]
    fn only_the_public_policy_permits_script() {
        // The whole point of the split. Stated as an equality on both sides:
        // "strict has no script-src" alone would still pass if `default-src`
        // were loosened, since script would then fall back to it.
        assert!(!Policy::Strict.csp().contains("script-src"));
        assert!(Policy::PublicScript.csp().contains("script-src 'self'"));
    }

    #[test]
    fn no_policy_permits_an_inline_script() {
        // `'unsafe-inline'` on `script-src` is what a successful injection needs.
        // Script on the public surface must be a served file, so that an
        // injected `<script>` in a rendered spec still does not run.
        //
        // Checked on `script-src` specifically — `style-src 'unsafe-inline'` is
        // deliberate and present in both, so a bare `contains` would fail here
        // for the wrong reason.
        for policy in POLICIES {
            let csp = policy.csp();
            let Some(rest) = csp.split("script-src").nth(1) else {
                continue;
            };
            let directive = rest.split(';').next().unwrap_or("");
            assert!(
                !directive.contains("unsafe-inline"),
                "{policy:?} permits an inline script: {csp}"
            );
        }
    }

    #[test]
    fn a_response_is_strict_unless_something_says_otherwise() {
        // The direction the default must fail in. A handler that forgets which
        // surface it is on produces a page whose script does not run — visible
        // at once — rather than a private page that quietly permits one.
        assert_eq!(Res::html(200, "x").policy, Policy::Strict);
        assert_eq!(Res::json(200, "{}").policy, Policy::Strict);
        assert_eq!(Policy::default(), Policy::Strict);
    }

    #[test]
    fn a_guessing_loop_is_throttled_before_it_touches_the_store() {
        // Behind a proxy every request shares one IP, so the limit must key on
        // the credential — and the anonymous bucket is where guessing lands.
        let mut f = Fixture::new("throttle");
        let mut last = 200;
        for _ in 0..40 {
            last = f.go(&Req::post("/enrol", "code=GUESS&label=x")).status;
        }
        assert_eq!(last, 429, "the guessing loop is cut off");
    }

    // -- the public surface -------------------------------------------------

    #[test]
    fn the_public_routes_do_not_exist_when_the_surface_is_off() {
        // Absent rather than present-and-refusing: a server nobody configured for
        // public intake should look like one that has none.
        let mut f = Fixture::new("public-off");
        for path in [
            public_route::FILE,
            public_route::SIGNIN,
            "/public/signin/abc",
            "/public/request/abc",
        ] {
            assert_eq!(f.go(&Req::get(path)).status, 404, "{path}");
        }
        assert_eq!(
            f.go(&Req::post(public_route::SIGNIN, "email=a%40x.com"))
                .status,
            404
        );
        assert_eq!(f.mailer.count(), 0, "and nothing was emailed");
    }

    #[test]
    fn asking_for_a_link_says_the_same_thing_whatever_happened() {
        // The response must not reveal whether an address has an account.
        let mut f = Fixture::new("signin-uniform").with_public(false);

        let fresh = f.go(&Req::post(public_route::SIGNIN, "email=new%40x.com"));
        let malformed = f.go(&Req::post(public_route::SIGNIN, "email=not-an-email"));
        let empty = f.go(&Req::post(public_route::SIGNIN, "email="));

        assert_eq!(fresh.status, 200);
        assert_eq!(fresh.body, malformed.body, "malformed looks identical");
        assert_eq!(fresh.body, empty.body, "so does empty");
        // Only what was *sent* differs.
        assert_eq!(f.mailer.count(), 1, "only the real address got mail");
    }

    #[test]
    fn a_revoked_account_is_sent_no_mail_and_cannot_sign_back_in() {
        // Revocation that a fresh sign-in undoes is not revocation. And a "your
        // account was revoked" mail is one an attacker can trigger at a victim's
        // address, so nothing is sent at all.
        let mut f = Fixture::new("revoked").with_public(false);
        f.signed_in("jo@x.com");

        let mut accounts = f.store.accounts().unwrap();
        let id = accounts.live()[0].id.clone();
        assert!(accounts.revoke(&id));
        f.store.put_accounts(&accounts).unwrap();

        let before = f.mailer.count();
        f.go(&Req::post(public_route::SIGNIN, "email=jo%40x.com"));
        assert_eq!(f.mailer.count(), before, "silence, not a notification");
    }

    #[test]
    fn a_get_on_a_sign_in_link_consumes_nothing() {
        // Mail scanners fetch every URL in a message within seconds. A GET that
        // spent the token would burn it before the human opened their inbox.
        let mut f = Fixture::new("prefetch").with_public(false);
        f.go(&Req::post(public_route::SIGNIN, "email=jo%40x.com"));
        let body = f.mailer.last_body().unwrap();
        let token = body
            .split(public_route::SIGNIN_PREFIX)
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .unwrap()
            .to_string();
        let path = format!("{}{token}", public_route::SIGNIN_PREFIX);

        // Three scanner prefetches.
        for _ in 0..3 {
            let res = f.go(&Req::get(&path));
            assert_eq!(res.status, 200);
            assert!(res.set_cookie.is_none(), "a GET signs nobody in");
        }
        // And the human's click still works.
        let res = f.go(&Req::post(&path, ""));
        assert!(cookie_token(&res).is_some(), "still spendable");
    }

    #[test]
    fn a_get_on_a_fabricated_link_looks_exactly_like_a_real_one() {
        // A 404 on an invalid token is a free validity oracle — cheaper than the
        // POST it would be guarding.
        let mut f = Fixture::new("oracle").with_public(false);
        f.go(&Req::post(public_route::SIGNIN, "email=jo%40x.com"));
        let body = f.mailer.last_body().unwrap();
        let real = body
            .split(public_route::SIGNIN_PREFIX)
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .unwrap()
            .to_string();

        // A forged token of the *same shape*, so any difference in the response
        // is about validity rather than about length.
        let forged = "0".repeat(real.len());

        let genuine = f.go(&Req::get(&format!("{}{real}", public_route::SIGNIN_PREFIX)));
        let fake = f.go(&Req::get(&format!(
            "{}{forged}",
            public_route::SIGNIN_PREFIX
        )));

        assert_eq!(genuine.status, fake.status);
        assert_eq!(
            genuine.body.replace(&real, "T"),
            fake.body.replace(&forged, "T"),
            "the pages differ only by the token echoed into the form"
        );
        // And the real one is still unspent — the GET checked nothing.
        assert!(f.store.links().unwrap().peek(&real, f.now_ms).is_some());
    }

    #[test]
    fn a_link_is_single_use() {
        let mut f = Fixture::new("single-use").with_public(false);
        f.go(&Req::post(public_route::SIGNIN, "email=jo%40x.com"));
        let body = f.mailer.last_body().unwrap();
        let token = body
            .split(public_route::SIGNIN_PREFIX)
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .unwrap()
            .to_string();
        let path = format!("{}{token}", public_route::SIGNIN_PREFIX);

        assert!(cookie_token(&f.go(&Req::post(&path, ""))).is_some());
        let again = f.go(&Req::post(&path, ""));
        assert!(cookie_token(&again).is_none(), "spent");
        assert!(again.body.contains("already been used"), "{}", again.body);
    }

    #[test]
    fn filing_publicly_requires_being_signed_in() {
        let mut f = Fixture::new("public-anon").with_public(false);
        let res = f.go(&Req::post(public_route::FILE, "text=a+thing&kind=bug"));
        assert!(res.body.contains("Sign in"), "{}", res.body);
        assert!(f.store.all().unwrap().is_empty(), "nothing was filed");
    }

    #[test]
    fn a_public_filing_cannot_name_a_repository_nobody_nominated() {
        // **The property is unchanged; the mechanism is not.** This used to
        // prove the body's repository was *ignored*, which is how a surface
        // serving exactly one kept a stranger from aiming work anywhere. Now the
        // form offers a choice, so an unnominated name is *refused* instead —
        // and refusing is what keeps the property.
        //
        // Rejected rather than quietly filed against the default: a fallback
        // would put the work somewhere the filer did not choose, with nothing on
        // the page saying so.
        let mut f = Fixture::new("public-repo").with_public(false);
        let session = f.signed_in("jo@x.com");

        let res = f.go(
            &Req::post(public_route::FILE, "text=a+thing&kind=bug&repo=secret-repo")
                .with_cookie(&session),
        );
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(
            f.store.all().unwrap().is_empty(),
            "nothing was filed anywhere"
        );
    }

    #[test]
    fn a_filer_chooses_among_the_nominated_repositories() {
        // The whole point of the set: one surface, several projects, and the
        // filer says which.
        let mut f = Fixture::new("public-pick")
            .with_public(false)
            .with_repos(&["intake", "memosy"]);
        let session = f.signed_in("jo@x.com");

        let res = f.go(
            &Req::post(public_route::FILE, "text=a+thing&kind=bug&repo=memosy")
                .with_cookie(&session),
        );
        assert_eq!(res.status, 200, "{}", res.body);
        assert_eq!(f.store.all().unwrap()[0].repo, "memosy");
    }

    #[test]
    fn a_filing_naming_nothing_is_refused_when_there_is_a_choice() {
        // With a picker on the form, an absent name did not come from the form —
        // and guessing which project somebody meant is exactly the fallback this
        // refuses to make.
        let mut f = Fixture::new("public-pick-none")
            .with_public(false)
            .with_repos(&["intake", "memosy"]);
        let session = f.signed_in("jo@x.com");

        let res =
            f.go(&Req::post(public_route::FILE, "text=a+thing&kind=bug").with_cookie(&session));
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(f.store.all().unwrap().is_empty());
    }

    #[test]
    fn a_public_filing_takes_the_only_repository_when_there_is_one() {
        // A one-repository surface renders no picker, so an absent name is
        // normal there — and must still work exactly as it did before the set
        // existed.
        let mut f = Fixture::new("public-one-repo").with_public(false);
        let session = f.signed_in("jo@x.com");

        let res =
            f.go(&Req::post(public_route::FILE, "text=a+thing&kind=bug").with_cookie(&session));
        assert_eq!(res.status, 200, "{}", res.body);

        let filed = f.store.all().unwrap();
        assert_eq!(filed.len(), 1);
        assert_eq!(filed[0].repo, "intake");
    }

    #[test]
    fn a_public_filing_is_not_claimable_until_it_has_been_screened() {
        // The core guarantee: nothing unscreened reaches the developer's machine.
        let mut f = Fixture::new("public-screened").with_public(true);
        let session = f.signed_in("jo@x.com");
        f.go(&Req::post(public_route::FILE, "text=a+thing&kind=bug").with_cookie(&session));

        let filed = f.store.all().unwrap();
        assert_eq!(filed[0].state, RequestState::Screening);
        assert!(
            f.store
                .claim_next(Serves::Anything, "d-test")
                .unwrap()
                .is_none(),
            "no daemon may claim it yet"
        );
    }

    #[test]
    fn with_screening_off_a_filing_queues_honestly_rather_than_pretending() {
        // A server that parks filings in `Screening` forever because nothing
        // screens them would be worse than one that plainly does not screen.
        let mut f = Fixture::new("public-unscreened").with_public(false);
        let session = f.signed_in("jo@x.com");
        f.go(&Req::post(public_route::FILE, "text=a+thing&kind=bug").with_cookie(&session));

        assert_eq!(f.store.all().unwrap()[0].state, RequestState::Queued);
        assert!(f
            .store
            .claim_next(Serves::Anything, "d-test")
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_filer_cannot_read_another_filers_request() {
        // Request ids are time-ordered and enumerable in seconds, so keying on an
        // id alone would expose every filing — including the developer's own.
        let mut f = Fixture::new("public-isolation").with_public(false);

        let alice = f.signed_in("alice@x.com");
        f.go(&Req::post(public_route::FILE, "text=alice+thing&kind=bug").with_cookie(&alice));
        let alice_id = f.store.all().unwrap()[0].id.clone();

        let bob = f.signed_in("bob@x.com");
        let res = f.go(
            &Req::get(&format!("{}{alice_id}", public_route::REQUEST_PREFIX)).with_cookie(&bob),
        );
        // Not found, not forbidden: "forbidden" would confirm the id exists.
        assert_eq!(res.status, 404, "{}", res.body);
        assert!(!res.body.contains("alice thing"), "{}", res.body);
    }

    /// A GitHub that answers without a network.
    struct StubGithub {
        login: std::result::Result<String, String>,
    }

    impl crate::oauth::Github for StubGithub {
        fn login_for(&self, _code: &str) -> sc_proto::Result<String> {
            self.login.clone().map_err(sc_proto::DcError::Backend)
        }
    }

    /// Drive the callback with a stub, as the route would.
    fn callback(f: &mut Fixture, code: &str, state: &str, gh: &StubGithub) -> Res {
        let path = format!(
            "{}?code={code}&state={state}",
            public_route::AUTH_GITHUB_CALLBACK
        );
        let req = Req::get(&path);
        let mut limiter = RateLimiter::new();
        let mut ctx = Ctx {
            store: &f.store,
            daemon_keys: &f.daemon_keys,
            limiter: &mut limiter,
            now_ms: f.now_ms,
            public: f.public.as_ref(),
            mailer: &f.mailer,
            write_lock: &f.write_lock,
            seen: &f.seen,
            roster: &f.roster,
            settings: &f.settings,
            accounts: &f.accounts,
            seal_key: f.seal_key.as_ref(),
            // Filled in by `handle` before dispatch, beside the caller.
            fresh_auth: false,
            rechecking: false,
        };
        finish_github_with(&mut ctx, &req, Locale::En, gh)
    }

    /// Start a sign-in and return the state token that was minted.
    fn start(f: &mut Fixture) -> String {
        let res = f.go(&Req::get(public_route::AUTH_GITHUB));
        assert_eq!(res.status, 200, "{}", res.body);
        // The plaintext exists only in the page — the store keeps a hash.
        res.body
            .split("state=")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("the page carries the state")
            .to_string()
    }

    /// Arm a claim code, as a fresh server's startup does.
    fn armed(f: &mut Fixture, code: &str) {
        let mut admin = f.store.admin().unwrap();
        assert!(admin.arm(code, f.now_ms));
        f.store.put_admin(&admin).unwrap();
    }

    #[test]
    fn setting_up_claims_the_server_for_whoever_signs_in() {
        // **The whole first-run path**, end to end: the code proves you can read
        // the container's log, and GitHub proves who you are. Neither alone is
        // enough, which is why they are separate steps.
        let mut f = Fixture::new("setup-claim").with_public(false);
        armed(&mut f, "ABC-123");

        // Step one: the code and the address.
        let res = f.go(&Req::post(
            private_route::SETUP,
            "code=ABC-123&base_url=https%3A%2F%2Fspecs.example.test",
        ));
        assert_eq!(res.status, 200, "{}", res.body);
        assert_eq!(
            f.store.settings().unwrap().base_url,
            "https://specs.example.test"
        );
        // The rest of the wizard is bound to this browser from here on.
        let setup = res
            .set_cookie
            .as_deref()
            .and_then(|c| c.strip_prefix(&format!("{SETUP_COOKIE}=")))
            .and_then(|c| c.split(';').next())
            .expect("a setup token was issued")
            .to_string();
        // The callback URL is shown for copying, now that it can be.
        assert!(
            res.body
                .contains("https://specs.example.test/public/auth/github/callback"),
            "{}",
            res.body
        );

        // Step two: the application.
        let res = f.go(&Req::post(
            private_route::SETUP_GITHUB,
            "client_id=cid&client_secret=csecret",
        )
        .with_setup(&setup));
        assert_eq!(res.status, 200, "{}", res.body);
        let settings = f.store.settings().unwrap();
        assert!(settings.has_github());
        // Stored sealed: the secret is not in the file.
        let raw = std::fs::read_to_string(f.store.settings_path()).unwrap();
        assert!(!raw.contains("csecret"), "{raw}");

        // Step three: the sign-in that decides who owns this. Nobody yet.
        assert!(!f.store.admin().unwrap().claimed());
        let state = res
            .body
            .split("state=")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("the page carries the state")
            .to_string();

        let res = callback(
            &mut f,
            "code",
            &state,
            &StubGithub {
                login: Ok("JameZ667".into()),
            },
        );
        assert_eq!(res.status, 200, "{}", res.body);
        let admin = f.store.admin().unwrap();
        assert!(admin.is("jamez667"), "lowercased on the way in");

        // And setup is gone.
        assert_eq!(f.go(&Req::get(private_route::SETUP)).status, 404);
        assert_eq!(
            f.go(&Req::post(
                private_route::SETUP,
                "code=ABC-123&base_url=https%3A%2F%2Fx.test"
            ))
            .status,
            404
        );
    }

    /// Every page an administrator is expected to find.
    ///
    /// Named once so the test below iterates it rather than a hand-written copy
    /// that goes stale the moment a page is added — which is the failure it
    /// exists to catch.
    const ADMIN_PAGES: [&str; 6] = [
        private_route::REVIEW,
        private_route::SETTINGS,
        private_route::REPOS,
        private_route::OWNERS,
        private_route::DAEMONS,
        private_route::ACCOUNTS,
    ];

    #[test]
    fn every_administrative_page_is_linked_from_the_surface() {
        // **The bug this pins, and it had already happened twice.** Four of
        // these were built, tested, and reachable only by somebody who already
        // knew the URL — the same failure the GitHub sign-in had, and it goes
        // unnoticed for the same reason: a test asks for a route directly,
        // which is exactly what a person cannot do.
        //
        // Asserted from EVERY page rather than one, because the nav lives in
        // the shell precisely so a page added later is linked by construction.
        let mut f = Fixture::new("admin-nav").with_public(false);
        let admin = f.as_admin();

        for from in ADMIN_PAGES {
            let res = f.go(&Req::get(from).with_cookie(&admin));
            assert_eq!(res.status, 200, "{from}: {}", res.body);
            for to in ADMIN_PAGES {
                assert!(
                    res.body.contains(&format!("href=\"{to}\"")),
                    "{from} does not link {to}"
                );
            }
        }
    }

    #[test]
    fn every_administrative_page_serves_with_no_public_surface() {
        // **A link that 404s is worse than no link.** Two of these were guarded
        // on a public surface existing, which was right while the switch was an
        // environment variable and these pages could not turn it on. Now they
        // can — so 404ing here hid the page exactly when somebody came to fix
        // the thing it is about.
        //
        // This is the state a freshly claimed server is in: claimed, no public
        // surface, and the administrator looking for where to turn one on.
        let mut f = Fixture::new("admin-nav-private");
        assert!(f.public.is_none(), "no public surface at all");
        let admin = f.as_admin();

        for path in ADMIN_PAGES {
            let res = f.go(&Req::get(path).with_cookie(&admin));
            assert_eq!(res.status, 200, "{path} is linked but 404s: {}", res.body);
        }
    }

    #[test]
    fn the_admin_navigation_is_not_on_the_public_surface() {
        // It names pages a filer cannot reach, and a 404 they can see the door
        // to is worse than one they cannot: it tells a stranger the addresses
        // are real.
        let mut f = Fixture::new("admin-nav-public").with_public(false);
        let filer = f.signed_in("jo@x.com");
        let res = f.go(&Req::get(public_route::FILE).with_cookie(&filer));
        for path in [private_route::SETTINGS, private_route::DAEMONS] {
            assert!(
                !res.body.contains(&format!("href=\"{path}\"")),
                "the public surface links {path}: {}",
                res.body
            );
        }
    }

    #[test]
    fn a_half_finished_setup_cannot_be_taken_over_by_somebody_else() {
        // **The hole this closes, and it was live.** Setup is more than one
        // step and the code is spent at the first, so everything after it was
        // guarded only by the server being unclaimed. Naming a GitHub
        // application decides which account can finish the claim — so an
        // interloper who reached step two would have supplied their own and
        // owned the server.
        //
        // It bit hardest on a MIGRATED volume, where seeding fills in the
        // address: "step one is already done" was then true for everybody, from
        // the first boot, with no code ever spent.
        let mut f = Fixture::new("setup-hijack").with_public(false);
        armed(&mut f, "ABC-123");

        let res = f.go(&Req::post(
            private_route::SETUP,
            "code=ABC-123&base_url=https%3A%2F%2Fspecs.example.test",
        ));
        assert_eq!(res.status, 200, "{}", res.body);
        let setup = res
            .set_cookie
            .as_deref()
            .and_then(|c| c.strip_prefix(&format!("{SETUP_COOKIE}=")))
            .and_then(|c| c.split(';').next())
            .expect("a setup token was issued")
            .to_string();

        // Somebody else, arriving with no cookie, is sent back to the code box
        // rather than shown the step that hands the server over.
        let theirs = f.go(&Req::get(private_route::SETUP));
        assert!(
            theirs.body.contains("Claim code"),
            "a stranger was shown a later step: {}",
            theirs.body
        );
        assert!(
            !theirs.body.contains("callback URL"),
            "a stranger was shown the application step: {}",
            theirs.body
        );

        // And cannot post to it.
        let res = f.go(&Req::post(
            private_route::SETUP_GITHUB,
            "client_id=theirs&client_secret=theirs",
        ));
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(
            !f.store.settings().unwrap().has_github(),
            "a stranger named the application that decides who owns this"
        );

        // A wrong token is no better than none.
        let res = f.go(&Req::post(
            private_route::SETUP_GITHUB,
            "client_id=theirs&client_secret=theirs",
        )
        .with_setup("not-the-token"));
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(!f.store.settings().unwrap().has_github());

        // The browser that spent the code still finishes normally.
        let res = f.go(&Req::post(
            private_route::SETUP_GITHUB,
            "client_id=mine&client_secret=mine",
        )
        .with_setup(&setup));
        assert_eq!(res.status, 200, "{}", res.body);
        assert!(f.store.settings().unwrap().has_github());
    }

    #[test]
    fn a_seeded_volume_does_not_look_half_set_up_to_a_stranger() {
        // The migration case specifically: an upgraded server already has an
        // address, and that must not be mistaken for "somebody is part-way
        // through claiming this".
        let mut f = Fixture::new("setup-seeded").with_public(false);
        armed(&mut f, "ABC-123");

        let mut settings = f.store.settings().unwrap();
        settings.base_url = "https://specs.example.test".into();
        settings.seeded = true;
        f.store.put_settings(&settings).unwrap();

        let res = f.go(&Req::get(private_route::SETUP));
        assert!(
            res.body.contains("Claim code"),
            "a seeded address skipped the code: {}",
            res.body
        );
    }

    #[test]
    fn an_abandoned_setup_stops_standing_open() {
        // The token shares the code's window. An abandoned wizard must not
        // leave the step that hands the server over reachable indefinitely.
        let mut f = Fixture::new("setup-abandoned").with_public(false);
        armed(&mut f, "ABC-123");
        let res = f.go(&Req::post(
            private_route::SETUP,
            "code=ABC-123&base_url=https%3A%2F%2Fspecs.example.test",
        ));
        let setup = res
            .set_cookie
            .as_deref()
            .and_then(|c| c.strip_prefix(&format!("{SETUP_COOKIE}=")))
            .and_then(|c| c.split(';').next())
            .expect("a setup token")
            .to_string();

        f.now_ms += crate::admin::CLAIM_TTL_MS;
        let res = f.go(&Req::post(
            private_route::SETUP_GITHUB,
            "client_id=late&client_secret=late",
        )
        .with_setup(&setup));
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(!f.store.settings().unwrap().has_github());
    }

    #[test]
    fn a_bad_address_does_not_burn_the_claim_code() {
        // **The code is the scarce thing.** Spending it on a typo would leave
        // the operator restarting the container to get another, which is a
        // needless indignity in the one flow that has to work first time.
        let mut f = Fixture::new("setup-bad-url").with_public(false);
        armed(&mut f, "ABC-123");

        let res = f.go(&Req::post(
            private_route::SETUP,
            "code=ABC-123&base_url=http%3A%2F%2Fspecs.example.test",
        ));
        assert_eq!(res.status, 400, "plain http on a public address");
        assert!(res.body.contains("https"), "{}", res.body);

        // Still armed, and the right address now works.
        let res = f.go(&Req::post(
            private_route::SETUP,
            "code=ABC-123&base_url=https%3A%2F%2Fspecs.example.test",
        ));
        assert_eq!(res.status, 200, "{}", res.body);
    }

    #[test]
    fn a_wrong_code_says_one_thing_and_leaves_the_real_one_usable() {
        // One message for wrong, expired and already-spent alike: distinguishing
        // them tells a guesser which half they got right. And a wrong guess must
        // not spend somebody else's code, or a stranger who cannot read the log
        // could still deny the claim to the person who can.
        let mut f = Fixture::new("setup-wrong-code").with_public(false);
        armed(&mut f, "ABC-123");

        let res = f.go(&Req::post(
            private_route::SETUP,
            "code=WRONG-1&base_url=https%3A%2F%2Fspecs.example.test",
        ));
        assert_eq!(res.status, 400);

        let res = f.go(&Req::post(
            private_route::SETUP,
            "code=ABC-123&base_url=https%3A%2F%2Fspecs.example.test",
        ));
        assert_eq!(res.status, 200, "the real code still works: {}", res.body);
    }

    #[test]
    fn the_setup_page_says_what_it_decided_about_cookies() {
        // Derived, not asked. "Is this a private network" is a question people
        // answer wrong, and answering it wrong drops `Secure` from every session
        // cookie without a word.
        let public = crate::page::setup_page("https://specs.example.test", None);
        assert!(public.contains("Secure"), "{public}");
        let private = crate::page::setup_page("http://localhost:8420", None);
        assert!(private.contains("not</strong> be marked"), "{private}");
    }

    #[test]
    fn a_claimed_server_has_no_setup_and_arms_no_code() {
        // Otherwise every restart would print a fresh key to the
        // administrator's own front door.
        let mut f = Fixture::new("setup-claimed").with_public(false);
        let mut admin = f.store.admin().unwrap();
        admin.claim("jamez667", 1);
        f.store.put_admin(&admin).unwrap();

        assert_eq!(f.go(&Req::get(private_route::SETUP)).status, 404);
        assert_eq!(
            f.go(&Req::post(
                private_route::SETUP_GITHUB,
                "client_id=a&client_secret=b"
            ))
            .status,
            404
        );
    }

    #[test]
    fn an_ordinary_sign_in_does_not_claim_anything() {
        // The claim rides the state, so only a hop that started at /setup can
        // finish as one. Somebody signing in normally on an unclaimed server is
        // just an account.
        let mut f = Fixture::new("setup-not-claim")
            .with_public(false)
            .with_github();
        let state = start(&mut f);
        let res = callback(
            &mut f,
            "code",
            &state,
            &StubGithub {
                login: Ok("stranger".into()),
            },
        );
        assert_eq!(res.status, 200, "{}", res.body);
        assert!(
            !f.store.admin().unwrap().claimed(),
            "an ordinary sign-in claimed the server"
        );
    }

    #[test]
    fn a_github_hop_stamps_the_browser_it_came_back_to() {
        // The freshness that guards secret changes, driven through the real
        // callback rather than asserted on the store. A stale session that goes
        // round again comes back fresh, on the SAME cookie.
        let mut f = Fixture::new("fresh-auth").with_public(false).with_github();

        let state = start(&mut f);
        let res = callback(
            &mut f,
            "code",
            &state,
            &StubGithub {
                login: Ok("jamez667".into()),
            },
        );
        let cookie = cookie_token(&res).expect("a session was opened");
        assert!(f.store.accounts().unwrap().session_fresh(&cookie, f.now_ms));

        // Time passes; the session still signs in and is no longer fresh.
        f.now_ms += crate::account::FRESH_AUTH_MS * 4;
        let accounts = f.store.accounts().unwrap();
        assert!(accounts.session_for(&cookie).is_some(), "still signed in");
        assert!(
            !accounts.session_fresh(&cookie, f.now_ms),
            "no longer fresh"
        );

        // Round again, carrying the cookie this time.
        let res = f.go(&Req::get(public_route::AUTH_GITHUB).with_cookie(&cookie));
        let state = res
            .body
            .split("state=")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("the page carries the state")
            .to_string();
        let path = format!(
            "{}?code=code&state={state}",
            public_route::AUTH_GITHUB_CALLBACK
        );
        let req = Req::get(&path).with_cookie(&cookie);
        let mut limiter = RateLimiter::new();
        let mut ctx = Ctx {
            store: &f.store,
            daemon_keys: &f.daemon_keys,
            limiter: &mut limiter,
            now_ms: f.now_ms,
            public: f.public.as_ref(),
            mailer: &f.mailer,
            write_lock: &f.write_lock,
            seen: &f.seen,
            roster: &f.roster,
            settings: &f.settings,
            accounts: &f.accounts,
            seal_key: f.seal_key.as_ref(),
            fresh_auth: false,
            rechecking: false,
        };
        let res = finish_github_with(
            &mut ctx,
            &req,
            Locale::En,
            &StubGithub {
                login: Ok("jamez667".into()),
            },
        );
        assert_eq!(res.status, 200, "{}", res.body);

        let accounts = f.store.accounts().unwrap();
        assert!(
            accounts.session_fresh(&cookie, f.now_ms),
            "the same cookie came back fresh"
        );
        assert_eq!(accounts.sessions.len(), 1, "and no second session");
    }

    #[test]
    fn signing_in_with_github_opens_a_session() {
        let mut f = Fixture::new("gh-signin").with_public(false).with_github();
        let state = start(&mut f);

        let res = callback(
            &mut f,
            "the-code",
            &state,
            &StubGithub {
                login: Ok("jamez667".into()),
            },
        );
        assert_eq!(res.status, 200, "{}", res.body);
        assert!(res.set_cookie.is_some(), "a session was opened");

        // The account records how they signed in — and only that.
        let accounts = f.store.accounts().unwrap();
        assert_eq!(accounts.accounts.len(), 1);
        assert_eq!(accounts.accounts[0].login.as_deref(), Some("jamez667"));
    }

    #[test]
    fn a_state_token_cannot_be_replayed() {
        // A callback URL sits in browser history and referrer logs. Spending the
        // state once is what stops one being reused.
        let mut f = Fixture::new("gh-replay").with_public(false).with_github();
        let state = start(&mut f);
        let gh = StubGithub {
            login: Ok("jamez667".into()),
        };

        assert_eq!(callback(&mut f, "code", &state, &gh).status, 200);
        assert_eq!(
            callback(&mut f, "code", &state, &gh).status,
            400,
            "the second use is refused"
        );
    }

    #[test]
    fn a_forged_callback_signs_nobody_in() {
        // No state this server issued, so nothing to spend.
        let mut f = Fixture::new("gh-forged").with_public(false).with_github();
        let res = callback(
            &mut f,
            "code",
            "never-issued",
            &StubGithub {
                login: Ok("jamez667".into()),
            },
        );
        assert_eq!(res.status, 400);
        assert!(res.set_cookie.is_none(), "no session");
        assert!(f.store.accounts().unwrap().accounts.is_empty());
    }

    #[test]
    fn a_refused_code_signs_nobody_in() {
        // GitHub declining the exchange — an expired code, or one already spent.
        let mut f = Fixture::new("gh-refused").with_public(false).with_github();
        let state = start(&mut f);
        let res = callback(
            &mut f,
            "code",
            &state,
            &StubGithub {
                login: Err("github refused the code".into()),
            },
        );
        assert_eq!(res.status, 400);
        assert!(res.set_cookie.is_none());
        assert!(f.store.accounts().unwrap().accounts.is_empty());
    }

    #[test]
    fn github_sign_in_is_linked_exactly_where_it_exists() {
        // **The bug this pins.** The route was built, reachable and tested —
        // and nothing on any page linked to it, so an owner's only way in was
        // to know the URL and type it. From the outside the feature looked
        // missing, because a route with no link is a route nobody uses.
        //
        // Asserted in both directions against the SAME condition the route
        // itself uses, because the two failures are the same drift pointing
        // opposite ways: a link with no route is a promise that 404s.
        let mut f = Fixture::new("gh-linked").with_public(false).with_github();
        let html = f.go(&Req::get(public_route::SIGNIN)).body;
        assert!(
            html.contains(public_route::AUTH_GITHUB),
            "the route exists and no page links to it: {html}"
        );
        assert_eq!(f.go(&Req::get(public_route::AUTH_GITHUB)).status, 200);

        // And with no application, neither the route nor the link. A button
        // that always fails is worse than no button.
        let mut f = Fixture::new("gh-none").with_public(false);
        let html = f.go(&Req::get(public_route::SIGNIN)).body;
        assert!(
            !html.contains(public_route::AUTH_GITHUB),
            "linked to a route that does not exist: {html}"
        );
        assert_eq!(f.go(&Req::get(public_route::AUTH_GITHUB)).status, 404);
    }

    #[test]
    fn signing_in_with_github_makes_a_named_owner_an_owner() {
        // The two halves meeting: GitHub says who, configuration says what they
        // may see.
        let mut f = Fixture::new("gh-owner")
            .with_public(false)
            .with_github()
            .with_owner("jamez667", &["intake"]);
        let state = start(&mut f);

        let res = callback(
            &mut f,
            "code",
            &state,
            &StubGithub {
                login: Ok("JameZ667".into()),
            },
        );
        assert_eq!(res.status, 200);
        let cookie = res
            .set_cookie
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .trim_start_matches("sc_device=")
            .to_string();

        // Recognised as an owner — and still not a device, so the private
        // surface stays closed.
        assert_eq!(
            f.go(&Req::get(private_route::REVIEW).with_cookie(&cookie))
                .status,
            404
        );
        for verb in REVIEW_VERBS {
            let res = f.go(&Req::post(&format!("/request/r-1/{verb}"), "").with_cookie(&cookie));
            assert_eq!(res.status, 401, "an owner reached {verb}");
        }
    }

    /// An owner signed in and holding a session cookie, with a request filed
    /// against each of two repositories.
    fn owner_fixture(tag: &str) -> (Fixture, String, String, String) {
        let mut f = Fixture::new(tag)
            .with_public(false)
            .with_repos(&["intake", "other"])
            .with_owner("jamez667", &["intake"]);
        let owner = f.signed_in_as_github("jamez667");

        let filer = f.signed_in("jo@x.com");
        f.go(
            &Req::post(public_route::FILE, "text=for+intake&kind=bug&repo=intake")
                .with_cookie(&filer),
        );
        f.go(
            &Req::post(public_route::FILE, "text=for+other&kind=bug&repo=other")
                .with_cookie(&filer),
        );

        let all = f.store.all().unwrap();
        let mine = all.iter().find(|r| r.repo == "intake").unwrap().id.clone();
        let theirs = all.iter().find(|r| r.repo == "other").unwrap().id.clone();
        (f, owner, mine, theirs)
    }

    #[test]
    fn re_admitting_work_is_bounded_per_repository() {
        // **The loop this closes was open.** `send_back` moves a request from
        // `AwaitingReview` back to `Queued`, so it is drafted again — and owners
        // have `send-back`. `max_daily_filings` never bounded it: that is
        // checked when something is *filed*, and this request was filed once.
        //
        // Two drafting runs allowed, so the third re-admission is refused.
        let (mut f, owner, mine, _) = owner_fixture("draft-budget");
        let cap = 2;
        f = f.with_draft_cap(cap);

        // Loop until it is refused. Asserting *that it stops* rather than on
        // which round: the count is of drafting runs, and a round both spends
        // one (the claim) and asks for another (the send-back), so tying the
        // test to a round number would be asserting arithmetic rather than the
        // property.
        let mut send_backs = 0;
        let mut refused = false;
        for _ in 0..20 {
            // A daemon claims it — which is where a drafting run is counted —
            // and hands back a spec.
            f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
            let payload =
                serde_json::to_string(&DraftedSpec::new(&mine, "# Spec", "specs/x")).unwrap();
            f.go(&Req::post(&wire::route::drafted(&mine), &payload).with_bearer(KEY));

            let res = f.go(&Req::post(
                &format!("{}{mine}/send-back", public_route::REQUEST_PREFIX),
                "note=again",
            )
            .with_cookie(&owner));
            if res.status == 429 {
                refused = true;
                break;
            }
            send_backs += 1;
        }

        assert!(refused, "the loop never stopped — this is the hole itself");
        assert!(
            send_backs <= cap,
            "{send_backs} send-backs got through a budget of {cap}"
        );
        // And the repository's spend is what stopped it.
        let spent = f.store.drafts_since("intake", 0).unwrap();
        assert!(spent >= cap, "{spent} runs against a cap of {cap}");
    }

    #[test]
    fn the_drafting_budget_is_the_repositorys_and_not_the_callers() {
        // Keyed on the repository, so a second owner does not double it — and
        // so the developer's own send-backs count against the same day. What is
        // being spent is runs against a project.
        let (mut f, owner, mine, _) = owner_fixture("draft-budget-shared");
        f = f.with_draft_cap(1);

        // The developer spends the repository's budget.
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        let payload = serde_json::to_string(&DraftedSpec::new(&mine, "# Spec", "specs/x")).unwrap();
        f.go(&Req::post(&wire::route::drafted(&mine), &payload).with_bearer(KEY));

        // And the owner finds it spent, without having spent any of it.
        let res = f.go(&Req::post(
            &format!("{}{mine}/send-back", public_route::REQUEST_PREFIX),
            "note=again",
        )
        .with_cookie(&owner));
        assert_eq!(res.status, 429, "{}", res.body);
    }

    #[test]
    fn a_request_written_before_drafts_were_counted_still_loads() {
        // The field is new; records on the live volume have none. An absent
        // list reads as zero runs, which is the safe direction — it cannot
        // refuse work on a count nobody recorded.
        let older = r#"{"id":"r-1","text":"a thing","repo":"alpha","kind":"bug",
                        "state":"queued","filed_ms":0}"#;
        let r: crate::store::Request = serde_json::from_str(older).unwrap();
        assert!(r.drafts.is_empty());
    }

    #[test]
    fn an_owner_sees_only_their_own_repositories() {
        let (mut f, owner, mine, theirs) = owner_fixture("owner-sees");

        let html = f.go(&Req::get(public_route::FILE).with_cookie(&owner)).body;
        assert!(html.contains("for intake"), "their own: {html}");
        assert!(!html.contains("for other"), "not somebody else's: {html}");

        // And by id, both directions.
        assert_eq!(
            f.go(&Req::get(&format!("{}{mine}", public_route::REQUEST_PREFIX)).with_cookie(&owner))
                .status,
            200
        );
        assert_eq!(
            f.go(
                &Req::get(&format!("{}{theirs}", public_route::REQUEST_PREFIX)).with_cookie(&owner)
            )
            .status,
            404,
            "not found rather than forbidden — a 403 confirms the id is real"
        );
    }

    #[test]
    fn an_owner_can_decline_but_the_page_offers_no_approve() {
        // Absent from the page as well as refused on the wire: there is nothing
        // for an approve to post to, because the route that accepts one is on
        // the developer's surface.
        let (mut f, owner, mine, _) = owner_fixture("owner-declines");

        // Get it to a state where a decision is possible.
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        let payload = serde_json::to_string(&DraftedSpec::new(&mine, "# Spec", "specs/x")).unwrap();
        f.go(&Req::post(&wire::route::drafted(&mine), &payload).with_bearer(KEY));

        let html = f
            .go(&Req::get(&format!("{}{mine}", public_route::REQUEST_PREFIX)).with_cookie(&owner))
            .body;
        assert!(html.contains("send-back"), "{html}");
        assert!(html.contains("discard"), "{html}");
        assert!(!html.contains("accept"), "no approve anywhere: {html}");

        // And the verb works.
        let res = f.go(&Req::post(
            &format!("{}{mine}/send-back", public_route::REQUEST_PREFIX),
            "note=too+vague",
        )
        .with_cookie(&owner));
        assert_eq!(res.status, 200, "{}", res.body);
        assert_eq!(
            f.store.get(&mine).unwrap().unwrap().state,
            crate::store::RequestState::Queued,
            "sent back for another pass"
        );
    }

    #[test]
    fn an_owner_may_release_their_own_repositorys_quarantined_work() {
        // The one owner verb that admits work, and deliberate. Screening is a
        // model reading a stranger's text, so it holds things it should not —
        // and an owner who can see the queue but not unblock it has to ask the
        // developer about every false positive.
        let (mut f, owner, mine, theirs) = owner_fixture("owner-release");

        for id in [&mine, &theirs] {
            let mut r = f.store.get(id).unwrap().unwrap();
            r.state = RequestState::Quarantined;
            f.store.put(&r).unwrap();
        }

        // Offered on the page, not merely accepted on the wire — a verb with no
        // button is one nobody uses.
        let html = f
            .go(&Req::get(&format!("{}{mine}", public_route::REQUEST_PREFIX)).with_cookie(&owner))
            .body;
        assert!(
            html.contains(&format!("/public/request/{mine}/release")),
            "{html}"
        );

        let res = f.go(&Req::post(
            &format!("{}{mine}/release", public_route::REQUEST_PREFIX),
            "",
        )
        .with_cookie(&owner));
        assert_eq!(res.status, 200, "{}", res.body);
        assert_eq!(
            f.store.get(&mine).unwrap().unwrap().state,
            RequestState::Queued,
            "released into the claimable queue"
        );

        // Somebody else's repository stays quarantined, and says not found
        // rather than forbidden.
        let res = f.go(&Req::post(
            &format!("{}{theirs}/release", public_route::REQUEST_PREFIX),
            "",
        )
        .with_cookie(&owner));
        assert_eq!(res.status, 404);
        assert_eq!(
            f.store.get(&theirs).unwrap().unwrap().state,
            RequestState::Quarantined
        );
    }

    #[test]
    fn an_owners_releases_are_bounded_by_the_repositorys_drafting_budget() {
        // **Why the budget shipped before this verb.** Release reaches the
        // developer's machine: each one is a drafting run. Without a bound, an
        // owner could loop release and send-back for ever, and the cost lands
        // on somebody else's laptop.
        let (mut f, owner, mine, _theirs) = owner_fixture("owner-release-budget");
        if let Some(p) = f.public.as_mut() {
            p.max_daily_drafts = 1;
        }

        // The request has already been drafted once today, so the budget is spent.
        let mut r = f.store.get(&mine).unwrap().unwrap();
        r.drafts = vec![f.now_ms];
        r.state = RequestState::Quarantined;
        f.store.put(&r).unwrap();

        let res = f.go(&Req::post(
            &format!("{}{mine}/release", public_route::REQUEST_PREFIX),
            "",
        )
        .with_cookie(&owner));
        assert_eq!(res.status, 429, "{}", res.body);
        assert_eq!(
            f.store.get(&mine).unwrap().unwrap().state,
            RequestState::Quarantined,
            "refused, and nothing moved"
        );
    }

    #[test]
    fn an_owner_cannot_decline_somebody_elses_repository() {
        let (mut f, owner, _, theirs) = owner_fixture("owner-not-theirs");
        for verb in OWNER_VERBS {
            let res = f.go(&Req::post(
                &format!("{}{theirs}/{verb}", public_route::REQUEST_PREFIX),
                "",
            )
            .with_cookie(&owner));
            assert_eq!(res.status, 404, "an owner reached {verb} on another repo");
        }
    }

    #[test]
    fn accepting_is_the_one_verb_no_owner_has() {
        // The two lists side by side. Every owner verb is a review verb, and
        // accepting is absent — asserted by name, so adding it to `OWNER_VERBS`
        // without thinking fails here.
        //
        // **The line is not "admits work".** `release` admits work, knowingly,
        // and is bounded by the drafting budget rather than kept away. The line
        // is that accepting SETTLES a request, and building it means opening
        // the IDE and running the pipeline — the developer's machine, and so
        // the developer's call.
        for verb in OWNER_VERBS {
            assert!(REVIEW_VERBS.contains(&verb), "{verb} is not a review verb");
        }
        for settling in ["accept", "accept/confirm"] {
            assert!(
                !OWNER_VERBS.contains(&settling),
                "{settling} settles a request and is not an owner's to reach"
            );
        }
    }

    #[test]
    fn an_owner_cannot_reach_the_private_surface_at_all() {
        // **The property this role turns on**, and it is structural rather than
        // a check somebody has to write: `Caller::Owner` does not match the
        // `Caller::Device` pattern the private surface is gated on, so every
        // route past that line is unreachable by type. There is no value of the
        // variant that gets through.
        //
        // That is what makes it worth testing here rather than verb by verb.
        // An owner *does* reach `send-back`, `discard` and `release` — on the
        // PUBLIC side, through `owner_route`, each behind an ownership check
        // and a drafting budget. None of that touches this: the private
        // handlers stay closed to them whatever `OWNER_VERBS` grows to.
        //
        // Iterates the shared constant, so a verb added later is covered
        // without anyone remembering to extend this list.
        let mut f = Fixture::new("owner-no-approve")
            .with_public(false)
            .with_owner("jamez667", &["intake"]);
        let owner = f.signed_in_as_github("jamez667");

        let filer = f.signed_in("jo@x.com");
        f.go(&Req::post(public_route::FILE, "text=a+thing&kind=bug").with_cookie(&filer));
        let id = f.store.all().unwrap()[0].id.clone();

        for verb in REVIEW_VERBS {
            let res = f.go(&Req::post(&format!("/request/{id}/{verb}"), "").with_cookie(&owner));
            assert_eq!(res.status, 401, "an owner reached {verb}: {}", res.body);
        }
        // And the developer's own review surface is not theirs either.
        assert_eq!(
            f.go(&Req::get(private_route::REVIEW).with_cookie(&owner))
                .status,
            404
        );
    }

    #[test]
    fn an_owner_is_recognised_only_because_the_roster_says_so() {
        // An owner is an account the developer *promotes*, never one that
        // promotes itself — so the same signed-in session is an owner or is
        // not, depending on a record only a device can write.
        let mut f = Fixture::new("owner-from-roster").with_public(false);
        let session = f.signed_in_as_github("jamez667");

        // Not named: an ordinary filer, and the filing form is what they get.
        assert_eq!(
            f.go(&Req::get(public_route::FILE).with_cookie(&session))
                .status,
            200
        );
        assert_eq!(
            f.go(&Req::get(private_route::REVIEW).with_cookie(&session))
                .status,
            404,
            "a GitHub login alone grants nothing"
        );
    }

    #[test]
    fn revoking_an_owner_demotes_a_live_session_on_the_very_next_request() {
        // **The property that had to survive the move off configuration.**
        // Deleting a line and redeploying was complete revocation — no session
        // to hunt down, no record that might disagree. The roster is a record,
        // so this is the test that says the mtime cache actually carries it.
        let (mut f, session, _mine, _theirs) = owner_fixture("owner-demote");

        // An owner: somebody else's filing for their repository is on the page.
        let html = f
            .go(&Req::get(public_route::FILE).with_cookie(&session))
            .body;
        assert!(html.contains("for intake"), "an owner sees it: {html}");
        // And even so, the private surface stays closed to them.
        assert_eq!(
            f.go(&Req::get(private_route::REVIEW).with_cookie(&session))
                .status,
            404
        );

        // The developer revokes them.
        let mut roster = f.store.roster().unwrap();
        assert!(roster.revoke("jamez667"));
        f.store.put_roster(&roster).unwrap();

        // **The next request**, with no restart and no redeploy. A cache that
        // held a startup snapshot would still say owner here, which is exactly
        // the failure the move off configuration must not introduce.
        let html = f
            .go(&Req::get(public_route::FILE).with_cookie(&session))
            .body;
        assert!(
            !html.contains("for intake"),
            "a revoked owner still reviews: {html}"
        );
        // Demoted, not signed out. They were an account before they were an
        // owner, and revocation returns them to being one — the page still
        // renders, and the filing form on it is theirs.
        assert_eq!(
            f.go(&Req::get(public_route::FILE).with_cookie(&session))
                .status,
            200
        );
    }

    #[test]
    fn an_owner_of_a_repository_this_surface_stopped_serving_is_not_one_here() {
        // The roster and the repository list are separately editable, so a
        // record can name something no longer collected here. `identify`
        // intersects the two, because granting it would be a permission that
        // looks applied and reaches nothing — what the configuration used to
        // refuse at boot, and what a record cannot.
        let mut f = Fixture::new("owner-stale-repo")
            .with_public(false)
            .with_repos(&["intake"])
            .with_owner("jamez667", &["something-else"]);
        let owner = f.signed_in_as_github("jamez667");

        let filer = f.signed_in("jo@x.com");
        f.go(
            &Req::post(public_route::FILE, "text=for+intake&kind=bug&repo=intake")
                .with_cookie(&filer),
        );

        let html = f.go(&Req::get(public_route::FILE).with_cookie(&owner)).body;
        assert!(
            !html.contains("for intake"),
            "owner of nothing this surface serves: {html}"
        );
    }

    #[test]
    fn an_owner_cannot_promote_anybody_including_themselves() {
        // Somebody who may promote may promote an accomplice, and revoking the
        // first would then not revoke the second. Proven structurally: the
        // roster's only writer lives past the device gate, which no
        // `Caller::Owner` satisfies.
        let (mut f, session, _mine, _theirs) = owner_fixture("owner-no-promote");

        for body in [
            "login=jamez667&repos=intake&repos=other",
            "login=accomplice&repos=intake",
        ] {
            let res = f.go(&Req::post(private_route::OWNERS, body).with_cookie(&session));
            // 401: they are somebody, and not a device. The gate refuses before
            // `set_owner` is ever reached — which is the point. There is no
            // check inside the handler that a later edit could drop.
            assert_eq!(
                res.status, 401,
                "an owner reached the roster with {body}: {}",
                res.body
            );
        }
        // The roster is untouched: no accomplice, and no second repository.
        let roster = f.store.roster().unwrap();
        assert!(roster.owner_for("accomplice").is_none());
        assert_eq!(roster.owner_for("jamez667").unwrap().repos, ["intake"]);
    }

    #[test]
    fn the_developer_promotes_and_revokes_from_the_owners_page() {
        let mut f = Fixture::new("owners-admin")
            .with_public(false)
            .with_repos(&["intake", "other"]);
        let device = f.as_admin();

        // Two repositories ticked arrive as a repeated field. A map keeps only
        // the last, which would grant one of the two — hence `form_values`.
        let res = f.go(&Req::post(
            private_route::OWNERS,
            "login=JameZ667&repos=intake&repos=other",
        )
        .with_cookie(&device));
        assert_eq!(res.status, 200, "{}", res.body);
        let roster = f.store.roster().unwrap();
        let owner = roster.owner_for("jamez667").expect("promoted");
        assert_eq!(owner.repos, ["intake", "other"], "both ticked repositories");

        // And revoking from the same page.
        let res = f.go(&Req::post("/owners/jamez667/revoke", "").with_cookie(&device));
        assert_eq!(res.status, 200, "{}", res.body);
        assert!(f.store.roster().unwrap().owner_for("jamez667").is_none());
    }

    #[test]
    fn a_repository_no_daemon_declared_is_not_enabled_by_accident() {
        // **A typo-catcher, not a gate.** Enabling `smrt-coder` writes a name
        // nothing will ever claim, and filings pile up against a repository
        // that does not exist. The check asks a machine that is polling now.
        let mut f = Fixture::new("repos-typo").with_public(false);
        let device = f.as_admin();

        // A daemon is polling and says what it serves.
        f.go(&Req::get(&format!("{}?repo=smart-coder", wire::route::WORK)).with_bearer(KEY));

        let res = f.go(&Req::post(private_route::REPOS, "name=smrt-coder").with_cookie(&device));
        assert_eq!(res.status, 200, "questioned, not refused: {}", res.body);
        assert!(
            f.store.roster().unwrap().enabled().is_empty(),
            "a misspelling was written on the first ask"
        );
        // The page says which case it is and what *is* on offer, rather than a
        // bare error somebody would click past.
        assert!(res.body.contains("smrt-coder"), "{}", res.body);
        assert!(res.body.contains("smart-coder"), "{}", res.body);

        // Confirmed by a daemon: enabled outright, and recorded as confirmed.
        let res = f.go(&Req::post(private_route::REPOS, "name=smart-coder").with_cookie(&device));
        assert_eq!(res.status, 200, "{}", res.body);
        let roster = f.store.roster().unwrap();
        assert_eq!(roster.enabled(), ["smart-coder"]);
        assert_eq!(roster.repos[0].served_by.as_deref(), Some("test-daemon"));
    }

    #[test]
    fn an_unconfirmed_repository_can_be_enabled_anyway_and_says_so() {
        // A `None` is not proof of a typo: `Seen` is empty for the first half
        // minute after a restart, and an older daemon declares nothing. So the
        // override has to exist — and taking it is recorded, or the page could
        // not later explain why nothing is being drafted.
        let mut f = Fixture::new("repos-anyway").with_public(false);
        let device = f.as_admin();

        let res = f.go(
            &Req::post(private_route::REPOS, "name=not-yet-polling&anyway=yes")
                .with_cookie(&device),
        );
        assert_eq!(res.status, 200, "{}", res.body);
        let roster = f.store.roster().unwrap();
        assert_eq!(roster.enabled(), ["not-yet-polling"]);
        assert_eq!(
            roster.repos[0].served_by, None,
            "an assertion is not a confirmation"
        );
    }

    #[test]
    fn a_surface_with_no_repositories_still_serves_and_says_why() {
        // Reachable the moment a developer disables the last one. Not a 404 —
        // that teaches somebody at a working address nothing — and not a
        // refusal to boot, which would put the page that fixes it out of reach
        // exactly when it is needed.
        let mut f = Fixture::new("repos-none").with_public(false);
        if let Some(p) = f.public.as_mut() {
            p.repos = crate::config::Repos::default();
        }
        let session = f.signed_in("jo@x.com");

        let res = f.go(&Req::get(public_route::FILE).with_cookie(&session));
        assert_eq!(res.status, 200, "the page still serves");
        assert!(
            !res.body.contains("<textarea"),
            "a form that always refuses is worse than none: {}",
            res.body
        );

        // And a filing submitted anyway is refused rather than landing
        // somewhere nobody chose.
        let res =
            f.go(&Req::post(public_route::FILE, "text=a+thing&kind=bug").with_cookie(&session));
        assert_eq!(res.status, 400);
        assert!(f.store.all().unwrap().is_empty());
    }

    #[test]
    fn disabling_a_repository_keeps_what_already_came_through_it() {
        // Disabling closes the door; it does not discard what came through it.
        // Deleting would make the button destructive in a way its name does not
        // say — and the developer's own review surface still shows the work.
        let mut f = Fixture::new("repos-disable")
            .with_public(false)
            .with_repos(&["intake"]);
        let device = f.as_admin();
        let filer = f.signed_in("jo@x.com");
        f.go(
            &Req::post(public_route::FILE, "text=a+thing&kind=bug&repo=intake").with_cookie(&filer),
        );
        assert_eq!(f.store.all().unwrap().len(), 1);

        let res = f.go(&Req::post("/repos/intake/disable", "").with_cookie(&device));
        assert_eq!(res.status, 200, "{}", res.body);
        assert!(f.store.roster().unwrap().enabled().is_empty());
        assert_eq!(f.store.all().unwrap().len(), 1, "the filing survived");
    }

    #[test]
    fn an_owner_cannot_reach_the_repository_switch() {
        // The same structural argument as the roster: the only writer lives
        // past the device gate. An owner who could enable a repository could
        // enable one they own and collect for it.
        let (mut f, session, _mine, _theirs) = owner_fixture("repos-owner");

        for (path, body) in [
            (private_route::REPOS, "name=whatever&anyway=yes"),
            ("/repos/intake/disable", ""),
        ] {
            let res = f.go(&Req::post(path, body).with_cookie(&session));
            assert_eq!(res.status, 401, "an owner reached {path}: {}", res.body);
        }
        assert_eq!(f.store.roster().unwrap().enabled(), ["intake", "other"]);
    }

    #[test]
    fn a_repository_this_server_does_not_serve_cannot_be_owned() {
        // Matched against the served set, never taken on trust — the same rule
        // as a public filing. A record naming something unserved would be a
        // permission that looks applied and grants nothing.
        let mut f = Fixture::new("owners-unknown-repo")
            .with_public(false)
            .with_repos(&["intake"]);
        let device = f.as_admin();

        let res = f.go(
            &Req::post(private_route::OWNERS, "login=jamez667&repos=not-served")
                .with_cookie(&device),
        );
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(f.store.roster().unwrap().owners.is_empty());

        // Nor an owner of nothing at all, which reads as promoted on the page
        // and grants nothing.
        let res = f.go(&Req::post(private_route::OWNERS, "login=jamez667").with_cookie(&device));
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(f.store.roster().unwrap().owners.is_empty());
    }

    #[test]
    fn a_filer_cannot_reach_any_review_verb() {
        // Iterates the shared constant, so a verb added later is covered without
        // anyone remembering to extend this list.
        let mut f = Fixture::new("public-no-review").with_public(false);
        let session = f.signed_in("jo@x.com");
        f.go(&Req::post(public_route::FILE, "text=a+thing&kind=bug").with_cookie(&session));
        let id = f.store.all().unwrap()[0].id.clone();

        for verb in REVIEW_VERBS {
            let res = f.go(&Req::post(&format!("/request/{id}/{verb}"), "").with_cookie(&session));
            assert_eq!(res.status, 401, "an account reached {verb}: {}", res.body);
        }
        // And the private list and detail pages are closed to them too.
        //
        // **404, not 401.** A signed-in filer is not a device, and the review
        // surface should not confirm its own address to them — the same
        // reasoning as somebody else's request id returning "not found" rather
        // than "forbidden".
        assert_eq!(
            f.go(&Req::get(private_route::REVIEW).with_cookie(&session))
                .status,
            404
        );
        assert_eq!(
            f.go(&Req::get(&format!("/request/{id}")).with_cookie(&session))
                .status,
            404
        );
        // `/` is the landing page and *is* reachable — it is the one public
        // thing here, and a filer seeing it is the design rather than a leak.
        assert_eq!(
            f.go(&Req::get(public_route::LANDING).with_cookie(&session))
                .status,
            200
        );
    }

    #[test]
    fn a_filer_cannot_reach_the_daemon_api() {
        let mut f = Fixture::new("public-no-daemon").with_public(false);
        let session = f.signed_in("jo@x.com");
        assert_eq!(
            f.go(&Req::get(wire::route::WORK).with_cookie(&session))
                .status,
            401
        );
    }

    #[test]
    fn signing_out_stops_the_session_working() {
        let mut f = Fixture::new("public-signout").with_public(false);
        let session = f.signed_in("jo@x.com");
        assert_eq!(
            f.go(&Req::get(public_route::FILE).with_cookie(&session))
                .status,
            200
        );

        f.go(&Req::post(public_route::SIGNOUT, "").with_cookie(&session));
        let after = f.go(&Req::get(public_route::FILE).with_cookie(&session));
        assert!(after.body.contains("Sign in"), "signed out: {}", after.body);
    }

    #[test]
    fn a_revoked_account_stops_filing_immediately() {
        // The developer's kill switch, which is the whole reason self-serve
        // signup is acceptable.
        let mut f = Fixture::new("public-revoke-live").with_public(false);
        let session = f.signed_in("jo@x.com");

        let mut accounts = f.store.accounts().unwrap();
        let id = accounts.live()[0].id.clone();
        accounts.revoke(&id);
        f.store.put_accounts(&accounts).unwrap();

        let res =
            f.go(&Req::post(public_route::FILE, "text=a+thing&kind=bug").with_cookie(&session));
        assert!(res.body.contains("Sign in"), "{}", res.body);
        assert!(f.store.all().unwrap().is_empty(), "nothing was filed");
    }

    #[test]
    fn the_developer_can_revoke_an_account_from_a_route() {
        // The lever that makes self-serve signup acceptable. Without a route it
        // means hand-editing accounts.json on the volume, which is not a
        // backstop anyone reaches for at the moment they need it.
        let mut f = Fixture::new("revoke-route").with_public(false);
        let session = f.signed_in("jo@x.com");
        let device = f.as_admin();

        // The filer specifically. The administrator now holds an account too —
        // they sign in with GitHub like everybody else — so "the only account"
        // is no longer a way to name the one being revoked.
        let id = f
            .store
            .accounts()
            .unwrap()
            .live()
            .iter()
            .find(|a| a.email_hint.contains("jo"))
            .expect("the filer")
            .id
            .clone();
        let listed = f.go(&Req::get("/accounts").with_cookie(&device));
        assert_eq!(listed.status, 200);
        assert!(listed.body.contains("jo***@x.com"), "{}", listed.body);

        let res = f.go(&Req::post(&format!("/accounts/{id}/revoke"), "").with_cookie(&device));
        assert_eq!(res.status, 200, "{}", res.body);
        let accounts = f.store.accounts().unwrap();
        assert!(accounts.live().iter().all(|a| a.id != id), "still live");
        // **And the administrator survived it.** Revoking a filer must not
        // reach the account the server is administered from, which is a real
        // hazard now that both are ordinary accounts.
        assert_eq!(
            f.go(&Req::get(private_route::REVIEW).with_cookie(&device))
                .status,
            200,
            "revoking a filer locked out the administrator"
        );

        // And the filer's session dies with it, without anyone walking sessions.
        let after = f.go(&Req::get(public_route::FILE).with_cookie(&session));
        assert!(after.body.contains("Sign in"), "{}", after.body);
    }

    #[test]
    fn a_filer_cannot_reach_the_accounts_surface() {
        // Otherwise anyone who signed up could revoke everyone else.
        let mut f = Fixture::new("accounts-closed").with_public(false);
        let session = f.signed_in("jo@x.com");
        let id = f.store.accounts().unwrap().live()[0].id.clone();

        assert_eq!(
            f.go(&Req::get("/accounts").with_cookie(&session)).status,
            404
        );
        assert_eq!(
            f.go(&Req::post(&format!("/accounts/{id}/revoke"), "").with_cookie(&session))
                .status,
            401
        );
        assert!(!f.store.accounts().unwrap().live().is_empty(), "still live");
    }

    #[test]
    fn revoking_twice_is_not_an_error() {
        // The caller asked for a state that now holds.
        let mut f = Fixture::new("revoke-twice").with_public(false);
        f.signed_in("jo@x.com");
        let device = f.as_admin();
        let id = f.store.accounts().unwrap().live()[0].id.clone();

        let path = format!("/accounts/{id}/revoke");
        assert_eq!(f.go(&Req::post(&path, "").with_cookie(&device)).status, 200);
        assert_eq!(f.go(&Req::post(&path, "").with_cookie(&device)).status, 200);
        assert_eq!(
            f.go(&Req::post("/accounts/never-existed/revoke", "").with_cookie(&device))
                .status,
            200
        );
    }

    #[test]
    fn the_public_pages_carry_the_same_security_headers() {
        // The headers are returned from one function for every response, so this
        // holds by construction — asserted anyway, since the public surface is
        // the one that renders model-authored text to strangers.
        let named: Vec<&str> = security_headers(Policy::PublicScript)
            .iter()
            .map(|(k, _)| *k)
            .collect();
        for required in [
            "Referrer-Policy",
            "Cache-Control",
            "Content-Security-Policy",
        ] {
            assert!(named.contains(&required));
        }
    }

    #[test]
    fn a_page_showing_more_than_one_authors_spec_is_never_served_with_script() {
        // **The real dividing line**, and the one the first reading of `Policy`
        // got wrong. It is not "the public surface gets script" — it is that a
        // page rendering one author's model output can afford it and a page
        // rendering many authors' cannot.
        //
        // An owner's pages sit on public paths and show every filer's spec for a
        // repository, so they are the second kind. They were served as the first
        // until the policy was chosen by caller rather than by path.
        let mut f = Fixture::new("policy-by-caller")
            .with_public(false)
            .with_owner("jamez667", &["intake"]);
        let owner = f.signed_in_as_github("jamez667");
        let filer = f.signed_in("jo@x.com");

        for path in [public_route::LANDING, public_route::FILE] {
            assert_eq!(
                f.go(&Req::get(path).with_cookie(&owner)).policy,
                Policy::Strict,
                "{path} shows several filers' specs to an owner"
            );
            // And a filer's own pages keep it, because the argument does reach
            // them: they wrote the input and can already read the output.
            assert_eq!(
                f.go(&Req::get(path).with_cookie(&filer)).policy,
                Policy::PublicScript,
                "{path} shows a filer only their own"
            );
        }
    }

    #[test]
    fn the_script_policy_reaches_the_public_surface_and_stops_there() {
        // Driven through `handle`, not by calling `csp()`, because the property
        // is about *routing*: the stamp is applied once at the dispatch site, so
        // what this really checks is that the dispatch site is the same one
        // every public route goes through.
        let mut f = Fixture::new("policy-split").with_public(false);
        let account = f.signed_in("filer@example.test");
        let device = f.as_admin();

        let signin_link = format!("{}sometoken", public_route::SIGNIN_PREFIX);
        for path in [public_route::SIGNIN, public_route::FILE, &signin_link] {
            let res = f.go(&Req::get(path).with_cookie(&account));
            assert_eq!(
                res.policy,
                Policy::PublicScript,
                "{path} is on the public surface"
            );
        }

        // A path that merely *looks* public is not, and must not pick the
        // permission up. `is_public_path` is an allowlist, so an unmatched
        // `/public/...` falls through to the private surface's device gate —
        // which is where its policy comes from too.
        let stray = f.go(&Req::get("/public/nothing-here").with_cookie(&account));
        assert_eq!(stray.status, 404, "not a public route");
        assert_eq!(stray.policy, Policy::Strict);

        // The private surface, including the routes a *device* reaches. These
        // render every filer's spec on one page, which is the reason the
        // permission does not extend here.
        for path in [private_route::REVIEW, "/accounts"] {
            let res = f.go(&Req::get(path).with_cookie(&device));
            assert_eq!(res.status, 200, "{path}: {}", res.body);
            assert_eq!(res.policy, Policy::Strict, "{path} is private");
        }

        // And the daemon's API, which is neither.
        assert_eq!(
            f.go(&Req::get("/api/v1/work").with_bearer(KEY)).policy,
            Policy::Strict
        );
    }

    // -- language -----------------------------------------------------------

    #[test]
    fn choosing_a_language_sets_a_cookie_and_renders_in_it() {
        let mut f = Fixture::new("lang-set").with_public(false);
        let res = f.go(&Req::post(public_route::LANGUAGE, "lang=fr"));

        assert_eq!(res.status, 200);
        let cookie = res.set_cookie.expect("a language cookie is set");
        assert!(cookie.starts_with(&format!("{LANG_COOKIE}=fr")), "{cookie}");
        // A preference, not a credential: readable by the page's own script, and
        // `Lax` so arriving from an external link still shows the chosen
        // language. Both are departures from the session cookie and both are
        // deliberate, so both are pinned.
        assert!(!cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Lax"), "{cookie}");
        assert!(cookie.contains("Secure"), "{cookie}");
        assert!(res.body.contains("<html lang=\"fr\""), "{}", res.body);
    }

    #[test]
    fn cookies_are_secure_on_a_deployed_server_and_not_on_loopback() {
        // A browser *discards* a `Secure` cookie sent over plain HTTP, so on
        // http://localhost sign-in and the switcher would both appear to do
        // nothing — the request succeeds and the cookie vanishes. That reads as
        // a bug in the feature rather than a property of the cookie.
        //
        // Asserted on **every** cookie this server sets, not just the language
        // one, because the failure is identical for the session cookie and
        // rather more confusing.
        let mut deployed = Fixture::new("secure-yes").with_public(false);
        let account = deployed.signed_in("filer@example.test");
        for res in [
            deployed.go(&Req::post(public_route::LANGUAGE, "lang=fr")),
            deployed.go(&Req::post(public_route::SIGNOUT, "").with_cookie(&account)),
        ] {
            let c = res.set_cookie.expect("a cookie is set");
            assert!(c.contains("; Secure"), "{c}");
        }

        let mut local = Fixture::new("secure-no").with_public(false).on_loopback();
        let account = local.signed_in("filer@example.test");
        for res in [
            local.go(&Req::post(public_route::LANGUAGE, "lang=fr")),
            local.go(&Req::post(public_route::SIGNOUT, "").with_cookie(&account)),
        ] {
            let c = res.set_cookie.expect("a cookie is set");
            assert!(!c.contains("Secure"), "{c}");
            // Everything else is unchanged — dropping `Secure` on loopback must
            // not quietly relax `HttpOnly` or `SameSite` with it.
            assert!(c.contains("SameSite="), "{c}");
        }
    }

    #[test]
    fn the_fonts_are_served_from_this_origin_as_real_woff2() {
        // The whole point of vendoring them. If these 404, the page silently
        // falls back to Georgia and nobody notices until they look closely —
        // and if they serve something that is not a font, the browser rejects
        // it just as quietly.
        let mut f = Fixture::new("fonts").with_public(false);
        for path in [public_route::FONT_BODY, public_route::FONT_DISPLAY] {
            let res = f.go(&Req::get(path));
            assert_eq!(res.status, 200, "{path}");
            assert_eq!(res.content_type, "font/woff2", "{path}");
            let bytes = res.binary.expect("a font is bytes, not a string");
            // The woff2 signature. A truncated download or an error page would
            // be served happily and render as no font at all.
            assert_eq!(&bytes[..4], b"wOF2", "{path} is not a woff2");
            assert!(bytes.len() > 10_000, "{path} is suspiciously small");
        }

        // Reachable **signed out**: the sign-in page is the first thing anyone
        // sees, and it should not be the one page rendered in a fallback face.
        assert_eq!(f.go(&Req::get(public_route::FONT_BODY)).status, 200);
    }

    #[test]
    fn the_stylesheet_asks_for_no_origin_but_this_one() {
        // `font-src 'self'` permits these two and refuses everything else, so a
        // stylesheet that named a remote face would produce an invisible
        // failure: the CSP blocks it, the page falls back, and nothing errors.
        let css = crate::page::PUBLIC_STYLE;
        for face in css.split("@font-face").skip(1) {
            let block = face.split('}').next().unwrap_or("");
            assert!(
                block.contains("url(/public/"),
                "a face is not served from this origin: {block}"
            );
        }
        assert!(!css.contains("fonts.googleapis"), "{css}");
        assert!(!css.contains("fonts.gstatic"), "{css}");
    }

    #[test]
    fn the_language_route_is_reachable_signed_out() {
        // The whole point. Somebody who cannot read the sign-in page is exactly
        // who needs this, so requiring an account first would mean reading a
        // page in a language they do not have.
        let mut f = Fixture::new("lang-anon").with_public(false);
        let res = f.go(&Req::post(public_route::LANGUAGE, "lang=fr"));
        assert_eq!(res.status, 200);
        assert!(res.body.contains("<html lang=\"fr\""));
    }

    #[test]
    fn an_unknown_language_falls_back_rather_than_reaching_the_page() {
        // The value is matched against the catalogues that exist, so nothing a
        // caller writes here reaches a page except by choosing among them.
        let mut f = Fixture::new("lang-unknown").with_public(false);
        for hostile in [
            "lang=de",
            "lang=",
            "lang=%3Cscript%3Ealert(1)%3C%2Fscript%3E",
            "lang=../../etc/passwd",
            "",
        ] {
            let res = f.go(&Req::post(public_route::LANGUAGE, hostile));
            assert_eq!(res.status, 200, "{hostile}");
            assert!(res.body.contains("<html lang=\"en\""), "{hostile}");
            // The page carries exactly one script — its own, from this origin —
            // and the hostile value contributes nothing. Asserted as "only the
            // expected tag" rather than "no script at all": the shell now has a
            // legitimate one, and a blanket ban would have to be deleted here,
            // taking the injection check with it.
            assert_eq!(
                res.body.matches("<script").count(),
                1,
                "{hostile}: {}",
                res.body
            );
            assert!(
                res.body
                    .contains("<script src=\"/public/app.js\" defer></script>"),
                "{hostile}: the only script is the surface's own"
            );
            assert!(!res.body.contains("alert(1)"), "{hostile}: {}", res.body);
            assert!(!res.body.contains("passwd"), "{hostile}: {}", res.body);
            let cookie = res.set_cookie.unwrap_or_default();
            assert!(
                cookie.starts_with(&format!("{LANG_COOKIE}=en")),
                "{hostile}: {cookie}"
            );
        }
    }

    #[test]
    fn a_signed_in_filers_pages_follow_their_chosen_language() {
        // The property that matters beyond the switcher itself: the locale is
        // decided once per request and reaches every page, not only the one the
        // switcher happens to re-render.
        let mut f = Fixture::new("lang-through").with_public(false);
        let account = f.signed_in("filer@example.test");

        let filing = f.go(&Req::get(public_route::FILE)
            .with_cookie(&account)
            .with_lang(Some("fr"), None));
        assert_eq!(filing.status, 200);
        assert!(filing.body.contains("<html lang=\"fr\""), "{}", filing.body);
        assert!(filing.body.contains("Déposer"), "{}", filing.body);

        // And the browser's header is honoured when nothing was chosen.
        let by_header = f.go(&Req::get(public_route::FILE)
            .with_cookie(&account)
            .with_lang(None, Some("fr-CA,fr;q=0.9,en;q=0.5")));
        assert!(by_header.body.contains("<html lang=\"fr\""));
    }

    #[test]
    fn the_private_surface_is_not_translated() {
        // One reader, who is the developer. Translating it would be catalogue
        // weight paid for nobody — asserted so that "the whole server is
        // localised" does not creep in later without the decision being retaken.
        let mut f = Fixture::new("lang-private").with_public(false);
        let device = f.as_admin();
        let res = f.go(&Req::get(private_route::REVIEW)
            .with_cookie(&device)
            .with_lang(Some("fr"), None));
        assert_eq!(res.status, 200);
        assert!(res.body.contains("<html lang=\"en\""), "{}", res.body);
    }

    #[test]
    fn a_public_path_is_strict_when_no_public_surface_is_configured() {
        // The 404 for a surface that does not exist is not *on* that surface.
        // Worth pinning: it is rendered from inside the `is_public_path` branch,
        // one line from the stamp, and is the easiest thing to sweep into it.
        let mut f = Fixture::new("policy-unconfigured");
        let res = f.go(&Req::get(public_route::SIGNIN));
        assert_eq!(res.status, 404);
        assert_eq!(res.policy, Policy::Strict);
    }

    #[test]
    fn an_account_cannot_file_past_its_daily_cap() {
        // The ceiling on model spend. Every filing that clears the screener
        // costs a full drafting run on the developer's machine, and 240/min is
        // no defence against something that expensive.
        let mut f = Fixture::new("daily-cap")
            .with_public(false)
            .with_caps(3, 100);
        let session = f.signed_in("jo@x.com");

        for i in 0..3 {
            let res = f.go(
                &Req::post(public_route::FILE, &format!("text=thing+{i}&kind=bug"))
                    .with_cookie(&session),
            );
            assert_eq!(res.status, 200, "filing {i}: {}", res.body);
        }

        let refused = f
            .go(&Req::post(public_route::FILE, "text=one+too+many&kind=bug").with_cookie(&session));
        assert_eq!(refused.status, 429, "{}", refused.body);
        assert!(refused.body.contains("limit"), "{}", refused.body);
        assert_eq!(f.store.all().unwrap().len(), 3, "nothing extra was written");
    }

    #[test]
    fn the_daily_cap_counts_filings_not_survivors() {
        // Discarding a request must not free up budget, or file-then-discard is
        // a way around the limit. The cost being capped is the *filing*.
        let mut f = Fixture::new("cap-counts")
            .with_public(false)
            .with_caps(2, 100);
        let session = f.signed_in("jo@x.com");

        f.go(&Req::post(public_route::FILE, "text=first&kind=bug").with_cookie(&session));
        let id = f.store.all().unwrap()[0].id.clone();
        f.store.discard(&id).unwrap();
        f.go(&Req::post(public_route::FILE, "text=second&kind=bug").with_cookie(&session));

        let refused =
            f.go(&Req::post(public_route::FILE, "text=third&kind=bug").with_cookie(&session));
        assert_eq!(refused.status, 429, "a discard did not refund the budget");
    }

    #[test]
    fn a_quarantined_filing_still_counts_against_the_cap() {
        // It cost a screening call. Refunding the budget for spam would make
        // "file spam until quarantined" a free way to keep filing.
        let mut f = Fixture::new("cap-quarantined")
            .with_public(true)
            .with_caps(1, 100);
        let session = f.signed_in("jo@x.com");

        f.go(&Req::post(public_route::FILE, "text=spam&kind=bug").with_cookie(&session));
        let id = f.store.all().unwrap()[0].id.clone();
        f.store
            .finish_screening(&id, Some("screened as spam"))
            .unwrap();

        let refused =
            f.go(&Req::post(public_route::FILE, "text=another&kind=bug").with_cookie(&session));
        assert_eq!(refused.status, 429, "quarantine did not refund the budget");
    }

    #[test]
    fn a_developers_own_filings_are_outside_the_account_cap() {
        // The ceiling bounds what strangers spend of the developer's budget, not
        // what the developer spends of their own — a device filing carries no
        // account, so it counts against nobody.
        let mut f = Fixture::new("cap-device")
            .with_public(false)
            .with_caps(1, 100);
        let device = f.as_admin();
        for i in 0..3 {
            let res = f.go(
                &Req::post("/file", &format!("text=thing+{i}&repo=alpha&kind=bug"))
                    .with_cookie(&device),
            );
            assert_eq!(res.status, 200, "device filing {i}: {}", res.body);
        }
        assert_eq!(f.store.all().unwrap().len(), 3);
    }

    #[test]
    fn revoking_does_not_free_a_slot_under_the_account_ceiling() {
        // A revoked address can never be re-created, so a freed slot could only
        // be taken by a *different* one — counting live accounts would let
        // burned identities be swapped one for one under a wall that looks
        // intact. The lever at the ceiling is raising it, not revoking.
        let mut f = Fixture::new("ceiling-revoked")
            .with_public(false)
            .with_caps(20, 1);
        f.signed_in("first@x.com");

        let mut accounts = f.store.accounts().unwrap();
        let id = accounts.live()[0].id.clone();
        accounts.revoke(&id);
        f.store.put_accounts(&accounts).unwrap();
        assert!(f.store.accounts().unwrap().live().is_empty());

        // A different address, with the only account revoked.
        f.go(&Req::post(public_route::SIGNIN, "email=second%40x.com"));
        let body = f.mailer.last_body().unwrap();
        let token = body
            .split(public_route::SIGNIN_PREFIX)
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .unwrap()
            .to_string();
        let res = f.go(&Req::post(
            &format!("{}{token}", public_route::SIGNIN_PREFIX),
            "",
        ));

        assert!(cookie_token(&res).is_none(), "the slot was not freed");
        assert_eq!(f.store.accounts().unwrap().accounts.len(), 1);
    }

    #[test]
    fn the_cap_is_per_account_not_global() {
        // Otherwise one busy filer silences everyone else — the same failure the
        // shared anonymous rate-limit bucket had.
        let mut f = Fixture::new("cap-per-account")
            .with_public(false)
            .with_caps(1, 100);

        let alice = f.signed_in("alice@x.com");
        f.go(&Req::post(public_route::FILE, "text=alice&kind=bug").with_cookie(&alice));
        assert_eq!(
            f.go(&Req::post(public_route::FILE, "text=more&kind=bug").with_cookie(&alice))
                .status,
            429
        );

        let bob = f.signed_in("bob@x.com");
        assert_eq!(
            f.go(&Req::post(public_route::FILE, "text=bob&kind=bug").with_cookie(&bob))
                .status,
            200,
            "another filer has their own budget"
        );
    }

    #[test]
    fn the_window_rolls_so_a_capped_filer_recovers() {
        // A cap that never forgives is a ban dressed as a limit.
        let mut f = Fixture::new("cap-rolls")
            .with_public(false)
            .with_caps(1, 100);
        let session = f.signed_in("jo@x.com");
        f.go(&Req::post(public_route::FILE, "text=first&kind=bug").with_cookie(&session));
        assert_eq!(
            f.go(&Req::post(public_route::FILE, "text=second&kind=bug").with_cookie(&session))
                .status,
            429
        );

        // A day later.
        f.now_ms += crate::config::FILING_WINDOW_MS + 1;
        assert_eq!(
            f.go(&Req::post(public_route::FILE, "text=tomorrow&kind=bug").with_cookie(&session))
                .status,
            200
        );
    }

    #[test]
    fn signup_stops_at_the_account_ceiling() {
        // What the per-account cap rests on: an id an attacker cannot vary is
        // one they can re-mint, and a script with a hundred disposable addresses
        // would otherwise hold a hundred budgets.
        let mut f = Fixture::new("account-cap")
            .with_public(false)
            .with_caps(20, 1);
        f.signed_in("first@x.com");
        assert_eq!(f.store.accounts().unwrap().accounts.len(), 1);

        // The link is still issued and spent — the refusal is at creation, so
        // nothing about it tells a stranger where the wall is.
        f.go(&Req::post(public_route::SIGNIN, "email=second%40x.com"));
        let body = f.mailer.last_body().unwrap();
        let token = body
            .split(public_route::SIGNIN_PREFIX)
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .unwrap()
            .to_string();
        let res = f.go(&Req::post(
            &format!("{}{token}", public_route::SIGNIN_PREFIX),
            "",
        ));

        assert!(cookie_token(&res).is_none(), "no session was opened");
        assert_eq!(
            f.store.accounts().unwrap().accounts.len(),
            1,
            "and no account was created"
        );
    }

    #[test]
    fn an_existing_filer_signs_in_past_the_account_ceiling() {
        // The ceiling is on *creation*. Locking out the people who already have
        // accounts would turn a signup wall into an outage.
        let mut f = Fixture::new("account-cap-existing")
            .with_public(false)
            .with_caps(20, 1);
        f.signed_in("jo@x.com");

        // Same address again, with the ceiling already reached.
        let session = f.signed_in("jo@x.com");
        assert_eq!(
            f.go(&Req::get(public_route::FILE).with_cookie(&session))
                .status,
            200
        );
        assert_eq!(f.store.accounts().unwrap().accounts.len(), 1);
    }

    #[test]
    fn a_public_filing_is_length_capped_like_any_other() {
        let mut f = Fixture::new("public-length").with_public(false);
        let session = f.signed_in("jo@x.com");

        let wordy = format!("text={}&kind=bug", "word+".repeat(MAX_WORDS + 10));
        let res = f.go(&Req::post(public_route::FILE, &wordy).with_cookie(&session));
        assert_eq!(res.status, 400);
        assert!(f.store.all().unwrap().is_empty());
    }

    #[test]
    fn a_public_path_is_matched_exactly_and_not_by_prefix() {
        // On the private surface a loose prefix fails closed (401). Here it fails
        // OPEN, so `/publicXYZ` matching would hand an unauthenticated caller a
        // route nobody meant to expose.
        assert!(is_public_path(public_route::FILE));
        assert!(is_public_path(public_route::SIGNIN));
        assert!(is_public_path(public_route::SIGNOUT));
        assert!(is_public_path("/public/signin/abc"));
        assert!(is_public_path("/public/request/abc"));

        for near_miss in [
            "/publicXYZ",
            "/public-admin",
            "/publicsignin/abc",
            "/publi",
            "/enrol",
            // The review surface must not become public by resembling one.
            "/review",
            // The private surface's own request route must not become public by
            // resembling one.
            "/request/abc",
        ] {
            assert!(!is_public_path(near_miss), "{near_miss} must not be public");
        }

        // `/` **is** public now — it is the landing page. Asserted here rather
        // than left as an absence from the list above, because "not in the
        // not-public list" and "deliberately public" read the same in a diff.
        assert!(is_public_path("/"), "the landing page is public");
    }

    #[test]
    fn public_traffic_and_enrolment_are_counted_separately() {
        // The property the bucket split exists for, asserted where the
        // classification actually happens rather than only in the limiter.
        // Anonymous throughout: the split being asserted is the one between
        // *route classes*, which is the only thing a caller with no credential
        // can be keyed on.
        let probe = |path: &str| -> Bucket { bucket_for(&None, path) };

        // Sending mail and spending a link cost something; reading a page does
        // not, and starving reads would itself be the denial of service.
        assert_eq!(probe(public_route::SIGNIN), Bucket::PublicWrite);
        assert_eq!(probe("/public/signin/abc"), Bucket::PublicWrite);
        assert_eq!(probe(public_route::FILE), Bucket::PublicRead);
        assert_eq!(probe("/public/request/abc"), Bucket::PublicRead);

        // The landing page is a page render like the others, so it shares their
        // budget rather than enrolment's — starving it would take the site down
        // for everyone who has not signed in.
        assert_eq!(probe(public_route::LANDING), Bucket::PublicRead);

        // And nothing public shares a budget with enrolment.
        assert_eq!(probe(private_route::SETUP), Bucket::AnonPrivate);
        assert_eq!(probe(private_route::REVIEW), Bucket::AnonPrivate);
    }

    #[test]
    fn an_enrolled_device_is_not_throttled_by_someone_elses_guessing() {
        let mut f = Fixture::new("throttle-isolated");
        let token = f.as_admin();
        for _ in 0..40 {
            f.go(&Req::post("/enrol", "code=GUESS&label=x"));
        }
        assert_eq!(
            f.go(&Req::get(private_route::REVIEW).with_cookie(&token))
                .status,
            200,
            "the developer's own device still works"
        );
    }

    // -- parsing ------------------------------------------------------------

    #[test]
    fn form_values_are_percent_decoded() {
        let fields = form_fields("text=add+a+%22health%22+check&repo=alpha");
        assert_eq!(fields.get("text").unwrap(), "add a \"health\" check");
        assert_eq!(fields.get("repo").unwrap(), "alpha");
    }

    #[test]
    fn a_malformed_form_does_not_panic() {
        // The body comes off the public internet; every shape of it must be
        // survivable.
        for body in ["", "=", "&&&", "a", "a=", "%", "%zz", "a=%2"] {
            let _ = form_fields(body);
        }
    }

    #[test]
    fn a_query_string_does_not_change_which_route_runs() {
        let mut f = Fixture::new("query");
        let token = f.as_admin();
        assert_eq!(
            f.go(&Req::get("/review?x=1").with_cookie(&token)).status,
            200
        );
    }

    #[test]
    fn a_listing_shows_what_needs_a_human_first() {
        let mut f = Fixture::new("order");
        let token = f.as_admin();
        let a = f.file(&token, "first", "alpha");
        let b = f.file(&token, "second", "beta");
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        let payload = serde_json::to_string(&DraftedSpec::new(&a, "# S", "specs/x")).unwrap();
        f.go(&Req::post(&wire::route::drafted(&a), &payload).with_bearer(KEY));

        let ordered = listing_order(f.store.all().unwrap());
        assert_eq!(ordered[0].id, a, "the one awaiting review is first");
        assert_eq!(ordered[1].id, b);
    }
}
