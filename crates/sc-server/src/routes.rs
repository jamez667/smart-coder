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
use crate::store::{new_id, Request, Serves, Store};
// `RequestState` is used only by this file's tests now: the routes settle a
// request through `Store`, which returns the record rather than being told a
// state, so nothing outside the test module names one.
#[cfg(test)]
use crate::store::RequestState;

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

/// Where this server answers, as the environment configured it.
///
/// **One place to ask.** The address used to live in the settings *and* the
/// environment, seeded from one to the other, and code read whichever was
/// nearer — which is how a variable could be set, correct, and ignored. It is an
/// environment variable now, and this is how everything reaches it.
///
/// Empty only on a server with no public surface configured at all, which cannot
/// serve anything and cannot be claimed.
fn configured_base(ctx: &Ctx<'_>) -> String {
    ctx.public.map(|p| p.base_url.clone()).unwrap_or_default()
}

/// The same question, during setup, where the answer above is wrong.
///
/// **A server being claimed has no public surface yet**, so [`secure_attr`]
/// falls back to `Secure` — and on `http://127.0.0.1` the browser then discards
/// the setup cookie, which is precisely the failure that function's own doc
/// describes: the request succeeds, the cookie is dropped, and the next step has
/// forgotten. The wizard becomes a loop back to step one.
///
/// During setup the address being typed *is* the source of truth, and it has
/// already passed [`crate::config::check_base_url`] — which permits plain HTTP
/// only for a private host. So this asks the same question of the same value,
/// and a deployed server still cannot be talked into dropping `Secure`, because
/// its address had to be `https://` to get this far.
fn secure_attr_for(base_url: &str) -> &'static str {
    if crate::config::secure_for(base_url) {
        "; Secure"
    } else {
        ""
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
    /// Where the administrator and owners sign in with a password.
    ///
    /// **Public because it has to be reachable by somebody holding nothing** —
    /// it is how they stop holding nothing. A sibling of the magic-link form
    /// rather than a private route, so both ways in live on the page a person
    /// arrives at.
    pub const SIGNIN_PASSWORD: &str = "/public/signin/password";
    /// End a session.
    pub const SIGNOUT: &str = "/public/signout";
    /// `/public/request/<id>` — one of the filer's own requests.
    pub const REQUEST_PREFIX: &str = "/public/request/";
    /// Choose a language. `POST`, because it sets a cookie — and reachable
    /// **signed out**, since somebody who cannot read the sign-in page is
    /// precisely who needs it.
    pub const LANGUAGE: &str = "/public/language";
    // **There is no `/public/app.js`.** This surface had a script of its own —
    // progressive enhancement for the sign-in dialog and the language picker —
    // served as a file rather than inlined, because the policy is
    // `script-src 'self'` and never `'unsafe-inline'`. The interface's own bundle
    // is at `crate::api::ui::SCRIPT_PATH` now and is served under the same rule,
    // which is the part of that reasoning worth keeping.
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
    /// Step two: choose the username and password that will own this server.
    pub const SETUP_ADMIN: &str = "/setup/admin";
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
///
/// **`accept/confirm` was here and is gone with the two-step page.** The rendered
/// surface asked at `accept` and settled at `accept/confirm`, because a page had
/// to carry the digest of what was read from one request to the next; the client
/// already holds the spec it rendered, so `accept` takes the digest in the one
/// post. A constant naming an address the server does not serve is worse than
/// useless — the tests that iterate this list would have been asserting a refusal
/// from a route that 404s for everybody, which passes without proving anything.
pub const REVIEW_VERBS: [&str; 4] = ["accept", "send-back", "discard", "release"];

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
    /// The `If-None-Match` header — the ETag a client already holds.
    ///
    /// Read by exactly one route, the string catalogue. It is here rather than
    /// in a header bag for the reason the field above states: [`Req`] holds only
    /// what the routes actually use, and a bag invites reading whatever happens
    /// to be in it.
    pub if_none_match: Option<String>,
    /// The `Origin` header, when the browser sent one.
    ///
    /// **Only the JSON API reads this.** The HTML surface is defended by
    /// `SameSite=Strict` on the session cookie, which is load-bearing rather
    /// than defence-in-depth — a cross-site POST simply arrives without a
    /// credential. That still holds for `fetch`, and this adds a second line
    /// rather than replacing the first.
    pub origin: Option<String>,
    /// The `Content-Type` header, when one was sent.
    ///
    /// The API demands `application/json` on every mutating call. A `<form>`
    /// cannot send that content type, so a cross-origin page cannot reach these
    /// endpoints without a preflight — and the `Origin` check above is what the
    /// preflight then fails.
    pub content_type: Option<String>,
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
            if_none_match: None,
            origin: None,
            content_type: None,
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
            if_none_match: None,
            origin: None,
            content_type: None,
            body: body.into(),
        }
    }

    /// A POST to the JSON API, with the content type it demands.
    ///
    /// Separate from [`Req::post`] rather than a flag on it: the content type is
    /// half of what stops a `<form>` reaching these endpoints, so a test that
    /// sets it should have to say so.
    #[cfg(test)]
    pub fn post_json(path: &str, body: &str) -> Req {
        let mut req = Req::post(path, body);
        req.content_type = Some("application/json".into());
        req
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
    /// An `ETag` for this body, when the handler has a cheap one.
    ///
    /// **Set on exactly one route** — the string catalogue — and deliberately
    /// not a general mechanism. Everything else this server answers is either a
    /// store read whose freshness matters or a compiled-in asset the client
    /// never re-fetches, and an ETag on a response carrying somebody's request
    /// text would be a validator for content `Cache-Control: no-store` exists to
    /// keep out of caches. See [`api_ui_strings`].
    pub etag: Option<String>,
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
            etag: None,
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
            etag: None,
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
            // `style-src` carries **both** `'self'` and `'unsafe-inline'`. The
            // interface ships one bundled stylesheet, which is a served file
            // and needs `'self'`; the rendered pages inline their CSS in a
            // `<style>` block and need the other. Dropping `'self'` leaves the
            // interface unstyled — a failure a `curl` cannot see, because the
            // header is present and correct-looking and only a browser refuses
            // anything.
            //
            // An inline *style* is not an inline *script*: the argument against
            // `'unsafe-inline'` on `script-src` is that it is what a successful
            // injection needs, and a style block cannot execute.
            Policy::PublicScript => {
                "default-src 'none'; script-src 'self'; \
                 style-src 'self' 'unsafe-inline'; \
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
    /// Set when this request's session proved itself with a password within
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
///
/// **A thin call through, and deliberately still a function.** It used to set a
/// thread-local holding [`PublicConfig::site_name`](crate::config::PublicConfig)
/// for the masthead every rendered page carried, and clear it on the way out so a
/// thread serving the next request could not show that one the previous request's
/// name. The client draws its own masthead now, so there is no renderer to feed
/// and no thread-local left to leak.
///
/// **The name itself is currently rendered nowhere.** It is still configured and
/// still read into `PublicConfig`; nothing on the JSON surface carries it, so a
/// client cannot show it. Giving it a home means a field on [`crate::api::Me`] —
/// which is where the client already reads what it may draw — rather than
/// reviving a per-request global.
pub fn handle(ctx: &mut Ctx<'_>, req: &Req) -> Res {
    handle_inner(ctx, req)
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

    // **The browser surface's JSON API.** Matched here, above the setup and
    // public paths, so it is dispatched in one place and can never fall through
    // into the private device gate — which answers HTML, and an HTML 404 to a
    // `fetch` is a parse error rather than a status the client can act on.
    //
    // Kept apart from `/api/v1/work` above: that one is the daemon's, holds a
    // bearer key, and is versioned by the wire protocol. Two audiences, two
    // prefixes, and no path a client of one can guess to reach the other.
    if path.starts_with(crate::api::PREFIX) {
        return api_route(ctx, req, method, &path, &caller);
    }

    // **The interface's document**, when this server is serving it. Answered for
    // every path the interface owns rather than only `/`, because the client
    // routes on the path itself — a reader who reloads on `/public` must get the
    // application, not a 404 from a server that only knew about `/`.
    //
    if method == "GET" && wants_document(&path) {
        // **`PublicScript`, including on the administrative addresses**, and
        // this is where spec 18's amendment actually lands. The private surface
        // ran no script at all, and the argument was that a page there renders
        // every filer's model-authored spec at once — a cross-tenant leak with
        // no equivalent on the other side. That argument has not stopped being
        // true; the cost is accepted and recorded rather than answered.
        //
        // What still holds, and is doing the work now:
        //
        // - `default-src 'none'` is unchanged, so no remote subresource is
        //   reachable. The exfiltration argument is about *remotes*, and it is
        //   untouched — a renderer bug can corrupt the page and cannot phone
        //   home.
        // - `script-src 'self'` and never `'unsafe-inline'`. The bundle is a
        //   served file; there is no inline hydration payload, because an
        //   inline allowance is what a successful injection needs.
        // - `connect-src 'self'`, so the interface can reach this server and
        //   nothing else.
        //
        // The residual risk, stated plainly: a bug in the client-side renderer
        // is now a cross-tenant XSS on the administrator's surface, where it
        // would previously have been a rendering glitch. The ban on `innerHTML`
        // and the browser harness are what bound it, and neither is as strong as
        // "no script runs".
        return Res::html(200, crate::api::ui::INDEX).with_policy(Policy::PublicScript);
    }

    // **The interface's own files.** Matched here so they are reachable signed
    // out — a stranger has to be able to load the page that offers them a way
    // in, which is the same reason the sign-in route is exempt from the public
    // surface being off.
    //
    // `PublicScript` because these *are* the script: `Strict` would have the
    // browser refuse the bundle it was just sent.
    if method == "GET" && path == crate::api::ui::SCRIPT_PATH {
        let mut res = Res::html(200, crate::api::ui::SCRIPT);
        res.content_type = "text/javascript; charset=utf-8";
        return res.with_policy(Policy::PublicScript);
    }
    if method == "GET" && path == crate::api::ui::STYLE_PATH {
        let mut res = Res::html(200, crate::api::ui::STYLE);
        res.content_type = "text/css; charset=utf-8";
        return res.with_policy(Policy::PublicScript);
    }
    // The two faces, compiled into the binary. Same-origin by construction:
    // there is no path from a request to a file on disk here, so no request can
    // name one.
    //
    // **Here rather than on the public surface**, which is where they used to
    // live — a server with public intake off answered 404 for its own
    // stylesheet's fonts, and rendered in fallback faces with every status code
    // correct. They belong with the bundle that asks for them.
    if method == "GET" && path == crate::api::ui::FONT_BODY_PATH {
        return font(crate::api::ui::FONT_BODY);
    }
    if method == "GET" && path == crate::api::ui::FONT_DISPLAY_PATH {
        return font(crate::api::ui::FONT_DISPLAY);
    }

    // **The wizard's forms are gone; its endpoints are not.** Setup is still the
    // one thing reachable without a credential — it is how the first one comes to
    // exist — and it is still guarded by the single-use claim code and by the
    // token binding the rest of it to the browser that spent it. Those live at
    // `POST /api/v1/ui/setup/code` and `setup/admin`, dispatched above with every
    // other mutating call so they pass the same same-origin check.
    //
    // Both still **stop existing the moment the server is claimed** rather than
    // existing and refusing: a 404 means a stranger cannot tell a claimed server
    // from one that never had a wizard. `GET /setup` answers the application
    // shell like every other browser path, which grants nothing — the endpoints
    // behind it are what decide.

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
        // **The magic-link landing is not part of the public surface**, even
        // though it lives at a public address, and that is why it is matched
        // before `ctx.public` is consulted at all.
        //
        // It is the one route reached by a navigation from *outside* this server
        // — a link in an email — so the reader arrives holding nothing and with
        // nowhere else to go. The administrator's own way back in used to be
        // here too, for the same shape of reason: a freshly claimed server starts
        // with the public surface off, and gating the way in on the surface being
        // on locks out the only person who can turn it on. That case is now
        // `POST /api/v1/ui/signin/password`, which is dispatched above this
        // block and so never depended on the surface either. Pinned by
        // `the_administrator_can_sign_in_with_no_public_surface`.
        let link_landing = path.starts_with(public_route::SIGNIN_PREFIX);
        return match ctx.public {
            Some(_) => {
                let policy = match &caller {
                    Some(Caller::Owner { .. }) => Policy::Strict,
                    _ => Policy::PublicScript,
                };
                public_route(ctx, req, method, &path, &caller).with_policy(policy)
            }
            None if link_landing => {
                public_route(ctx, req, method, &path, &caller).with_policy(Policy::PublicScript)
            }
            // No public surface configured: this 404 is not *on* that surface, so
            // it is served strict like every other non-public response.
            None => Res::html(404, crate::api::NOT_FOUND),
        };
    }

    // Everything else has no handler at all.
    //
    // **The developer's own surface used to begin here**, behind a
    // `let Some(Caller::Admin { .. }) = caller else { 404 }` — the pattern that
    // made the owner role safe by *structure*: an owner may decline work and may
    // not accept it, and that was enforced not by a check inside the accept
    // handler but by every accepting verb living past that line, where no
    // `Caller::Owner` could reach it.
    //
    // That reasoning did not go away with the pages; it moved to
    // [`api_write`], which states the same rule with the same `let ... else` and
    // the same 404, and to `api_verb` for the per-verb split. Repeating the gate
    // here would gate nothing — there is no handler behind it — and a gate
    // guarding nothing is one somebody later trusts.
    //
    // The answers are the ones the private surface always gave. **404 rather
    // than unauthorized** on a GET: a 401 on `/review` tells a stranger the
    // address is real, and that is the fact being withheld from a signed-in
    // owner and a signed-in filer alike. A write gets 401, because a caller
    // sending one is not browsing and an honest client needs to know its
    // credential is the problem.
    if method == "GET" {
        return Res::html(404, crate::api::NOT_FOUND);
    }
    error(401, "unauthorized")
}

/// Check a password and, if it holds, open or refresh the session for it.
///
/// **The two named roles only.** Filers keep magic links: they are strangers,
/// and a stranger should not be made to keep a credential for a site they may
/// use once. The administrator and owners are people who come back, and asking
/// them to hold a password is what removes a third party from the path.
///
/// Everything that decides whether somebody gets in lives here exactly once —
/// the backoff that records failures, the re-authentication onto the browser's
/// existing session. It was split out so the form and the JSON endpoint could
/// not drift; the form is gone and only [`api_sign_in_with_password`] calls it,
/// and it stays a function because a second copy of this is a second place for
/// the backoff to be forgotten.
///
/// `Err(())` is deliberately uninformative: no caller may tell a wrong password
/// from an unknown login from a backoff, because neither may the person asking.
/// One answer for all three, or a guesser learns which half they got right.
fn check_password_for_session(
    ctx: &mut Ctx<'_>,
    req: &Req,
    login: &str,
    password: &str,
) -> Result<String, ()> {
    if login.is_empty() || password.is_empty() {
        return Err(());
    }

    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut accounts = match ctx.store.accounts() {
        Ok(a) => a,
        Err(_) => return Err(()),
    };

    // The check records the attempt either way, so the write below has to
    // happen on failure too — otherwise the backoff would never accumulate.
    let outcome = accounts.check_password(login, password, ctx.now_ms);
    if ctx.store.put_accounts(&accounts).is_err() {
        return Err(());
    }
    invalidate_accounts(ctx);

    let id = match outcome {
        Ok(id) => id,
        Err(retry_at) => {
            // Logged for the operator, never shown: the answer says the same
            // thing whichever failure it was.
            crate::log::warn("password refused")
                .with("login", login.to_ascii_lowercase())
                .with("retry_in_s", retry_at.saturating_sub(ctx.now_ms) / 1000)
                .emit();
            drop(_guard);
            return Err(());
        }
    };

    // **A re-authentication lands on the browser's existing session** rather
    // than opening a second one beside it. Otherwise proving yourself again
    // would mean signing out and back in, and the stale session would stay live
    // next to the fresh one — two credentials where the point was to refresh
    // one.
    let session = match req
        .cookie_token
        .as_deref()
        .filter(|t| accounts.refresh_session(t, ctx.now_ms))
    {
        Some(existing) => existing.to_string(),
        None => accounts.open_session(&id, ctx.now_ms),
    };
    if ctx.store.put_accounts(&accounts).is_err() {
        return Err(());
    }
    invalidate_accounts(ctx);
    drop(_guard);

    crate::log::info("signed in")
        .with("login", login.to_ascii_lowercase())
        .emit();
    Ok(session)
}

/// The cookie a fresh session is carried in.
///
/// **`Strict`, where the GitHub return needed `Lax`.** That relaxation existed
/// only because the browser arrived back from github.com; a password POST is
/// same-origin, so the tighter setting is available and is taken.
fn session_cookie(ctx: &Ctx<'_>, session: &str) -> String {
    let secure = secure_attr(ctx);
    format!("{COOKIE}={session}; Path=/; HttpOnly; SameSite=Strict{secure}; Max-Age=31536000")
}

/// One of the interface's API paths, spelled from the prefix rather than by
/// hand — a literal here would keep matching nothing if `PREFIX` ever moved.
fn api_path(rest: &str) -> String {
    format!("{}{rest}", crate::api::PREFIX)
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
        // **Credential guessing, which is what `AnonPrivate` is for** — its own
        // doc names this exact family. Not `PublicWrite`: that bucket is both
        // looser (30/min against 20) and *shared*, so somebody grinding
        // passwords would lock every filer out of asking for a magic link. The
        // path is public; the traffic is not.
        //
        // **Both spellings of the route, and this is load-bearing.** The
        // interface posts JSON to `/api/v1/ui/signin/password` while the form
        // posts to `/public/signin/password`; if only one were named here the
        // other would fall through to the generic API arm and get a different
        // budget, which is a bypass rather than an oversight.
        None if path == public_route::SIGNIN_PASSWORD || path == api_path("signin/password") => {
            Bucket::AnonPrivate
        }
        // Asking for a link costs an email either way it is asked for, so the
        // JSON endpoint shares the form's bucket rather than the API's.
        //
        // Choosing a language joins them: it is a POST, so it cannot be
        // `PublicRead`, but it costs one cookie and no store read — and left in
        // `AnonPrivate` a stranger switching language would spend a *fifth* of a
        // 20/min budget doing the one thing that makes the page readable to
        // them. It sits with the form route it twins, which is already here by
        // way of `is_public_path`.
        None if path == api_path("signin")
            || path == api_path("signout")
            || path == api_path("language") =>
        {
            Bucket::PublicWrite
        }
        // **`/me` is what a stranger's browser asks first.** The landing page is
        // public and the client cannot render it without knowing whether anybody
        // is signed in, so this is a page read in every sense that matters to a
        // rate limiter — `AnonPrivate`'s 20/min would throttle ordinary browsing
        // of a public page.
        //
        // Only `me`, and only for a caller with no credential. Everything else
        // under the prefix stays in the tight bucket below: an anonymous request
        // for somebody's *data* is either a mistake or a probe, and neither
        // deserves a public allowance.
        // **The catalogue rides with `me`, and for the identical reason.** It is
        // the second thing a stranger's browser asks and the landing page cannot
        // be drawn without it, so it is a page read in every sense a rate
        // limiter cares about. Left in `AnonPrivate` it would spend a *second*
        // slot of a 20/min budget on every load — turning a reader who reloads a
        // few times into a 429 on their own language, which renders as an
        // interface with no words in it.
        //
        // It is also the cheapest response this server has: no store read, no
        // allocation beyond one serialisation of a compiled-in constant, and a
        // 304 for anybody who has loaded the site before.
        None if path == ME_PATH || path == STRINGS_PATH => Bucket::PublicRead,
        // **`PublicRead`, like the fonts** — 600/min rather than the 20 an
        // anonymous private request gets. These are two immutable, compiled-in
        // responses with no store read behind them, and they are fetched on
        // every single page load.
        //
        // They were falling through to `AnonPrivate` because they are not on the
        // public path list, and that broke things quietly: a page load spends
        // three of a 20/min budget before rendering, so a reader who reloads a
        // few times gets a 429 on their *stylesheet* and sees an unstyled page.
        // Nothing checking status codes notices, because the document was fine.
        None if path == crate::api::ui::SCRIPT_PATH || path == crate::api::ui::STYLE_PATH => {
            Bucket::PublicRead
        }
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
    // **Wider than what `public_route` still handles**, deliberately. Most of
    // these are GETs the interface's shell answers before this is ever asked, so
    // the only thing reaching here on one of them is a *method* the surface does
    // not have — and this is what decides that the answer is the public 404
    // rather than the private surface's 401. A path dropped from this list stops
    // being public and starts being a door a stranger gets `unauthorized` at,
    // which is the fact the 404 exists to withhold.
    path == public_route::LANDING
        || path == public_route::FILE
        || path == public_route::SIGNIN
        || path == public_route::SIGNOUT
        || path == public_route::LANGUAGE
        || path.starts_with(public_route::SIGNIN_PREFIX)
        || path.starts_with(public_route::REQUEST_PREFIX)
}

/// The browser surface's JSON API.
///
/// **Every response here is JSON, including the failures.** The HTML surface
/// answers a stranger on a private path with a rendered 404 page; a `fetch`
/// cannot read that, so this answers the same *status* with a body the client
/// can parse. The status codes are deliberately identical to the HTML surface's
/// — see the module doc for why 404-rather-than-403 is load-bearing rather than
/// sloppy.
///
/// `Policy::Strict` throughout, which is the default: a JSON body is not a
/// document, nothing loads a subresource from it, and there is no reason to
/// relax anything.
/// `/api/v1/ui/me`, spelled out.
///
/// A constant because [`bucket_for`] and [`api_route`] both need it, and the
/// rate-limit classifier disagreeing with the route matcher is the failure the
/// route constants module exists to prevent.
use crate::api::{AccountView, FiledRequest, ReviewRequest, SettingsView};

const ME_PATH: &str = "/api/v1/ui/me";

/// `/api/v1/ui/strings`, spelled out. Same reasoning as [`ME_PATH`]: the rate
/// limiter and the route matcher must not be able to disagree about it.
pub const STRINGS_PATH: &str = "/api/v1/ui/strings";

fn api_route(
    ctx: &mut Ctx<'_>,
    req: &Req,
    method: &str,
    path: &str,
    caller: &Option<Caller>,
) -> Res {
    let rest = path.trim_start_matches(crate::api::PREFIX);
    match (method, rest) {
        // Who am I, and what may I do? The one endpoint reachable by anybody,
        // because signing in is reachable by anybody and the client has to be
        // able to ask before it has an answer.
        ("GET", "me") => {
            // The repositories this surface offers, so a filer's form has
            // something to render. Empty when nothing is configured, which is
            // the honest answer: there is nothing to file against.
            let offered = ctx
                .public
                .map(|p| p.repos.names().to_vec())
                .unwrap_or_default();
            match serde_json::to_string(&crate::api::Me::of_with_repos(caller.as_ref(), &offered)) {
                Ok(body) => Res::json(200, body),
                Err(e) => error(500, &e.to_string()),
            }
        }

        // The words this interface draws itself out of, in the language this
        // request negotiated. Reachable by anybody, for the same reason `me` is:
        // a stranger sees the landing page and the sign-in dialog, and neither
        // can be drawn without them.
        ("GET", "strings") => api_ui_strings(req),

        // **The wizard, which is reachable with no credential at all** — it is
        // how the first one is obtained. Guarded by the single-use claim code
        // instead, and by the token that binds the rest of it to one browser.
        //
        // Every arm stops existing the moment the server is claimed: not
        // "exists and refuses", because a 404 means a stranger cannot tell a
        // claimed server from one that never had setup.
        ("GET", "setup") => api_setup_state(ctx, req),
        // The caller's own requests, or the ones they review. **One path, three
        // answers**, exactly as `GET /` is three pages — which is the shape the
        // HTML surface already had and the client already has to understand.
        ("GET", "requests") => api_requests(ctx, req, caller),

        // One request. The 404s here are the load-bearing kind: another filer's
        // request and an owner's non-owned repository both answer *not found*,
        // because a 403 would confirm the id is real.
        ("GET", rest) if rest.starts_with("requests/") => {
            api_request(ctx, req, caller, rest.trim_start_matches("requests/"))
        }

        // The administrative lists. Each is `Caller::Admin` only, and the gate is
        // the same `let ... else` the HTML surface uses rather than a re-check
        // written out again here.
        ("GET", "settings") => api_admin(ctx, caller, AdminView::Settings),
        ("GET", "owners") => api_admin(ctx, caller, AdminView::Owners),
        ("GET", "repos") => api_admin(ctx, caller, AdminView::Repos),
        ("GET", "daemons") => api_admin(ctx, caller, AdminView::Daemons),
        ("GET", "accounts") => api_admin(ctx, caller, AdminView::Accounts),

        // **Every mutating call passes the CSRF guard first.** Written as one
        // arm rather than a check inside each handler, for the reason the CSP
        // stamping site gives: a guard added per handler is a guard eventually
        // missing from one.
        ("POST", rest) => {
            if let Err(refusal) = same_origin(ctx, req, rest) {
                return refusal;
            }
            api_write(ctx, req, caller, rest)
        }

        _ => error(404, "no such endpoint"),
    }
}

/// Refuse a mutating call that did not come from this server's own page.
///
/// **A second line, not a replacement.** `SameSite=Strict` on the session cookie
/// is still what actually stops a cross-site POST — the request simply arrives
/// with no credential and resolves to a stranger. That has been the whole
/// defence, load-bearing rather than defence-in-depth, and it is easy to lose by
/// accident: one endpoint reachable with a `Lax` cookie, or the interface moved
/// to another origin, and there is nothing behind it.
///
/// So this demands two things a cross-origin page cannot supply together:
///
/// - **`Content-Type: application/json`.** A `<form>` can only send three
///   content types and this is not among them, so reaching these endpoints from
///   a form is impossible; `fetch` can set it, but only after a preflight.
/// - **An `Origin` that is this server.** The browser sets it and a page cannot
///   forge it, so the preflight a `fetch` must pass is the one this fails.
///
/// A request with no `Origin` at all is allowed through: `curl` sends none, and
/// so do the tests. That is not a hole — a caller with no browser is not a
/// caller a browser can be tricked into being.
fn same_origin(ctx: &Ctx<'_>, req: &Req, path: &str) -> std::result::Result<(), Res> {
    let json = req
        .content_type
        .as_deref()
        .is_some_and(|c| c.trim_start().starts_with("application/json"));
    if !json {
        return Err(error(415, "this endpoint takes application/json"));
    }
    let Some(origin) = req.origin.as_deref() else {
        return Ok(());
    };
    // The configured address is the only origin this surface is ever served
    // from — it is what sign-in links are built from and what decides whether
    // cookies carry `Secure`, so a mismatch here is a request from somewhere
    // else by definition.
    //
    // **Read from the settings, not from `ctx.public`.** A server being set up
    // has no public surface, so that source is empty — and an empty `ours`
    // refused *everything*, including the wizard that is the only way to give
    // the server an address in the first place. The setup flow could not
    // complete on a fresh volume, which is the one flow with no fallback.
    // **One source now.** This read from the settings as well, because the
    // wizard used to store the address there before any surface existed. The
    // address is an environment variable and nothing else, so there is one place
    // to look and the fallback is gone with the field.
    let ours = ctx.public.map(|p| p.base_url.clone()).unwrap_or_default();
    if !ours.is_empty() && origin.trim_end_matches('/') == ours.trim_end_matches('/') {
        return Ok(());
    }
    // **Nothing configured means nothing to compare against**, and that is only
    // true of a server nobody has claimed. Exempting the wizard lets a fresh
    // volume be set up at all; exempting *everything* would turn an unconfigured
    // server into one with no CSRF defence, which the first version of this did
    // — an evil origin could discard a request and the test caught it.
    //
    // Anything else on a server with no address is refused. There is nothing
    // there to reach yet anyway.
    if ours.is_empty() && path.starts_with("setup/") {
        return Ok(());
    }
    Err(error(403, "cross-origin"))
}

/// The mutating half of the browser API.
fn api_write(ctx: &mut Ctx<'_>, req: &Req, caller: &Option<Caller>, rest: &str) -> Res {
    // `POST requests/{id}/{verb}` — the review and owner verbs.
    if let Some(tail) = rest.strip_prefix("requests/") {
        let Some((id, verb)) = tail.split_once('/') else {
            return error(404, "no such endpoint");
        };
        return api_verb(ctx, req, caller, id, verb);
    }

    // **The wizard, before the administrator gate below** — it is how the first
    // administrator comes to exist, so requiring one would be circular. It is
    // guarded instead by the single-use claim code and by the token binding the
    // rest of it to one browser.
    //
    // Reached through here rather than beside the read arms so it passes the
    // same-origin check like every other mutating call. A guard added per
    // handler is a guard eventually missing from one, and this is the handler it
    // would be missing from.
    match rest {
        "setup/code" => return api_spend_code(ctx, req),
        "setup/admin" => return api_claim(ctx, req),
        // **The three ways in and out, above the gate for the same reason the
        // wizard is**: requiring an administrator to sign in would be circular.
        // They are guarded instead by the same-origin check every mutating call
        // passes, by the backoff on the account, and by the rate limiter.
        "signin" => return api_request_link(ctx, req),
        "signin/password" => return api_sign_in_with_password(ctx, req),
        "signout" => return api_sign_out(ctx, req),
        // **Choosing a language, above the administrator gate and deliberately.**
        // Somebody who cannot read the page is exactly who needs this control,
        // and requiring an account first would mean signing in through a page
        // they cannot read. It reads no store, writes no store and names nobody
        // — the whole effect is one preference cookie.
        //
        // Reached from here rather than beside the read arms so it passes the
        // same-origin check with every other mutating call: it sets a cookie,
        // and a route that sets a cookie belongs behind that gate even when the
        // cookie is only a preference.
        //
        // **The JSON twin of `POST /public/language`**, and a twin rather than a
        // replacement — that route is a form target and answers a document,
        // because a form POST is a navigation. Both write the cookie through
        // `language_cookie`, so they cannot come to disagree about what a
        // language selection means.
        "language" => return api_set_language(ctx, req),
        // **Filing needs an account, not an administrator**, so it is above the
        // gate below and checks for itself. The account is the credential: it
        // costs a confirmed mailbox, which is what stops this being a
        // free-for-all.
        "file" => return api_file(ctx, req, caller),
        _ => {}
    }

    // Everything below administers the server. **The same gate the private
    // surface uses, and the same answer**: 404, not 401 — the administrative
    // surface does not exist for anybody else.
    let Some(Caller::Admin { .. }) = caller else {
        return error(404, "no such endpoint");
    };

    let body: serde_json::Value = if req.body.trim().is_empty() {
        serde_json::Value::Null
    } else {
        match serde_json::from_str(&req.body) {
            Ok(v) => v,
            Err(e) => return error(400, &format!("that body is not JSON: {e}")),
        }
    };
    let text = |k: &str| {
        body.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };

    match rest {
        "settings" => api_settings(ctx, &body),
        "owners" => api_add_owner(ctx, &text("login"), &body),
        "repos" => api_add_repo(
            ctx,
            &text("name"),
            body.get("anyway")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        ),
        "daemons" => api_mint_daemon(ctx, &text("label")),
        _ => {
            // `{list}/{id}/revoke` — the four revoke verbs, which share a shape.
            let Some((list, tail)) = rest.split_once('/') else {
                return error(404, "no such endpoint");
            };
            let Some((id, verb)) = tail.split_once('/') else {
                return error(404, "no such endpoint");
            };
            if verb != "revoke" && verb != "disable" {
                return error(404, "no such verb");
            }
            api_revoke(ctx, list, id)
        }
    }
}

/// Write one group of settings.
///
/// **There is no freshness gate here any more**, because there is no secret left
/// to write. The mail key and the screening key are environment variables; what
/// this surface still holds is a switch, a flag and four ceilings.
fn api_settings(ctx: &mut Ctx<'_>, body: &serde_json::Value) -> Res {
    let flag = |k: &str| body.get(k).and_then(|v| v.as_bool());
    // A number that is absent or null means "use the built-in default", which is
    // not the same as zero — the settings page has always said so.
    let cap = |k: &str| body.get(k).and_then(|v| v.as_u64()).map(|n| n as usize);

    // **No secret is writable here any more**, so there is no freshness gate.
    // The mail key and the screening key are environment variables; what is left
    // on this surface is a switch, a flag and four ceilings.
    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut s = match ctx.store.settings() {
        Ok(s) => s,
        Err(e) => return error(500, &e.to_string()),
    };

    if let Some(v) = flag("public") {
        s.public = v;
    }
    if let Some(v) = flag("show_spec") {
        s.show_spec = Some(v);
    }
    // **The address, the site name and the mail settings are not writable.**
    // They are environment variables now, so a request naming one is asking for
    // something this surface does not do — said plainly rather than accepted and
    // silently dropped, which is the failure the move was meant to end.
    for gone in [
        "base_url",
        "site_name",
        "mail_provider",
        "mail_from",
        "mail_from_name",
        "mail_key",
    ] {
        if body.get(gone).is_some() {
            return error(
                400,
                &format!("{gone} is set in the environment - change it in the stack and redeploy"),
            );
        }
    }

    for (name, field) in [
        ("max_daily_filings", &mut s.max_daily_filings),
        ("max_daily_drafts", &mut s.max_daily_drafts),
        ("max_accounts", &mut s.max_accounts),
        ("max_outstanding_links", &mut s.max_outstanding_links),
    ] {
        if body.get(name).is_some() {
            *field = cap(name);
        }
    }

    if let Err(e) = ctx.store.put_settings(&s) {
        return error(500, &e.to_string());
    }
    invalidate_settings(ctx);
    drop(_guard);
    match serde_json::to_string(&SettingsView::of(&s)) {
        Ok(b) => Res::json(200, b),
        Err(e) => error(500, &e.to_string()),
    }
}

/// Name somebody an owner of some repositories.
///
/// **Repository names are matched against what this surface serves**, never
/// taken on trust — the same rule as a public filing, for the same reason. A
/// name that matches nothing would be a permission that looks applied and grants
/// nothing, which is precisely what the configuration used to refuse to boot on
/// and a record cannot.
fn api_add_owner(ctx: &mut Ctx<'_>, login: &str, body: &serde_json::Value) -> Res {
    if let Err(e) = check_login(login) {
        return error(400, &e);
    }
    let asked: Vec<String> = body
        .get("repos")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|r| r.as_str().map(|s| s.trim().to_string()))
                .collect()
        })
        .unwrap_or_default();
    let served = ctx.public.map(|p| &p.repos);
    let repos: Vec<String> = asked
        .into_iter()
        .filter(|r| served.is_some_and(|s| s.accepts(r)))
        .collect();
    if repos.is_empty() {
        // Not silently written as an owner of nothing: that reads as promoted
        // and grants nothing at all.
        return error(400, "pick at least one repository this server serves");
    }

    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut roster = match ctx.store.roster() {
        Ok(r) => r,
        Err(e) => return error(500, &e.to_string()),
    };
    roster.set_owner(login, &repos, ctx.now_ms);
    // Seeding is a first-use thing, and this volume has now been administered.
    // Without it a restart would re-apply the configured seed over a roster
    // somebody built by hand.
    roster.seeded = true;
    if let Err(e) = ctx.store.put_roster(&roster) {
        return error(500, &e.to_string());
    }
    invalidate_roster(ctx);
    drop(_guard);
    match serde_json::to_string(&roster.owners) {
        Ok(b) => Res::json(200, b),
        Err(e) => error(500, &e.to_string()),
    }
}

/// Enable a repository for public filing.
fn api_add_repo(ctx: &mut Ctx<'_>, name: &str, anyway: bool) -> Res {
    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut roster = match ctx.store.roster() {
        Ok(r) => r,
        Err(e) => return error(500, &e.to_string()),
    };
    // **A repository no daemon has offered is refused unless forced.** Naming
    // one nothing serves produces a queue that never drains, and the operator
    // finds out when somebody files into it.
    let offered = ctx
        .seen
        .lock()
        .map(|s| s.offered(ctx.now_ms))
        .unwrap_or_default();
    let served_by = offered.iter().find(|r| *r == name).map(|_| {
        ctx.seen
            .lock()
            .ok()
            .and_then(|s| s.declared_by(name, ctx.now_ms))
            .unwrap_or_default()
    });
    if !anyway && served_by.is_none() {
        return error(
            409,
            "no machine has offered that repository - send anyway: true to enable it regardless",
        );
    }
    roster.enable(name, served_by, ctx.now_ms);
    roster.seeded = true;
    if let Err(e) = ctx.store.put_roster(&roster) {
        return error(500, &e.to_string());
    }
    invalidate_roster(ctx);
    drop(_guard);
    match serde_json::to_string(&roster.repos) {
        Ok(b) => Res::json(200, b),
        Err(e) => error(500, &e.to_string()),
    }
}

/// Mint a key for a machine. **Shown once and never again.**
fn api_mint_daemon(ctx: &mut Ctx<'_>, label: &str) -> Res {
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
    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut roster = match ctx.store.roster() {
        Ok(r) => r,
        Err(e) => return error(500, &e.to_string()),
    };
    let key = auth::mint_secret();
    roster.set_daemon(label, &auth::hash(&key), ctx.now_ms);
    roster.seeded = true;
    if let Err(e) = ctx.store.put_roster(&roster) {
        return error(500, &e.to_string());
    }
    invalidate_roster(ctx);
    drop(_guard);
    // **The response is the only copy.** The volume holds a hash, so nothing can
    // read it back — `Cache-Control: no-store` on every response is what stops
    // it being written down along the way.
    match serde_json::to_string(&serde_json::json!({ "label": label, "key": key })) {
        Ok(b) => Res::json(200, b),
        Err(e) => error(500, &e.to_string()),
    }
}

/// Revoke an account, an owner, a machine, or disable a repository.
///
/// **Revoked records are kept, not deleted** — a list that silently shrinks
/// cannot answer "did I already deal with that?", so the developer revokes twice
/// or worries they never did.
///
/// **Which is why a second revoke is not an error.** Every one of the four
/// underlying calls answers `false` for "already in that state" and "no such
/// record" alike, and neither is a failure worth telling the caller about: they
/// asked for a state that now holds. The page handlers this replaced said so
/// explicitly and answered 200; returning 404 here would make the button report a
/// failure for doing exactly what it was pressed for — and would tell a caller
/// which ids exist, which the write path has no business answering.
fn api_revoke(ctx: &mut Ctx<'_>, list: &str, id: &str) -> Res {
    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    if list == "accounts" {
        let mut accounts = match ctx.store.accounts() {
            Ok(a) => a,
            Err(e) => return error(500, &e.to_string()),
        };
        // Only written when something changed — an unchanged file is not worth a
        // write, and the answer below is the same either way.
        if accounts.revoke(id) {
            if let Err(e) = ctx.store.put_accounts(&accounts) {
                return error(500, &e.to_string());
            }
            invalidate_accounts(ctx);
        }
        drop(_guard);
        let view: Vec<_> = accounts.accounts.iter().map(AccountView::of).collect();
        return match serde_json::to_string(&view) {
            Ok(b) => Res::json(200, b),
            Err(e) => error(500, &e.to_string()),
        };
    }

    let mut roster = match ctx.store.roster() {
        Ok(r) => r,
        Err(e) => return error(500, &e.to_string()),
    };
    let changed = match list {
        "owners" => roster.revoke(id),
        "daemons" => roster.revoke_daemon(id),
        "repos" => roster.disable(id),
        // **This one is still a 404**, and it is a different question: an
        // unknown *list* is an address this API does not have, rather than a
        // record already in the state that was asked for.
        _ => return error(404, "no such endpoint"),
    };
    if changed {
        if let Err(e) = ctx.store.put_roster(&roster) {
            return error(500, &e.to_string());
        }
        invalidate_roster(ctx);
    }
    drop(_guard);
    let body = match list {
        "owners" => serde_json::to_string(&roster.owners),
        "daemons" => serde_json::to_string(&roster.daemons),
        _ => serde_json::to_string(&roster.repos),
    };
    match body {
        Ok(b) => Res::json(200, b),
        Err(e) => error(500, &e.to_string()),
    }
}

/// Act on a request: send back, discard, release, accept.
///
/// **The gate is the caller's variant, exactly as it is on the HTML surface.**
/// An owner reaches `send-back`, `discard` and `release`; only the administrator
/// reaches `accept`, and that is enforced here by matching the variant rather
/// than by a boolean somebody has to remember to check.
fn api_verb(ctx: &mut Ctx<'_>, req: &Req, caller: &Option<Caller>, id: &str, verb: &str) -> Res {
    let body: serde_json::Value = if req.body.trim().is_empty() {
        serde_json::Value::Null
    } else {
        match serde_json::from_str(&req.body) {
            Ok(v) => v,
            Err(e) => return error(400, &format!("that body is not JSON: {e}")),
        }
    };
    let note = body
        .get("note")
        .and_then(|n| n.as_str())
        .unwrap_or_default();
    let digest = body
        .get("digest")
        .and_then(|d| d.as_str())
        .unwrap_or_default();

    match caller {
        Some(Caller::Admin { .. }) => {
            // **The developer's own verbs are bounded too**, and this is the
            // only place that is now true. It used to be checked in the rendered
            // review page's POST handler; deleting that would have taken the
            // whole cap with it, because nothing else counted a redraft. See
            // [`drafting_budget`] for why it is the repository's spend and not
            // the caller's, and why it is not a refusal of authority.
            if matches!(verb, "send-back" | "release") {
                if let Err(res) = charge_a_draft(ctx, id) {
                    return res;
                }
            }
            let outcome = match verb {
                "send-back" => ctx.store.send_back(id, note),
                "discard" => ctx.store.discard(id),
                "release" => ctx.store.release(id),
                // **The digest is the handshake, not decoration.** Accepting
                // means accepting *these bytes*; a redraft landing between
                // reading and accepting changes the digest, and the store
                // refuses rather than approving text nobody read.
                "accept" => {
                    if digest.is_empty() {
                        return error(400, "accepting needs the digest of the spec you read");
                    }
                    ctx.store.accept(id, digest)
                }
                _ => return error(404, "no such verb"),
            };
            match outcome {
                Ok(_) => api_request(ctx, req, caller, id),
                Err(e) => error(400, &e.to_string()),
            }
        }
        Some(Caller::Owner { repos, .. }) => {
            // Their repository, or nothing — and *nothing* is 404, because a 403
            // would confirm the id is real.
            //
            // The repository is carried out of this read rather than fetched
            // again below: the ownership check and the drafting budget ask about
            // the same record, and two reads could disagree.
            let repo = match ctx.store.get(id) {
                Ok(Some(r)) if repos.iter().any(|owned| owned == &r.repo) => r.repo,
                Ok(_) => return error(404, "no such request"),
                Err(e) => return error(500, &e.to_string()),
            };
            // `send-back` and `release` put the request back in the claimable
            // queue, so each costs a drafting run on the developer's machine —
            // bounded per repository, not per owner, which is what stops a
            // second owner doubling the day's spend.
            if matches!(verb, "send-back" | "release") {
                if let Err(res) = drafting_budget(ctx, &repo) {
                    return res;
                }
            }
            let outcome = match verb {
                "send-back" => ctx.store.send_back(id, note),
                "discard" => ctx.store.discard(id),
                // The one owner verb that admits work. Screening is a model
                // judging a stranger's text, so it has false positives — and an
                // owner who can see their repository's queue but not unblock it
                // has to ask the developer about every one of them. Bounded by
                // the budget taken above.
                "release" => ctx.store.release(id),
                // **An owner may not accept**, and this is where that is true.
                // Not a 403: the verb does not exist for them.
                _ => return error(404, "no such verb"),
            };
            match outcome {
                Ok(_) => api_request(ctx, req, caller, id),
                Err(e) => error(400, &e.to_string()),
            }
        }
        // A filer, a stranger, a daemon: the review surface is not theirs.
        _ => error(404, "no such request"),
    }
}

/// Charge a re-admitting verb against its request's repository.
///
/// A wrapper over [`drafting_budget`] for the caller who has an id and not yet a
/// repository. **A request that cannot be read is not charged**, which matches
/// what the rendered surface did: the verb below re-reads it and fails properly,
/// and refusing here would turn a transient read failure into what looks like a
/// spend limit.
fn charge_a_draft(ctx: &Ctx<'_>, id: &str) -> std::result::Result<(), Res> {
    match ctx.store.get(id) {
        Ok(Some(r)) => drafting_budget(ctx, &r.repo),
        _ => Ok(()),
    }
}

/// The interface's words, in the language this request negotiated.
///
/// **Reachable by anybody, and that is not a relaxation.** A stranger sees the
/// landing page and the sign-in dialog before they have any credential at all,
/// and neither can be drawn without these — so this endpoint is exactly as
/// public as `me`, and for the same reason. It reads nothing from the store and
/// names nobody: the response depends on the `Accept-Language` header and the
/// language cookie, and on nothing else about the caller.
///
/// The locale comes from [`Req::locale`], which is cookie-then-header — the same
/// negotiation the magic-link landing uses. There is no `?lang=` parameter and
/// there must not be one: a second way to select a language is a second thing
/// that can disagree with the cookie the switcher writes, and the reader would
/// see the switcher appear not to work.
///
/// ## Why this is worth an ETag when nothing else here is
///
/// The catalogue is `&'static str` compiled into the binary. **It cannot change
/// while the process runs** — the only way to alter a string is to rebuild the
/// image and redeploy, which replaces the process. That is a stronger guarantee
/// than most cacheable resources have, and it is what makes the client's
/// `localStorage` copy safe to render from before any request has come back.
///
/// It is also what makes the ETag necessary rather than merely nice. A cache
/// that only a deploy invalidates is a cache with no natural expiry, so without
/// a validator a reader who has ever loaded this site keeps the strings they
/// first saw until they clear their browser — including after a deploy that
/// fixed a mistranslation. The tag is a hash of the exact bytes being sent, so
/// it changes when and only when the catalogue does, and a stale cache cannot
/// outlive the deploy that made it stale.
///
/// The tag covers the **body**, which includes the locale code — so `en` and
/// `fr` have different tags, and a reader switching language cannot be answered
/// 304 against the catalogue they were holding a moment ago.
///
/// `Cache-Control: no-store` still rides on this response with every other. That
/// is not a contradiction: `no-store` addresses caches this server does not
/// control, and the client here is not caching *as* an HTTP cache — it stores
/// the body itself and echoes the validator back by hand. The one thing that
/// would be wrong is a shared proxy holding this, and `no-store` is what stops
/// that.
fn api_ui_strings(req: &Req) -> Res {
    // **`?lang=` overrides the negotiation, and it has to.** Choosing a language
    // is `POST /api/v1/ui/language`, which sets the cookie — but every mutating
    // call passes `same_origin`, and a server with no configured address has
    // nothing to compare an `Origin` against and refuses all of them. On a fresh
    // deployment that left the switcher moving and nothing happening.
    //
    // Reading a catalogue in a named language changes nothing and reveals
    // nothing: the strings are compiled into the binary and identical for every
    // caller. So the read carries the choice, the cookie is set when it can be,
    // and the interface is translatable before the server is configured rather
    // than after.
    let asked = req
        .path
        .split_once('?')
        .and_then(|(_, q)| {
            q.split('&')
                .filter_map(|kv| kv.split_once('='))
                .find(|(k, _)| *k == "lang")
                .map(|(_, v)| v)
        })
        .and_then(crate::i18n::Locale::parse);
    let locale = asked.unwrap_or_else(|| req.locale());
    let body = match serde_json::to_string(&crate::api::UiStrings::of(locale)) {
        Ok(body) => body,
        Err(e) => return error(500, &e.to_string()),
    };

    // Hashed with the same function credentials go through, for no reason other
    // than that it is the one hash this crate has — there is nothing secret
    // here, and an ETag is a fingerprint rather than a secret. Quoted because
    // the grammar requires it, and a bare tag is silently ignored by some
    // proxies rather than rejected.
    let etag = format!("\"{}\"", auth::hash(&body));

    // A 304 carries **no body**, which is the entire saving: the client already
    // holds the catalogue and this says so in a few dozen bytes rather than a
    // few kilobytes. Compared verbatim — this server mints one tag per
    // catalogue, so a client either echoes it or does not.
    if req.if_none_match.as_deref() == Some(etag.as_str()) {
        let mut res = Res::json(304, String::new());
        res.etag = Some(etag);
        return res;
    }

    let mut res = Res::json(200, body);
    res.etag = Some(etag);
    res
}

/// Where the wizard has got to.
///
/// **The state is explicit here where the pages left it implicit.** A `GET
/// /setup` returned step one or step two depending on three things at once —
/// whether the server was claimed, whether an address was set, and whether this
/// browser held the token. A client cannot infer that from a rendered page, so
/// the server says it.
fn api_setup_state(ctx: &mut Ctx<'_>, req: &Req) -> Res {
    let admin = match ctx.store.admin() {
        Ok(a) => a,
        Err(e) => return error(500, &e.to_string()),
    };
    // Claimed means gone, for the same reason the page 404s: a stranger must
    // not be able to tell a claimed server from one that never had a wizard.
    if admin.claimed() {
        return error(404, "no such endpoint");
    }
    let mine = admin.setting_up(req.cookie_setup.as_deref(), ctx.now_ms);
    let body = serde_json::json!({
        // `code` or `admin` — which step this browser may take.
        //
        // **The token is the whole answer now.** It also depended on an address
        // being set, because the wizard asked for one; the address is an
        // environment variable, so the only question left is whether this
        // browser spent the code.
        "step": if mine { "admin" } else { "code" },
        "base_url": configured_base(ctx),
        "min_password": crate::auth::MIN_PASSWORD,
    });
    match serde_json::to_string(&body) {
        Ok(b) => Res::json(200, b),
        Err(e) => error(500, &e.to_string()),
    }
}

/// Step one: spend the claim code and name the address.
fn api_spend_code(ctx: &mut Ctx<'_>, req: &Req) -> Res {
    let body: serde_json::Value = match serde_json::from_str(&req.body) {
        Ok(v) => v,
        Err(e) => return error(400, &format!("that body is not JSON: {e}")),
    };
    let code = body
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim();
    // **The address is no longer asked for here.** It is an environment variable
    // and the server refuses to start without a valid one, so by the time
    // anybody reaches this step it is already settled — and there is no typo
    // left that could burn the one claim code the operator has.

    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut admin = match ctx.store.admin() {
        Ok(a) => a,
        Err(e) => return error(500, &e.to_string()),
    };
    if admin.claimed() {
        return error(404, "no such endpoint");
    }
    let Some(setup_token) = admin.spend(code, ctx.now_ms) else {
        // **One message for every failure** — wrong, expired, already spent.
        // Distinguishing them tells a guesser which half they got right.
        return error(400, "that code was not accepted");
    };
    if let Err(e) = ctx.store.put_admin(&admin) {
        return error(500, &e.to_string());
    }

    drop(_guard);

    // The token binding the rest of the wizard to this browser. `Lax` rather
    // than `Strict` matches the cookie the pages set, and it is not a session:
    // it grants nothing once the server is claimed.
    //
    // **From the configured address**, because an unclaimed server has no public
    // surface and the usual answer is then `Secure` — which over plain HTTP
    // means the browser discards this token and the wizard loops back to step
    // one. The address is in the environment, so it is known here regardless.
    let secure = secure_attr_for(&configured_base(ctx));
    let mut res = Res::json(200, "{\"step\":\"admin\"}");
    res.set_cookie = Some(format!(
        "{SETUP_COOKIE}={setup_token}; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age={}",
        crate::admin::CLAIM_TTL_MS / 1000
    ));
    res
}

/// Step two: choose the credential that will own this server.
fn api_claim(ctx: &mut Ctx<'_>, req: &Req) -> Res {
    let body: serde_json::Value = match serde_json::from_str(&req.body) {
        Ok(v) => v,
        Err(e) => return error(400, &format!("that body is not JSON: {e}")),
    };
    let login = body
        .get("login")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim();
    let password = body
        .get("password")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut admin = match ctx.store.admin() {
        Ok(a) => a,
        Err(e) => return error(500, &e.to_string()),
    };
    if admin.claimed() {
        return error(404, "no such endpoint");
    }
    // **The step that hands the server over**, so it is bound to the browser
    // that spent the code. Without this, everything past step one is guarded
    // only by the server being unclaimed, and whoever arrives next sets their
    // own password and owns it.
    if !admin.setting_up(req.cookie_setup.as_deref(), ctx.now_ms) {
        return error(
            400,
            "start again from the claim code - setting this server up has to be finished in the browser that started it",
        );
    }
    if let Err(e) = check_login(login) {
        return error(400, &e);
    }

    let mut accounts = match ctx.store.accounts() {
        Ok(a) => a,
        Err(e) => return error(500, &e.to_string()),
    };
    let account = match accounts.create_login(login, password, ctx.now_ms) {
        Ok(a) => a,
        Err(e) => return error(400, &e),
    };
    let session = accounts.open_session(&account.id, ctx.now_ms);
    if let Err(e) = ctx.store.put_accounts(&accounts) {
        return error(500, &e.to_string());
    }
    invalidate_accounts(ctx);

    // **The account is written before the claim.** A server that recorded the
    // claim and then failed to store the account would be owned by a login
    // nobody can sign in as — unrecoverable without deleting the volume. This
    // ordering fails the other way: an unclaimed server with a spare account,
    // which the next attempt simply names differently.
    admin.claim(login, ctx.now_ms);
    if let Err(e) = ctx.store.put_admin(&admin) {
        return error(500, &e.to_string());
    }

    let mut settings = match ctx.store.settings() {
        Ok(s) => s,
        Err(e) => return error(500, &e.to_string()),
    };
    settings.seeded = true;
    if let Err(e) = ctx.store.put_settings(&settings) {
        return error(500, &e.to_string());
    }
    invalidate_settings(ctx);
    drop(_guard);

    crate::log::warn("server claimed")
        .with("login", login.to_ascii_lowercase())
        .with("note", "this account now administers this server")
        .emit();

    // Signed in already: they just proved themselves by choosing the credential,
    // and asking them to type it again immediately would be ceremony.
    //
    // **From the address, not from `ctx.public`.** A server being claimed has no
    // public surface yet, so the usual answer is `Secure` — and over plain HTTP
    // the browser discards the session, so the claim succeeds and the reader is
    // immediately signed out. The address decided this at step one.
    let secure = secure_attr_for(&configured_base(ctx));
    let mut res = Res::json(200, "{\"claimed\":true}");
    res.set_cookie = Some(format!(
        "{COOKIE}={session}; Path=/; HttpOnly; SameSite=Strict{secure}; Max-Age=31536000"
    ));
    res
}

/// Which administrative list is being asked for.
///
/// An enum rather than five near-identical handlers, so the admin gate and the
/// error handling are written once. Adding a list is a variant, not a copy of
/// the gate that might be copied wrong.
enum AdminView {
    Settings,
    Owners,
    Repos,
    Daemons,
    Accounts,
}

/// Ask for a magic link, from the interface.
///
/// **The answer is identical in every case** — unknown address, existing
/// account, revoked account, malformed input, over the outstanding cap. Only
/// what gets *sent* differs, so this cannot be used to discover whether an
/// address has an account. The form route this mirrors makes the same promise
/// and both call the same `try_send_link`, so neither can drift into leaking it.
fn api_request_link(ctx: &mut Ctx<'_>, req: &Req) -> Res {
    // **Refused outright when nothing can send.** Every other failure here is
    // deliberately indistinguishable; that argument does not reach this one,
    // because "this server has no mail provider" is not a fact about any person,
    // and silently accepting an address nobody will ever act on is worse.
    if !has_mail(ctx) {
        return error(503, req.locale().strings().signin_no_mail);
    }

    let body: serde_json::Value =
        serde_json::from_str(&req.body).unwrap_or(serde_json::Value::Null);
    let raw = body
        .get("email")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    if let Err(e) = try_send_link(ctx, raw) {
        // Logged for the operator, never shown: the answer must look the same
        // whether or not mail went out.
        //
        // `warn`, not `error`: the common cause is the outstanding-links cap,
        // which is the design working.
        crate::log::warn("sign-in link not sent")
            .text("err", e)
            .emit();
    }
    Res::json(200, "{\"sent\":true}")
}

/// Sign in with a password, from the interface.
///
/// The check itself is [`check_password_for_session`], shared with the form
/// route — see there for why it is one implementation rather than two.
fn api_sign_in_with_password(ctx: &mut Ctx<'_>, req: &Req) -> Res {
    let body: serde_json::Value =
        serde_json::from_str(&req.body).unwrap_or(serde_json::Value::Null);
    let login = body
        .get("login")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    let password = body
        .get("password")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let Ok(session) = check_password_for_session(ctx, req, &login, &password) else {
        // **One message for every failure**, matching what the form renders. A
        // client that could tell "no such login" from "wrong password" would be
        // an account enumerator.
        return error(401, req.locale().strings().signin_wrong);
    };

    let mut res = Res::json(200, "{\"signed_in\":true}");
    res.set_cookie = Some(session_cookie(ctx, &session));
    res
}

/// Sign out, from the interface.
///
/// **Revokes the session server-side**, not merely dropping the cookie: a token
/// that still opens a door is not signed out just because this browser forgot
/// it.
fn api_sign_out(ctx: &mut Ctx<'_>, req: &Req) -> Res {
    if let Some(token) = req.cookie_token.as_deref() {
        let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());
        if let Ok(mut accounts) = ctx.store.accounts() {
            let hashed = crate::auth::hash(token);
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
    let mut res = Res::json(200, "{\"signed_out\":true}");
    // Max-Age=0 so the browser drops it rather than carrying a dead token.
    res.set_cookie = Some(format!(
        "{COOKIE}=; Path=/; HttpOnly; SameSite=Strict{secure}; Max-Age=0"
    ));
    res
}

/// File a request, from the interface.
///
/// **Two callers, two sets of rules**, mirroring the split the form routes
/// already had between `POST /file` and `POST /public`.
///
/// A signed-in filer goes through [`file_now`]: the configured repositories, the
/// daily cap, the screener. An account costs a confirmed mailbox, and that is
/// the thing standing between this and an open pipe to somebody's model budget.
///
/// The administrator files against any repository with no cap, because the caps
/// bound what strangers can spend on somebody else's hardware and they own it.
///
/// An owner is answered 404: they have no account id to file against, and
/// inventing one would put work in the queue attributed to nobody.
fn api_file(ctx: &mut Ctx<'_>, req: &Req, caller: &Option<Caller>) -> Res {
    let body: serde_json::Value =
        serde_json::from_str(&req.body).unwrap_or(serde_json::Value::Null);
    let text = body
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let repo = body.get("repo").and_then(|v| v.as_str());
    let kind = body
        .get("kind")
        .and_then(|v| v.as_str())
        .and_then(IntakeKind::parse)
        .unwrap_or_default();

    // **The administrator files on their own machine, and the rules are not the
    // filer's.** No daily cap, no screening, and any repository rather than the
    // configured public set: the caps exist to bound what strangers can spend on
    // somebody else's hardware, and the person who owns the hardware is not a
    // stranger to it. This is the same split `POST /file` and `POST /public`
    // already had — two capabilities that happen to both produce a request.
    if let Some(Caller::Admin { .. }) = caller {
        let text = text.trim();
        if text.is_empty() {
            return error(400, "a request needs some text");
        }
        let Some(repo) = repo.map(str::trim).filter(|r| !r.is_empty()) else {
            return error(400, "choose which repository this is about");
        };
        if let Err(msg) = check_length(text) {
            return error(400, &msg);
        }

        let request = Request::new(new_id(), text, repo, kind);
        return match ctx.store.put(&request) {
            Ok(()) => match serde_json::to_string(&ReviewRequest::of(&request, true)) {
                Ok(b) => Res::json(200, b),
                Err(e) => error(500, &e.to_string()),
            },
            Err(e) => error(500, &e.to_string()),
        };
    }

    let Some(Caller::Account { id }) = caller else {
        return error(404, "no such endpoint");
    };
    let account_id = id.clone();

    match file_now(ctx, &account_id, repo, text, kind) {
        // The filer's own narrow view of what they just filed — the same type
        // `GET requests` gives them, so the client has one shape to render and
        // no field here that a filer may not see.
        Ok(request) => {
            let show_spec = ctx.public.is_some_and(|p| p.show_spec);
            match serde_json::to_string(&FiledRequest::of(&request, show_spec, req.locale())) {
                Ok(b) => Res::json(200, b),
                Err(e) => error(500, &e.to_string()),
            }
        }
        Err(refusal) => error(refusal.status(), &refusal.message(req.locale())),
    }
}

/// The request list, answered according to who is asking.
fn api_requests(ctx: &mut Ctx<'_>, req: &Req, caller: &Option<Caller>) -> Res {
    let all = match ctx.store.all() {
        Ok(a) => a,
        Err(e) => return error(500, &e.to_string()),
    };
    let body = match caller {
        // Everything, with the artifact paths — this is their own machine.
        Some(Caller::Admin { .. }) => {
            let list: Vec<_> = all
                .iter()
                .map(|r| ReviewRequest::with_coverage(r, true, Some(coverage_of(ctx, &r.repo))))
                .collect();
            serde_json::to_string(&list)
        }
        // **Only the repositories they own**, and the set is the one carried on
        // the variant rather than re-derived here. `Caller::Owner` pre-intersects
        // it with what this surface serves precisely so no call site has to.
        Some(Caller::Owner { repos, .. }) => {
            let list: Vec<_> = all
                .iter()
                .filter(|r| repos.iter().any(|owned| owned == &r.repo))
                .map(|r| ReviewRequest::with_coverage(r, false, Some(coverage_of(ctx, &r.repo))))
                .collect();
            serde_json::to_string(&list)
        }
        // Their own, narrowed. `show_spec` is the operator's decision about
        // whether a filer may read the spec drafted from their request.
        Some(Caller::Account { id }) => {
            let show_spec = ctx.public.is_some_and(|p| p.show_spec);
            let locale = req.locale();
            let list: Vec<_> = all
                .iter()
                .filter(|r| r.filed_by(id))
                .map(|r| FiledRequest::of(r, show_spec, locale))
                .collect();
            serde_json::to_string(&list)
        }
        // A stranger has no requests, and a daemon has no browser identity.
        // Empty rather than 401: the client asks this on a page a stranger may
        // legitimately be looking at.
        Some(Caller::Daemon { .. }) | None => Ok("[]".to_string()),
    };
    match body {
        Ok(b) => Res::json(200, b),
        Err(e) => error(500, &e.to_string()),
    }
}

/// One request, gated the way its page is.
fn api_request(ctx: &mut Ctx<'_>, req: &Req, caller: &Option<Caller>, id: &str) -> Res {
    let found = match ctx.store.get(id) {
        Ok(r) => r,
        Err(e) => return error(500, &e.to_string()),
    };
    // **Absent and forbidden are the same answer.** Whichever it is, the caller
    // learns only that they have nothing here.
    let Some(r) = found else {
        return error(404, "no such request");
    };
    let body = match caller {
        Some(Caller::Admin { .. }) => serde_json::to_string(&ReviewRequest::with_coverage(
            &r,
            true,
            Some(coverage_of(ctx, &r.repo)),
        )),
        Some(Caller::Owner { repos, .. }) if repos.iter().any(|owned| owned == &r.repo) => {
            serde_json::to_string(&ReviewRequest::with_coverage(
                &r,
                false,
                Some(coverage_of(ctx, &r.repo)),
            ))
        }
        Some(Caller::Account { id: account }) if r.filed_by(account) => {
            let show_spec = ctx.public.is_some_and(|p| p.show_spec);
            serde_json::to_string(&FiledRequest::of(&r, show_spec, req.locale()))
        }
        // An owner outside their repositories, a filer who did not file this, a
        // stranger, a daemon: not found.
        _ => return error(404, "no such request"),
    };
    match body {
        Ok(b) => Res::json(200, b),
        Err(e) => error(500, &e.to_string()),
    }
}

/// An administrative list. `Caller::Admin` or nothing.
fn api_admin(ctx: &mut Ctx<'_>, caller: &Option<Caller>, view: AdminView) -> Res {
    // The same gate the private surface uses, and the same answer: **404, not
    // 401**. The administrative surface does not exist for anybody else, and
    // saying "unauthorized" would tell a stranger the address is real.
    let Some(Caller::Admin { .. }) = caller else {
        return error(404, "no such endpoint");
    };
    let body = match view {
        AdminView::Settings => match ctx.store.settings() {
            // `SettingsView` renders presence and a date, never a secret — the
            // same rule the settings page follows. There is no read path for a
            // stored secret anywhere in this server and this does not add one.
            Ok(s) => serde_json::to_string(&SettingsView::of(&s)),
            Err(e) => return error(500, &e.to_string()),
        },
        AdminView::Owners => match ctx.store.roster() {
            Ok(r) => serde_json::to_string(&r.owners),
            Err(e) => return error(500, &e.to_string()),
        },
        AdminView::Repos => match ctx.store.roster() {
            Ok(r) => serde_json::to_string(&r.repos),
            Err(e) => return error(500, &e.to_string()),
        },
        AdminView::Daemons => match ctx.store.roster() {
            Ok(r) => serde_json::to_string(&r.daemons),
            Err(e) => return error(500, &e.to_string()),
        },
        AdminView::Accounts => match ctx.store.accounts() {
            // `AccountView` carries the hint, never `email_hash` and never the
            // password hash.
            Ok(a) => {
                let list: Vec<_> = a.accounts.iter().map(AccountView::of).collect();
                serde_json::to_string(&list)
            }
            Err(e) => return error(500, &e.to_string()),
        },
    };
    match body {
        Ok(b) => Res::json(200, b),
        Err(e) => error(500, &e.to_string()),
    }
}

/// Is this a path the single-page interface should answer with its document?
///
/// **An allowlist, not "anything that did not match".** A catch-all would turn
/// every mistyped API path into a 200 holding an HTML document, which is the
/// failure mode that makes a client's error handling useless — it can no longer
/// tell "no such request" from "here is the application again".
fn wants_document(path: &str) -> bool {
    path == public_route::LANDING
        || path == public_route::FILE
        || path == public_route::SIGNIN
        || path.starts_with(public_route::REQUEST_PREFIX)
        // The administrative addresses. **Answering the document here does not
        // grant anything**: every one of them is still a `Caller::Admin` check
        // at the API, and the interface draws its menu from what `/me` returned.
        // What this decides is only whether a reader who types `/settings` gets
        // the application or a 404 — and a 404 at an address that works when
        // navigated to from inside the application is the kind of inconsistency
        // that reads as a broken server.
        //
        // **This does change one thing, and it is worth naming.** The private
        // surface answers a stranger 404 today, and the reason is recorded: a
        // door they can see is worse than one they cannot, because it tells them
        // the addresses are real. Serving the document means a guessed address
        // returns 200.
        //
        // What that reasoning actually protects is the *naming* of the
        // addresses, and that survives: the menu is drawn from `/me`, so a
        // stranger is shown nothing, and the test asserting it still passes. A
        // guesser now learns an address is served — which the open-source
        // interface already tells them — and still learns nothing about whether
        // it holds anything, because every API call behind it 404s.
        || path == private_route::REVIEW
        || path == private_route::SETTINGS
        || path == private_route::REPOS
        || path == private_route::OWNERS
        || path == private_route::DAEMONS
        || path == private_route::ACCOUNTS
        || path.starts_with("/request/")
        // **The wizard.** Reachable with no credential, because it is how the
        // first one comes to exist. The document grants nothing: the endpoints
        // behind it still demand the claim code, and they 404 once the server is
        // claimed.
        || path == private_route::SETUP
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

/// Was this request's session proved with a password recently enough to change
/// a secret?
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

    // A login is required for anything above a filer. A magic-link account has
    // none and is an `Account` whatever any record says — so a claim naming an
    // email address grants nothing rather than escalating.
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

    // An account whose login **the roster names** is an
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
//
// **There is no `browser_route` any more.** Every administrative page — the
// review list, the accounts, owners, repositories and machines, the settings,
// one request's detail — was a rendered document with a form beside it, and the
// interface reaches all of them through `/api/v1/ui/` instead. The gate that
// made them administrator-only survives unchanged above: it is the
// `let Some(Caller::Admin { .. }) = caller else` in `handle_inner`, which is
// what made "an owner cannot accept" structural rather than a check inside each
// handler, and `api_write` states the same rule for the JSON side.
// ---------------------------------------------------------------------------

/// Whether anything currently offers to draft for `repo`, for the API.
///
/// The lock is recovered rather than propagated for the same reason
/// [`who_serves`] recovers it: this is a diagnostic hanging off a request, and
/// refusing the whole list because the hint is unavailable is the worse answer.
fn coverage_of(ctx: &Ctx<'_>, repo: &str) -> crate::daemons::Coverage {
    match ctx.seen.lock() {
        Ok(s) => s.coverage(repo, ctx.now_ms),
        Err(poisoned) => poisoned.into_inner().coverage(repo, ctx.now_ms),
    }
}

// ---------------------------------------------------------------------------
// The public surface
//
// A sibling of `browser_route`, not a caller of it. The review verbs live in
// that function and this one never reaches them — unreachable by structure
// rather than by a check somebody has to remember.
// ---------------------------------------------------------------------------

/// The public surface, reduced to the one thing on it that is not the client.
///
/// **Everything else here is gone.** The landing page, the filing form, a
/// filer's own request, an owner's queue and the verbs on it — all of them were
/// rendered documents with forms, and the interface reaches every one through
/// `/api/v1/ui/`. What is left is the set of addresses a browser can arrive at
/// holding nothing and *not* by way of the application: the magic-link landing,
/// the two faces the stylesheet names, and the language cookie.
fn public_route(
    ctx: &mut Ctx<'_>,
    req: &Req,
    method: &str,
    path: &str,
    _caller: &Option<Caller>,
) -> Res {
    // Decided once here rather than re-derived per response, so two answers to
    // one request cannot disagree about which language they are in.
    let locale = req.locale();

    match (method, path) {
        // Choosing a language. Signed out on purpose: somebody who cannot read
        // the page is exactly who needs this, and requiring an account first
        // would mean reading a page in a language they do not have.
        //
        // **Kept even though nothing on the interface posts to it.** It is the
        // only writer of the cookie `Req::locale` reads, and the magic-link
        // landing below is the only page left that reads it — so deleting this
        // would leave that reader with `Accept-Language` and no way to override
        // it. The interface wants a language control of its own; when it has
        // one, this is the endpoint it should reach for.
        ("POST", public_route::LANGUAGE) => set_language(ctx, req),

        // The landing page a link opens. **Changes nothing** — mail scanners
        // fetch every URL in a message, and a GET that spent the token would
        // burn it before the human saw it.
        ("GET", p) if p.starts_with(public_route::SIGNIN_PREFIX) => {
            let token = p.trim_start_matches(public_route::SIGNIN_PREFIX);
            // Rendered whether or not the token is real: a 404 on an invalid one
            // would be a free validity oracle, cheaper than the POST it guards.
            Res::html(200, signin_confirm_page(token, locale))
        }
        // **This must stay below any arm matching a longer path under the same
        // prefix.** `SIGNIN_PASSWORD` lived under `SIGNIN_PREFIX` and had to be
        // matched first, or a typed password reached `complete_sign_in` as if it
        // were a magic-link token — it would fail, and the sign-in the two named
        // roles depend on would simply stop working, with nothing in the logs
        // naming a cause. That form is gone to `/api/v1/ui/signin/password`,
        // which is dispatched before this function is reached at all, so the
        // collision no longer exists. The ordering rule is recorded because the
        // prefix is still a prefix: anything added under it wants the same care.
        // Pinned by `a_password_post_is_not_read_as_a_magic_link_token`.
        ("POST", p) if p.starts_with(public_route::SIGNIN_PREFIX) => {
            let token = p.trim_start_matches(public_route::SIGNIN_PREFIX);
            complete_sign_in(ctx, token, locale)
        }

        // **404, not a redirect to the application.** Every browser *document*
        // path is answered by the shell above this function; anything reaching
        // here is a method or an address this surface does not have, and the
        // honest answer is that there is nothing at it.
        _ => Res::html(404, crate::api::NOT_FOUND),
    }
}

/// The document a magic link lands on, and the two it can fail into.
///
/// **The one place this server still renders HTML from a request.** Everything
/// else a browser sees is the application shell plus JSON, which is the right
/// shape for a surface reached from inside itself. This is reached from
/// *outside* — a link in an email, in a mail client, possibly on a device that
/// has never seen this site — so it must be a real document that works on
/// arrival. Handing a `fetch`-shaped JSON body to that reader renders as text on
/// a white page.
///
/// Built from `concat!` of a doctype, an inline `<style>` and a body, following
/// [`crate::api::NOT_FOUND`]: no subresource, so nothing here can fail to load
/// and nothing depends on the interface bundle being reachable. `style-src`
/// allows `'unsafe-inline'` on every policy, so the block needs no relaxation.
mod link_page {
    /// The document, in three constant pieces with the two variable ones
    /// between them.
    ///
    /// Split in the *renderer* rather than formatted from a template held in the
    /// catalogue, for the reason [`crate::i18n`] gives about placeholders — but
    /// here the split costs nothing, because both values are this crate's own.
    pub const HEAD: &str = "<!doctype html><html lang=\"";
    pub const HEAD_2: &str = concat!(
        "\"><head><meta charset=\"utf-8\">",
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">",
        "<title>",
    );
    /// The stylesheet, inline. Deliberately the same handful of rules as the
    /// 404's: this page is read once, on the way to somewhere else, and a copy
    /// of the interface's design system would be a second stylesheet to keep in
    /// step for no reader's benefit.
    pub const STYLE: &str = concat!(
        "</title><style>body{font:16px/1.6 system-ui,sans-serif;margin:0;",
        "min-height:100vh;display:grid;place-items:center;text-align:center;",
        "background:#fbfaf8;color:#1a1a1a}main{max-width:32rem;padding:1.5rem}",
        "a{color:#3b5bdb}p{color:#555}",
        "button{font:inherit;font-weight:600;cursor:pointer;padding:.7rem 1.2rem;",
        "border:0;border-radius:.5rem;background:#3b5bdb;color:#fff}",
        "@media(prefers-color-scheme:dark){body{background:#16161a;color:#e8e6e3}",
        "a{color:#8da2fb}p{color:#a8a5a0}}</style></head><body><main>"
    );
    pub const TAIL: &str = "</main></body></html>";
}

/// Escape for HTML text content and attributes.
///
/// Applied to **everything** that did not come from this crate. The only value
/// reaching these pages from a request is the link token, which lands inside a
/// `form action` — so this is the whole of what stands between a crafted URL and
/// an attribute break.
///
/// The catalogue strings are escaped too rather than trusted. They are static
/// and this crate owns them, but it is
/// `no_catalogue_string_carries_markup_or_a_format_placeholder` that enforces
/// that, and a renderer escaping only *some* of its inputs is one where the next
/// string added is the exception.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Wrap a body in the document.
///
/// Carries the `lang` attribute, without which a screen reader pronounces French
/// with English phonemes — the accessibility failure translating a page at all is
/// meant to avoid rather than to cause.
fn link_document(locale: Locale, title: &str, body: &str) -> String {
    format!(
        "{head}{lang}{head2}{title}{style}{body}{tail}",
        head = link_page::HEAD,
        lang = esc(locale.code()),
        head2 = link_page::HEAD_2,
        title = esc(title),
        style = link_page::STYLE,
        body = body,
        tail = link_page::TAIL,
    )
}

/// Ask before spending the link.
///
/// **A button, not an automatic POST**, for the reason the GET arm gives: mail
/// scanners fetch every URL in a message, and a link spent by a scanner is one
/// the human never gets to use.
///
/// **Translated, unlike the 404.** A filer is a stranger who may not read
/// English, and this is the page standing between them and an account — so the
/// catalogue is consulted here even though the surface around it is the client's.
fn signin_confirm_page(token: &str, locale: Locale) -> String {
    let s = locale.strings();
    link_document(
        locale,
        s.confirm_title,
        &format!(
            "<h1>{title}</h1><p>{intro}</p>\
             <form method=\"post\" action=\"{prefix}{token}\">\
             <button type=\"submit\">{submit}</button></form>\
             <p>{note}</p>",
            title = esc(s.confirm_title),
            intro = esc(s.confirm_intro),
            prefix = public_route::SIGNIN_PREFIX,
            token = esc(token),
            submit = esc(s.confirm_submit),
            note = esc(s.confirm_not_you),
        ),
    )
}

/// A link that could not be spent.
///
/// "Invalid link" to somebody whose sign-in just worked reads as a bug, so a
/// second click is told apart from a forgery. That leaks only that a token once
/// existed — and it was theirs.
///
/// **200, not 400.** The status has never distinguished these: a failing status
/// on an invalid token is a validity oracle for anything reading the response
/// line, which is cheaper to script than reading the body.
fn signin_failed_page(already_used: bool, locale: Locale) -> String {
    let s = locale.strings();
    let body = if already_used {
        format!(
            "<p>{lead}<a href=\"{file}\">{link}</a>.</p>",
            lead = esc(s.link_already_used),
            file = public_route::FILE,
            link = esc(s.link_already_used_link),
        )
    } else {
        format!("<p>{}</p>", esc(s.link_expired))
    };
    link_document(
        locale,
        s.link_failed_title,
        &format!(
            "<h1>{title}</h1>{body}\
             <p><a href=\"{signin}\">{again}</a></p>",
            title = esc(s.link_failed_title),
            signin = public_route::SIGNIN,
            again = esc(s.link_ask_again),
        ),
    )
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
            return Res::html(200, signin_failed_page(true, locale))
        }
        Err(account::LinkError::Invalid) => {
            return Res::html(200, signin_failed_page(false, locale))
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
            return Res::html(200, signin_failed_page(false, locale));
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
                return Res::html(200, signin_failed_page(false, locale));
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

    // **The application, with the cookie on it.** This POST is a form submit from
    // the landing page, so the browser *navigates* — there is nothing here to
    // hand a JSON body to, and the reader must arrive somewhere usable. The shell
    // is what every other browser path answers, and the client asks `/me` on load
    // and finds the session this response just opened.
    //
    // `PublicScript` because the shell is the script: served `Strict`, the
    // browser would refuse the bundle the document it was just given asks for,
    // and a reader who signed in successfully would land on a blank page. It is
    // stamped here rather than inherited, because the dispatch above stamps the
    // *public surface's* policy and this response is reached on a server that may
    // have no public surface at all.
    let mut res = Res::html(200, crate::api::ui::INDEX).with_policy(Policy::PublicScript);
    res.set_cookie = Some(format!(
        "{COOKIE}={session}; Path=/; HttpOnly; SameSite=Strict{secure}; Max-Age=31536000"
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
        etag: None,
        hold_for_work: false,
        policy: Policy::PublicScript,
    }
}

/// The cookie that remembers a language choice.
///
/// **One builder for both routes that set it.** The form target and the JSON
/// endpoint write the same cookie with the same attributes, and a second copy of
/// this format string is how the two would eventually come to mean different
/// things — one `Lax`, one `Strict`, and a switcher that works on one surface.
///
/// Not `HttpOnly`: this is a preference, not a credential, and the interface may
/// read it. `SameSite=Lax` rather than `Strict` so that arriving from an
/// external link — which is how somebody reaches a filing page — still shows the
/// language they chose.
fn language_cookie(ctx: &Ctx<'_>, locale: Locale) -> String {
    format!(
        "{LANG_COOKIE}={}; Path=/; SameSite=Lax{}; Max-Age=31536000",
        locale.code(),
        secure_attr(ctx)
    )
}

/// Remember the reader's language, and answer the catalogue they chose.
///
/// **The endpoint the interface's switcher posts to.** Takes no session and
/// touches no store, exactly like the form route below it — this is safe to
/// leave reachable signed out because somebody who cannot read the current page
/// is precisely who needs it, and requiring an account first would mean signing
/// in through a page they cannot read.
///
/// It answers the **new catalogue**, not an acknowledgement. The client needs
/// the strings to redraw, and returning them here saves a second round trip on
/// the one interaction whose whole point is that the reader is waiting to
/// understand the page. The ETag comes with it, so the client stores the new
/// catalogue under its own key with a validator already attached.
fn api_set_language(ctx: &Ctx<'_>, req: &Req) -> Res {
    // An unknown code selects the default rather than erroring, matching the
    // form route: the value is matched against the catalogues this server
    // actually has, so nothing a caller writes reaches a page except by
    // selecting among them.
    let locale = serde_json::from_str::<serde_json::Value>(&req.body)
        .ok()
        .and_then(|v| v.get("lang")?.as_str().and_then(Locale::parse))
        .unwrap_or_default();

    // Built from the *chosen* locale rather than from `req.locale()`, which
    // still reads the old cookie — this request carries the one being replaced.
    let body = match serde_json::to_string(&crate::api::UiStrings::of(locale)) {
        Ok(body) => body,
        Err(e) => return error(500, &e.to_string()),
    };
    let mut res = Res::json(200, body);
    res.etag = Some(format!("\"{}\"", auth::hash(&res.body)));
    res.set_cookie = Some(language_cookie(ctx, locale));
    res
}

/// Remember the reader's language.
///
/// Takes no session and touches no store: this sets a preference cookie and
/// answers. It is the one public write that costs nothing to serve, which is why
/// it is safe to leave reachable signed out.
///
/// **There is no `next=` parameter and no redirect.** A "return to where you
/// were" field on a route reachable by anyone is an open redirect waiting to be
/// found, and this surface is small enough that landing back on the application
/// is no real loss.
///
/// **Kept beside the JSON endpoint above rather than replaced by it.** This is a
/// `<form>` target: it answers a document, because a form POST is a navigation
/// and the browser must be handed something to display. It is what a reader with
/// no script has, and it is the writer the magic-link landing depends on — that
/// page is server-rendered from the catalogue and has no client to ask.
fn set_language(ctx: &Ctx<'_>, req: &Req) -> Res {
    let fields = form_fields(&req.body);
    // An unknown code selects the default rather than erroring. The value is
    // matched against the catalogues this server actually has, so nothing a
    // caller writes here reaches a rendered page except by choosing among them.
    let locale = fields
        .get("lang")
        .and_then(|v| Locale::parse(v))
        .unwrap_or_default();

    // The application, for the reason `complete_sign_in` gives: a form POST is a
    // navigation, so the answer has to be something a browser can display.
    let mut res = Res::html(200, crate::api::ui::INDEX).with_policy(Policy::PublicScript);
    res.set_cookie = Some(language_cookie(ctx, locale));
    res
}

/// Can this server send a sign-in link?
///
/// Rendered on, so a surface with no provider says so rather than offering a
/// form that accepts an address and sends nothing.
fn has_mail(ctx: &Ctx<'_>) -> bool {
    ctx.public.is_some_and(|p| p.mail.is_some())
}

/// The routes that change a secret, and therefore need a fresh sign-in.
///
/// Named once so the test proving each one refuses a stale session iterates this
/// list rather than a copy that goes stale when a second is added. A route added
/// here without a `require_fresh` call fails that test, which is the safe
/// direction for the omission to fall.
pub const SENSITIVE_VERBS: [&str; 1] = [private_route::SETTINGS_SECRET];

/// Make the next request re-read the settings.
fn invalidate_settings(ctx: &Ctx<'_>) {
    if let Ok(mut cache) = ctx.settings.lock() {
        cache.invalidate();
    }
}

/// Is this a username this server will store?
///
/// **One function, two callers** — setup and `/owners` — so the rule cannot
/// hold in one place and not the other. It lands in a URL for revocation and in
/// every log line about this person, so it is kept to what reads back
/// unambiguously.
fn check_login(login: &str) -> std::result::Result<(), String> {
    // **An email address, not a username.** The two used to be separate, which
    // meant one person could hold a password account and a magic-link account
    // that nothing could reconcile. One address, one row, two ways to prove it
    // is yours \u2014 see [`crate::account::Accounts::create_login`] for what that
    // costs.
    if !account::valid_email(login) {
        return Err("that is not an email address".to_string());
    }
    Ok(())
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
/// Why a filing was refused, kept apart from how the refusal is rendered.
///
/// **The form and the JSON endpoint must refuse for the same reasons.** Each
/// variant carries its status, so neither caller decides one for itself — a
/// refusal that is 400 on one surface and 200 on the other is the kind of drift
/// that lets a client believe something was filed when it was not.
#[derive(Debug)]
enum FilingRefused {
    /// No public surface is configured. 404 rather than 403: it does not exist.
    NoSurface,
    /// The repository was absent where a choice was required, or not one of the
    /// configured set. Never falls back to a default — see [`file_now`].
    Repo,
    /// Empty. Rendered from the caller's locale rather than carrying a message,
    /// so this refusal is translated on both surfaces.
    Empty,
    /// Past the word ceiling. Carries the message, which names the limit.
    Text(String),
    /// Over the daily cap. Carries how many, because the message says so.
    TooMany(usize),
    /// The volume refused the write.
    Store(String),
}

impl FilingRefused {
    fn status(&self) -> u16 {
        match self {
            FilingRefused::NoSurface => 404,
            FilingRefused::Repo | FilingRefused::Empty | FilingRefused::Text(_) => 400,
            FilingRefused::TooMany(_) => 429,
            FilingRefused::Store(_) => 500,
        }
    }

    fn message(&self, locale: Locale) -> String {
        match self {
            FilingRefused::NoSurface => "no such page".to_string(),
            FilingRefused::Repo => locale.strings().file_repo_unknown.to_string(),
            FilingRefused::Empty => locale.strings().error_empty.to_string(),
            FilingRefused::Text(m) => m.clone(),
            FilingRefused::TooMany(n) => format!(
                "That is {n} requests in a day, which is the limit. Each one is \
                 written up by hand on someone's machine, so the cap is there to \
                 keep that manageable — try again tomorrow, or say the rest in a \
                 request you have already filed."
            ),
            FilingRefused::Store(e) => e.clone(),
        }
    }
}

/// File a request, or say why not. **Renders nothing.**
///
/// Split out so the form route and the JSON endpoint cannot drift. Everything
/// that decides whether a request is accepted lives here exactly once — the
/// repository check, the length ceiling, the daily cap and the lock the cap is
/// counted under — and the two callers differ only in what they render from the
/// answer.
fn file_now(
    ctx: &mut Ctx<'_>,
    account_id: &str,
    repo_field: Option<&str>,
    text: &str,
    kind: IntakeKind,
) -> std::result::Result<Request, FilingRefused> {
    let Some(public) = ctx.public else {
        return Err(FilingRefused::NoSurface);
    };
    let screened = public.screen.is_some();

    // **Checked against the configured set, never trusted.** The picker renders
    // from that same set, so an honest filer always sends one of these; anything
    // else was hand-crafted, and the answer is to refuse rather than to fall
    // back on a default.
    //
    // Falling back would file the request against a repository the filer did not
    // choose, and nothing would say so — the work would simply land somewhere
    // else. A refusal is the honest failure.
    //
    // A surface serving one repository renders no field, so an absent name is
    // normal there and takes the only one. With several, absent means the form
    // was not the thing that sent this. With **none**, `first()` is `None` and
    // this refuses, which is right: there is nothing to file against.
    let repo = match repo_field.map(str::trim) {
        None | Some("") if public.repos.is_single() => match public.repos.first() {
            Some(only) => only.to_string(),
            None => return Err(FilingRefused::Repo),
        },
        Some(named) if public.repos.accepts(named) => named.to_string(),
        _ => return Err(FilingRefused::Repo),
    };

    let text = text.trim();
    if text.is_empty() {
        return Err(FilingRefused::Empty);
    }
    if let Err(msg) = check_length(text) {
        return Err(FilingRefused::Text(msg));
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
        Ok(n) if n >= public.max_daily_filings => return Err(FilingRefused::TooMany(n)),
        Ok(_) => {}
        Err(e) => return Err(FilingRefused::Store(e.to_string())),
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
        Ok(()) => Ok(request),
        Err(e) => Err(FilingRefused::Store(e.to_string())),
    }
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
            });
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
                fresh_auth: false,
                rechecking: true,
            };
            handle(&mut ctx, req)
        }

        /// Sign in as a filer, returning the session cookie.
        /// Sign in as somebody who holds a username, returning the session
        /// cookie.
        ///
        /// Builds the account directly because the OAuth routes do not exist
        /// yet — this is precisely what their callback will do, so the *rest*
        /// of the owner behaviour can be proven before the flow that produces
        /// it. Whether the login is an owner still comes from the configuration,
        /// which is the property under test.
        fn signed_in_with_login(&mut self, login: &str) -> String {
            // **Through `create_login`**, the same path `/setup` and `/owners`
            // use, rather than assembling an `Account` by hand. A fixture that
            // built its own would keep passing after the real constructor grew a
            // rule — the uniqueness check is exactly such a rule, and it went in
            // during this change.
            let mut accounts = self.store.accounts().unwrap();
            let account = accounts
                .create_login(login, "fixture-password", self.now_ms)
                .expect("the fixture picks unused logins");
            let session = accounts.open_session(&account.id, self.now_ms);
            self.store.put_accounts(&accounts).unwrap();
            session
        }

        /// Sign in the way a filer does: ask for a link, then spend it.
        ///
        /// **Two different surfaces, deliberately.** Asking is the interface's
        /// endpoint, because that is where a filer types their address now. The
        /// spend is still the form POST on `/public/signin/<token>`, because
        /// that is the *one* rendered page left — it is reached from an email,
        /// so it has to be a document with a button rather than a `fetch`.
        fn signed_in(&mut self, email: &str) -> String {
            let asked = self.go(&Req::post_json(
                &api_path("signin"),
                &serde_json::json!({ "email": email }).to_string(),
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

    /// Act on a request through the endpoint the interface posts verbs to.
    ///
    /// **One address for every reviewer now, where there were two.** The rendered
    /// surface had the developer's verbs on `/request/{id}/{verb}` and the
    /// owner's on `/public/request/{id}/{verb}`, because owners live on the
    /// public side and the private surface was gated on a variant they do not
    /// have. `api_verb` matches on the caller's variant instead, so the split
    /// that used to be two route trees is a `match` — and the *answers* are
    /// unchanged, which is what the tests below still assert: an owner outside
    /// their repositories gets 404, and `accept` does not exist for them.
    fn verb_on(f: &mut Fixture, session: &str, id: &str, verb: &str, body: &str) -> Res {
        f.go(
            &Req::post_json(&api_path(&format!("requests/{id}/{verb}")), body).with_cookie(session),
        )
    }

    /// Sign in with a password, through the endpoint the interface posts to.
    ///
    /// **The form at `/public/signin/password` is gone and the check behind it is
    /// not.** `check_password_for_session` was split out so a form and a `fetch`
    /// could not drift apart on the backoff or the cookie flags; only one caller
    /// is left, and everything those tests assert about backoff, indistinguishable
    /// refusals and session flags is asserted here against it.
    fn sign_in_with_password(f: &mut Fixture, login: &str, password: &str) -> Res {
        f.go(&Req::post_json(
            &api_path("signin/password"),
            &serde_json::json!({ "login": login, "password": password }).to_string(),
        ))
    }

    /// Ask for a sign-in link and return the token out of the emailed body.
    ///
    /// **Asking moved to the interface's endpoint and spending did not.** The
    /// link lands on `/public/signin/<token>`, which is still a rendered page —
    /// it is reached by a navigation out of an email, so it has to be a document
    /// with a button rather than a `fetch`. This is the seam between the two, and
    /// it is written once because four tests cross it.
    fn link_token_for(f: &mut Fixture, email: &str) -> String {
        let asked = f.go(&Req::post_json(
            &api_path("signin"),
            &serde_json::json!({ "email": email }).to_string(),
        ));
        assert_eq!(asked.status, 200, "{}", asked.body);
        let body = f.mailer.last_body().expect("a link was emailed");
        body.split(public_route::SIGNIN_PREFIX)
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .expect("the body carries a link")
            .to_string()
    }

    /// Mint a machine key through the endpoint the machines page uses, and
    /// return the key it hands back exactly once.
    ///
    /// **The key used to come out of `<pre>` in the rendered page.** It comes out
    /// of a JSON field now, and pulling that apart at four call sites would put
    /// four copies of "which field carries the secret" in the suite — a detail
    /// that will move again before the shape of these tests does.
    fn mint_daemon(f: &mut Fixture, admin: &str, label: &str) -> String {
        let res = f.go(&Req::post_json(
            &api_path("daemons"),
            &serde_json::json!({ "label": label }).to_string(),
        )
        .with_cookie(admin));
        assert_eq!(res.status, 200, "{}", res.body);
        let minted: serde_json::Value = serde_json::from_str(&res.body).unwrap();
        minted["key"]
            .as_str()
            .expect("minting hands the key back")
            .to_string()
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
            admin.claim("jamez667@example.test", self.now_ms);
            self.store.put_admin(&admin).unwrap();
            self.signed_in_with_login("jamez667@example.test")
        }

        /// File as the administrator, through the endpoint the interface uses.
        ///
        /// **The text arrives percent-encoded**, because every caller of this
        /// was written against `POST /file` and passes `add+a+health+check`.
        /// Decoding here rather than editing sixty call sites keeps the change
        /// mechanical — and what those tests are about is what happens *after* a
        /// request exists, not how the plus signs got in.
        fn file(&mut self, token: &str, text: &str, repo: &str) -> String {
            let body = serde_json::json!({
                "text": percent_decode(text),
                "repo": repo,
                "kind": "feature",
            });
            let res =
                self.go(&Req::post_json(&api_path("file"), &body.to_string()).with_cookie(token));
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
    fn a_request_nothing_serves_says_so_rather_than_waiting_silently() {
        // **The one diagnostic that answers "why has nothing happened to this."**
        // Three states, and they send an operator to three different places:
        // start a daemon, fix a repository name, or wait.
        //
        // This nearly disappeared. The reasoning used to be built in the rendered
        // page from the poll record, and when the interface moved to JSON the DTO
        // had no field to carry it — so this test briefly had no subject at all.
        // The answer was to give it one rather than to lose the diagnostic.
        let mut f = Fixture::new("coverage-in-the-api");
        let token = f.as_admin();
        let id = f.file(&token, "something", "alpha");

        let coverage = |f: &mut Fixture, token: &str| -> String {
            let res = f.go(&Req::get(&format!("/api/v1/ui/requests/{id}")).with_cookie(token));
            assert_eq!(res.status, 200);
            let v: serde_json::Value = serde_json::from_str(&res.body).unwrap();
            v["coverage"].as_str().unwrap_or("absent").to_string()
        };

        // Nothing has ever polled: the operator needs to *start* a daemon, which
        // is a different problem from the one below.
        assert_eq!(coverage(&mut f, &token), "no-daemon-seen");

        // A daemon polls, but offers a repository this request is not for. This
        // is the way the system most often wedges — a name that does not match
        // what `queue add-repo` was given — and it must not read as "no daemon".
        f.go(&Req::get(&format!("{}?repo=beta", wire::route::WORK)).with_bearer(KEY));
        assert_eq!(coverage(&mut f, &token), "unserved");

        // And once something offers it, the request is merely waiting its turn.
        f.go(&Req::get(&format!("{}?repo=alpha", wire::route::WORK)).with_bearer(KEY));
        assert_eq!(coverage(&mut f, &token), "served");
    }

    #[test]
    fn signing_in_over_json_sets_the_same_session_as_the_form() {
        // The interface posts JSON; the magic-link email still lands on a form.
        // **Both must open the same kind of session** — a second implementation
        // is where the backoff, the cookie flags, or the session refresh quietly
        // stop matching, which is why `check_password_for_session` is shared.
        let mut f = Fixture::new("json-signin");
        f.as_admin();

        let res = f.go(&Req::post_json(
            "/api/v1/ui/signin/password",
            r#"{"login":"jamez667@example.test","password":"fixture-password"}"#,
        ));
        assert_eq!(res.status, 200, "{}", res.body);

        let cookie = res.set_cookie.clone().expect("a session");
        assert!(cookie.contains("HttpOnly"), "not readable from script");
        assert!(
            cookie.contains("SameSite=Strict"),
            "the GitHub-era Lax relaxation is gone and must not come back"
        );

        // And it actually signs somebody in, rather than merely answering 200.
        let token = cookie_token(&res).expect("a session was opened");
        let me = f.go(&Req::get("/api/v1/ui/me").with_cookie(&token));
        assert!(
            me.body.contains("administrator"),
            "the session the JSON endpoint opened is a real one: {}",
            me.body
        );
    }

    #[test]
    fn a_refused_password_says_the_same_thing_however_it_was_wrong() {
        // **The refusal must not be an account enumerator.** A client that could
        // tell "no such login" from "wrong password" would let somebody map who
        // has an account here, which is what the form route refuses to do — so
        // the JSON one has to refuse identically.
        let mut f = Fixture::new("json-signin-refused");
        f.as_admin();

        let wrong_password = f.go(&Req::post_json(
            "/api/v1/ui/signin/password",
            r#"{"login":"jamez667@example.test","password":"not it"}"#,
        ));
        let no_such_login = f.go(&Req::post_json(
            "/api/v1/ui/signin/password",
            r#"{"login":"nobody@example.test","password":"not it"}"#,
        ));

        assert_eq!(wrong_password.status, 401);
        assert_eq!(no_such_login.status, 401);
        assert_eq!(
            wrong_password.body, no_such_login.body,
            "these two must be indistinguishable"
        );
        assert!(
            wrong_password.set_cookie.is_none(),
            "a refusal hands out nothing"
        );
    }

    #[test]
    fn signing_out_over_json_revokes_rather_than_forgetting() {
        // **Dropping the cookie is not signing out.** A token this browser
        // forgot but the server still honours is one anybody holding it can
        // still use, so the session has to die server-side.
        let mut f = Fixture::new("json-signout").with_public(false);
        let session = f.signed_in("filer@example.test");

        let res = f.go(&Req::post_json("/api/v1/ui/signout", "{}").with_cookie(&session));
        assert_eq!(res.status, 200, "{}", res.body);
        assert!(
            res.set_cookie
                .as_deref()
                .unwrap_or_default()
                .contains("Max-Age=0"),
            "the browser is told to drop it too"
        );

        // The old token is now worth nothing, which is the part that matters.
        let after = f.go(&Req::get("/api/v1/ui/me").with_cookie(&session));
        assert!(
            after.body.contains("anonymous"),
            "the revoked session must not still identify anybody: {}",
            after.body
        );
    }

    #[test]
    fn asking_for_a_link_over_json_cannot_be_used_to_find_accounts() {
        // The form route answers identically whatever happened, so that probing
        // addresses reveals nothing. **The JSON endpoint inherits that promise**
        // — and a client is a much easier thing to probe with than a form.
        let mut f = Fixture::new("json-link").with_public(false);
        f.signed_in("known@example.test");

        let known = f.go(&Req::post_json(
            "/api/v1/ui/signin",
            r#"{"email":"known@example.test"}"#,
        ));
        let unknown = f.go(&Req::post_json(
            "/api/v1/ui/signin",
            r#"{"email":"stranger@example.test"}"#,
        ));

        assert_eq!(known.status, unknown.status);
        assert_eq!(
            known.body, unknown.body,
            "whether an address has an account must not be readable from here"
        );
    }

    #[test]
    fn a_filer_can_file_through_the_json_api() {
        // The interface had no way to file at all: `POST /public` took a form
        // and the client rendered none, so the public surface was a read-only
        // page that invited people to say what they needed. This is that gap.
        let mut f = Fixture::new("json-file").with_public(false);
        let session = f.signed_in("filer@example.test");

        let res = f.go(&Req::post_json(
            "/api/v1/ui/file",
            r#"{"text":"the export button does nothing","kind":"bug"}"#,
        )
        .with_cookie(&session));
        assert_eq!(res.status, 200, "{}", res.body);

        // It really landed, rather than merely answering 200.
        let stored = f.store.all().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].text, "the export button does nothing");

        // **The filer gets their own narrow view back.** Not the reviewer's:
        // no repository, no artifact directory, no daemon note — the same type
        // `GET requests` gives them, so there is nothing here to leak.
        assert!(!res.body.contains("artifact_dir"), "{}", res.body);
        assert!(!res.body.contains("\"repo\""), "{}", res.body);
    }

    #[test]
    fn filing_over_json_refuses_a_repository_this_surface_does_not_serve() {
        // **Never falls back to a default.** Filing against a repository nobody
        // chose would land the work somewhere else with nothing saying so, so
        // the honest answer is a refusal — and the JSON endpoint must give the
        // same one the form does, since it shares `file_now`.
        let mut f = Fixture::new("json-file-repo").with_public(false);
        let session = f.signed_in("filer@example.test");

        let res = f.go(&Req::post_json(
            "/api/v1/ui/file",
            r#"{"text":"something","kind":"bug","repo":"not-served"}"#,
        )
        .with_cookie(&session));
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(f.store.all().unwrap().is_empty(), "nothing was written");
    }

    #[test]
    fn the_administrator_files_over_json_without_the_filers_limits() {
        // **Two capabilities that both produce a request**, which is the split
        // `POST /file` and `POST /public` already had. The caps bound what
        // strangers can spend on somebody else's hardware; the person who owns
        // the hardware is not a stranger to it.
        //
        // The tight cap here is the point: a filer would be refused on the
        // fourth, and the administrator is not.
        let mut f = Fixture::new("json-file-admin")
            .with_public(false)
            .with_caps(1, 20);
        let admin = f.as_admin();

        for n in 0..3 {
            let res = f.go(&Req::post_json(
                "/api/v1/ui/file",
                &format!(r#"{{"text":"admin request {n}","repo":"anything-at-all","kind":"bug"}}"#),
            )
            .with_cookie(&admin));
            assert_eq!(res.status, 200, "filing {n}: {}", res.body);
        }
        assert_eq!(f.store.all().unwrap().len(), 3, "the cap did not apply");

        // Any repository, not the configured public set — an administrator
        // files against their own machine's checkouts.
        assert_eq!(f.store.all().unwrap()[0].repo, "anything-at-all");

        // But a repository is still required: there is no single default to
        // fall back on, and guessing would put work somewhere nobody chose.
        let no_repo = f.go(&Req::post_json(
            "/api/v1/ui/file",
            r#"{"text":"where does this go","kind":"bug"}"#,
        )
        .with_cookie(&admin));
        assert_eq!(no_repo.status, 400, "{}", no_repo.body);
    }

    #[test]
    fn filing_over_json_needs_an_account() {
        // An account costs a confirmed mailbox, and that is the thing standing
        // between this endpoint and an open pipe to somebody's model budget.
        // **404, not 401**: the endpoint does not exist for a stranger.
        let mut f = Fixture::new("json-file-anon").with_public(false);

        let res = f.go(&Req::post_json(
            "/api/v1/ui/file",
            r#"{"text":"something","kind":"bug"}"#,
        ));
        assert_eq!(res.status, 404, "{}", res.body);
        assert!(f.store.all().unwrap().is_empty(), "nothing was written");
    }

    #[test]
    fn filing_over_json_is_capped_the_same_as_the_form() {
        // Every filing that clears the screener costs a full drafting run, so
        // the daily cap is the real ceiling rather than the rate limiter. It
        // lives in `file_now`, which is why a second filing route cannot slip
        // past it — this is the test that would fail if one ever did.
        let mut f = Fixture::new("json-file-cap")
            .with_public(false)
            .with_caps(3, 20);
        let session = f.signed_in("filer@example.test");

        let cap = 3;
        for n in 0..cap {
            let res = f.go(&Req::post_json(
                "/api/v1/ui/file",
                &format!(r#"{{"text":"request number {n}","kind":"bug"}}"#),
            )
            .with_cookie(&session));
            assert_eq!(res.status, 200, "filing {n} of {cap}: {}", res.body);
        }

        let over = f.go(&Req::post_json(
            "/api/v1/ui/file",
            r#"{"text":"one too many","kind":"bug"}"#,
        )
        .with_cookie(&session));
        assert_eq!(over.status, 429, "{}", over.body);
        assert_eq!(
            f.store.all().unwrap().len(),
            cap,
            "the one over the cap was not written"
        );
    }

    #[test]
    fn the_client_knows_exactly_the_paths_the_server_serves_a_document_for() {
        // **Two lists that must agree, in two languages.** The server decides
        // which addresses get the bundle; the client decides what to draw once
        // it has it. A path the server serves and the client does not know is a
        // working address rendering "Not found"; a path the client claims and
        // the server does not is one that can never arrive.
        //
        // Read out of the bundle rather than restated here, so this fails when
        // somebody edits one side alone — which is the whole failure mode. The
        // assertion is deliberately crude: it checks each path is *compared
        // against*, not that the expression means what it says. A test that
        // parsed TypeScript would be a second implementation of the thing it
        // checks.
        //
        // **`===` is load-bearing.** The first version looked for the quoted
        // path alone and passed while the client was missing one entirely:
        // `"daemons"` appears in the bundle as an API path segment, so the
        // literal was always found and the test asserted nothing. Minification
        // rewrites the comparison to `B==="/daemons"` but keeps the operator, so
        // pinning to that is what makes this a real check. Verified by deleting
        // a path from the client and watching this fail.
        let client = crate::api::ui::SCRIPT;

        for path in [
            public_route::LANDING,
            public_route::FILE,
            public_route::SIGNIN,
            private_route::REVIEW,
            private_route::SETTINGS,
            private_route::REPOS,
            private_route::OWNERS,
            private_route::DAEMONS,
            private_route::ACCOUNTS,
            private_route::SETUP,
        ] {
            assert!(
                wants_document(path),
                "{path} should be served the interface"
            );
            assert!(
                client.contains(&format!("===\"{path}\"")),
                "the client does not know {path}, so it will draw Not found there"
            );
        }

        // **The magic-link landing is deliberately not among them.** It is a
        // navigation out of an email that the server still renders itself, so
        // the client must not claim it — see `wants_document`.
        assert!(
            !wants_document("/public/signin/sometoken"),
            "the magic-link landing is server-rendered"
        );
    }

    #[test]
    fn a_language_can_be_read_before_the_server_has_an_address() {
        // **The switcher has to work on a server nobody has configured yet.**
        // Choosing a language is a mutating call, and `same_origin` refuses all
        // of those when there is no configured address to compare an `Origin`
        // against — so on a fresh deployment the POST always fails and the
        // picker moved with nothing happening.
        //
        // Reading the catalogue in a named language is the way out: it changes
        // nothing and reveals nothing, because the strings are compiled in and
        // identical for every caller.
        let mut f = Fixture::new("lang-before-address");

        // The POST really is refused here — if this ever starts passing, the
        // fallback below is no longer needed and this test should say so rather
        // than quietly testing a path nobody takes.
        let mut choose = api_post("/api/v1/ui/language", r#"{"lang":"fr"}"#);
        choose.origin = Some("http://somewhere.example".into());
        let chosen = f.go(&choose);
        assert_eq!(chosen.status, 403, "{}", chosen.body);

        // But the read is answered, in the language asked for.
        let read = f.go(&Req::get("/api/v1/ui/strings?lang=fr"));
        assert_eq!(read.status, 200, "{}", read.body);
        let body: serde_json::Value = serde_json::from_str(&read.body).unwrap();
        assert_eq!(body["locale"], "fr");

        // And an unknown language falls back to negotiation rather than
        // failing: a client asking for something this server has no catalogue
        // for should get a page, not an error.
        let unknown = f.go(&Req::get("/api/v1/ui/strings?lang=de"));
        assert_eq!(unknown.status, 200, "{}", unknown.body);
        let body: serde_json::Value = serde_json::from_str(&unknown.body).unwrap();
        assert_eq!(body["locale"], "en");
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

    // **`a_request_nothing_serves_says_so_rather_than_waiting_silently` was
    // here, and is deleted rather than retargeted.** It asserted the three
    // diagnostics a request's page draws when no daemon serves its repository —
    // "No daemon has connected", "No connected daemon serves <repo>", and the
    // `queue serve` / `add-repo` fixes each names. That reasoning is built in
    // `page::private` from the poll record, and there is no field on
    // `api::ReviewRequest` carrying any of it: the JSON says what state a
    // request is in, not why nothing is moving it. There was nothing on the API
    // to point the test at.
    //
    // **Worth knowing: nothing else covers those strings.** They live only in
    // `page::private`, which has no test of its own for them, so deleting this
    // left them unasserted. If the interface grows an equivalent — a reason a
    // request is stuck, rather than only its state — it wants a test here.

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

        // **Read from `/me` rather than the page.** Every browser path answers
        // the interface's document now, signed in or not, so "are they still
        // signed in" is a question only the API answers — and it is the same
        // question the client asks before it draws anything.
        let before = f.go(&Req::get(ME_PATH).with_cookie(&session));
        assert!(before.body.contains("\"filer\""), "{}", before.body);

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
        // **Through the API's revoke**, because the form route is gone. The lever
        // is the same one — `api_revoke` calls `invalidate_accounts` at the same
        // point the page's handler did — so what this pins is unchanged: the
        // write, and the cache being dropped with it.
        let res = f.go(
            &Req::post_json(&api_path(&format!("accounts/{id}/revoke")), "{}").with_cookie(&admin),
        );
        assert_eq!(res.status, 200, "{}", res.body);

        let after = f.go(&Req::get(ME_PATH).with_cookie(&session));
        assert!(
            after.body.contains("\"anonymous\""),
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
        // **Asked of `/me`, not of the filing address.** Every browser path
        // answers the interface's document whether or not anybody is signed in,
        // so a 200 there says nothing at all; whether the session resolves to a
        // live account is a question only the API answers.
        let mut f = Fixture::new("signup-cached").with_public(false);
        let session = f.signed_in("new@x.com");
        let me = f.go(&Req::get(ME_PATH).with_cookie(&session));
        assert!(
            me.body.contains("\"filer\""),
            "a session opened this instant did not work: {}",
            me.body
        );
    }

    #[test]
    fn a_minted_key_is_shown_once_and_then_only_its_hash_is_kept() {
        // **The one place this server prints a secret.** Handed back by the call
        // that made it and never again, which is strictly better than an
        // environment variable sitting in a stack editor for the life of the
        // deployment.
        //
        // **The warning moved and the property did not.** The page used to carry
        // "this is the only time it is shown" beside the key; the client draws
        // that line now, so there is no markup here to look for. What still has to
        // be true — and is the whole reason the warning exists — is that the key
        // is in the minting response and in *no* later read: the list endpoint
        // behind the machines page returns records, and a record holds a hash.
        let mut f = Fixture::new("daemon-mint").with_public(false);
        let admin = f.as_admin();

        let res = f
            .go(&Req::post_json(&api_path("daemons"), r#"{"label":"laptop"}"#).with_cookie(&admin));
        assert_eq!(res.status, 200, "{}", res.body);
        let minted: serde_json::Value = serde_json::from_str(&res.body).unwrap();
        let key = minted["key"]
            .as_str()
            .expect("the key comes back")
            .to_string();

        let roster = f.store.roster().unwrap();
        let record = roster
            .daemons
            .iter()
            .find(|d| d.label == "laptop")
            .expect("minted");
        assert!(!record.revoked);
        assert!(
            !res.body.contains(&record.key_hash),
            "the hash is not handed out beside the key"
        );

        // Reading the list back does not show it again — which is the assertion
        // the "only time it is shown" line was standing in for.
        let again = f.go(&Req::get(&api_path("daemons")).with_cookie(&admin));
        assert_eq!(again.status, 200, "{}", again.body);
        assert!(
            !again.body.contains(&key),
            "the key was readable a second time: {}",
            again.body
        );

        // And nothing reversible reached the volume.
        let raw = std::fs::read_to_string(f.store.roster_path()).unwrap();
        assert!(raw.contains(&record.key_hash), "the hash is stored");
        assert!(!raw.contains(&key), "the key itself never lands on disk");
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

        let key = mint_daemon(&mut f, &admin, "laptop");

        assert_eq!(
            f.go(&Req::get(wire::route::WORK).with_bearer(&key)).status,
            200,
            "a freshly minted key could not claim"
        );

        let res =
            f.go(&Req::post_json(&api_path("daemons/laptop/revoke"), "{}").with_cookie(&admin));
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

        let first = mint_daemon(&mut f, &admin, "laptop");
        let second = mint_daemon(&mut f, &admin, "laptop");
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
        //
        // **Asked of the API, not the page.** `/daemons` answers the interface's
        // document to anybody; what an owner cannot get is the machine list
        // behind it, which is where the gate now lives.
        let (mut f, session, _mine, _theirs) = owner_fixture("daemon-owner");
        assert_eq!(
            f.go(&Req::get(&api_path("daemons")).with_cookie(&session))
                .status,
            404
        );
        // The writes too, and **404 rather than 401**: minting does not exist
        // for an owner, which is the same answer the list gives.
        for (rest, body) in [
            ("daemons", r#"{"label":"theirs"}"#),
            ("daemons/laptop/revoke", "{}"),
        ] {
            let res = f.go(&Req::post_json(&api_path(rest), body).with_cookie(&session));
            assert_eq!(res.status, 404, "an owner reached {rest}: {}", res.body);
        }
        assert!(
            f.store.roster().unwrap().daemons.is_empty(),
            "an owner minted a machine key"
        );
    }

    #[test]
    fn a_machine_name_that_would_not_read_back_is_refused() {
        // The label lands in a revocation URL and in every log line about this
        // machine, so it is kept to what reads back unambiguously.
        let mut f = Fixture::new("daemon-label").with_public(false);
        let admin = f.as_admin();
        for bad in ["", "has spaces", "slash/es", "../..", &"x".repeat(65)] {
            let res = f.go(&Req::post_json(
                &api_path("daemons"),
                &serde_json::json!({ "label": bad }).to_string(),
            )
            .with_cookie(&admin));
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
        // **The refusal is all that is left to assert here, and it is the half
        // that mattered.** The sign-in address answers the interface's document
        // like every other browser path, so there is no longer a rendered
        // "cannot send" line or an email form to look for in the markup — the
        // client draws both. What must not change is what happens when somebody
        // submits an address anyway: refused outright, and no link minted. That
        // now happens at `POST /api/v1/ui/signin`, which is where the address a
        // reader types arrives; `api_request_link` makes the check its first act
        // for exactly this reason.
        let mut f = Fixture::new("no-mail").with_public(false);
        if let Some(p) = f.public.as_mut() {
            p.mail = None;
        }

        let res = f.go(&Req::post_json(
            &api_path("signin"),
            r#"{"email":"jo@x.com"}"#,
        ));
        assert_eq!(res.status, 503, "{}", res.body);
        assert!(
            f.store.links().unwrap().links.is_empty(),
            "a link was minted"
        );

        // And with a provider configured the same POST is accepted, so the 503
        // above is the missing provider rather than the endpoint being shut.
        let mut f = Fixture::new("with-mail").with_public(false);
        let res = f.go(&Req::post_json(
            &api_path("signin"),
            r#"{"email":"jo@x.com"}"#,
        ));
        assert_eq!(res.status, 200, "{}", res.body);
        assert_eq!(f.mailer.count(), 1, "and the link was actually sent");
    }

    #[test]
    fn the_public_surface_is_turned_on_from_the_settings_api() {
        // The switch that used to be an environment variable. A freshly claimed
        // server has it off, and turning it on is deliberate rather than a side
        // effect of naming a repository.
        //
        // **Renamed off "page", and the switch is now stated rather than
        // toggled.** `POST /settings/public` flipped whatever was there, because
        // an unticked checkbox submits nothing and a form had no way to say
        // "off"; JSON does, so the endpoint takes the value the reader chose. The
        // property is the same one either way — the setting is a deliberate write
        // that lands on the volume, and it can be turned back off — and stating
        // it is strictly the safer of the two, since a toggle applied twice by a
        // retried request silently undoes itself.
        let mut f = Fixture::new("settings-public").with_public(false);
        let admin = f.as_admin();

        // The fixture's `with_public` writes the *config*, which is only a seed
        // now; the volume decides.
        assert!(!f.store.settings().unwrap().public);

        let res =
            f.go(&Req::post_json(&api_path("settings"), r#"{"public":true}"#).with_cookie(&admin));
        assert_eq!(res.status, 200, "{}", res.body);
        assert!(f.store.settings().unwrap().public);

        // And it can be turned off again, which is the half a checkbox could not
        // express.
        let res =
            f.go(&Req::post_json(&api_path("settings"), r#"{"public":false}"#).with_cookie(&admin));
        assert_eq!(res.status, 200, "{}", res.body);
        assert!(!f.store.settings().unwrap().public);
    }

    #[test]
    fn a_cap_can_be_raised_without_a_restart() {
        // The point of moving these off the environment: a ceiling raised to
        // stop refusing filings is worth nothing if it waits for a redeploy.
        let mut f = Fixture::new("settings-caps").with_public(false);
        let admin = f.as_admin();

        let res = f.go(
            &Req::post_json(&api_path("settings"), r#"{"max_daily_filings":7}"#)
                .with_cookie(&admin),
        );
        assert_eq!(res.status, 200, "{}", res.body);
        assert_eq!(f.store.settings().unwrap().max_daily_filings, Some(7));

        // **Null is "the built-in default", not zero** — which would be a surface
        // that accepts nothing and reads as broken. This was the empty string a
        // form submits; JSON can say the absence outright, so it does.
        let res = f.go(
            &Req::post_json(&api_path("settings"), r#"{"max_daily_filings":null}"#)
                .with_cookie(&admin),
        );
        assert_eq!(res.status, 200, "{}", res.body);
        assert_eq!(f.store.settings().unwrap().max_daily_filings, None);

        // **And the setting a request does not name is left alone**, which is the
        // property that replaced "nonsense is refused". A form posted the whole
        // group at once, so an unparseable field had to be rejected or it would
        // have been written as a default over a value somebody chose. JSON names
        // only what it changes, so the failure mode is gone by construction —
        // what has to hold instead is that naming one cap does not silently reset
        // its neighbours.
        f.go(
            &Req::post_json(&api_path("settings"), r#"{"max_daily_filings":7}"#)
                .with_cookie(&admin),
        );
        let res = f.go(
            &Req::post_json(&api_path("settings"), r#"{"max_accounts":2}"#).with_cookie(&admin),
        );
        assert_eq!(res.status, 200, "{}", res.body);
        let s = f.store.settings().unwrap();
        assert_eq!(s.max_accounts, Some(2));
        assert_eq!(
            s.max_daily_filings,
            Some(7),
            "a cap nobody named was overwritten"
        );
    }

    #[test]
    fn an_owner_cannot_reach_the_settings() {
        // The same structural argument as the roster: past the gate, so no value
        // of `Caller::Owner` gets here.
        //
        // **Renamed off "page", and both halves asked of the API.** `/settings`
        // serves the interface's document to anybody; reading the settings and
        // writing them are what an owner cannot do, and both answer 404 — the
        // administrative surface does not exist for them.
        let (mut f, session, _mine, _theirs) = owner_fixture("settings-owner");
        assert_eq!(
            f.go(&Req::get(&api_path("settings")).with_cookie(&session))
                .status,
            404
        );
        let res = f
            .go(&Req::post_json(&api_path("settings"), r#"{"public":true}"#).with_cookie(&session));
        assert_eq!(res.status, 404, "an owner wrote the settings: {}", res.body);
        assert!(
            !f.store.settings().unwrap().public,
            "and the switch did not move"
        );
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
            .with_owner("jamez667@example.test", &["intake"]);
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
        // A claim naming an email address must not escalate: a login is
        // required for anything above a filer, so the claim never matches.
        let mut f = Fixture::new("admin-magic-link").with_public(false);
        let mut admin = f.store.admin().unwrap();
        admin.claim("jo@x.com", 1);
        f.store.put_admin(&admin).unwrap();

        let session = f.signed_in("jo@x.com");
        // **Asked of the review API.** `/review` answers the interface's
        // document to anybody; what the claim must not have granted is the
        // review list, and a filer's is empty because they review nothing.
        let res = f.go(&Req::get("/api/v1/ui/requests").with_cookie(&session));
        assert_eq!(res.status, 200);
        assert_eq!(res.body, "[]", "a magic-link claim granted review");
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
    fn a_stranger_with_a_login_is_not_the_administrator() {
        // The claim is one login. Anybody else who signs in is an account.
        let mut f = Fixture::new("gh-stranger").with_public(false);
        let mut admin = f.store.admin().unwrap();
        admin.claim("jamez667@example.test", 1);
        f.store.put_admin(&admin).unwrap();

        let session = f.signed_in_with_login("somebody-else@example.test");
        // **Asked of the administrative API.** Every browser path serves the
        // interface's document now, so what proves they are not the
        // administrator is that the settings behind it are not found to them.
        assert_eq!(
            f.go(&Req::get("/api/v1/ui/settings").with_cookie(&session))
                .status,
            404
        );
    }

    #[test]
    fn an_unclaimed_server_has_no_administrator_at_all() {
        // Not "everybody" and not "the first person": until the claim is made,
        // the private surface belongs to nobody.
        let mut f = Fixture::new("unclaimed").with_public(false);
        let session = f.signed_in_with_login("jamez667@example.test");
        // **Asked of the administrative API**, which is where the gate lives now
        // that every browser path answers the interface's document.
        assert_eq!(
            f.go(&Req::get("/api/v1/ui/settings").with_cookie(&session))
                .status,
            404
        );
    }

    #[test]
    fn the_private_surface_is_not_found_to_anybody_who_is_not_the_administrator() {
        // **Replaces the enrolment-page test**, whose premise went with the code
        // box. The property that mattered survives, and it has moved: every
        // browser path answers the interface's document now, so the refusal
        // lives at the API behind it. A 401 there would tell a stranger the
        // address is real, so everything private is *not found* to everyone
        // else.
        let mut f = Fixture::new("private-404");
        for path in [
            "/api/v1/ui/accounts",
            "/api/v1/ui/owners",
            "/api/v1/ui/repos",
            "/api/v1/ui/requests/anything",
        ] {
            let res = f.go(&Req::get(path));
            assert_eq!(res.status, 404, "{path}: {}", res.body);
        }
        // The review list is the exception, and deliberately: it answers an
        // empty list rather than 404, because the client asks for it on a page
        // a stranger may legitimately be looking at. Nothing is disclosed —
        // there is simply nothing they may see.
        let res = f.go(&Req::get("/api/v1/ui/requests"));
        assert_eq!(res.status, 200);
        assert_eq!(res.body, "[]", "a stranger reviews nothing");
        // And a POST is 401 rather than 404: it is not a page anybody browses
        // to, so there is no address to confirm.
        assert_eq!(f.go(&Req::post("/owners", "login=x&repos=y")).status, 401);
    }

    #[test]
    fn a_dead_cookie_is_told_where_to_sign_in() {
        // Somebody whose session stopped working is the one person most likely
        // to hit this, and a bare "there is nothing here" is a confusing answer
        // at an address that worked yesterday.
        //
        // **The server no longer writes that answer; it supplies the fact the
        // answer is built from.** The private paths serve the interface's
        // document to anybody, and the client decides what to show — so what
        // has to hold here is that a cookie matching nothing resolves to a
        // stranger rather than to a half-live session. `role: anonymous` is
        // what sends the interface to the sign-in surface.
        let mut f = Fixture::new("dead-cookie").with_public(false);
        let res = f.go(&Req::get(ME_PATH).with_cookie("a-token-that-matches-nothing"));
        assert_eq!(res.status, 200, "{}", res.body);
        assert!(res.body.contains("\"anonymous\""), "{}", res.body);
        // And nothing was granted along the way: the review list is empty and
        // the administrative lists are not found.
        assert_eq!(
            f.go(&Req::get("/api/v1/ui/settings").with_cookie("a-token-that-matches-nothing"))
                .status,
            404
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

        // Through the endpoint the interface files with. `check_length` is the
        // same call in both, so what is pinned here is that the administrator's
        // filing path still makes it — a route that skipped it would let text the
        // screener never sees reach a daemon.
        let file = |f: &mut Fixture, text: &str| -> Res {
            f.go(&Req::post_json(
                &api_path("file"),
                &serde_json::json!({ "text": text, "repo": "alpha" }).to_string(),
            )
            .with_cookie(&token))
        };

        let empty = file(&mut f, "   ");
        assert_eq!(empty.status, 400);

        // Too many words, each of them tiny.
        let over = file(&mut f, &"word ".repeat(MAX_WORDS + 10));
        assert_eq!(over.status, 400);
        assert!(over.body.contains("words"), "{}", over.body);

        // And one enormous token, which the word count alone would wave through.
        let over = file(&mut f, &"x".repeat(MAX_BYTES + 1));
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
        // The slugs are the same on the wire as they were in the form's select,
        // and an unrecognised one still falls back rather than being refused: a
        // client sending a kind this server has not heard of is a version skew,
        // and losing the request over a label would be the wrong trade.
        for (slug, expected) in [
            ("bug", IntakeKind::Bug),
            ("feature", IntakeKind::Feature),
            ("improvement", IntakeKind::Improvement),
            ("feedback", IntakeKind::Feedback),
            ("nonsense", IntakeKind::Feature),
        ] {
            let body =
                serde_json::json!({ "text": "a thing", "repo": "alpha", "kind": slug }).to_string();
            let res = f.go(&Req::post_json(&api_path("file"), &body).with_cookie(&token));
            assert_eq!(res.status, 200, "{slug}: {}", res.body);
            let last = f.store.all().unwrap().last().unwrap().clone();
            assert_eq!(last.kind, expected, "{slug}");
        }
    }

    /// Read a request as its reviewer, and take the digest the server offers
    /// for accepting it.
    ///
    /// **This is the read half of the accept handshake.** It stood in a hidden
    /// field on the confirmation page and it is a field on `ReviewRequest` now;
    /// either way the reviewer accepts the bytes they were handed, and this is
    /// how a test comes to hold them.
    fn digest_for(f: &mut Fixture, token: &str, id: &str) -> String {
        let res = f.go(&Req::get(&api_path(&format!("requests/{id}"))).with_cookie(token));
        assert_eq!(res.status, 200, "{}", res.body);
        let v: serde_json::Value = serde_json::from_str(&res.body).unwrap();
        v["spec_digest"]
            .as_str()
            .expect("a drafted request carries the digest to accept it with")
            .to_string()
    }

    /// Accept a request, carrying the digest given.
    fn accept_with(f: &mut Fixture, token: &str, id: &str, digest: &str) -> Res {
        f.go(&Req::post_json(
            &api_path(&format!("requests/{id}/accept")),
            &serde_json::json!({ "digest": digest }).to_string(),
        )
        .with_cookie(token))
    }

    #[test]
    fn accepting_binds_to_the_exact_bytes_the_reviewer_read() {
        // Spec 20: approve is a deliberate action taken below the full artifact.
        //
        // **This used to be `approving_takes_two_deliberate_posts_and_the_first_
        // decides_nothing`, and the name had stopped being true.** The rendered
        // surface asked with `POST /request/{id}/accept`, drew the whole spec with
        // a digest in a hidden field, and settled with a second POST to
        // `/accept/confirm`; the two posts were how a page carried the digest from
        // the read to the write. The API has no such problem — the client already
        // holds the spec it rendered, so it sends the digest in the one post that
        // accepts.
        //
        // The property the two posts existed to produce is what is asserted here,
        // and it is unchanged: **an accept names the bytes it is consenting to.**
        // Reading does not decide anything, and deciding is impossible without
        // having read.
        let mut f = Fixture::new("accept");
        let token = f.as_admin();
        let id = f.file(&token, "a+thing", "alpha");
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        let payload = serde_json::to_string(&DraftedSpec::new(&id, "# Spec", "specs/x")).unwrap();
        f.go(&Req::post(&wire::route::drafted(&id), &payload).with_bearer(KEY));

        let digest = digest_for(&mut f, &token, &id);
        assert_eq!(
            f.store.require(&id).unwrap().state,
            RequestState::AwaitingReview,
            "reading it decides nothing"
        );

        let settled = accept_with(&mut f, &token, &id, &digest);
        assert_eq!(settled.status, 200, "{}", settled.body);
        // `Accepted` is not `Done`: nothing was built, and the developer picks it
        // up in their IDE on their own schedule.
        assert_eq!(f.store.require(&id).unwrap().state, RequestState::Accepted);
    }

    #[test]
    fn an_approval_of_text_that_changed_under_the_reviewer_is_refused() {
        // The reviewer opens v1 on a train; `queue serve` pushes a redraft while
        // they read. Accepting must not settle v2 on the strength of having read
        // v1 — consent attaches to bytes, not to an id. **The one post carrying a
        // digest asserts this as squarely as the two-step page did**, because the
        // digest the client holds is the one it was handed when it rendered.
        let mut f = Fixture::new("stale");
        let token = f.as_admin();
        let id = f.file(&token, "a+thing", "alpha");
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        let v1 = serde_json::to_string(&DraftedSpec::new(&id, "# Version one", "specs/x")).unwrap();
        f.go(&Req::post(&wire::route::drafted(&id), &v1).with_bearer(KEY));

        let stale = digest_for(&mut f, &token, &id);

        // The daemon redrafts under them. Through the real path — sent back,
        // requeued, claimed again — because a daemon may now only report on a
        // claim it currently holds.
        f.go(&Req::post_json(
            &api_path(&format!("requests/{id}/send-back")),
            r#"{"note":"redo"}"#,
        )
        .with_cookie(&token));
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        let v2 = serde_json::to_string(&DraftedSpec::new(&id, "# Version two", "specs/x")).unwrap();
        f.go(&Req::post(&wire::route::drafted(&id), &v2).with_bearer(KEY));

        let refused = accept_with(&mut f, &token, &id, &stale);
        assert_eq!(refused.status, 400, "{}", refused.body);
        assert!(refused.body.contains("changed"), "{}", refused.body);
        assert_eq!(
            f.store.require(&id).unwrap().state,
            RequestState::AwaitingReview,
            "left reviewable rather than half-decided"
        );
    }

    #[test]
    fn an_accept_with_no_digest_at_all_is_refused() {
        // The obvious bypass: POST the committing route directly rather than
        // reading first. It must not succeed by omission.
        //
        // **Renamed off "confirm", which no longer names anything.** There is one
        // post now instead of an ask and a confirm, and the bypass it has to
        // refuse is the same one: arriving at the write without having done the
        // read. An empty digest and a wrong digest are both refused, so neither
        // omitting the field nor guessing at it settles anything.
        let mut f = Fixture::new("no-digest");
        let token = f.as_admin();
        let id = f.file(&token, "a+thing", "alpha");
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        let payload = serde_json::to_string(&DraftedSpec::new(&id, "# Spec", "specs/x")).unwrap();
        f.go(&Req::post(&wire::route::drafted(&id), &payload).with_bearer(KEY));

        for body in ["{}", r#"{"digest":""}"#, r#"{"digest":"nonsense"}"#] {
            let res = f.go(
                &Req::post_json(&api_path(&format!("requests/{id}/accept")), body)
                    .with_cookie(&token),
            );
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

        // **There is nothing to send.** A queued request carries no
        // `spec_digest`, so a client following the handshake cannot even
        // construct the accept — and one that invents a digest anyway is refused
        // on the state before the digest is looked at.
        let read = f.go(&Req::get(&api_path(&format!("requests/{id}"))).with_cookie(&token));
        assert_eq!(read.status, 200, "{}", read.body);
        let v: serde_json::Value = serde_json::from_str(&read.body).unwrap();
        assert!(
            v["spec_digest"].is_null(),
            "a request with no draft offered something to accept: {}",
            read.body
        );

        let res = accept_with(&mut f, &token, &id, "anything-at-all");
        assert_eq!(res.status, 400, "{}", res.body);

        // And an id that names nothing is **not found** rather than refused,
        // which is the same answer its read gives — a 403 or a 400 here would
        // confirm which ids are real.
        let missing = f.go(&Req::get(&api_path("requests/nope")).with_cookie(&token));
        assert_eq!(missing.status, 404, "{}", missing.body);
    }

    #[test]
    fn sending_back_requeues_it_with_the_note() {
        let mut f = Fixture::new("send-back");
        let token = f.as_admin();
        let id = f.file(&token, "a+thing", "alpha");
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        let payload = serde_json::to_string(&DraftedSpec::new(&id, "# Vague", "specs/x")).unwrap();
        f.go(&Req::post(&wire::route::drafted(&id), &payload).with_bearer(KEY));

        let res = f.go(&Req::post_json(
            &api_path(&format!("requests/{id}/send-back")),
            r#"{"note":"name the actual roles"}"#,
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
        //
        // **Asked of the API**, which is where the verbs are now. That is where
        // the property has to hold: `api_verb` matches the four it knows and
        // answers *no such verb* to everything else, so a verb naming
        // implementation cannot exist without somebody writing it into that
        // match. The administrator's cookie is deliberately present — this is not
        // about authority, it is that the capability is absent for the person who
        // has every other one.
        let mut f = Fixture::new("no-build");
        let token = f.as_admin();
        let id = f.file(&token, "a+thing", "alpha");

        for verb in ["build", "run", "implement", "merge", "deploy"] {
            let res = f.go(
                &Req::post_json(&api_path(&format!("requests/{id}/{verb}")), "{}")
                    .with_cookie(&token),
            );
            assert_eq!(res.status, 404, "{verb} must not exist: {}", res.body);
        }
        assert_eq!(
            f.store.require(&id).unwrap().state,
            RequestState::Queued,
            "a refused verb still moved the request"
        );
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
        //
        // **Except the way the administrator gets back in.** `SIGNIN` used to be
        // in this list, and that was the lockout: a claimed server starts with
        // the surface off, so the one person who could turn it on had no door.
        // See `the_administrator_can_sign_in_with_no_public_surface`.
        // **The addresses serve the interface's document, like every other
        // browser path** — what "does not exist" now means is that nothing
        // behind them works. A server with no public surface still hands a
        // stranger the application shell; it just has nothing to give it.
        let mut f = Fixture::new("public-off");
        // **Filing does not exist**, which is the whole of the public surface —
        // 404 rather than a refusal, for the same reason every other absent
        // endpoint answers one.
        assert_eq!(
            f.go(&Req::post_json(
                &api_path("file"),
                r#"{"text":"a thing","kind":"bug"}"#
            ))
            .status,
            404
        );
        // And a stranger is told they may do nothing at all — no filing, which
        // is the whole of the public surface.
        let me = f.go(&Req::get(ME_PATH));
        assert!(me.body.contains("\"anonymous\""), "{}", me.body);
        assert!(me.body.contains("\"file\":false"), "{}", me.body);
        assert_eq!(
            f.go(&Req::get("/api/v1/ui/requests/abc")).status,
            404,
            "and there is nothing to read"
        );
        // Asking for a *link* still gets nowhere — that is filer traffic, and it
        // costs an email a server with no public surface should not be sending.
        // The endpoint answers 503 rather than 404 because a server with no
        // public surface has no mail provider either, and "this server cannot
        // send" is the honest first refusal; what matters here is that no address
        // is taken and nothing goes out.
        let asked = f.go(&Req::post_json(
            &api_path("signin"),
            r#"{"email":"a@x.com"}"#,
        ));
        assert_ne!(asked.status, 200, "an address was accepted: {}", asked.body);
        assert_eq!(f.mailer.count(), 0, "and nothing was emailed");
        assert!(
            f.store.links().unwrap().links.is_empty(),
            "nor was a link minted"
        );
    }

    #[test]
    fn asking_for_a_link_says_the_same_thing_whatever_happened() {
        // The response must not reveal whether an address has an account.
        //
        // **The endpoint the interface asks with**, which is the only one left —
        // and the same three shapes of input, since a client is a far easier
        // thing to probe with than a form was. `asking_for_a_link_over_json_cannot
        // _be_used_to_find_accounts` covers the known-versus-unknown pair; this
        // covers the malformed and empty ones, which is the other half of the
        // same promise.
        let mut f = Fixture::new("signin-uniform").with_public(false);

        let ask = |f: &mut Fixture, email: &str| -> Res {
            f.go(&Req::post_json(
                &api_path("signin"),
                &serde_json::json!({ "email": email }).to_string(),
            ))
        };
        let fresh = ask(&mut f, "new@x.com");
        let malformed = ask(&mut f, "not-an-email");
        let empty = ask(&mut f, "");

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

        // **Through the endpoint, not the deleted form.** A POST to a route that
        // no longer exists sends no mail either, so leaving this pointed at the
        // old address would have kept passing while asserting nothing.
        let before = f.mailer.count();
        let res = f.go(&Req::post_json(
            &api_path("signin"),
            r#"{"email":"jo@x.com"}"#,
        ));
        assert_eq!(res.status, 200, "and it is indistinguishable: {}", res.body);
        assert_eq!(f.mailer.count(), before, "silence, not a notification");
    }

    #[test]
    fn a_get_on_a_sign_in_link_consumes_nothing() {
        // Mail scanners fetch every URL in a message within seconds. A GET that
        // spent the token would burn it before the human opened their inbox.
        let mut f = Fixture::new("prefetch").with_public(false);
        let token = link_token_for(&mut f, "jo@x.com");
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
        let real = link_token_for(&mut f, "jo@x.com");

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
        let token = link_token_for(&mut f, "jo@x.com");
        let path = format!("{}{token}", public_route::SIGNIN_PREFIX);

        assert!(cookie_token(&f.go(&Req::post(&path, ""))).is_some());
        let again = f.go(&Req::post(&path, ""));
        assert!(cookie_token(&again).is_none(), "spent");
        assert!(again.body.contains("already been used"), "{}", again.body);
    }

    #[test]
    fn filing_publicly_requires_being_signed_in() {
        // **404, and no longer a rendered invitation to sign in.** The page used
        // to answer a stranger's filing with the sign-in surface, which is the
        // client's job now; what the server owes is that the filing does not
        // happen and that the endpoint does not exist for somebody holding
        // nothing. The account is the credential — it costs a confirmed mailbox,
        // and that is what stands between this and an open pipe to a model
        // budget.
        let mut f = Fixture::new("public-anon").with_public(false);
        let res = f.go(&Req::post_json(
            &api_path("file"),
            r#"{"text":"a thing","kind":"bug"}"#,
        ));
        assert_eq!(res.status, 404, "{}", res.body);
        assert!(f.store.all().unwrap().is_empty(), "nothing was filed");

        // And the interface is told as much before it draws a form at all.
        let me = f.go(&Req::get(ME_PATH));
        assert!(me.body.contains("\"file\":false"), "{}", me.body);
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

        let res = f.go(&Req::post_json(
            &api_path("file"),
            r#"{"text":"a thing","kind":"bug","repo":"secret-repo"}"#,
        )
        .with_cookie(&session));
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

        // **The set the filer picks from comes from `/me`.** They own no
        // repositories, so it is that or nothing — the client cannot invent the
        // list, and filing against a name this surface does not serve is refused.
        let me = f.go(&Req::get(ME_PATH).with_cookie(&session));
        let mine: serde_json::Value = serde_json::from_str(&me.body).unwrap();
        let offered: Vec<&str> = mine["repos"]
            .as_array()
            .expect("a filer is told what they may file against")
            .iter()
            .filter_map(|r| r.as_str())
            .collect();
        assert_eq!(offered, ["intake", "memosy"], "{}", me.body);

        file_publicly_as(&mut f, &session, "a thing", "memosy");
        assert_eq!(f.store.all().unwrap()[0].repo, "memosy");
    }

    #[test]
    fn a_filing_naming_nothing_is_refused_when_there_is_a_choice() {
        // With more than one repository on offer, an absent name is a client that
        // did not ask — and guessing which project somebody meant is exactly the
        // fallback this refuses to make.
        let mut f = Fixture::new("public-pick-none")
            .with_public(false)
            .with_repos(&["intake", "memosy"]);
        let session = f.signed_in("jo@x.com");

        let res = f.go(
            &Req::post_json(&api_path("file"), r#"{"text":"a thing","kind":"bug"}"#)
                .with_cookie(&session),
        );
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(f.store.all().unwrap().is_empty());
    }

    #[test]
    fn a_public_filing_takes_the_only_repository_when_there_is_one() {
        // A one-repository surface draws no picker, so an absent name is normal
        // there — and must still work exactly as it did before the set existed.
        let mut f = Fixture::new("public-one-repo").with_public(false);
        let session = f.signed_in("jo@x.com");

        let res = f.go(
            &Req::post_json(&api_path("file"), r#"{"text":"a thing","kind":"bug"}"#)
                .with_cookie(&session),
        );
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
        file_publicly_as(&mut f, &session, "a thing", "intake");

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
        file_publicly_as(&mut f, &session, "a thing", "intake");

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
        file_publicly_as(&mut f, &alice, "alice thing", "intake");
        let alice_id = f.store.all().unwrap()[0].id.clone();

        let bob = f.signed_in("bob@x.com");
        // **Asked of the API.** `/public/request/{id}` answers the interface's
        // document to anybody — it has to, or a reload would 404 on an address
        // the client routes on — so the isolation lives on the endpoint the
        // client then fetches, and that is where it is asserted.
        let res = f.go(&Req::get(&format!("/api/v1/ui/requests/{alice_id}")).with_cookie(&bob));
        // Not found, not forbidden: "forbidden" would confirm the id exists.
        assert_eq!(res.status, 404, "{}", res.body);
        assert!(!res.body.contains("alice thing"), "{}", res.body);
        // Nor is it in the list Bob is handed.
        let listed = f.go(&Req::get("/api/v1/ui/requests").with_cookie(&bob));
        assert!(!listed.body.contains("alice thing"), "{}", listed.body);
    }

    /// Arm a claim code, as a fresh server's startup does.
    fn armed(f: &mut Fixture, code: &str) {
        let mut admin = f.store.admin().unwrap();
        assert!(admin.arm(code, f.now_ms));
        f.store.put_admin(&admin).unwrap();
    }

    /// Spend a claim code, and return the setup token step one issues.
    ///
    /// **The address is not part of this any more.** Step one used to take the
    /// code *and* the base URL together, which is why a typo could burn the one
    /// code an operator has; the address is an environment variable now and the
    /// server refuses to start without a valid one, so the step carries the code
    /// alone.
    fn spend_code(f: &mut Fixture, code: &str) -> Res {
        f.go(&Req::post_json(
            &api_path("setup/code"),
            &serde_json::json!({ "code": code }).to_string(),
        ))
    }

    /// The setup token out of step one's `Set-Cookie`.
    fn setup_token(res: &Res) -> String {
        res.set_cookie
            .as_deref()
            .and_then(|c| c.strip_prefix(&format!("{SETUP_COOKIE}=")))
            .and_then(|c| c.split(';').next())
            .expect("a setup token was issued")
            .to_string()
    }

    /// Step two, as a request: claim the server with a login and a password.
    ///
    /// Returns the [`Req`] rather than the answer, because every caller but one
    /// attaches a setup cookie to it — and whether that cookie is there, wrong,
    /// or absent is the thing those tests are about.
    fn claim_as(login: &str, password: &str) -> Req {
        Req::post_json(
            &api_path("setup/admin"),
            &serde_json::json!({ "login": login, "password": password }).to_string(),
        )
    }

    #[test]
    fn setting_up_claims_the_server_for_whoever_signs_in() {
        // **The whole first-run path**, end to end: the code proves you can read
        // the container's log, and the step after it sets the credential that
        // will own the server. They are separate steps so the second is bound to
        // the browser that spent the code.
        let mut f = Fixture::new("setup-claim").with_public(false);
        armed(&mut f, "ABC-123");

        // Step one: the code. **The address is not asked for any more** — it is
        // an environment variable and the server refuses to start without a valid
        // one, so by the time anybody is here it is already settled and there is
        // no typo left that could burn the operator's one claim code.
        let res = spend_code(&mut f, "ABC-123");
        assert_eq!(res.status, 200, "{}", res.body);
        // So what step one produces is the token and nothing else. The rest of
        // the wizard is bound to this browser from here on.
        let setup = setup_token(&res);
        // Nobody owns it yet: spending the code is one proof, not the claim.
        assert!(!f.store.admin().unwrap().claimed());

        // And the server now says which step this browser may take, which is the
        // question the rendered wizard answered by drawing one form or the other.
        let state = f.go(&Req::get(&api_path("setup")).with_setup(&setup));
        assert!(state.body.contains("\"admin\""), "{}", state.body);

        // Step two: the credential that decides who owns this.
        let res =
            f.go(&claim_as("JameZ667@example.test", "correct-horse-battery").with_setup(&setup));
        assert_eq!(res.status, 200, "{}", res.body);
        let admin = f.store.admin().unwrap();
        assert!(
            admin.is("jamez667@example.test"),
            "lowercased on the way in"
        );

        // The password is stored hashed and never handed back.
        assert!(!res.body.contains("correct-horse-battery"), "{}", res.body);
        let raw = std::fs::read_to_string(f.store.accounts_path()).unwrap();
        assert!(!raw.contains("correct-horse-battery"), "{raw}");
        assert!(raw.contains("$argon2id$"), "and it is the slow hash");

        // And they are signed in already — they just chose the credential, so
        // asking for it again immediately would be ceremony. **Asked of the
        // administrative API**, which is what being the administrator now buys:
        // the pages answer the interface's document to anybody.
        let session = cookie_token(&res).expect("signed in");
        assert_eq!(
            f.go(&Req::get("/api/v1/ui/settings").with_cookie(&session))
                .status,
            200
        );

        // And setup is gone — the endpoint, which is the thing that grants
        // anything. The address still serves the interface, as every address
        // does.
        assert_eq!(f.go(&Req::get(&api_path("setup"))).status, 404);
        assert_eq!(
            f.go(&Req::post_json(
                &api_path("setup/code"),
                r#"{"code":"ABC-123"}"#
            ))
            .status,
            404
        );
    }

    #[test]
    fn a_password_signs_the_administrator_in() {
        // The ordinary path, and the one that was unreachable for two days: a
        // credential on this server's own origin, no third party in it.
        let mut f = Fixture::new("password-signin").with_public(false);
        let mut admin = crate::admin::Admin::default();
        admin.claim("jamez667@example.test", f.now_ms);
        f.store.put_admin(&admin).unwrap();
        let mut accounts = f.store.accounts().unwrap();
        accounts
            .create_login("jamez667@example.test", "correct-horse-battery", f.now_ms)
            .unwrap();
        f.store.put_accounts(&accounts).unwrap();

        let res = sign_in_with_password(&mut f, "JameZ667@example.test", "correct-horse-battery");
        assert_eq!(res.status, 200, "{}", res.body);

        // **`Strict`**, which the GitHub return could not have. Nothing arrives
        // here cross-site any more, so nothing needs the relaxation.
        let set = res.set_cookie.as_deref().expect("a session cookie");
        assert!(set.contains("SameSite=Strict"), "{set}");
        assert!(set.contains("HttpOnly"), "{set}");

        // And it is the administrator's session, not a filer's. **Asked of the
        // administrative API**: `/settings` answers the interface's document to
        // anybody, so the settings themselves are what being the administrator
        // buys.
        let session = cookie_token(&res).expect("signed in");
        assert_eq!(
            f.go(&Req::get(&api_path("settings")).with_cookie(&session))
                .status,
            200
        );
    }

    #[test]
    fn a_wrong_password_backs_off_and_a_right_one_clears_it() {
        // **What GitHub used to do for us.** The rate limiter alone allows
        // ~29,000 guesses a day against a known username, which is not a bound
        // worth calling one.
        let mut f = Fixture::new("password-backoff").with_public(false);
        let mut accounts = f.store.accounts().unwrap();
        accounts
            .create_login("jamez667@example.test", "correct-horse-battery", f.now_ms)
            .unwrap();
        f.store.put_accounts(&accounts).unwrap();

        for _ in 0..5 {
            let res = sign_in_with_password(&mut f, "jamez667@example.test", "not-the-password");
            assert_eq!(res.status, 401, "{}", res.body);
        }
        let waiting = f.store.accounts().unwrap();
        let account = waiting
            .by_login("jamez667@example.test")
            .expect("still there");
        assert!(
            account.next_attempt_ms > f.now_ms,
            "the wrong guesses bought a wait"
        );
        // **Three, not five, and that is the property.** The first two are free;
        // the third bought the delay, and the two attempts after it were refused
        // *before* the password was checked at all. A locked-out account must
        // not also be a way to spend this server's CPU on argon2, so the counter
        // standing still under a delay is the evidence that it does not.
        assert_eq!(account.failed_attempts, 3);

        // **The right password during the wait is still refused**, and that is
        // the point: an attacker who guesses correctly on the fourth attempt
        // gains nothing until the delay has run.
        let during =
            sign_in_with_password(&mut f, "jamez667@example.test", "correct-horse-battery");
        assert_eq!(during.status, 401, "{}", during.body);

        // Past the wait, the right password works and the count goes back to
        // nothing — a person who mistyped it a few times is not penalised for
        // the rest of the day.
        f.now_ms = account.next_attempt_ms + 1;
        let res = sign_in_with_password(&mut f, "jamez667@example.test", "correct-horse-battery");
        assert_eq!(res.status, 200, "{}", res.body);
        let after = f.store.accounts().unwrap();
        let account = after
            .by_login("jamez667@example.test")
            .expect("still there");
        assert_eq!(account.failed_attempts, 0);
        assert_eq!(account.next_attempt_ms, 0);
    }

    #[test]
    fn every_sign_in_failure_says_the_same_thing() {
        // No such account, wrong password, and still backing off are three
        // different facts, and telling them apart tells a guesser which half
        // they got right. One answer for all three.
        let mut f = Fixture::new("password-one-answer").with_public(false);
        let mut accounts = f.store.accounts().unwrap();
        accounts
            .create_login("jamez667@example.test", "correct-horse-battery", f.now_ms)
            .unwrap();
        f.store.put_accounts(&accounts).unwrap();

        let no_such = sign_in_with_password(&mut f, "nobody-at-all", "correct-horse-battery");
        let wrong = sign_in_with_password(&mut f, "jamez667@example.test", "not-the-password");
        assert_eq!(no_such.status, 401);
        assert_eq!(wrong.status, 401);
        assert_eq!(no_such.body, wrong.body, "one answer, not two");

        // **And the third fact, which is the one only this test reaches.** Guess
        // until the account is backing off, then send the *right* password: that
        // refusal must be the same one again. A distinguishable "you are locked
        // out" would tell a guesser they had found the password, which is the
        // whole thing the backoff is protecting.
        for _ in 0..4 {
            sign_in_with_password(&mut f, "jamez667@example.test", "not-the-password");
        }
        let backing_off =
            sign_in_with_password(&mut f, "jamez667@example.test", "correct-horse-battery");
        assert_eq!(backing_off.status, 401);
        assert_eq!(
            backing_off.body, wrong.body,
            "a backoff is distinguishable from a wrong password"
        );
    }

    #[test]
    fn a_password_post_is_not_read_as_a_magic_link_token() {
        // **An ordering the match arms carry silently.** The password address
        // sits under the magic-link *prefix* — `/public/signin/password` did, and
        // `/api/v1/ui/signin/password` does under an API route matched by prefix
        // too — so a dispatcher that reached the link arm first would feed the
        // typed password to `complete_sign_in` as a token. It would fail, which
        // is the dangerous part: the two roles with no other way in would simply
        // stop being able to sign in, with nothing naming a cause.
        //
        // **The hazard survived the move**, which is why this test did. The
        // password endpoint is dispatched inside `api_route`, above the public
        // block that owns `SIGNIN_PREFIX`, and this is what pins that ordering.
        let mut f = Fixture::new("password-arm-order").with_public(false);
        let mut accounts = f.store.accounts().unwrap();
        accounts
            .create_login("jamez667@example.test", "correct-horse-battery", f.now_ms)
            .unwrap();
        f.store.put_accounts(&accounts).unwrap();

        let res = sign_in_with_password(&mut f, "jamez667@example.test", "correct-horse-battery");
        assert_eq!(res.status, 200, "{}", res.body);
        assert!(
            cookie_token(&res).is_some(),
            "the password arm ran, not the token consumer"
        );
        // And nothing was spent as if it had been a link.
        assert!(
            f.store.links().unwrap().links.is_empty(),
            "the password was treated as a token"
        );
    }

    #[test]
    fn a_password_is_never_rendered_back_or_stored_in_the_clear() {
        // The property `a_credential_is_never_stored_in_the_clear` names, for
        // the one credential a human chooses — where a fast hash would have left
        // that test passing while the property it names had quietly weakened.
        let mut f = Fixture::new("password-never-echoed").with_public(false);
        let mut accounts = f.store.accounts().unwrap();
        accounts
            .create_login("jamez667@example.test", "correct-horse-battery", f.now_ms)
            .unwrap();
        f.store.put_accounts(&accounts).unwrap();

        let raw = std::fs::read_to_string(f.store.accounts_path()).unwrap();
        assert!(!raw.contains("correct-horse-battery"), "{raw}");
        assert!(raw.contains("$argon2id$"), "and it is the slow hash");

        // **Both outcomes**, because the tempting place to echo a credential is
        // the refusal — a form redrawing itself with what was typed.
        for password in ["correct-horse-battery", "not-the-password"] {
            let res = sign_in_with_password(&mut f, "jamez667@example.test", password);
            assert!(
                !res.body.contains("correct-horse-battery")
                    && !res.body.contains("not-the-password"),
                "{}",
                res.body
            );
        }
    }

    #[test]
    fn the_administrator_can_sign_in_with_no_public_surface() {
        // **The lockout this closes, and it was live.** A freshly claimed server
        // starts with the public surface off, the password form lives at a
        // public address, and every public address 404s when there is no public
        // surface. So: claim the server, let the setup session lapse, and the
        // only way back in is gone — leaving the one person who could turn the
        // surface on unable to reach the switch.
        //
        // Found against the built binary, not here. A route test asks for the
        // path directly and gets a fixture that happens to have a surface.
        // **No `with_public` at all** — that builder turns the surface *on*
        // (its bool is the screener). A fixture that called it would test the
        // opposite of what this test is named for.
        let mut f = Fixture::new("signin-no-public");
        let mut admin = crate::admin::Admin::default();
        admin.claim("jamez667@example.test", f.now_ms);
        f.store.put_admin(&admin).unwrap();
        let mut accounts = f.store.accounts().unwrap();
        accounts
            .create_login("jamez667@example.test", "correct-horse-battery", f.now_ms)
            .unwrap();
        f.store.put_accounts(&accounts).unwrap();

        // The address that draws the sign-in surface still answers the
        // interface...
        assert_eq!(f.go(&Req::get(public_route::SIGNIN)).status, 200);

        // ...and the endpoint behind it works. **That endpoint is under the API
        // prefix, which is dispatched above the public block entirely** — so it
        // never depended on `ctx.public` in the first place, which is exactly
        // what makes this lockout closed rather than merely unlikely.
        let res = sign_in_with_password(&mut f, "jamez667@example.test", "correct-horse-battery");
        assert_eq!(res.status, 200, "{}", res.body);
        let session = cookie_token(&res).expect("signed in");
        assert_eq!(
            f.go(&Req::get(&api_path("settings")).with_cookie(&session))
                .status,
            200,
            "and it reaches the switch that turns the public surface on"
        );

        // **The rest of the public surface stays shut** to a stranger. This is
        // one door for the two named roles, not a way to serve filing pages from
        // a server that has none.
        //
        // The addresses themselves answer the interface's document — every
        // browser path does — so what "shut" means is that a stranger is granted
        // nothing behind them.
        let mut cold = Fixture::new("signin-no-public-cold");
        let me = cold.go(&Req::get(ME_PATH));
        assert!(me.body.contains("\"file\":false"), "{}", me.body);
        assert_eq!(cold.go(&Req::get("/api/v1/ui/requests/abc")).status, 404);
        assert_eq!(
            cold.go(&Req::post(public_route::SIGNIN, "email=a%40x.com"))
                .status,
            404,
            "and asking for a magic link is still gone"
        );
    }

    #[test]
    fn guessing_passwords_cannot_lock_filers_out_of_asking_for_a_link() {
        // **Not `PublicWrite`.** That bucket is shared with the magic-link form,
        // so an attacker grinding passwords would deny every filer a sign-in
        // link — turning a credential attack into an outage for everybody else.
        // `AnonPrivate` is what its own doc names for this.
        assert_eq!(
            bucket_for(&None, public_route::SIGNIN_PASSWORD),
            Bucket::AnonPrivate,
        );
        assert_eq!(
            bucket_for(&None, public_route::SIGNIN),
            Bucket::PublicWrite,
            "and asking for a link is still the public bucket",
        );
    }

    // -- the browser surface's JSON API --------------------------------------

    #[test]
    fn who_am_i_answers_a_stranger_rather_than_refusing() {
        // The landing page is public and the client cannot render it without
        // knowing whether anybody is signed in. A 401 here would mean the SPA
        // could not draw its own front door.
        let mut f = Fixture::new("api-me-anon").with_public(false);
        let res = f.go(&Req::get("/api/v1/ui/me"));
        assert_eq!(res.status, 200, "{}", res.body);
        assert_eq!(res.content_type, "application/json");

        let me: serde_json::Value = serde_json::from_str(&res.body).unwrap();
        assert_eq!(me["role"], "anonymous");
        assert_eq!(me["can"]["file"], false);
        assert_eq!(me["can"]["administer"], false);
    }

    #[test]
    fn who_am_i_tells_the_administrator_what_they_may_do() {
        let mut f = Fixture::new("api-me-admin").with_public(false);
        let session = f.as_admin();
        let res = f.go(&Req::get("/api/v1/ui/me").with_cookie(&session));
        assert_eq!(res.status, 200, "{}", res.body);

        let me: serde_json::Value = serde_json::from_str(&res.body).unwrap();
        assert_eq!(me["role"], "administrator");
        assert_eq!(me["can"]["accept"], true);
        assert_eq!(me["can"]["administer"], true);
    }

    #[test]
    fn who_am_i_never_tells_an_owner_they_may_accept() {
        // The owner role in one line: review yes, accept no. The server enforces
        // it by variant identity; this stops the interface offering a button
        // that would 404.
        let mut f = Fixture::new("api-me-owner")
            .with_public(false)
            .with_repos(&["intake"])
            .with_owner("jo@x.com", &["intake"]);
        let session = f.signed_in_with_login("jo@x.com");
        let res = f.go(&Req::get("/api/v1/ui/me").with_cookie(&session));
        assert_eq!(res.status, 200, "{}", res.body);

        let me: serde_json::Value = serde_json::from_str(&res.body).unwrap();
        assert_eq!(me["role"], "owner");
        assert_eq!(me["can"]["review"], true);
        assert_eq!(me["can"]["accept"], false, "an owner cannot accept");
        assert_eq!(me["can"]["administer"], false);
        assert_eq!(me["repos"][0], "intake");
    }

    #[test]
    fn the_json_api_answers_json_even_when_it_refuses() {
        // **The reason this dispatch sits above the private device gate.** That
        // gate answers a rendered HTML 404, and an HTML body handed to a `fetch`
        // is a parse error rather than a status the client can act on.
        let mut f = Fixture::new("api-404").with_public(false);
        let res = f.go(&Req::get("/api/v1/ui/nothing-here"));
        assert_eq!(res.status, 404);
        assert_eq!(res.content_type, "application/json");
        assert!(
            serde_json::from_str::<serde_json::Value>(&res.body).is_ok(),
            "the body parses as JSON: {}",
            res.body
        );
    }

    #[test]
    fn the_json_api_is_never_served_with_script() {
        // A JSON body is not a document: nothing loads a subresource from it and
        // nothing runs in it. `Policy::Strict` is the default and this asserts
        // the API did not quietly acquire the public one by sitting near it.
        let mut f = Fixture::new("api-policy").with_public(false);
        assert_eq!(f.go(&Req::get("/api/v1/ui/me")).policy, Policy::Strict);
        assert_eq!(
            f.go(&Req::get("/api/v1/ui/nothing-here")).policy,
            Policy::Strict
        );
    }

    #[test]
    fn a_daemon_key_buys_nothing_on_the_browser_api() {
        // Two prefixes, two audiences. A daemon holds a bearer key for
        // `/api/v1/work` and has no browser identity to report.
        let mut f = Fixture::new("api-daemon").with_public(false);
        let res = f.go(&Req::get("/api/v1/ui/me").with_bearer(KEY));
        assert_eq!(res.status, 200);
        let me: serde_json::Value = serde_json::from_str(&res.body).unwrap();
        assert_eq!(me["role"], "anonymous");
        assert_eq!(me["can"]["review"], false);
    }

    #[test]
    fn asking_who_i_am_is_not_rate_limited_like_a_password_guess() {
        // `/me` is what every browser asks first, including a stranger loading
        // the public landing page. `AnonPrivate`'s 20/min would throttle
        // ordinary reading; everything else under the prefix keeps it.
        assert_eq!(bucket_for(&None, "/api/v1/ui/me"), Bucket::PublicRead);
        assert_eq!(
            bucket_for(&None, "/api/v1/ui/requests"),
            Bucket::AnonPrivate,
            "an anonymous request for somebody's data is a probe, not browsing"
        );
    }

    // -- the string catalogue the client draws itself from ------------------

    #[test]
    fn a_stranger_can_read_the_catalogue_in_the_language_they_asked_for() {
        // **Reachable holding nothing**, because the landing page and the
        // sign-in dialog are the first things a stranger sees and neither can be
        // drawn without these. Exactly as public as `/me`, and for that reason.
        let mut f = Fixture::new("strings-anon").with_public(false);

        let en = f.go(&Req::get(STRINGS_PATH));
        assert_eq!(en.status, 200, "{}", en.body);
        let body: serde_json::Value = serde_json::from_str(&en.body).unwrap();
        // The locale travels *beside* the strings: the client cannot derive the
        // code from the text, and it needs one for `<html lang>`.
        assert_eq!(body["locale"], "en");
        assert_eq!(
            body["strings"]["landing_point_2_title"],
            "A spec, not a ticket"
        );

        // And the negotiation is the one `Req::locale` already does — cookie,
        // then header. No `?lang=`, which would be a second thing able to
        // disagree with the cookie the switcher writes.
        let fr = f.go(&Req::get(STRINGS_PATH).with_lang(None, Some("fr,en;q=0.8")));
        let body: serde_json::Value = serde_json::from_str(&fr.body).unwrap();
        assert_eq!(body["locale"], "fr");
        assert_eq!(
            body["strings"]["landing_point_2_title"],
            "Une spécification, pas un ticket"
        );

        // A cookie beats the header, so the switcher is not silently overridden
        // by whatever the browser was installed with.
        let cookie_wins = f.go(&Req::get(STRINGS_PATH).with_lang(Some("fr"), Some("en")));
        let body: serde_json::Value = serde_json::from_str(&cookie_wins.body).unwrap();
        assert_eq!(body["locale"], "fr");
    }

    #[test]
    fn a_held_catalogue_is_revalidated_rather_than_resent() {
        // The whole point of the ETag. The catalogue is compiled in and cannot
        // change while the process runs, so a client that has it should be told
        // "still yours" in a few dozen bytes rather than handed it again.
        let mut f = Fixture::new("strings-etag").with_public(false);

        let first = f.go(&Req::get(STRINGS_PATH));
        let tag = first
            .etag
            .clone()
            .expect("the catalogue carries a validator");
        assert!(tag.starts_with('"') && tag.ends_with('"'), "{tag}");

        let again = f.go(&Req::get(STRINGS_PATH));
        assert_eq!(
            again.etag.as_deref(),
            Some(tag.as_str()),
            "stable per build"
        );

        let mut held = Req::get(STRINGS_PATH);
        held.if_none_match = Some(tag.clone());
        let third = f.go(&held);
        assert_eq!(third.status, 304);
        assert!(
            third.body.is_empty(),
            "a 304 carries no body: {}",
            third.body
        );
        assert_eq!(third.etag.as_deref(), Some(tag.as_str()));
    }

    #[test]
    fn switching_language_cannot_be_answered_from_the_previous_catalogue() {
        // **The tag covers the body, and the body carries the locale code.** A
        // reader who switches to French while holding the English tag must not
        // be told "nothing changed" — that is the one 304 that would leave the
        // interface in a language the reader just rejected.
        let mut f = Fixture::new("strings-etag-lang").with_public(false);

        let english = f.go(&Req::get(STRINGS_PATH));
        let en_tag = english.etag.expect("a validator");

        let mut held = Req::get(STRINGS_PATH).with_lang(Some("fr"), None);
        held.if_none_match = Some(en_tag.clone());
        let french = f.go(&held);
        assert_eq!(
            french.status, 200,
            "a different language is a different body"
        );
        assert_ne!(french.etag.as_deref(), Some(en_tag.as_str()));
        assert!(french.body.contains("\"locale\":\"fr\""), "{}", french.body);
    }

    #[test]
    fn reading_the_catalogue_is_not_rate_limited_like_a_probe() {
        // It rides with `/me` for the identical reason: it is the second thing a
        // stranger's browser asks, and the landing page cannot be drawn without
        // it. In `AnonPrivate` a reader who reloads a few times would get a 429
        // on their own language — an interface with no words in it.
        assert_eq!(bucket_for(&None, STRINGS_PATH), Bucket::PublicRead);
        // Choosing one is a POST, so it cannot be `PublicRead` — but it costs a
        // cookie and no store read, and it is the one action that makes the page
        // legible to the reader asking for it.
        assert_eq!(
            bucket_for(&None, &api_path("language")),
            Bucket::PublicWrite
        );
    }

    #[test]
    fn choosing_a_language_over_json_sets_the_cookie_and_answers_the_new_words() {
        // One round trip: the cookie the server negotiates on, and the strings
        // to redraw with. Posting and then re-fetching would draw the page twice
        // in the language the reader just rejected.
        let mut f = Fixture::new("strings-set-lang").with_public(false);

        let res = f.go(&Req::post_json(&api_path("language"), "{\"lang\":\"fr\"}"));
        assert_eq!(res.status, 200, "{}", res.body);
        let cookie = res.set_cookie.clone().expect("a language cookie is set");
        assert!(cookie.starts_with(&format!("{LANG_COOKIE}=fr")), "{cookie}");
        // The same attributes the form route writes, because both build it with
        // `language_cookie` — a preference rather than a credential.
        assert!(!cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Lax"), "{cookie}");

        let body: serde_json::Value = serde_json::from_str(&res.body).unwrap();
        assert_eq!(body["locale"], "fr");
        assert_eq!(body["strings"]["nav_signin"], "Se connecter");
        // And it carries a validator, so the client stores the new catalogue
        // with one already attached rather than fetching again to get one.
        assert!(res.etag.is_some());

        // An unknown code selects the default rather than erroring, matching the
        // form route: a stale cookie is not an attack.
        let odd = f.go(&Req::post_json(
            &api_path("language"),
            "{\"lang\":\"../etc\"}",
        ));
        let body: serde_json::Value = serde_json::from_str(&odd.body).unwrap();
        assert_eq!(body["locale"], "en");
    }

    #[test]
    fn the_catalogue_reaches_the_client_as_a_flat_object_of_every_field() {
        // The wire shape is the struct definition, derived rather than
        // hand-listed — so a field added to `Strings` arrives at the client with
        // nothing else edited. This pins that: the payload has as many keys as
        // the catalogue has fields, and one of the newest is among them.
        let mut f = Fixture::new("strings-shape").with_public(false);
        let res = f.go(&Req::get(STRINGS_PATH));
        let body: serde_json::Value = serde_json::from_str(&res.body).unwrap();
        let strings = body["strings"].as_object().expect("a flat object");

        // Every value is a string; nothing nested, so the client can index it
        // without a shape to reason about.
        assert!(strings.values().all(|v| v.is_string()), "{strings:?}");
        // A sample from each half — one the server has always rendered, one the
        // client added — so this fails if either group stops being sent.
        assert!(strings.contains_key("state_received"));
        assert!(strings.contains_key("setup_min_password_chars_one"));
        assert!(strings.contains_key("review_state_quarantined"));
    }

    #[test]
    fn a_filer_is_never_told_their_request_was_quarantined() {
        // **The rule the HTML surface enforced with a coarse label**, carried
        // into JSON. `Screening`, `Quarantined` and `Queued` all read as
        // "received": a filer learning theirs was quarantined learns this server
        // screens, which is what a spammer tunes against.
        let (mut f, filer, id) = quarantined_fixture("api-coarse");

        let res = f.go(&Req::get("/api/v1/ui/requests").with_cookie(&filer));
        assert_eq!(res.status, 200, "{}", res.body);
        let body = res.body.to_ascii_lowercase();
        assert!(!body.contains("quarantin"), "{}", res.body);
        assert!(!body.contains("spam"), "{}", res.body);
        assert!(!body.contains("screening"), "{}", res.body);

        // And by id, the same.
        let one = f.go(&Req::get(&format!("/api/v1/ui/requests/{id}")).with_cookie(&filer));
        assert_eq!(one.status, 200, "{}", one.body);
        assert!(
            !one.body.to_ascii_lowercase().contains("quarantin"),
            "{}",
            one.body
        );
    }

    #[test]
    fn a_filer_is_never_sent_a_path_on_the_developers_machine() {
        // `artifact_dir` is a directory on somebody else's computer, `note` is
        // daemon failure text naming repositories, and `repo` is the repository
        // name. The filer's type has no field for any of them, so this cannot
        // regress by a handler forgetting to strip one.
        let (mut f, filer, id) = filed_fixture("api-narrow");

        let res = f.go(&Req::get(&format!("/api/v1/ui/requests/{id}")).with_cookie(&filer));
        assert_eq!(res.status, 200, "{}", res.body);
        let v: serde_json::Value = serde_json::from_str(&res.body).unwrap();
        assert!(v.get("artifact_dir").is_none(), "{}", res.body);
        assert!(v.get("note").is_none(), "{}", res.body);
        assert!(v.get("repo").is_none(), "{}", res.body);
        assert!(
            v.get("summary").is_some(),
            "but they do see their own: {}",
            res.body
        );
    }

    #[test]
    fn another_filers_request_is_not_found_rather_than_forbidden() {
        // **404, never 403.** A 403 confirms the id is real, which is the fact
        // being withheld. Ids are time-ordered and enumerable in seconds, so
        // this is the difference between a list nobody can build and one anybody
        // can.
        let (mut f, _filer, id) = filed_fixture("api-not-mine");
        let stranger = f.signed_in("someone-else@x.com");

        let res = f.go(&Req::get(&format!("/api/v1/ui/requests/{id}")).with_cookie(&stranger));
        assert_eq!(res.status, 404, "{}", res.body);
        assert_eq!(res.content_type, "application/json");
    }

    #[test]
    fn an_owner_reads_only_the_repositories_they_own() {
        let (mut f, owner, _mine, _theirs) = owner_fixture("api-owner-list");

        let res = f.go(&Req::get("/api/v1/ui/requests").with_cookie(&owner));
        assert_eq!(res.status, 200, "{}", res.body);
        let list: Vec<serde_json::Value> = serde_json::from_str(&res.body).unwrap();
        assert!(!list.is_empty(), "they own something");
        for r in &list {
            assert_eq!(r["repo"], "intake", "never somebody else's: {r}");
        }
    }

    #[test]
    fn an_owner_is_not_given_the_developers_artifact_path() {
        // It is a path on a machine the owner does not have, and naming it tells
        // them how the developer's disk is laid out. The administrator gets it;
        // an owner does not.
        let (mut f, owner, _mine, _theirs) = owner_fixture("api-owner-narrow");
        let res = f.go(&Req::get("/api/v1/ui/requests").with_cookie(&owner));
        let list: Vec<serde_json::Value> = serde_json::from_str(&res.body).unwrap();
        for r in &list {
            assert!(r.get("artifact_dir").is_none(), "{r}");
        }
    }

    #[test]
    fn the_administrative_lists_do_not_exist_for_anybody_else() {
        // The same answer the private surface gives: **404, not 401**. Saying
        // "unauthorized" would tell a stranger the address is real.
        let mut f = Fixture::new("api-admin-gate").with_public(false);
        let filer = f.signed_in("jo@x.com");

        for path in [
            "/api/v1/ui/settings",
            "/api/v1/ui/owners",
            "/api/v1/ui/repos",
            "/api/v1/ui/daemons",
            "/api/v1/ui/accounts",
        ] {
            assert_eq!(f.go(&Req::get(path)).status, 404, "signed out: {path}");
            assert_eq!(
                f.go(&Req::get(path).with_cookie(&filer)).status,
                404,
                "a filer: {path}"
            );
        }
    }

    #[test]
    fn the_accounts_endpoint_carries_a_hint_and_no_address() {
        // The hint is `j***@example.com` — enough to recognise an account you
        // meant to revoke, not enough to be a contact list. The hash and the
        // password hash stay on the volume.
        let mut f = Fixture::new("api-accounts").with_public(false);
        let admin = f.as_admin();
        f.signed_in("jo@x.com");

        let res = f.go(&Req::get("/api/v1/ui/accounts").with_cookie(&admin));
        assert_eq!(res.status, 200, "{}", res.body);
        assert!(
            !res.body.contains("jo@x.com"),
            "not the address: {}",
            res.body
        );
        assert!(!res.body.contains("email_hash"), "{}", res.body);
        assert!(!res.body.contains("password_hash"), "{}", res.body);
        assert!(res.body.contains("email_hint"), "{}", res.body);
    }

    #[test]
    fn a_stranger_asking_for_requests_gets_an_empty_list_not_an_error() {
        // The client asks this on a page a stranger may legitimately be reading.
        // An empty list is the true answer; a 401 would make the landing page
        // impossible to render.
        let mut f = Fixture::new("api-anon-requests").with_public(false);
        let res = f.go(&Req::get("/api/v1/ui/requests"));
        assert_eq!(res.status, 200);
        assert_eq!(res.body, "[]");
    }

    #[test]
    fn the_administrator_is_sent_the_digest_of_the_spec_they_are_reading() {
        // **The accept handshake.** Accepting means accepting *these bytes*; if a
        // redraft lands between reading and accepting, the digest no longer
        // matches and the accept is refused rather than approving text nobody
        // read. The client cannot thread a digest it was never given.
        let (mut f, id) = drafted_fixture("api-digest");
        let admin = f.as_admin();

        let res = f.go(&Req::get(&format!("/api/v1/ui/requests/{id}")).with_cookie(&admin));
        assert_eq!(res.status, 200, "{}", res.body);
        let v: serde_json::Value = serde_json::from_str(&res.body).unwrap();
        let digest = v["spec_digest"]
            .as_str()
            .expect("a digest travels with the spec");
        let spec = v["spec"].as_str().expect("and so does the spec");
        assert_eq!(
            digest,
            auth::hash(spec),
            "and it is the digest of that spec"
        );
    }

    /// A JSON POST, as the SPA sends one.
    fn api_post(path: &str, body: &str) -> Req {
        let mut r = Req::post(path, body);
        r.content_type = Some("application/json".into());
        r
    }

    #[test]
    fn accepting_needs_the_digest_of_the_spec_that_was_read() {
        // **The handshake, carried into JSON.** Accepting means accepting *these
        // bytes*. If a redraft lands between reading and accepting, the digest
        // stops matching and the accept is refused rather than silently
        // approving text nobody read.
        let (mut f, id) = drafted_fixture("api-accept-digest");
        let admin = f.as_admin();

        // Without one: refused, and told why.
        let bare =
            f.go(&api_post(&format!("/api/v1/ui/requests/{id}/accept"), "{}").with_cookie(&admin));
        assert_eq!(bare.status, 400, "{}", bare.body);

        // With the wrong one: refused.
        let wrong = f.go(&api_post(
            &format!("/api/v1/ui/requests/{id}/accept"),
            "{\"digest\":\"0000000000000000000000000000000000000000000000000000000000000000\"}",
        )
        .with_cookie(&admin));
        assert_eq!(wrong.status, 400, "{}", wrong.body);

        // With the right one: accepted.
        let spec = f.store.get(&id).unwrap().unwrap().spec.unwrap();
        let good = f.go(&api_post(
            &format!("/api/v1/ui/requests/{id}/accept"),
            &format!("{{\"digest\":\"{}\"}}", auth::hash(&spec)),
        )
        .with_cookie(&admin));
        assert_eq!(good.status, 200, "{}", good.body);
        assert_eq!(
            f.store.get(&id).unwrap().unwrap().state,
            RequestState::Accepted
        );
    }

    #[test]
    fn an_owner_cannot_accept_through_the_json_api_either() {
        // The role's whole shape. On the HTML surface this is enforced by
        // `Caller::Owner` not matching the admin gate — no value of the variant
        // reaches an accepting handler. The API matches the variant the same
        // way, and answers **404**: the verb does not exist for them.
        let (mut f, owner, mine, _theirs) = owner_fixture("api-owner-accept");
        let mut r = f.store.get(&mine).unwrap().unwrap();
        r.state = RequestState::AwaitingReview;
        r.spec = Some("# spec".to_string());
        f.store.put(&r).unwrap();

        let res = f.go(&api_post(
            &format!("/api/v1/ui/requests/{mine}/accept"),
            &format!("{{\"digest\":\"{}\"}}", auth::hash("# spec")),
        )
        .with_cookie(&owner));
        assert_eq!(res.status, 404, "{}", res.body);
        assert_ne!(
            f.store.get(&mine).unwrap().unwrap().state,
            RequestState::Accepted,
            "and nothing was accepted"
        );
    }

    #[test]
    fn an_owner_may_send_back_and_discard_their_own_repositories() {
        let (mut f, owner, mine, _theirs) = owner_fixture("api-owner-verbs");
        let mut r = f.store.get(&mine).unwrap().unwrap();
        r.state = RequestState::AwaitingReview;
        r.spec = Some("# spec".to_string());
        f.store.put(&r).unwrap();

        let res = f.go(&api_post(
            &format!("/api/v1/ui/requests/{mine}/send-back"),
            "{\"note\":\"needs more detail\"}",
        )
        .with_cookie(&owner));
        assert_eq!(res.status, 200, "{}", res.body);
    }

    #[test]
    fn an_owner_acting_outside_their_repositories_is_not_found() {
        // 404, never 403 — a 403 confirms the id is real.
        let (mut f, owner, _mine, theirs) = owner_fixture("api-owner-outside");
        let res = f.go(
            &api_post(&format!("/api/v1/ui/requests/{theirs}/discard"), "{}").with_cookie(&owner),
        );
        assert_eq!(res.status, 404, "{}", res.body);
    }

    #[test]
    fn a_filer_cannot_reach_a_review_verb() {
        let (mut f, filer, id) = filed_fixture("api-filer-verb");
        let res =
            f.go(&api_post(&format!("/api/v1/ui/requests/{id}/discard"), "{}").with_cookie(&filer));
        assert_eq!(res.status, 404, "{}", res.body);
    }

    #[test]
    fn a_mutating_call_must_say_it_is_json() {
        // **A `<form>` cannot send `application/json`.** Demanding it means a
        // cross-origin page cannot reach these endpoints without a preflight —
        // and the origin check is what that preflight then fails. Today the only
        // defence is `SameSite=Strict`, which is load-bearing rather than
        // defence-in-depth; this is the second line.
        let (mut f, id) = drafted_fixture("api-content-type");
        let admin = f.as_admin();

        // A form-shaped POST, exactly what a cross-origin page could send.
        let form = f.go(
            &Req::post(&format!("/api/v1/ui/requests/{id}/discard"), "note=x").with_cookie(&admin),
        );
        assert_eq!(form.status, 415, "{}", form.body);
        assert_ne!(
            f.store.get(&id).unwrap().unwrap().state,
            RequestState::Discarded,
            "and nothing happened"
        );
    }

    #[test]
    fn a_mutating_call_from_another_origin_is_refused() {
        let (mut f, id) = drafted_fixture("api-origin");
        let admin = f.as_admin();

        let mut req = api_post(&format!("/api/v1/ui/requests/{id}/discard"), "{}");
        req.origin = Some("https://evil.example".into());
        let res = f.go(&req.with_cookie(&admin));
        assert_eq!(res.status, 403, "{}", res.body);

        // And this server's own origin is fine.
        let mut ours = api_post(&format!("/api/v1/ui/requests/{id}/discard"), "{}");
        ours.origin = Some("https://specs.example.test".into());
        assert_eq!(f.go(&ours.with_cookie(&admin)).status, 200);
    }

    #[test]
    fn a_verb_answers_with_the_request_as_it_now_stands() {
        // The HTML surface re-rendered the page after a POST; there are no
        // redirects anywhere in this crate. The API returns the mutated record
        // for the same reason — one round trip, and the client never has to
        // guess what changed.
        let (mut f, id) = drafted_fixture("api-verb-echo");
        let admin = f.as_admin();

        let res = f.go(&api_post(
            &format!("/api/v1/ui/requests/{id}/send-back"),
            "{\"note\":\"more detail please\"}",
        )
        .with_cookie(&admin));
        assert_eq!(res.status, 200, "{}", res.body);
        let v: serde_json::Value = serde_json::from_str(&res.body).unwrap();
        assert_eq!(v["id"], id.as_str());
        assert_eq!(v["state"], "queued", "the state it is now in: {}", res.body);
    }

    #[test]
    fn the_interface_can_load_its_own_stylesheet() {
        // **A failure no `curl` can see.** The header is present and looks right
        // either way; only a browser refuses the stylesheet, and the result is
        // an unstyled page rather than an error anybody would notice in a test
        // that only checks status codes.
        //
        // The rendered pages inline their CSS and need `'unsafe-inline'`; the
        // interface ships a bundled file and needs `'self'`. Both are in there,
        // and this is what says so.
        let csp = Policy::PublicScript.csp();
        assert!(csp.contains("style-src 'self' 'unsafe-inline'"), "{csp}");
        assert!(csp.contains("script-src 'self'"), "{csp}");
    }

    #[test]
    fn the_interfaces_files_are_reachable_signed_out() {
        // A stranger has to load the page that offers them a way in. If the
        // bundle were behind the sign-in it guards, nobody could ever reach it.
        let mut f = Fixture::new("ui-assets").with_public(false);
        for path in [crate::api::ui::SCRIPT_PATH, crate::api::ui::STYLE_PATH] {
            let res = f.go(&Req::get(path));
            assert_eq!(res.status, 200, "{path}");
            assert_eq!(res.policy, Policy::PublicScript, "{path}");
        }
    }

    #[test]
    fn the_interface_answers_only_the_paths_it_owns() {
        // **An allowlist, not a catch-all.** Answering every unmatched path with
        // the document would turn a mistyped API call into a 200 holding HTML,
        // and a client could no longer tell "no such request" from "here is the
        // application again".
        assert!(wants_document(public_route::LANDING));
        assert!(wants_document(public_route::FILE));
        assert!(wants_document("/public/request/r-1"));
        assert!(!wants_document("/api/v1/ui/nope"));
        assert!(!wants_document("/ui/app.js"));

        // **The administrative addresses answer the document too**, and that is
        // a deliberate change from when this test was written. Serving it grants
        // nothing: every API call behind these is still a `Caller::Admin` check,
        // and the menu is drawn from `/me`, so a stranger is shown no door. What
        // it buys is that typing `/settings` and navigating to it from inside
        // the application behave the same.
        assert!(wants_document(private_route::SETTINGS));
        assert!(wants_document(private_route::REVIEW));
        assert!(wants_document("/request/r-1"));
    }

    #[test]
    fn the_interfaces_files_are_read_budget_not_guess_budget() {
        // **Found by the browser harness, not by a unit test.** These were
        // falling through to `AnonPrivate` — 20/min, the bucket for credential
        // guessing — because they are not on the public path list. A page load
        // fetches three things, so a reader who reloaded a few times got a 429
        // on their *stylesheet* and saw an unstyled page.
        //
        // Nothing that checks status codes catches that: the document itself
        // was 200 every time. It took a browser noticing the page had no
        // background.
        for path in [crate::api::ui::SCRIPT_PATH, crate::api::ui::STYLE_PATH] {
            assert_eq!(bucket_for(&None, path), Bucket::PublicRead, "{path}");
        }
    }

    #[test]
    fn a_minted_daemon_key_is_returned_once_and_never_stored() {
        // **The response is the only copy.** The volume holds a hash, so nothing
        // can read it back — which is what makes `Cache-Control: no-store` on
        // every response load-bearing rather than tidy.
        let mut f = Fixture::new("api-mint").with_public(false);
        let admin = f.as_admin();

        let res =
            f.go(&api_post("/api/v1/ui/daemons", "{\"label\":\"laptop\"}").with_cookie(&admin));
        assert_eq!(res.status, 200, "{}", res.body);
        let v: serde_json::Value = serde_json::from_str(&res.body).unwrap();
        let key = v["key"].as_str().expect("a key came back");
        assert!(key.len() >= 32, "and it is a real one: {key}");

        // Never on the volume in the clear.
        let raw = std::fs::read_to_string(f.store.roster_path()).unwrap();
        assert!(!raw.contains(key), "the key was written down: {raw}");

        // And listing the machines afterwards does not include it.
        let list = f.go(&Req::get("/api/v1/ui/daemons").with_cookie(&admin));
        assert!(!list.body.contains(key), "{}", list.body);
    }

    #[test]
    fn a_repository_nothing_serves_is_refused_unless_forced() {
        // Naming one no daemon offers produces a queue that never drains, and
        // the operator finds out when somebody files into it. Refused with a
        // conflict rather than silently accepted — and overridable, because the
        // daemon may simply not have polled yet.
        let mut f = Fixture::new("api-repo-unserved").with_public(false);
        let admin = f.as_admin();

        let refused =
            f.go(&api_post("/api/v1/ui/repos", "{\"name\":\"nothing\"}").with_cookie(&admin));
        assert_eq!(refused.status, 409, "{}", refused.body);

        let forced = f.go(
            &api_post("/api/v1/ui/repos", "{\"name\":\"nothing\",\"anyway\":true}")
                .with_cookie(&admin),
        );
        assert_eq!(forced.status, 200, "{}", forced.body);
    }

    #[test]
    fn the_administrative_writes_do_not_exist_for_anybody_else() {
        // 404, never 401 — the same answer the read endpoints give.
        let mut f = Fixture::new("api-write-gate").with_public(false);
        let filer = f.signed_in("jo@x.com");

        for (path, body) in [
            ("/api/v1/ui/settings", "{\"site_name\":\"x\"}"),
            ("/api/v1/ui/owners", "{\"login\":\"jo@x.com\"}"),
            ("/api/v1/ui/repos", "{\"name\":\"x\"}"),
            ("/api/v1/ui/daemons", "{\"label\":\"x\"}"),
            ("/api/v1/ui/accounts/a-1/revoke", "{}"),
        ] {
            assert_eq!(
                f.go(&api_post(path, body)).status,
                404,
                "signed out: {path}"
            );
            assert_eq!(
                f.go(&api_post(path, body).with_cookie(&filer)).status,
                404,
                "a filer: {path}"
            );
        }
    }

    #[test]
    fn an_owner_can_be_named_and_revoked_through_the_api() {
        let mut f = Fixture::new("api-owners")
            .with_public(false)
            .with_repos(&["intake"]);
        let admin = f.as_admin();

        let added = f.go(&api_post(
            "/api/v1/ui/owners",
            "{\"login\":\"jo@x.com\",\"repos\":[\"intake\"]}",
        )
        .with_cookie(&admin));
        assert_eq!(added.status, 200, "{}", added.body);
        assert!(added.body.contains("jo@x.com"), "{}", added.body);

        let revoked =
            f.go(&api_post("/api/v1/ui/owners/jo@x.com/revoke", "{}").with_cookie(&admin));
        assert_eq!(revoked.status, 200, "{}", revoked.body);
        // **Kept, not deleted.** A list that silently shrinks cannot answer "did
        // I already deal with that?".
        assert!(revoked.body.contains("jo@x.com"), "{}", revoked.body);
        assert!(
            revoked.body.contains("\"revoked\":true"),
            "{}",
            revoked.body
        );
    }

    #[test]
    fn the_wizard_claims_a_server_through_the_api() {
        // **The whole first-run path**, end to end. The code proves you can read
        // the container's log; the step after it sets the credential that will
        // own the server, and is bound to the browser that spent the code.
        let mut f = Fixture::new("api-setup").with_public(false);
        armed(&mut f, "ABC-123");

        // Before anything: the wizard says which step this browser may take.
        let state = f.go(&Req::get("/api/v1/ui/setup"));
        assert_eq!(state.status, 200, "{}", state.body);
        let v: serde_json::Value = serde_json::from_str(&state.body).unwrap();
        assert_eq!(v["step"], "code");

        // Step one.
        let spent = f.go(&api_post(
            "/api/v1/ui/setup/code",
            "{\"code\":\"ABC-123\",\"base_url\":\"https://specs.example.test\"}",
        ));
        assert_eq!(spent.status, 200, "{}", spent.body);
        let setup = spent
            .set_cookie
            .as_deref()
            .and_then(|c| c.strip_prefix(&format!("{SETUP_COOKIE}=")))
            .and_then(|c| c.split(';').next())
            .expect("a setup token was issued")
            .to_string();
        // Nobody owns it yet: spending the code is one proof, not the claim.
        assert!(!f.store.admin().unwrap().claimed());

        // The state now reflects the browser holding the token.
        let state = f.go(&Req::get("/api/v1/ui/setup").with_setup(&setup));
        let v: serde_json::Value = serde_json::from_str(&state.body).unwrap();
        assert_eq!(v["step"], "admin");

        // Step two.
        let claimed = f.go(&api_post(
            "/api/v1/ui/setup/admin",
            "{\"login\":\"JameZ667@example.test\",\"password\":\"correct-horse-battery\"}",
        )
        .with_setup(&setup));
        assert_eq!(claimed.status, 200, "{}", claimed.body);
        assert!(
            f.store.admin().unwrap().is("jamez667@example.test"),
            "lowercased on the way in"
        );

        // The password is stored hashed and never rendered back.
        assert!(!claimed.body.contains("correct-horse-battery"));
        let raw = std::fs::read_to_string(f.store.accounts_path()).unwrap();
        assert!(!raw.contains("correct-horse-battery"), "{raw}");
        assert!(raw.contains("$argon2id$"), "and it is the slow hash");

        // And they are signed in already — they just chose the credential.
        let session = cookie_token(&claimed).expect("signed in");
        assert_eq!(
            f.go(&Req::get("/api/v1/ui/settings").with_cookie(&session))
                .status,
            200
        );

        // The wizard is gone.
        assert_eq!(f.go(&Req::get("/api/v1/ui/setup")).status, 404);
    }

    #[test]
    fn every_wrong_code_through_the_api_gets_the_same_answer() {
        // Wrong, expired and already spent are three different facts, and
        // telling them apart tells a guesser which half they got right.
        let mut f = Fixture::new("api-setup-codes").with_public(false);
        armed(&mut f, "ABC-123");

        let wrong = f.go(&api_post(
            "/api/v1/ui/setup/code",
            "{\"code\":\"XYZ-999\",\"base_url\":\"https://specs.example.test\"}",
        ));
        // Spend the real one, then try it again.
        f.go(&api_post(
            "/api/v1/ui/setup/code",
            "{\"code\":\"ABC-123\",\"base_url\":\"https://specs.example.test\"}",
        ));
        let spent = f.go(&api_post(
            "/api/v1/ui/setup/code",
            "{\"code\":\"ABC-123\",\"base_url\":\"https://specs.example.test\"}",
        ));
        assert_eq!(wrong.status, 400);
        assert_eq!(spent.status, 400);
        assert_eq!(wrong.body, spent.body, "one answer, not two");
    }

    #[test]
    fn a_half_finished_setup_cannot_be_taken_over_through_the_api() {
        // **The hole this closes, and it was live on the pages.** Setup is more
        // than one step and the code is spent at the first, so everything after
        // it was guarded only by the server being unclaimed. Choosing the
        // password decides who owns the server — an interloper who reached step
        // two would have set their own and taken it.
        let mut f = Fixture::new("api-setup-interloper").with_public(false);
        armed(&mut f, "ABC-123");

        f.go(&api_post(
            "/api/v1/ui/setup/code",
            "{\"code\":\"ABC-123\",\"base_url\":\"https://specs.example.test\"}",
        ));

        // Somebody else, holding no token, tries to finish it.
        let stolen = f.go(&api_post(
            "/api/v1/ui/setup/admin",
            "{\"login\":\"interloper@x.com\",\"password\":\"correct-horse-battery\"}",
        ));
        assert_eq!(stolen.status, 400, "{}", stolen.body);
        assert!(!f.store.admin().unwrap().claimed(), "and nobody owns it");
    }

    #[test]
    fn the_wizard_does_not_exist_once_the_server_is_claimed() {
        // 404 rather than a refusal: a stranger must not be able to tell a
        // claimed server from one that never had a wizard.
        let mut f = Fixture::new("api-setup-gone").with_public(false);
        f.as_admin();

        assert_eq!(f.go(&Req::get("/api/v1/ui/setup")).status, 404);
        assert_eq!(
            f.go(&api_post(
                "/api/v1/ui/setup/code",
                "{\"code\":\"ABC-123\",\"base_url\":\"https://x.test\"}"
            ))
            .status,
            404
        );
    }

    #[test]
    fn setting_up_over_plain_http_does_not_send_a_secure_cookie() {
        // **Found in a browser, not here.** A server being claimed has no public
        // surface, so `secure_attr` falls back to `Secure` — and over
        // `http://127.0.0.1` the browser silently discards both the setup token
        // and the session that follows. The wizard loops back to step one, and
        // the claim signs the reader straight out.
        //
        // Its own doc predicted exactly this: "the request succeeds, the cookie
        // is dropped, and the next page has forgotten. That reads as a bug in
        // the feature." It was one, on the only flow with no fallback.
        //
        // The address is the source of truth during setup, and it has already
        // been validated — plain HTTP is permitted only for a private host.
        assert_eq!(secure_attr_for("http://127.0.0.1:8420"), "");
        assert_eq!(secure_attr_for("http://192.168.1.9:8420"), "");
        assert_eq!(secure_attr_for("https://specs.example.test"), "; Secure");

        // End to end: the cookie a local trial actually receives.
        // **`with_public` gives the fixture an address**, which is where the
        // answer comes from now — the settings no longer hold one. Its base URL
        // is `https://…`, so this asserts that case; the plain-HTTP one is the
        // three direct calls above.
        let mut f = Fixture::new("setup-secure").with_public(false);
        armed(&mut f, "ABC-123");
        let spent = f.go(&api_post("/api/v1/ui/setup/code", "{\"code\":\"ABC-123\"}"));
        let cookie = spent.set_cookie.as_deref().expect("a setup token");
        assert!(
            cookie.contains("Secure"),
            "an https address gets it: {cookie}"
        );
    }

    #[test]
    fn the_wizard_is_reachable_before_the_server_has_an_address() {
        // **The origin check refused the flow that gives the server an origin.**
        // `same_origin` compared against the configured address, a fresh volume
        // has none, and an empty one matched nothing — so every setup POST was
        // 403 and the wizard could not complete. Found in a browser: the page
        // showed "cross-origin" and stayed on step one.
        //
        // The exemption is narrow. Only `setup/` is allowed through, and only
        // while there is no address to check against.
        // **No `with_public`**, because that builder gives the fixture an
        // address — and an address is exactly what a server being set up does
        // not have yet. Using it would test the case that already worked.
        let mut f = Fixture::new("setup-origin");
        armed(&mut f, "ABC-123");

        let mut req = api_post(
            "/api/v1/ui/setup/code",
            "{\"code\":\"ABC-123\",\"base_url\":\"http://127.0.0.1:8420\"}",
        );
        req.origin = Some("http://127.0.0.1:8420".into());
        assert_eq!(f.go(&req).status, 200, "the wizard is reachable");

        // And nothing else is: an unconfigured server is not an open one.
        let mut other = api_post("/api/v1/ui/daemons", "{\"label\":\"x\"}");
        other.origin = Some("https://evil.example".into());
        assert_ne!(f.go(&other).status, 200);
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
        // knew the URL — the same failure the sign-in flow had, and it goes
        // unnoticed for the same reason: a test asks for a route directly,
        // which is exactly what a person cannot do.
        //
        // **The server no longer draws the menu, so it can no longer be
        // asserted from the markup.** The client builds its own navigation, and
        // what it builds it from is `can.administer` plus an endpoint per page.
        // So the property becomes: the administrator is told they administer,
        // and every administrative address has a working endpoint behind it. A
        // page added with no endpoint is still caught — it has nothing to draw.
        let mut f = Fixture::new("admin-nav").with_public(false);
        let admin = f.as_admin();

        let me = f.go(&Req::get(ME_PATH).with_cookie(&admin));
        assert!(
            me.body.contains("\"administer\":true"),
            "the administrator is not told they administer: {}",
            me.body
        );

        // One endpoint per administrative page, named from the page's own
        // constant so a page added later has to be added here too.
        for page in ADMIN_PAGES {
            let endpoint = match page {
                private_route::REVIEW => "/api/v1/ui/requests".to_string(),
                other => format!("/api/v1/ui{other}"),
            };
            let res = f.go(&Req::get(&endpoint).with_cookie(&admin));
            assert_eq!(
                res.status, 200,
                "{page} has no endpoint behind it at {endpoint}: {}",
                res.body
            );
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
        // guarded only by the server being unclaimed. Choosing the password
        // decides which account can finish the claim — so an interloper who
        // reached step two would have set their own and
        // owned the server.
        //
        // It bit hardest on a MIGRATED volume, where seeding fills in the
        // address: "step one is already done" was then true for everybody, from
        // the first boot, with no code ever spent.
        let mut f = Fixture::new("setup-hijack").with_public(false);
        armed(&mut f, "ABC-123");

        let res = spend_code(&mut f, "ABC-123");
        assert_eq!(res.status, 200, "{}", res.body);
        let setup = setup_token(&res);

        // Somebody else, arriving with no cookie, is sent back to the code box
        // rather than shown the step that hands the server over.
        //
        // **The step is the API's answer now, not a rendered form.** `/setup`
        // serves the interface's document to anybody; which step a browser may
        // take is decided by whether it holds the token, and the server says so
        // explicitly rather than leaving the client to infer it from HTML.
        let theirs = f.go(&Req::get(&api_path("setup")));
        assert_eq!(theirs.status, 200, "{}", theirs.body);
        assert!(
            theirs.body.contains("\"step\":\"code\""),
            "a stranger was shown a later step: {}",
            theirs.body
        );

        // And cannot post to it.
        let res = f.go(&claim_as("theirs", "another-good-password"));
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(
            !f.store.admin().unwrap().claimed(),
            "a stranger claimed the server"
        );

        // A wrong token is no better than none.
        let res = f.go(&claim_as("theirs", "another-good-password").with_setup("not-the-token"));
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(!f.store.admin().unwrap().claimed());

        // The browser that spent the code still finishes normally.
        let res = f.go(&claim_as("mine@example.test", "correct-horse-battery").with_setup(&setup));
        assert_eq!(res.status, 200, "{}", res.body);
        assert!(f.store.admin().unwrap().claimed());
    }

    #[test]
    fn a_seeded_volume_does_not_look_half_set_up_to_a_stranger() {
        // The migration case specifically: an upgraded server already has an
        // address, and that must not be mistaken for "somebody is part-way
        // through claiming this".
        let mut f = Fixture::new("setup-seeded").with_public(false);
        armed(&mut f, "ABC-123");

        let mut settings = f.store.settings().unwrap();
        settings.seeded = true;
        f.store.put_settings(&settings).unwrap();

        // **Asked of the setup API**, which is where the step lives now that
        // every browser path answers the interface's document.
        let res = f.go(&Req::get("/api/v1/ui/setup"));
        assert_eq!(res.status, 200, "{}", res.body);
        assert!(
            res.body.contains("\"step\":\"code\""),
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
        let res = spend_code(&mut f, "ABC-123");
        let setup = setup_token(&res);

        f.now_ms += crate::admin::CLAIM_TTL_MS;
        let res = f.go(&claim_as("late", "correct-horse-battery").with_setup(&setup));
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(!f.store.admin().unwrap().claimed());
    }

    // **`a_bad_address_does_not_burn_the_claim_code` was here, and is deleted
    // rather than retargeted.** It sent a valid code alongside a plain-HTTP
    // address, asserted the step was refused for the address, and then proved the
    // code was still spendable — the scarce thing surviving a typo in the thing
    // beside it. **Step one no longer takes an address.** It is an environment
    // variable and the server refuses to start without one that passes
    // `check_base_url`, so there is no field left to typo and no second reason
    // for step one to fail. The subject is gone, not merely relocated.
    //
    // The property underneath it — a refused step one leaves the code usable —
    // is still asserted, by `a_wrong_code_says_one_thing_and_leaves_the_real_one_
    // usable` immediately below: it spends a wrong code, then the right one, and
    // requires the right one to work. That is now the only way step one can fail,
    // so the coverage is complete rather than reduced. What the address itself
    // must satisfy is checked at startup instead — `check_base_url` in
    // `crate::config`, where its own tests live.

    #[test]
    fn a_wrong_code_says_one_thing_and_leaves_the_real_one_usable() {
        // One message for wrong, expired and already-spent alike: distinguishing
        // them tells a guesser which half they got right. And a wrong guess must
        // not spend somebody else's code, or a stranger who cannot read the log
        // could still deny the claim to the person who can.
        let mut f = Fixture::new("setup-wrong-code").with_public(false);
        armed(&mut f, "ABC-123");

        let res = spend_code(&mut f, "WRONG-1");
        assert_eq!(res.status, 400);
        let wrong = res.body.clone();

        // Expired and already-spent give that same answer, which is what makes
        // the wording carry no information: the code above never existed.
        assert!(
            wrong.contains("that code was not accepted"),
            "one message for every failure: {wrong}"
        );

        let res = spend_code(&mut f, "ABC-123");
        assert_eq!(res.status, 200, "the real code still works: {}", res.body);

        // And now that it *is* spent, trying it again gives the same answer a
        // wrong one did.
        let again = spend_code(&mut f, "ABC-123");
        assert_eq!(again.status, 400);
        assert_eq!(
            again.body, wrong,
            "an already-spent code is distinguishable from a wrong one"
        );
    }

    #[test]
    fn the_setup_step_decides_secure_from_the_address_rather_than_asking() {
        // Derived, not asked. "Is this a private network" is a question people
        // answer wrong, and answering it wrong drops `Secure` from every session
        // cookie without a word.
        //
        // **Asserted on the decision, not on a sentence.** The wizard used to
        // render a paragraph explaining which way it had gone, and this read that
        // paragraph. The page is gone and the interface says it now; what has to
        // stay true is the decision itself, which is one function and testable
        // directly — a stronger subject than the prose that described it.
        assert_eq!(secure_attr_for("https://specs.example.test"), "; Secure");
        assert_eq!(secure_attr_for("http://localhost:8420"), "");
    }

    #[test]
    fn a_claimed_server_has_no_setup_and_arms_no_code() {
        // Otherwise every restart would print a fresh key to the
        // administrator's own front door.
        let mut f = Fixture::new("setup-claimed").with_public(false);
        let mut admin = f.store.admin().unwrap();
        admin.claim("jamez667@example.test", 1);
        f.store.put_admin(&admin).unwrap();

        // **Asked of the setup API.** `/setup` answers the interface's document
        // like every other browser path — the wizard's own endpoints are what
        // stop existing once the server is claimed, and 404 rather than a
        // refusal is what keeps a claimed server indistinguishable from one
        // that never had a wizard.
        assert_eq!(f.go(&Req::get(&api_path("setup"))).status, 404);
        assert_eq!(
            f.go(&claim_as("a", "correct-horse-battery")).status,
            404,
            "the step that hands the server over still existed"
        );
        assert_eq!(
            spend_code(&mut f, "ABC-123").status,
            404,
            "and so did the one before it"
        );
    }

    /// An owner signed in and holding a session cookie, with a request filed
    /// against each of two repositories.
    /// A public surface with one request filed by one filer.
    ///
    /// Returns the fixture, the filer's session, and the request id. Filed
    /// through the real route rather than written to the store, so the record
    /// carries whatever filing actually sets — including `account_id`, which is
    /// what every "is this mine" check keys on.
    fn filed_fixture(tag: &str) -> (Fixture, String, String) {
        let mut f = Fixture::new(tag).with_public(false).with_repos(&["intake"]);
        let filer = f.signed_in("jo@x.com");
        file_publicly_as(&mut f, &filer, "please fix the thing", "intake");
        let id = f.store.all().unwrap()[0].id.clone();
        (f, filer, id)
    }

    /// File as a signed-in filer, through the endpoint the interface uses.
    ///
    /// **`assert`s the status here rather than at the call site.** These fixtures
    /// exist so a test can start from "a request exists"; a filing that silently
    /// did not happen turns every assertion afterwards into a confusing failure
    /// about an empty store.
    fn file_publicly_as(f: &mut Fixture, session: &str, text: &str, repo: &str) {
        let body = serde_json::json!({ "text": text, "kind": "bug", "repo": repo });
        let res = f.go(&Req::post_json(&api_path("file"), &body.to_string()).with_cookie(session));
        assert_eq!(res.status, 200, "filing {repo}: {}", res.body);
    }

    /// File as a signed-in filer and hand back the answer, whatever it is.
    ///
    /// The sibling of [`file_publicly_as`] for the tests where **being refused
    /// is the point** — every cap in this file reaches its ceiling and then
    /// inspects the refusal, so those cannot use a helper that asserts success.
    fn try_filing_as(f: &mut Fixture, session: &str, text: &str) -> Res {
        let body = serde_json::json!({ "text": text, "kind": "bug" });
        f.go(&Req::post_json(&api_path("file"), &body.to_string()).with_cookie(session))
    }

    /// The same, with the request held by the screener.
    fn quarantined_fixture(tag: &str) -> (Fixture, String, String) {
        let (f, filer, id) = filed_fixture(tag);
        let mut r = f.store.get(&id).unwrap().unwrap();
        r.state = RequestState::Quarantined;
        f.store.put(&r).unwrap();
        (f, filer, id)
    }

    /// The same, with a drafted spec awaiting review.
    fn drafted_fixture(tag: &str) -> (Fixture, String) {
        let (f, _filer, id) = filed_fixture(tag);
        let mut r = f.store.get(&id).unwrap().unwrap();
        r.state = RequestState::AwaitingReview;
        r.spec = Some("# A spec\n\nSomething a model wrote.".to_string());
        r.artifact_dir = Some("/home/dev/specs/r-1".to_string());
        f.store.put(&r).unwrap();
        (f, id)
    }

    fn owner_fixture(tag: &str) -> (Fixture, String, String, String) {
        let mut f = Fixture::new(tag)
            .with_public(false)
            .with_repos(&["intake", "other"])
            .with_owner("jamez667@example.test", &["intake"]);
        let owner = f.signed_in_with_login("jamez667@example.test");

        let filer = f.signed_in("jo@x.com");
        file_publicly_as(&mut f, &filer, "for intake", "intake");
        file_publicly_as(&mut f, &filer, "for other", "other");

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

            let res = verb_on(&mut f, &owner, &mine, "send-back", r#"{"note":"again"}"#);
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
        let res = verb_on(&mut f, &owner, &mine, "send-back", r#"{"note":"again"}"#);
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

        // **The list comes from the API.** The pages all answer the interface's
        // document, so what an owner may see is decided by what the review
        // endpoint hands over — filtered to their repositories, which is the
        // property under test.
        let listed = f
            .go(&Req::get("/api/v1/ui/requests").with_cookie(&owner))
            .body;
        assert!(listed.contains("for intake"), "their own: {listed}");
        assert!(
            !listed.contains("for other"),
            "not somebody else's: {listed}"
        );

        // And by id, both directions.
        assert_eq!(
            f.go(&Req::get(&format!("/api/v1/ui/requests/{mine}")).with_cookie(&owner))
                .status,
            200
        );
        assert_eq!(
            f.go(&Req::get(&format!("/api/v1/ui/requests/{theirs}")).with_cookie(&owner))
                .status,
            404,
            "not found rather than forbidden — a 403 confirms the id is real"
        );
    }

    #[test]
    fn an_owner_can_decline_and_is_never_offered_approve() {
        // Refused on the wire, and **not offered to the client either**.
        //
        // **Renamed off "the page", which is gone.** The rendered review page
        // decided by construction which buttons an owner saw, because the server
        // built the markup; the client builds it now, so the same decision has to
        // be a fact the server states. `can.accept` is that fact, and for an
        // owner it is false — a client drawing an approve anyway would be drawing
        // one the server refuses, which is the next assertion down.
        let (mut f, owner, mine, _) = owner_fixture("owner-declines");

        // Get it to a state where a decision is possible.
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        let payload = serde_json::to_string(&DraftedSpec::new(&mine, "# Spec", "specs/x")).unwrap();
        f.go(&Req::post(&wire::route::drafted(&mine), &payload).with_bearer(KEY));

        let me = f.go(&Req::get(ME_PATH).with_cookie(&owner));
        assert!(me.body.contains("\"review\":true"), "{}", me.body);
        assert!(
            me.body.contains("\"accept\":false"),
            "an owner was offered approve: {}",
            me.body
        );
        // And the request is theirs to read, which is what makes deciding on it
        // possible at all.
        assert_eq!(
            f.go(&Req::get(&api_path(&format!("requests/{mine}"))).with_cookie(&owner))
                .status,
            200
        );

        // **And the server backs the flag up.** Accepting is not merely undrawn
        // for an owner, it does not exist — 404 rather than a refusal, because
        // `api_verb`'s owner arm has no `accept` case at all.
        let digest = crate::auth::hash("# Spec");
        let refused = verb_on(
            &mut f,
            &owner,
            &mine,
            "accept",
            &serde_json::json!({ "digest": digest }).to_string(),
        );
        assert_eq!(refused.status, 404, "an owner accepted: {}", refused.body);
        assert_eq!(
            f.store.get(&mine).unwrap().unwrap().state,
            crate::store::RequestState::AwaitingReview,
            "and nothing was settled"
        );

        // And the verb they do have works.
        let res = verb_on(
            &mut f,
            &owner,
            &mine,
            "send-back",
            r#"{"note":"too vague"}"#,
        );
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

        // **Reachable by the interface, not merely accepted on the wire** — a
        // verb the client cannot see is one nobody uses. The page carries no
        // buttons any more, so what stands in for "offered" is that the owner
        // is told they review and is handed the request the verb acts on.
        let me = f.go(&Req::get(ME_PATH).with_cookie(&owner));
        assert!(me.body.contains("\"review\":true"), "{}", me.body);
        let one = f.go(&Req::get(&api_path(&format!("requests/{mine}"))).with_cookie(&owner));
        assert_eq!(one.status, 200, "{}", one.body);
        assert!(
            one.body.contains("quarantined"),
            "the state a release acts on: {}",
            one.body
        );

        let res = verb_on(&mut f, &owner, &mine, "release", "{}");
        assert_eq!(res.status, 200, "{}", res.body);
        assert_eq!(
            f.store.get(&mine).unwrap().unwrap().state,
            RequestState::Queued,
            "released into the claimable queue"
        );

        // Somebody else's repository stays quarantined, and says not found
        // rather than forbidden.
        let res = verb_on(&mut f, &owner, &theirs, "release", "{}");
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

        let res = verb_on(&mut f, &owner, &mine, "release", "{}");
        assert_eq!(res.status, 429, "{}", res.body);
        assert_eq!(
            f.store.get(&mine).unwrap().unwrap().state,
            RequestState::Quarantined,
            "refused, and nothing moved"
        );
    }

    #[test]
    fn an_owner_cannot_decline_somebody_elses_repository() {
        // **Pointed at the API, or it would assert nothing.** The owner verbs
        // used to live under `/public/request/`, and every address under that
        // prefix answers 404 now — so left where it was, this would pass on a
        // server that had no ownership check at all. The check lives in
        // `api_verb`'s owner arm, and that is what this asks.
        let (mut f, owner, _, theirs) = owner_fixture("owner-not-theirs");
        for verb in OWNER_VERBS {
            let res = verb_on(&mut f, &owner, &theirs, verb, "{}");
            assert_eq!(res.status, 404, "an owner reached {verb} on another repo");
        }
        // Not found, not forbidden — and nothing moved.
        assert_eq!(
            f.store.get(&theirs).unwrap().unwrap().state,
            RequestState::Queued
        );
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
        assert!(
            !OWNER_VERBS.contains(&"accept"),
            "accept settles a request and is not an owner's to reach"
        );
    }

    #[test]
    fn an_owner_cannot_reach_a_repository_they_do_not_own() {
        // **The property this role turns on**, and it is still structural rather
        // than a check somebody has to write.
        //
        // **Renamed off "the private surface", which no longer exists.** The
        // argument used to be about a route tree: `Caller::Owner` did not match
        // the pattern the private surface was gated on, so every handler past
        // that line was unreachable by type. The tree is gone and the reasoning
        // moved intact into `api_verb`, which matches on the variant — the owner
        // arm reaches a repository they own and nothing else, and has no `accept`
        // case in it at all.
        //
        // Iterates the shared constant, so a verb added later is covered without
        // anyone remembering to extend this list. The repository here is one they
        // were never named for, which is the case a bad gate lets through.
        let mut f = Fixture::new("owner-no-approve")
            .with_public(false)
            .with_repos(&["intake", "other"])
            .with_owner("jamez667@example.test", &["other"]);
        let owner = f.signed_in_with_login("jamez667@example.test");

        let filer = f.signed_in("jo@x.com");
        file_publicly_as(&mut f, &filer, "a thing", "intake");
        let id = f.store.all().unwrap()[0].id.clone();

        for verb in REVIEW_VERBS {
            let res = verb_on(&mut f, &owner, &id, verb, "{}");
            assert_eq!(res.status, 404, "an owner reached {verb}: {}", res.body);
        }
        assert_eq!(
            f.store.get(&id).unwrap().unwrap().state,
            RequestState::Queued,
            "and nothing moved"
        );
        // And the developer's own administrative surface is not theirs either.
        // **Asked of the API**, which is where that refusal lives now that every
        // browser path answers the interface's document.
        assert_eq!(
            f.go(&Req::get(&api_path("settings")).with_cookie(&owner))
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
        let session = f.signed_in_with_login("jamez667@example.test");

        // **Asked of `/me`, which is the roster's answer made explicit.** The
        // browser paths all serve the interface's document, so what says whether
        // this session is an owner is the capability the server reports and the
        // review list it will actually hand over — both of which come from the
        // roster and neither of which a login alone can move.
        let me = f.go(&Req::get(ME_PATH).with_cookie(&session));
        assert!(me.body.contains("\"filer\""), "a login alone: {}", me.body);
        assert!(
            me.body.contains("\"review\":false"),
            "a login alone granted review: {}",
            me.body
        );
        let listed = f.go(&Req::get(&api_path("requests")).with_cookie(&session));
        assert_eq!(listed.body, "[]", "a login alone reviews nothing");
    }

    #[test]
    fn revoking_an_owner_demotes_a_live_session_on_the_very_next_request() {
        // **The property that had to survive the move off configuration.**
        // Deleting a line and redeploying was complete revocation — no session
        // to hunt down, no record that might disagree. The roster is a record,
        // so this is the test that says the mtime cache actually carries it.
        let (mut f, session, _mine, _theirs) = owner_fixture("owner-demote");

        // An owner: somebody else's filing for their repository is in the list
        // the API hands them. **Read from the API**, because the pages answer
        // the interface's document to anybody and so say nothing about role.
        let listed = f
            .go(&Req::get("/api/v1/ui/requests").with_cookie(&session))
            .body;
        assert!(listed.contains("for intake"), "an owner sees it: {listed}");
        // And even so, the administrative surface stays closed to them.
        assert_eq!(
            f.go(&Req::get("/api/v1/ui/settings").with_cookie(&session))
                .status,
            404
        );

        // The developer revokes them.
        let mut roster = f.store.roster().unwrap();
        assert!(roster.revoke("jamez667@example.test"));
        f.store.put_roster(&roster).unwrap();

        // **The next request**, with no restart and no redeploy. A cache that
        // held a startup snapshot would still say owner here, which is exactly
        // the failure the move off configuration must not introduce.
        let listed = f
            .go(&Req::get("/api/v1/ui/requests").with_cookie(&session))
            .body;
        assert!(
            !listed.contains("for intake"),
            "a revoked owner still reviews: {listed}"
        );
        // Demoted, not signed out. They were an account before they were an
        // owner, and revocation returns them to being one — so `/me` still
        // names them a filer rather than a stranger, and they may still file.
        let me = f.go(&Req::get(ME_PATH).with_cookie(&session));
        assert!(
            me.body.contains("\"filer\""),
            "signed out, not demoted: {}",
            me.body
        );
        assert!(me.body.contains("\"file\":true"), "{}", me.body);
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
            .with_owner("jamez667@example.test", &["something-else"]);
        let owner = f.signed_in_with_login("jamez667@example.test");

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
            r#"{"login":"jamez667@example.test","repos":["intake","other"]}"#,
            r#"{"login":"accomplice","repos":["intake"]}"#,
        ] {
            let res = f.go(&Req::post_json(&api_path("owners"), body).with_cookie(&session));
            // **404, not 401.** The gate refuses before `set_owner` is ever
            // reached — which is the point; there is no check inside the handler
            // that a later edit could drop — and the answer says the endpoint
            // does not exist for them rather than that they are the wrong person
            // at a real address.
            assert_eq!(
                res.status, 404,
                "an owner reached the roster with {body}: {}",
                res.body
            );
        }
        // The roster is untouched: no accomplice, and no second repository.
        let roster = f.store.roster().unwrap();
        assert!(roster.owner_for("accomplice").is_none());
        assert_eq!(
            roster.owner_for("jamez667@example.test").unwrap().repos,
            ["intake"]
        );
    }

    #[test]
    fn the_developer_promotes_and_revokes_owners() {
        // **Renamed off "the owners page".** The page is the interface's document
        // like every other address; promoting and revoking are two endpoints, and
        // they are what this asserts.
        let mut f = Fixture::new("owners-admin")
            .with_public(false)
            .with_repos(&["intake", "other"]);
        let device = f.as_admin();

        // **Two repositories in one call, which is the case that used to bite.**
        // Ticked checkboxes arrived as a repeated form field and a map would have
        // kept only the last, granting one of the two — hence `form_values`. A
        // JSON array cannot lose an element that way, so the hazard is gone by
        // construction; what still has to hold is that both are granted.
        let res = f.go(&Req::post_json(
            &api_path("owners"),
            r#"{"login":"JameZ667@example.test","repos":["intake","other"]}"#,
        )
        .with_cookie(&device));
        assert_eq!(res.status, 200, "{}", res.body);
        let roster = f.store.roster().unwrap();
        let owner = roster.owner_for("jamez667@example.test").expect("promoted");
        assert_eq!(owner.repos, ["intake", "other"], "both repositories");

        // And revoking again.
        let res = f.go(
            &Req::post_json(&api_path("owners/jamez667@example.test/revoke"), "{}")
                .with_cookie(&device),
        );
        assert_eq!(res.status, 200, "{}", res.body);
        assert!(f
            .store
            .roster()
            .unwrap()
            .owner_for("jamez667@example.test")
            .is_none());
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

        let res = f.go(
            &Req::post_json(&api_path("repos"), r#"{"name":"smrt-coder"}"#).with_cookie(&device),
        );
        // **409, where the page asked the question in HTML.** Still a question
        // rather than a refusal — the answer names the way through — but a status
        // a client can act on, which is what "here is a confirmation page" cannot
        // be for a `fetch`.
        assert_eq!(
            res.status, 409,
            "questioned, not silently accepted: {}",
            res.body
        );
        assert!(
            f.store.roster().unwrap().enabled().is_empty(),
            "a misspelling was written on the first ask"
        );
        // And it says how to go ahead anyway, rather than being a bare error
        // somebody would click past.
        assert!(res.body.contains("anyway"), "{}", res.body);

        // **Worth knowing: the half that named the alternative is gone.** The
        // rendered page listed what *is* on offer beside the name that was not —
        // "you typed smrt-coder, a machine is offering smart-coder" — which is
        // what makes a typo obvious rather than merely refused. Nothing on the
        // API carries the offered set: `Seen::offered` is read inside
        // `api_add_repo` to make this decision and never returned. If the
        // machines endpoint ever grows the repositories each daemon declares,
        // that assertion belongs here again.

        // Confirmed by a daemon: enabled outright, and recorded as confirmed.
        let res = f.go(
            &Req::post_json(&api_path("repos"), r#"{"name":"smart-coder"}"#).with_cookie(&device),
        );
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

        let res = f.go(&Req::post_json(
            &api_path("repos"),
            r#"{"name":"not-yet-polling","anyway":true}"#,
        )
        .with_cookie(&device));
        assert_eq!(res.status, 200, "{}", res.body);
        let roster = f.store.roster().unwrap();
        assert_eq!(roster.enabled(), ["not-yet-polling"]);
        assert_eq!(
            roster.repos[0].served_by, None,
            "an assertion is not a confirmation"
        );
    }

    #[test]
    fn a_surface_with_no_repositories_still_serves_and_says_so() {
        // Reachable the moment a developer disables the last one. Not a 404 —
        // that teaches somebody at a working address nothing — and not a refusal
        // to boot, which would put the surface that fixes it out of reach exactly
        // when it is needed.
        //
        // **Renamed from "says why", because the sentence moved to the client.**
        // The page used to draw an explanation instead of a `<textarea>`, and
        // this read the markup for the absent form. The client draws both now, so
        // the server's half is the empty offer: `/me` hands back no repositories,
        // which is what a client needs to draw the explanation rather than a form
        // that always refuses.
        let mut f = Fixture::new("repos-none").with_public(false);
        if let Some(p) = f.public.as_mut() {
            p.repos = crate::config::Repos::default();
        }
        let session = f.signed_in("jo@x.com");

        let res = f.go(&Req::get(public_route::FILE).with_cookie(&session));
        assert_eq!(res.status, 200, "the address still serves");

        let me = f.go(&Req::get(ME_PATH).with_cookie(&session));
        let mine: serde_json::Value = serde_json::from_str(&me.body).unwrap();
        assert!(
            mine["repos"].as_array().is_none_or(|r| r.is_empty()),
            "a repository was offered on a surface that serves none: {}",
            me.body
        );

        // And a filing submitted anyway is refused rather than landing
        // somewhere nobody chose.
        let res = f.go(
            &Req::post_json(&api_path("file"), r#"{"text":"a thing","kind":"bug"}"#)
                .with_cookie(&session),
        );
        assert_eq!(res.status, 400, "{}", res.body);
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
        file_publicly_as(&mut f, &filer, "a thing", "intake");
        assert_eq!(f.store.all().unwrap().len(), 1);

        let res =
            f.go(&Req::post_json(&api_path("repos/intake/disable"), "{}").with_cookie(&device));
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

        // **Asked of the API**, or it would assert nothing: the form routes are
        // gone and everything answers 404 there, so this would pass on a server
        // with no gate at all. The gate is the `let Some(Caller::Admin { .. })`
        // in `api_write`, and 404 rather than 401 is what keeps the
        // administrative surface from existing for anybody else.
        for (rest, body) in [
            ("repos", r#"{"name":"whatever","anyway":true}"#),
            ("repos/intake/disable", "{}"),
        ] {
            let res = f.go(&Req::post_json(&api_path(rest), body).with_cookie(&session));
            assert_eq!(res.status, 404, "an owner reached {rest}: {}", res.body);
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

        let res = f.go(&Req::post_json(
            &api_path("owners"),
            r#"{"login":"jamez667@example.test","repos":["not-served"]}"#,
        )
        .with_cookie(&device));
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(f.store.roster().unwrap().owners.is_empty());

        // Nor an owner of nothing at all, which reads as promoted and grants
        // nothing.
        let res = f.go(&Req::post_json(
            &api_path("owners"),
            r#"{"login":"jamez667@example.test"}"#,
        )
        .with_cookie(&device));
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(f.store.roster().unwrap().owners.is_empty());

        // And an empty array is the same answer as no array at all — a client
        // that sent the field and unticked everything has still asked for a
        // permission that grants nothing.
        let res = f.go(&Req::post_json(
            &api_path("owners"),
            r#"{"login":"jamez667@example.test","repos":[]}"#,
        )
        .with_cookie(&device));
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(f.store.roster().unwrap().owners.is_empty());
    }

    #[test]
    fn a_filer_cannot_reach_any_review_verb() {
        // Iterates the shared constant, so a verb added later is covered without
        // anyone remembering to extend this list.
        let mut f = Fixture::new("public-no-review").with_public(false);
        let session = f.signed_in("jo@x.com");
        file_publicly_as(&mut f, &session, "a thing", "intake");
        let id = f.store.all().unwrap()[0].id.clone();

        // **404, and against their own request.** A filer holds the id — they
        // filed it — so this is the case a gate keyed on "can you name it" would
        // let through. `api_verb`'s catch-all arm is what refuses, and it says
        // *no such request* rather than *not allowed*, because a filer has no
        // business learning that a review surface is there to be refused from.
        for verb in REVIEW_VERBS {
            let res = verb_on(&mut f, &session, &id, verb, "{}");
            assert_eq!(res.status, 404, "an account reached {verb}: {}", res.body);
        }
        assert_eq!(
            f.store.get(&id).unwrap().unwrap().state,
            RequestState::Queued,
            "and nothing moved"
        );
        // And the review *data* is closed to them. **Asked of the API**: the
        // browser paths answer the interface's document to anybody, so the
        // refusal that matters is the one on what sits behind them.
        //
        // This filer filed this request, so they do see it — as its author,
        // through the narrow view. What they must never get is the reviewer's:
        // no repository, no artifact directory, no digest to accept against.
        // That is the line the two API types draw by construction.
        let listed = f.go(&Req::get(&api_path("requests")).with_cookie(&session));
        assert_eq!(listed.status, 200);
        assert!(
            !listed.body.contains("artifact_dir") && !listed.body.contains("spec_digest"),
            "a filer was handed the reviewer's view: {}",
            listed.body
        );
        let one = f.go(&Req::get(&api_path(&format!("requests/{id}"))).with_cookie(&session));
        assert_eq!(one.status, 200, "their own filing is theirs to read");
        assert!(
            !one.body.contains("artifact_dir") && !one.body.contains("spec_digest"),
            "a filer was handed the reviewer's view: {}",
            one.body
        );
        // And `/me` says so, which is what stops the interface drawing a review
        // surface at all.
        let me = f.go(&Req::get(ME_PATH).with_cookie(&session));
        assert!(me.body.contains("\"review\":false"), "{}", me.body);
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
        // **Asked of `/me`.** The browser paths serve the interface's document
        // whether or not anybody is signed in, so what proves a session stopped
        // working is the identity the server reports for its cookie.
        let mut f = Fixture::new("public-signout").with_public(false);
        let session = f.signed_in("jo@x.com");
        let before = f.go(&Req::get(ME_PATH).with_cookie(&session));
        assert!(before.body.contains("\"filer\""), "{}", before.body);

        // **Through the endpoint the interface posts to.** The form route is
        // gone, and a POST to a route that does not exist also leaves nobody
        // signed in — so pointed at the old address this would have kept passing
        // while asserting nothing at all.
        let out = f.go(&Req::post_json(&api_path("signout"), "{}").with_cookie(&session));
        assert_eq!(out.status, 200, "{}", out.body);
        let after = f.go(&Req::get(ME_PATH).with_cookie(&session));
        assert!(
            after.body.contains("\"anonymous\""),
            "signed out: {}",
            after.body
        );
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

        // **404 rather than a rendered invitation to sign in.** The cookie is
        // still in the browser and still names a session; what it no longer names
        // is a live account, so the caller resolves to a stranger and the filing
        // endpoint does not exist for them.
        let res = f.go(
            &Req::post_json(&api_path("file"), r#"{"text":"a thing","kind":"bug"}"#)
                .with_cookie(&session),
        );
        assert_eq!(res.status, 404, "{}", res.body);
        assert!(f.store.all().unwrap().is_empty(), "nothing was filed");

        // And on the very next request, which is the part revocation is for.
        let me = f.go(&Req::get(ME_PATH).with_cookie(&session));
        assert!(me.body.contains("\"anonymous\""), "{}", me.body);
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
        // a username and password rather than a magic link, but an account all
        // the same — so "the only account" is no longer a way to name the one
        // being revoked.
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
        // The list comes from the API now — `/accounts` is the interface's
        // document, and the hint that identifies an account to revoke is in the
        // JSON behind it.
        let listed = f.go(&Req::get(&api_path("accounts")).with_cookie(&device));
        assert_eq!(listed.status, 200);
        assert!(listed.body.contains("jo***@x.com"), "{}", listed.body);

        let res = f.go(
            &Req::post_json(&api_path(&format!("accounts/{id}/revoke")), "{}").with_cookie(&device),
        );
        assert_eq!(res.status, 200, "{}", res.body);
        let accounts = f.store.accounts().unwrap();
        assert!(accounts.live().iter().all(|a| a.id != id), "still live");
        // **And the administrator survived it.** Revoking a filer must not
        // reach the account the server is administered from, which is a real
        // hazard now that both are ordinary accounts. Asked of the API, which
        // is where being the administrator still means something.
        assert_eq!(
            f.go(&Req::get(&api_path("settings")).with_cookie(&device))
                .status,
            200,
            "revoking a filer locked out the administrator"
        );

        // And the filer's session dies with it, without anyone walking sessions.
        let after = f.go(&Req::get(ME_PATH).with_cookie(&session));
        assert!(after.body.contains("\"anonymous\""), "{}", after.body);
    }

    #[test]
    fn a_filer_cannot_reach_the_accounts_surface() {
        // Otherwise anyone who signed up could revoke everyone else.
        //
        // **The list is asked of the API.** `/accounts` answers the interface's
        // document to anybody; the account list behind it is the thing a filer
        // must not be handed, and 404 rather than 401 keeps the address
        // unconfirmed.
        let mut f = Fixture::new("accounts-closed").with_public(false);
        let session = f.signed_in("jo@x.com");
        let id = f.store.accounts().unwrap().live()[0].id.clone();

        assert_eq!(
            f.go(&Req::get(&api_path("accounts")).with_cookie(&session))
                .status,
            404
        );
        // The write too, and **404 rather than 401** for the same reason: the
        // administrative surface does not exist for a filer.
        assert_eq!(
            f.go(
                &Req::post_json(&api_path(&format!("accounts/{id}/revoke")), "{}")
                    .with_cookie(&session)
            )
            .status,
            404
        );
        assert!(!f.store.accounts().unwrap().live().is_empty(), "still live");
    }

    #[test]
    fn revoking_twice_is_not_an_error() {
        // The caller asked for a state that now holds.
        //
        // **This is the test that caught a real regression in the move.** The
        // page handler said so in as many words and answered 200; `api_revoke`
        // was written to answer 404 when the underlying call reported "nothing
        // changed" — which conflates "already revoked" with "no such record" and
        // reports a failure for doing exactly what the button was pressed for. It
        // would also have turned a write into an id oracle. Fixed there, asserted
        // here, and asserted across all four lists rather than only accounts,
        // because they share the shape and would have shared the mistake.
        let mut f = Fixture::new("revoke-twice")
            .with_public(false)
            .with_repos(&["intake"])
            .with_owner("jamez667@example.test", &["intake"]);
        f.signed_in("jo@x.com");
        let device = f.as_admin();
        let id = f.store.accounts().unwrap().live()[0].id.clone();
        mint_daemon(&mut f, &device, "laptop");

        let twice = |f: &mut Fixture, rest: &str| {
            for pass in 1..=2 {
                let res = f.go(&Req::post_json(&api_path(rest), "{}").with_cookie(&device));
                assert_eq!(res.status, 200, "{rest} pass {pass}: {}", res.body);
            }
        };
        twice(&mut f, &format!("accounts/{id}/revoke"));
        twice(&mut f, "owners/jamez667@example.test/revoke");
        twice(&mut f, "daemons/laptop/revoke");
        twice(&mut f, "repos/intake/disable");

        // And a record that never existed is the same answer, so the write is
        // not a way to ask which ids are real.
        for rest in [
            "accounts/never-existed/revoke",
            "owners/nobody/revoke",
            "daemons/no-such-machine/revoke",
            "repos/no-such-repo/disable",
        ] {
            let res = f.go(&Req::post_json(&api_path(rest), "{}").with_cookie(&device));
            assert_eq!(res.status, 200, "{rest}: {}", res.body);
        }
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

    // **`a_page_showing_more_than_one_authors_spec_is_never_served_with_script`
    // was here, and is deleted rather than retargeted.** It pinned the rule that
    // the policy is chosen by *caller* rather than by path: an owner's pages sit
    // on public addresses and render every filer's spec, so they were served
    // `Strict` while a filer's own pages — one author, their own text — kept
    // `PublicScript`.
    //
    // That rule no longer has anything to decide on these paths. Every address
    // the interface routes on answers the same document, and it is one served
    // bundle: withholding script from it by caller would withhold the
    // application itself from owners. The amendment and what it costs are
    // recorded in full at the dispatch site in `handle_inner` — the residual
    // risk being that a renderer bug is now a cross-tenant XSS rather than a
    // rendering glitch, bounded by `default-src 'none'`, by `script-src 'self'`
    // never becoming `'unsafe-inline'`, and by the ban on `innerHTML`.
    //
    // The caller-based branch itself is still live for the public routes that
    // are *not* documents, and `the_script_policy_reaches_the_public_surface_and
    // _stops_there` is what now pins where script does and does not reach.

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

        // **The administrative addresses carry `PublicScript` too now, and that
        // is the amendment rather than a regression.** They answer the same
        // document as everything else — one bundle, served from this origin —
        // so refusing script there would refuse the application to the person
        // who administers the server. The cost is recorded at the dispatch site:
        // a renderer bug is a cross-tenant XSS on this surface, bounded by
        // `default-src 'none'` and by never allowing `'unsafe-inline'`.
        for path in [private_route::REVIEW, "/accounts"] {
            let res = f.go(&Req::get(path).with_cookie(&device));
            assert_eq!(res.status, 200, "{path}: {}", res.body);
            assert_eq!(
                res.policy,
                Policy::PublicScript,
                "{path} answers the interface's document"
            );
        }

        // **What has not moved: the JSON behind it is `Strict`.** A body nothing
        // renders has no reason to permit anything, and this is the assertion
        // that stops the relaxation above spreading past the document.
        for path in [ME_PATH, "/api/v1/ui/requests", "/api/v1/ui/settings"] {
            let res = f.go(&Req::get(path).with_cookie(&device));
            assert_eq!(res.policy, Policy::Strict, "{path} is data, not a document");
        }

        // And the daemon's API, which is neither.
        assert_eq!(
            f.go(&Req::get("/api/v1/work").with_bearer(KEY)).policy,
            Policy::Strict
        );
    }

    // -- language -----------------------------------------------------------

    #[test]
    fn choosing_a_language_sets_a_cookie_and_the_server_answers_in_it() {
        // **Renamed from "renders in it", because rendering moved.** The route
        // answered a page in the chosen language; it answers the application
        // shell now, which is one document in one markup whatever the cookie
        // says — the client translates itself. What the server still owes is the
        // cookie, and that the *next* request is answered in the language it
        // names: the coarse state label on a filer's request is translated
        // server-side, deliberately, and is the thing the choice has to reach.
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

        // And the cookie it set is one the server reads back.
        let session = f.signed_in("filer@example.test");
        file_publicly_as(&mut f, &session, "a thing", "intake");
        let filed = f.go(&Req::get(&api_path("requests"))
            .with_cookie(&session)
            .with_lang(Some("fr"), None));
        assert!(filed.body.contains("reçue"), "{}", filed.body);
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
        // **Signing out is asked of the endpoint the interface posts to**, since
        // the form route is gone — and a POST to a route that does not exist sets
        // no cookie at all, so `expect` here would have caught the drift rather
        // than the assertion doing it.
        let mut deployed = Fixture::new("secure-yes").with_public(false);
        let account = deployed.signed_in("filer@example.test");
        for res in [
            deployed.go(&Req::post(public_route::LANGUAGE, "lang=fr")),
            deployed.go(&Req::post_json(&api_path("signout"), "{}").with_cookie(&account)),
        ] {
            let c = res.set_cookie.expect("a cookie is set");
            assert!(c.contains("; Secure"), "{c}");
        }

        let mut local = Fixture::new("secure-no").with_public(false).on_loopback();
        let account = local.signed_in("filer@example.test");
        for res in [
            local.go(&Req::post(public_route::LANGUAGE, "lang=fr")),
            local.go(&Req::post_json(&api_path("signout"), "{}").with_cookie(&account)),
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
        //
        // **Both configurations, because only one of them used to work.** The
        // fonts were served by an arm inside the public surface's routes, so a
        // server with public intake switched off answered 404 for them while its
        // own stylesheet went on asking — the interface rendered in fallback
        // faces with every status code correct. This test existed and did not
        // catch it: it ran with the surface on, which is the case that worked.
        for public in [true, false] {
            let mut f = Fixture::new(&format!("fonts-{public}"));
            if public {
                f = f.with_public(false);
            }
            for path in [
                crate::api::ui::FONT_BODY_PATH,
                crate::api::ui::FONT_DISPLAY_PATH,
            ] {
                let res = f.go(&Req::get(path));
                assert_eq!(res.status, 200, "{path} with public={public}");
                assert_eq!(res.content_type, "font/woff2", "{path}");
                let bytes = res.binary.expect("a font is bytes, not a string");
                // The woff2 signature. A truncated download or an error page
                // would be served happily and render as no font at all.
                assert_eq!(&bytes[..4], b"wOF2", "{path} is not a woff2");
                assert!(bytes.len() > 10_000, "{path} is suspiciously small");
            }

            // Reachable **signed out**: the sign-in page is the first thing
            // anyone sees, and it should not be the one rendered in a fallback
            // face.
            assert_eq!(f.go(&Req::get(crate::api::ui::FONT_BODY_PATH)).status, 200);
        }
    }

    #[test]
    fn the_stylesheet_asks_for_no_origin_but_this_one() {
        // `font-src 'self'` permits these two and refuses everything else, so a
        // stylesheet that named a remote face would produce an invisible
        // failure: the CSP blocks it, the page falls back, and nothing errors.
        //
        // **Retargeted from the rendered surface's stylesheet at the interface's
        // own.** The property is about the CSP and the faces, not about which
        // stylesheet holds them, and this is the one a browser now loads. It is
        // also what pins the stylesheet to the exact addresses this server
        // answers on: a bundle rebuilt with a Google URL fails here rather than
        // in a browser nobody is watching.
        //
        // **Asserted against the constants, not a prefix.** The previous version
        // looked for `url(/public/`, which is a shape rather than an address —
        // it would have passed just as happily while the server served the fonts
        // from somewhere else entirely, which is exactly what had happened.
        let css = crate::api::ui::STYLE;
        let served = [
            crate::api::ui::FONT_BODY_PATH,
            crate::api::ui::FONT_DISPLAY_PATH,
        ];
        for face in css.split("@font-face").skip(1) {
            let block = face.split('}').next().unwrap_or("");
            assert!(
                served.iter().any(|p| block.contains(&format!("url({p})"))),
                "a face names an address this server does not serve: {block}"
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
        // The cookie is what the route produces — the answer is the application
        // shell, which is the same document in every language.
        assert!(
            res.set_cookie
                .as_deref()
                .unwrap_or_default()
                .starts_with(&format!("{LANG_COOKIE}=fr")),
            "{:?}",
            res.set_cookie
        );
    }

    #[test]
    fn an_unknown_language_falls_back_rather_than_reaching_the_cookie() {
        // The value is matched against the catalogues that exist, so nothing a
        // caller writes here reaches anything except by choosing among them.
        //
        // **Renamed from "reaching the page".** The answer is the application
        // shell — a compiled-in constant with nothing interpolated into it, so
        // "the hostile value did not reach the markup" is true by construction
        // and no longer a claim about this route. Where the value *can* still go
        // is the cookie, which is written from what the caller sent; that is what
        // `Locale::parse` stands between, and it is what this now watches. The
        // one-script assertion is kept on the shell beside it, because the shell
        // is what a browser executes and a bundle that grew an inline block would
        // be a real change.
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
            // **The cookie is the default, not the hostile value.** A cookie
            // holding `<script>` is not itself an injection, but it is read back
            // by `Req::locale` on every later request — the value must never be
            // anything but a code this server has a catalogue for.
            let cookie = res.set_cookie.clone().unwrap_or_default();
            assert!(
                cookie.starts_with(&format!("{LANG_COOKIE}=en")),
                "{hostile}: {cookie}"
            );
            assert!(!cookie.contains("alert(1)"), "{hostile}: {cookie}");
            assert!(!cookie.contains("passwd"), "{hostile}: {cookie}");
            // And the document is the shell, unchanged by any of it.
            assert!(res.body.contains("<html lang=\"en\""), "{hostile}");
            assert!(!res.body.contains("alert(1)"), "{hostile}: {}", res.body);
            assert!(!res.body.contains("passwd"), "{hostile}: {}", res.body);
        }

        // Exactly one script, and it is a served file from this origin. Asserted
        // as "only the expected tag" rather than "no script at all": the shell
        // has a legitimate one, and a blanket ban would have to be deleted here,
        // taking the injection check with it.
        //
        // **Comments are stripped first**, and that is not cosmetic. The shell's
        // own comment explains why there is no inline block and spells `<script>`
        // to do it — counting raw occurrences finds two and fails on the prose
        // rather than on the markup. What matters is what a browser executes.
        let shell = crate::api::ui::INDEX;
        let markup: String = shell
            .split("<!--")
            .enumerate()
            .map(|(i, part)| {
                if i == 0 {
                    part.to_string()
                } else {
                    part.split_once("-->")
                        .map(|(_, r)| r)
                        .unwrap_or("")
                        .to_string()
                }
            })
            .collect();
        assert_eq!(markup.matches("<script").count(), 1, "{markup}");
        assert!(
            markup.contains(&format!("src=\"{}\"", crate::api::ui::SCRIPT_PATH)),
            "the only script is the interface's own: {markup}"
        );
        assert!(
            !markup.contains("'unsafe-inline'") && !markup.contains("<script>"),
            "the shell grew an inline block: {markup}"
        );
    }

    #[test]
    fn a_signed_in_filers_pages_follow_their_chosen_language() {
        // The property that matters beyond the switcher itself: the locale is
        // decided once per request and reaches everything served, not only the
        // thing the switcher happens to re-render.
        //
        // **Asserted on the JSON.** The pages are one document in one language
        // now — the client translates itself — but the server still translates
        // what only it can: `FiledRequest` carries the *coarse state label*,
        // deliberately blurred and therefore built server-side. So that label is
        // where per-request locale still has to land, and it is what this
        // watches.
        let mut f = Fixture::new("lang-through").with_public(false);
        let account = f.signed_in("filer@example.test");
        file_publicly_as(&mut f, &account, "a thing", "intake");

        let filed = f.go(&Req::get(&api_path("requests"))
            .with_cookie(&account)
            .with_lang(Some("fr"), None));
        assert_eq!(filed.status, 200, "{}", filed.body);
        assert!(filed.body.contains("reçue"), "{}", filed.body);

        // And the browser's header is honoured when nothing was chosen.
        let by_header = f.go(&Req::get(&api_path("requests"))
            .with_cookie(&account)
            .with_lang(None, Some("fr-CA,fr;q=0.9,en;q=0.5")));
        assert!(by_header.body.contains("reçue"), "{}", by_header.body);

        // And English is not simply what every locale answers.
        let english = f.go(&Req::get(&api_path("requests"))
            .with_cookie(&account)
            .with_lang(Some("en"), None));
        assert!(english.body.contains("received"), "{}", english.body);
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
        //
        // **Finding a path this branch still reaches took two tries.** `FILE` is
        // one the interface routes on, so it answers the document long before
        // here. `/public/signin/<token>` looked like the answer and is not: the
        // magic-link landing is deliberately exempt from the surface being
        // configured — it is reached from an email by somebody holding nothing —
        // so it is served rather than 404ed. `SIGNOUT` is neither: a public
        // address, not a document, and with no surface behind it there is nothing
        // there.
        let mut f = Fixture::new("policy-unconfigured");
        let res = f.go(&Req::get(public_route::SIGNOUT));
        assert_eq!(res.status, 404, "{}", res.body);
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
            let res = try_filing_as(&mut f, &session, &format!("thing {i}"));
            assert_eq!(res.status, 200, "filing {i}: {}", res.body);
        }

        let refused = try_filing_as(&mut f, &session, "one too many");
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

        file_publicly_as(&mut f, &session, "first", "intake");
        let id = f.store.all().unwrap()[0].id.clone();
        f.store.discard(&id).unwrap();
        file_publicly_as(&mut f, &session, "second", "intake");

        let refused = try_filing_as(&mut f, &session, "third");
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

        file_publicly_as(&mut f, &session, "spam", "intake");
        let id = f.store.all().unwrap()[0].id.clone();
        f.store
            .finish_screening(&id, Some("screened as spam"))
            .unwrap();

        // **And the filer is never told which of the two it was.** The coarse
        // label a filer reads says "received" for `Screening`, `Quarantined` and
        // `Queued` alike — learning it was quarantined is learning that this
        // server screens, which is exactly what a spammer tunes against.
        let listed = f.go(&Req::get(&api_path("requests")).with_cookie(&session));
        assert!(
            !listed.body.contains("quarantined") && !listed.body.contains("screened as spam"),
            "a filer was told the screener held it: {}",
            listed.body
        );

        let refused = try_filing_as(&mut f, &session, "another");
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
                &Req::post_json(
                    &api_path("file"),
                    &serde_json::json!({ "text": format!("thing {i}"), "repo": "alpha", "kind": "bug" })
                        .to_string(),
                )
                .with_cookie(&device),
            );
            assert_eq!(res.status, 200, "administrator filing {i}: {}", res.body);
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

        // A different address, with the only account revoked. **Asked through the
        // endpoint**: pointed at the deleted form this read back the link the
        // *first* sign-in emailed, so it spent a token for the address that
        // already had an account and proved nothing about the ceiling.
        let token = link_token_for(&mut f, "second@x.com");
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
        file_publicly_as(&mut f, &alice, "alice", "intake");
        assert_eq!(try_filing_as(&mut f, &alice, "more").status, 429);

        let bob = f.signed_in("bob@x.com");
        assert_eq!(
            try_filing_as(&mut f, &bob, "bob").status,
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
        file_publicly_as(&mut f, &session, "first", "intake");
        assert_eq!(try_filing_as(&mut f, &session, "second").status, 429);

        // A day later.
        f.now_ms += crate::config::FILING_WINDOW_MS + 1;
        let res = try_filing_as(&mut f, &session, "tomorrow");
        assert_eq!(res.status, 200, "{}", res.body);
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
        let token = link_token_for(&mut f, "second@x.com");
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

        // Same address again, with the ceiling already reached. **Asked of
        // `/me`**: the filing address answers the interface's document to
        // anybody, so it cannot say whether a session was opened — the identity
        // the server reports for the cookie is what can.
        let session = f.signed_in("jo@x.com");
        let me = f.go(&Req::get(ME_PATH).with_cookie(&session));
        assert!(
            me.body.contains("\"filer\""),
            "an existing filer was locked out by the signup wall: {}",
            me.body
        );
        assert_eq!(f.store.accounts().unwrap().accounts.len(), 1);
    }

    #[test]
    fn a_public_filing_is_length_capped_like_any_other() {
        let mut f = Fixture::new("public-length").with_public(false);
        let session = f.signed_in("jo@x.com");

        // The same `check_length` an administrator's filing goes through, which
        // is the property: the screener's "sees the whole request" guarantee is
        // only true if what it screens is what was accepted, so the two filing
        // paths cannot drift to different ceilings.
        let res = try_filing_as(&mut f, &session, &"word ".repeat(MAX_WORDS + 10));
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(res.body.contains("words"), "{}", res.body);
        assert!(f.store.all().unwrap().is_empty());

        let res = try_filing_as(&mut f, &session, &"x".repeat(MAX_BYTES + 1));
        assert_eq!(res.status, 400, "{}", res.body);
        assert!(res.body.contains("characters"), "{}", res.body);
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
        // **The JSON endpoints share their form's budget, not the API's.** The
        // interface posts to `/api/v1/ui/signin/password`; if that fell through
        // to the generic API arm, credential guessing would have found a route
        // with a different allowance and the `AnonPrivate` reasoning above would
        // protect only the spelling nobody uses.
        assert_eq!(
            probe("/api/v1/ui/signin/password"),
            Bucket::AnonPrivate,
            "guessing passwords over JSON is still guessing passwords"
        );
        assert_eq!(probe("/api/v1/ui/signin"), Bucket::PublicWrite);
        assert_eq!(probe("/api/v1/ui/signout"), Bucket::PublicWrite);

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
