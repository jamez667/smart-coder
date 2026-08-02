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

use sc_proto::wire::{self, DraftFailed, DraftedSpec, PollResponse, WireError, WorkItem};
use sc_proto::IntakeKind;

use std::sync::Mutex;

use crate::account;
use crate::auth::{self, Caller, Credentials};
use crate::config::PublicConfig;
use crate::i18n::Locale;
use crate::mail::Mailer;
use crate::ratelimit::{Bucket, RateLimiter};
use crate::store::{new_id, Request, RequestState, Store};

/// The cookie a browser carries once enrolled.
pub const COOKIE: &str = "sc_device";

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
}

/// The verbs that decide a request's fate.
///
/// Named once so the test proving an account cannot reach any of them iterates
/// this list rather than a hand-written copy that goes stale the moment a verb
/// is added.
pub const REVIEW_VERBS: [&str; 5] = [
    "approve",
    "approve/confirm",
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
    /// Script from this origin only. The public surface.
    ///
    /// Permitted here and nowhere else because of who reads what: a filer's
    /// pages show **their own** requests, so a script that went wrong reaches
    /// only its author's data. The private surface renders every filer's spec on
    /// one page, and the same argument does not reach it.
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
    pub daemon_key: &'a str,
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
}

/// Route one request.
pub fn handle(ctx: &mut Ctx<'_>, req: &Req) -> Res {
    let creds = match ctx.store.credentials() {
        Ok(c) => c,
        Err(e) => return error(500, &format!("the credential store is unreadable: {e}")),
    };
    let caller = identify(ctx, req, &creds);

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
    if !ctx
        .limiter
        .allow(bucket_for(&caller, &path, ctx), ctx.now_ms)
    {
        return error(429, "too many requests — wait a minute and try again");
    }

    // The daemon-facing API. Its routes are shared constants, so the two ends
    // cannot disagree about the strings.
    if path.starts_with("/api/v1/work") {
        if caller != Some(Caller::Daemon) {
            return error(401, "unauthorized");
        }
        return daemon_route(ctx, req, method, &path);
    }

    // Enrolment is the one route reachable without a credential — it is how a
    // credential is obtained. It is guarded by the single-use code instead.
    if method == "POST" && path == "/enrol" {
        return enrol(ctx, req, creds);
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
    if is_public_path(&path) {
        return match ctx.public {
            Some(_) => {
                public_route(ctx, req, method, &path, &caller).with_policy(Policy::PublicScript)
            }
            // No public surface configured: this 404 is not *on* that surface, so
            // it is served strict like every other non-public response.
            None => Res::html(404, crate::page::not_found()),
        };
    }

    // Everything else is the browser surface.
    let Some(Caller::Device { .. }) = caller else {
        // An un-enrolled browser gets the enrolment page rather than a bare 401,
        // because the person on the other end is the developer and the next thing
        // they need is the box to type their code into.
        if method == "GET" {
            return Res::html(401, crate::page::enrol_page());
        }
        return error(401, "unauthorized");
    };
    browser_route(ctx, req, method, &path)
}

/// Which budget this request is counted against.
///
/// An authenticated caller is keyed on its credential's *hash*. An anonymous one
/// is keyed on the **route class** — never on anything the caller chooses, since
/// a per-email or per-`X-Forwarded-For` bucket lets an attacker mint a fresh
/// budget per value, which is no limit at all.
fn bucket_for(caller: &Option<Caller>, path: &str, ctx: &Ctx<'_>) -> Bucket {
    match caller {
        Some(Caller::Daemon) => Bucket::Credential(auth::hash(ctx.daemon_key)),
        Some(Caller::Device { id }) => Bucket::Credential(auth::hash(id)),
        // A signed-in filer gets their own budget. Safe to key on, unlike an
        // email or a forwarded header, because an account id is minted by this
        // server and costs a confirmed mailbox to obtain — the caller cannot vary
        // it to mint fresh budgets.
        Some(Caller::Account { id }) => Bucket::Credential(auth::hash(id)),
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
        None => Bucket::Enrol,
    }
}

/// Is this one of the public surface's paths?
///
/// Exact equality for fixed paths and `starts_with` only on prefixes ending in
/// `/`, so `/publicXYZ` cannot match — on the private surface a loose prefix
/// fails *closed* (401), but here it fails **open**.
fn is_public_path(path: &str) -> bool {
    path == public_route::FILE
        || path == public_route::SIGNIN
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
fn identify(ctx: &Ctx<'_>, req: &Req, creds: &Credentials) -> Option<Caller> {
    if let Some(bearer) = &req.bearer {
        if auth::matches(bearer, &auth::hash(ctx.daemon_key)) {
            return Some(Caller::Daemon);
        }
    }
    let token = req.cookie_token.as_deref()?;

    if let Some(device) = creds.device_for(token) {
        return Some(Caller::Device {
            id: device.id.clone(),
        });
    }

    // Only now is the account store read — lazily, and only when a public
    // surface exists at all. It is unbounded and attacker-sized, so parsing it
    // on every request would let a stranger choose how much work each one costs.
    ctx.public?;
    let accounts = ctx.store.accounts().ok()?;
    accounts
        .session_for(token)
        .map(|a| Caller::Account { id: a.id.clone() })
}

// ---------------------------------------------------------------------------
// The daemon side
// ---------------------------------------------------------------------------

fn daemon_route(ctx: &mut Ctx<'_>, req: &Req, method: &str, path: &str) -> Res {
    if method == "GET" && path == wire::route::WORK {
        return match ctx.store.claim_next() {
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
                .record_drafted(id, &payload.spec, &payload.artifact_dir)
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
            match ctx.store.record_failed(id, &payload.reason) {
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

fn enrol(ctx: &mut Ctx<'_>, req: &Req, mut creds: Credentials) -> Res {
    let form = form_fields(&req.body);
    let code = form.get("code").cloned().unwrap_or_default();
    let label = form.get("label").cloned().unwrap_or_default();

    let Some((_device, token)) = creds.enrol(code.trim(), &label, ctx.now_ms) else {
        // Deliberately the same message whether the code was wrong, absent or
        // already spent: distinguishing them tells a guesser which half they got
        // right.
        return Res::html(401, crate::page::enrol_page_with_error());
    };
    if let Err(e) = ctx.store.put_credentials(&creds) {
        return error(500, &format!("could not record the device: {e}"));
    }

    let secure = secure_attr(ctx);
    let mut res = Res::html(200, crate::page::enrolled_page());
    // HttpOnly so script cannot read it; SameSite=Strict so a cross-site form
    // cannot ride it; Secure because this is served over TLS at the proxy.
    res.set_cookie = Some(format!(
        "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict{secure}; Max-Age=31536000"
    ));
    res
}

fn browser_route(ctx: &mut Ctx<'_>, req: &Req, method: &str, path: &str) -> Res {
    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => match ctx.store.all() {
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

        ("GET", p) if p.starts_with("/request/") => {
            let id = p.trim_start_matches("/request/");
            match ctx.store.get(id) {
                Ok(Some(r)) => Res::html(200, crate::page::detail(&r)),
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
            if verb == "approve" {
                return ask_to_approve(ctx, id);
            }

            let outcome = match verb {
                "approve/confirm" => {
                    let digest = form.get("digest").cloned().unwrap_or_default();
                    ctx.store.approve(id, &digest)
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
                Ok(r) => Res::html(200, crate::page::detail(&r)),
                Err(e) => Res::html(400, crate::page::message(&e.to_string())),
            }
        }

        _ => Res::html(404, crate::page::not_found()),
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

    // Decided once here and passed down, rather than re-derived per page. Every
    // response from this surface is in the same language as every other, which
    // is not true if each renderer negotiates for itself.
    let locale = req.locale();

    match (method, path) {
        // Ask for a link. Reachable signed-out — it is how one signs in.
        ("GET", public_route::SIGNIN) => Res::html(200, crate::page::signin_page_in(locale)),
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
            None => Res::html(200, crate::page::signin_page_in(locale)),
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

    match (method, path) {
        ("GET", public_route::FILE) => match mine(ctx, account_id) {
            Ok(list) => Res::html(200, crate::page::public_file_page(&list, show_spec, locale)),
            Err(e) => error(500, &e.to_string()),
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
    Res::html(200, crate::page::accounts_page(&accounts))
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

/// Send a sign-in link, or quietly do nothing.
///
/// **The response is identical in every case** — unknown address, existing
/// account, revoked account, malformed input, over the outstanding cap. Only
/// what gets *sent* differs, so this cannot be used to discover whether an
/// address has an account.
fn request_sign_in(ctx: &mut Ctx<'_>, req: &Req) -> Res {
    let form = form_fields(&req.body);
    let raw = form.get("email").cloned().unwrap_or_default();
    let sent = try_send_link(ctx, &raw);
    if let Err(e) = sent {
        // Logged for the operator, never shown: the page must look the same
        // whether or not mail went out.
        eprintln!("sign-in link not sent: {e}");
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
                eprintln!(
                    "signup refused: {} accounts exist, the limit is {cap}",
                    accounts.accounts.len()
                );
                return Res::html(200, crate::page::signin_failed_page(false, locale));
            }
            accounts.create(&email_hash, &email_hint, ctx.now_ms).id
        }
    };
    let session = accounts.open_session(&id, ctx.now_ms);
    if let Err(e) = ctx.store.put_accounts(&accounts) {
        return error(500, &e.to_string());
    }
    let secure = secure_attr(ctx);

    let mut res = Res::html(200, crate::page::public_file_page(&[], false, locale));
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
            }
        }
    }
    let secure = secure_attr(ctx);
    let mut res = Res::html(200, crate::page::signin_page_in(req.locale()));
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
    let mut res = Res::html(200, crate::page::signin_page_in(locale));
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

/// File a request from the public surface.
///
/// The repository comes from **configuration**, never the body — so a stranger
/// cannot aim work at a repository the operator did not nominate for public
/// intake. The form has no such field, and one submitted anyway is ignored.
fn file_publicly(ctx: &mut Ctx<'_>, req: &Req, account_id: &str) -> Res {
    let locale = req.locale();
    let Some(public) = ctx.public else {
        return Res::html(404, crate::page::public_not_found(locale));
    };
    let repo = public.repo.clone();
    let screened = public.screen.is_some();

    let form = form_fields(&req.body);
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
fn ask_to_approve(ctx: &mut Ctx<'_>, id: &str) -> Res {
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
    Res::html(200, crate::page::confirm_approve(&req, &spec, &digest))
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

    struct Fixture {
        store: Store,
        limiter: RateLimiter,
        dir: PathBuf,
        /// `None` unless a test turns the public surface on, so every existing
        /// test keeps exercising a private-only server.
        public: Option<PublicConfig>,
        mailer: crate::mail::testing::Recording,
        write_lock: Mutex<()>,
        /// Advanced by tests that need a link to expire.
        now_ms: u64,
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
                now_ms: 1_000,
            }
        }

        /// Turn the public surface on, as a configured deployment would.
        fn with_public(mut self, screened: bool) -> Fixture {
            self.public = Some(PublicConfig {
                repo: "intake".into(),
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
                max_accounts: crate::config::DEFAULT_MAX_ACCOUNTS,
                show_spec: true,
            });
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

        fn go(&mut self, req: &Req) -> Res {
            let mut ctx = Ctx {
                store: &self.store,
                daemon_key: KEY,
                limiter: &mut self.limiter,
                now_ms: self.now_ms,
                public: self.public.as_ref(),
                mailer: &self.mailer,
                write_lock: &self.write_lock,
            };
            handle(&mut ctx, req)
        }

        /// Sign in as a filer, returning the session cookie.
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
        fn enrolled(&mut self) -> String {
            let mut creds = Credentials::default();
            creds.set_enrol_code("ABC-123");
            self.store.put_credentials(&creds).unwrap();
            let res = self.go(&Req::post("/enrol", "code=ABC-123&label=phone"));
            assert_eq!(res.status, 200, "{}", res.body);
            let cookie = res.set_cookie.expect("a token was issued");
            cookie
                .trim_start_matches(&format!("{COOKIE}="))
                .split(';')
                .next()
                .unwrap()
                .to_string()
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
        let token = f.enrolled();
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
        let token = f.enrolled();

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
    fn a_drafted_spec_comes_back_and_the_request_awaits_review() {
        let mut f = Fixture::new("drafted");
        let token = f.enrolled();
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
        let token = f.enrolled();
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
        let token = f.enrolled();
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
    fn an_un_enrolled_browser_gets_the_enrolment_page_not_a_bare_401() {
        // The person on the other end is the developer, and the next thing they
        // need is the box to type their code into.
        let mut f = Fixture::new("unenrolled");
        let res = f.go(&Req::get("/"));
        assert_eq!(res.status, 401);
        assert!(res.body.contains("enrol"), "{}", res.body);
    }

    #[test]
    fn a_wrong_enrolment_code_says_nothing_about_which_half_was_wrong() {
        // Distinguishing "no code armed" from "wrong code" from "already spent"
        // tells a guesser which half they got right.
        let mut f = Fixture::new("enrol-wrong");
        let mut creds = Credentials::default();
        creds.set_enrol_code("ABC-123");
        f.store.put_credentials(&creds).unwrap();

        let wrong = f.go(&Req::post("/enrol", "code=XYZ-999&label=phone"));
        let none = {
            f.store.put_credentials(&Credentials::default()).unwrap();
            f.go(&Req::post("/enrol", "code=ABC-123&label=phone"))
        };
        assert_eq!(wrong.status, 401);
        assert_eq!(wrong.status, none.status);
        assert_eq!(
            wrong.body, none.body,
            "the two failures are indistinguishable"
        );
        assert!(wrong.set_cookie.is_none());
    }

    #[test]
    fn an_enrolled_device_gets_an_httponly_strict_cookie() {
        // Script must not read it, and a cross-site form must not ride it.
        let mut f = Fixture::new("cookie");
        let mut creds = Credentials::default();
        creds.set_enrol_code("ABC-123");
        f.store.put_credentials(&creds).unwrap();

        let res = f.go(&Req::post("/enrol", "code=ABC-123&label=phone"));
        let cookie = res.set_cookie.unwrap();
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Strict"), "{cookie}");
        assert!(cookie.contains("Secure"), "{cookie}");
    }

    #[test]
    fn a_revoked_device_is_refused_while_the_others_still_work() {
        let mut f = Fixture::new("revoked");
        let phone = f.enrolled();
        // Arm the second code on the *existing* store — a fresh `Credentials`
        // would wipe the phone this test is about.
        let mut creds = f.store.credentials().unwrap();
        creds.set_enrol_code("DEF-456");
        f.store.put_credentials(&creds).unwrap();
        let laptop = f
            .go(&Req::post("/enrol", "code=DEF-456&label=laptop"))
            .set_cookie
            .unwrap()
            .trim_start_matches(&format!("{COOKIE}="))
            .split(';')
            .next()
            .unwrap()
            .to_string();

        let mut creds = f.store.credentials().unwrap();
        let phone_id = creds
            .device_for(&phone)
            .map(|d| d.id.clone())
            .expect("the phone is live");
        assert!(creds.revoke(&phone_id));
        f.store.put_credentials(&creds).unwrap();

        assert_eq!(f.go(&Req::get("/").with_cookie(&phone)).status, 401);
        assert_eq!(f.go(&Req::get("/").with_cookie(&laptop)).status, 200);
    }

    #[test]
    fn filing_a_request_names_a_repository_and_never_a_path() {
        // The form has no field for a path, so traversal is unreachable rather
        // than mitigated (spec 18).
        let mut f = Fixture::new("file");
        let token = f.enrolled();
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
        let token = f.enrolled();

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
        let token = f.enrolled();
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
        let mut f = Fixture::new("approve");
        let token = f.enrolled();
        let id = f.file(&token, "a+thing", "alpha");
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        let payload = serde_json::to_string(&DraftedSpec::new(&id, "# Spec", "specs/x")).unwrap();
        f.go(&Req::post(&wire::route::drafted(&id), &payload).with_bearer(KEY));

        let asked = f.go(&Req::post(&format!("/request/{id}/approve"), "").with_cookie(&token));
        assert_eq!(asked.status, 200, "{}", asked.body);
        assert_eq!(
            f.store.require(&id).unwrap().state,
            RequestState::AwaitingReview,
            "the first post asks, it does not decide"
        );
        assert!(asked.body.contains("/approve/confirm"), "{}", asked.body);

        let digest = digest_from(&asked.body);
        let settled = f.go(&Req::post(
            &format!("/request/{id}/approve/confirm"),
            &format!("digest={digest}"),
        )
        .with_cookie(&token));
        assert_eq!(settled.status, 200, "{}", settled.body);
        // `Ready` is not `Done`: nothing was built, and the developer picks it up
        // in their IDE on their own schedule.
        assert_eq!(f.store.require(&id).unwrap().state, RequestState::Ready);
    }

    #[test]
    fn an_approval_of_text_that_changed_under_the_reviewer_is_refused() {
        // The reviewer opens v1 on a train; `queue serve` pushes a redraft while
        // they read. Confirming must not settle v2 on the strength of having read
        // v1 — consent attaches to bytes, not to an id.
        let mut f = Fixture::new("stale");
        let token = f.enrolled();
        let id = f.file(&token, "a+thing", "alpha");
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        let v1 = serde_json::to_string(&DraftedSpec::new(&id, "# Version one", "specs/x")).unwrap();
        f.go(&Req::post(&wire::route::drafted(&id), &v1).with_bearer(KEY));

        let asked = f.go(&Req::post(&format!("/request/{id}/approve"), "").with_cookie(&token));
        let stale = digest_from(&asked.body);

        // The daemon redrafts under them. Through the real path — sent back,
        // requeued, claimed again — because a daemon may now only report on a
        // claim it currently holds.
        f.go(&Req::post(&format!("/request/{id}/send-back"), "notes=redo").with_cookie(&token));
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        let v2 = serde_json::to_string(&DraftedSpec::new(&id, "# Version two", "specs/x")).unwrap();
        f.go(&Req::post(&wire::route::drafted(&id), &v2).with_bearer(KEY));

        let refused = f.go(&Req::post(
            &format!("/request/{id}/approve/confirm"),
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
        let token = f.enrolled();
        let id = f.file(&token, "a+thing", "alpha");
        f.go(&Req::get(wire::route::WORK).with_bearer(KEY));
        let payload = serde_json::to_string(&DraftedSpec::new(&id, "# Spec", "specs/x")).unwrap();
        f.go(&Req::post(&wire::route::drafted(&id), &payload).with_bearer(KEY));

        for body in ["", "digest=", "digest=nonsense"] {
            let res =
                f.go(
                    &Req::post(&format!("/request/{id}/approve/confirm"), body).with_cookie(&token)
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
        let token = f.enrolled();
        let id = f.file(&token, "a+thing", "alpha");

        let res = f.go(&Req::post(&format!("/request/{id}/approve"), "").with_cookie(&token));
        assert_eq!(res.status, 400, "{}", res.body);
        let missing = f.go(&Req::post("/request/nope/approve", "").with_cookie(&token));
        assert_eq!(missing.status, 404);
    }

    #[test]
    fn sending_back_requeues_it_with_the_note() {
        let mut f = Fixture::new("send-back");
        let token = f.enrolled();
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
        let token = f.enrolled();
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
    fn a_public_filing_uses_the_configured_repo_whatever_the_body_says() {
        // Proves the repository field is *ignored*, not merely hidden — a
        // stranger must not be able to aim work at a repo nobody nominated.
        let mut f = Fixture::new("public-repo").with_public(false);
        let session = f.signed_in("jo@x.com");

        let res = f.go(
            &Req::post(public_route::FILE, "text=a+thing&kind=bug&repo=secret-repo")
                .with_cookie(&session),
        );
        assert_eq!(res.status, 200, "{}", res.body);

        let filed = f.store.all().unwrap();
        assert_eq!(filed.len(), 1);
        assert_eq!(
            filed[0].repo, "intake",
            "the configured repo, not the body's"
        );
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
            f.store.claim_next().unwrap().is_none(),
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
        assert!(f.store.claim_next().unwrap().is_some());
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
        assert_eq!(f.go(&Req::get("/").with_cookie(&session)).status, 401);
        assert_eq!(
            f.go(&Req::get(&format!("/request/{id}")).with_cookie(&session))
                .status,
            401
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
        let device = f.enrolled();

        let id = f.store.accounts().unwrap().live()[0].id.clone();
        let listed = f.go(&Req::get("/accounts").with_cookie(&device));
        assert_eq!(listed.status, 200);
        assert!(listed.body.contains("jo***@x.com"), "{}", listed.body);

        let res = f.go(&Req::post(&format!("/accounts/{id}/revoke"), "").with_cookie(&device));
        assert_eq!(res.status, 200, "{}", res.body);
        assert!(f.store.accounts().unwrap().live().is_empty());

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
            401
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
        let device = f.enrolled();
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
    fn the_script_policy_reaches_the_public_surface_and_stops_there() {
        // Driven through `handle`, not by calling `csp()`, because the property
        // is about *routing*: the stamp is applied once at the dispatch site, so
        // what this really checks is that the dispatch site is the same one
        // every public route goes through.
        let mut f = Fixture::new("policy-split").with_public(false);
        let account = f.signed_in("filer@example.test");
        let device = f.enrolled();

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
        assert_eq!(stray.status, 401, "not a public route");
        assert_eq!(stray.policy, Policy::Strict);

        // The private surface, including the routes a *device* reaches. These
        // render every filer's spec on one page, which is the reason the
        // permission does not extend here.
        for path in ["/", "/accounts"] {
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
        let device = f.enrolled();
        let res = f.go(&Req::get("/")
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
        let device = f.enrolled();
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
            "/",
            "/enrol",
            // The private surface's own request route must not become public by
            // resembling one.
            "/request/abc",
        ] {
            assert!(!is_public_path(near_miss), "{near_miss} must not be public");
        }
    }

    #[test]
    fn public_traffic_and_enrolment_are_counted_separately() {
        // The property the bucket split exists for, asserted where the
        // classification actually happens rather than only in the limiter.
        let mut f = Fixture::new("bucket-split").with_public(false);
        let probe = |f: &mut Fixture, path: &str| -> Bucket {
            let mut limiter = RateLimiter::new();
            let ctx = Ctx {
                store: &f.store,
                daemon_key: KEY,
                limiter: &mut limiter,
                now_ms: f.now_ms,
                public: f.public.as_ref(),
                mailer: &f.mailer,
                write_lock: &f.write_lock,
            };
            bucket_for(&None, path, &ctx)
        };

        // Sending mail and spending a link cost something; reading a page does
        // not, and starving reads would itself be the denial of service.
        assert_eq!(probe(&mut f, public_route::SIGNIN), Bucket::PublicWrite);
        assert_eq!(probe(&mut f, "/public/signin/abc"), Bucket::PublicWrite);
        assert_eq!(probe(&mut f, public_route::FILE), Bucket::PublicRead);
        assert_eq!(probe(&mut f, "/public/request/abc"), Bucket::PublicRead);

        // And nothing public shares a budget with enrolment.
        assert_eq!(probe(&mut f, "/enrol"), Bucket::Enrol);
        assert_eq!(probe(&mut f, "/"), Bucket::Enrol);
    }

    #[test]
    fn an_enrolled_device_is_not_throttled_by_someone_elses_guessing() {
        let mut f = Fixture::new("throttle-isolated");
        let token = f.enrolled();
        for _ in 0..40 {
            f.go(&Req::post("/enrol", "code=GUESS&label=x"));
        }
        assert_eq!(
            f.go(&Req::get("/").with_cookie(&token)).status,
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
        let token = f.enrolled();
        assert_eq!(f.go(&Req::get("/?x=1").with_cookie(&token)).status, 200);
    }

    #[test]
    fn a_listing_shows_what_needs_a_human_first() {
        let mut f = Fixture::new("order");
        let token = f.enrolled();
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
