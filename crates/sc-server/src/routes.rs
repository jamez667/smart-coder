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
use crate::mail::Mailer;
use crate::ratelimit::{Bucket, RateLimiter};
use crate::store::{new_id, Request, RequestState, Store};

/// The cookie a browser carries once enrolled.
pub const COOKIE: &str = "sc_device";

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
    pub body: String,
}

impl Req {
    pub fn get(path: &str) -> Req {
        Req {
            method: "GET".into(),
            path: path.into(),
            bearer: None,
            cookie_token: None,
            body: String::new(),
        }
    }

    pub fn post(path: &str, body: &str) -> Req {
        Req {
            method: "POST".into(),
            path: path.into(),
            bearer: None,
            cookie_token: None,
            body: body.into(),
        }
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
    /// A `Set-Cookie` value, used exactly once: at enrolment.
    pub set_cookie: Option<String>,
    /// Set when the handler wants the caller to hold the connection open — the
    /// long-poll. The HTTP layer waits, then calls back.
    pub hold_for_work: bool,
}

impl Res {
    pub fn json(status: u16, body: impl Into<String>) -> Res {
        Res {
            status,
            content_type: "application/json",
            body: body.into(),
            set_cookie: None,
            hold_for_work: false,
        }
    }

    pub fn html(status: u16, body: impl Into<String>) -> Res {
        Res {
            status,
            content_type: "text/html; charset=utf-8",
            body: body.into(),
            set_cookie: None,
            hold_for_work: false,
        }
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

/// The three headers on **every** response, without exception.
///
/// Rendering model-authored Markdown is an exfiltration path: one hallucinated
/// remote image leaks the page URL — which identifies the request — through the
/// `Referer` header. `sc-web` sends none of these today; spec 18 says this path
/// must not inherit that.
///
/// They are returned from one function rather than added per route, because a
/// header added per route is a header eventually missing from one.
pub fn security_headers() -> [(&'static str, &'static str); 5] {
    [
        // No `Referer` anywhere, so a remote subresource cannot leak the URL.
        ("Referrer-Policy", "no-referrer"),
        // A drafted spec is not something to leave in a proxy or a browser cache.
        ("Cache-Control", "no-store"),
        // No remote subresources at all: the CSP is what makes the exfiltration
        // path unreachable rather than merely unreferred.
        (
            "Content-Security-Policy",
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; \
             base-uri 'none'; frame-ancestors 'none'",
        ),
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
    if is_public_path(&path) {
        return match ctx.public {
            Some(_) => public_route(ctx, req, method, &path, &caller),
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

    let mut res = Res::html(200, crate::page::enrolled_page());
    // HttpOnly so script cannot read it; SameSite=Strict so a cross-site form
    // cannot ride it; Secure because this is served over TLS at the proxy.
    res.set_cookie = Some(format!(
        "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Secure; Max-Age=31536000"
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

    match (method, path) {
        // Ask for a link. Reachable signed-out — it is how one signs in.
        ("GET", public_route::SIGNIN) => Res::html(200, crate::page::signin_page()),
        ("POST", public_route::SIGNIN) => request_sign_in(ctx, req),

        ("POST", public_route::SIGNOUT) => sign_out(ctx, req),

        // The landing page a link opens. **Changes nothing** — mail scanners
        // fetch every URL in a message, and a GET that spent the token would
        // burn it before the human saw it.
        ("GET", p) if p.starts_with(public_route::SIGNIN_PREFIX) => {
            let token = p.trim_start_matches(public_route::SIGNIN_PREFIX);
            // Rendered whether or not the token is real: a 404 on an invalid one
            // would be a free validity oracle, cheaper than the POST it guards.
            Res::html(200, crate::page::signin_confirm_page(token))
        }
        ("POST", p) if p.starts_with(public_route::SIGNIN_PREFIX) => {
            complete_sign_in(ctx, p.trim_start_matches(public_route::SIGNIN_PREFIX))
        }

        // Everything below needs a signed-in filer.
        _ => match account_id {
            Some(id) => signed_in_route(ctx, req, method, path, &id),
            None => Res::html(200, crate::page::signin_page()),
        },
    }
}

fn signed_in_route(
    ctx: &mut Ctx<'_>,
    req: &Req,
    method: &str,
    path: &str,
    account_id: &str,
) -> Res {
    let show_spec = ctx.public.map(|p| p.show_spec).unwrap_or(false);

    match (method, path) {
        ("GET", public_route::FILE) => match mine(ctx, account_id) {
            Ok(list) => Res::html(200, crate::page::public_file_page(&list, show_spec)),
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
                    Res::html(200, crate::page::public_detail(&r, show_spec))
                }
                // Somebody else's request is *not found*, not forbidden:
                // "forbidden" would confirm the id exists.
                Ok(_) => Res::html(404, crate::page::not_found()),
                Err(e) => error(500, &e.to_string()),
            }
        }

        _ => Res::html(404, crate::page::not_found()),
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
    Res::html(200, crate::page::signin_sent_page())
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
fn complete_sign_in(ctx: &mut Ctx<'_>, token: &str) -> Res {
    let _guard = ctx.write_lock.lock().unwrap_or_else(|p| p.into_inner());

    let mut links = match ctx.store.links() {
        Ok(l) => l,
        Err(e) => return error(500, &e.to_string()),
    };
    let (email_hash, email_hint) = match links.consume(token, ctx.now_ms) {
        Ok(v) => v,
        Err(account::LinkError::AlreadyUsed) => {
            return Res::html(200, crate::page::signin_failed_page(true))
        }
        Err(account::LinkError::Invalid) => {
            return Res::html(200, crate::page::signin_failed_page(false))
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
            return Res::html(200, crate::page::signin_failed_page(false));
        }
    }
    let id = match accounts.by_email(&email_hash) {
        Some(a) => a.id.clone(),
        None => accounts.create(&email_hash, &email_hint, ctx.now_ms).id,
    };
    let session = accounts.open_session(&id, ctx.now_ms);
    if let Err(e) = ctx.store.put_accounts(&accounts) {
        return error(500, &e.to_string());
    }

    let mut res = Res::html(200, crate::page::public_file_page(&[], false));
    res.set_cookie = Some(format!(
        "{COOKIE}={session}; Path=/; HttpOnly; SameSite=Strict; Secure; Max-Age=31536000"
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
    let mut res = Res::html(200, crate::page::signin_page());
    // Max-Age=0 so the browser drops it rather than carrying a dead token.
    res.set_cookie = Some(format!(
        "{COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Secure; Max-Age=0"
    ));
    res
}

/// File a request from the public surface.
///
/// The repository comes from **configuration**, never the body — so a stranger
/// cannot aim work at a repository the operator did not nominate for public
/// intake. The form has no such field, and one submitted anyway is ignored.
fn file_publicly(ctx: &mut Ctx<'_>, req: &Req, account_id: &str) -> Res {
    let Some(public) = ctx.public else {
        return Res::html(404, crate::page::not_found());
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
        return Res::html(400, crate::page::message("A request needs some text."));
    }
    if let Err(msg) = check_length(text) {
        return Res::html(400, crate::page::message(&msg));
    }

    let request = Request::public(new_id(), text, &repo, kind, account_id, screened);
    match ctx.store.put(&request) {
        Ok(()) => Res::html(200, crate::page::public_filed(&request)),
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
                mail: crate::config::MailConfig {
                    provider: crate::mail::Provider::Brevo,
                    api_key: KEY.into(),
                    from: "noreply@example.test".into(),
                    from_name: "Smart Coder".into(),
                },
                screen: screened.then(|| crate::config::ScreenConfig {
                    api_key: KEY.into(),
                    url: "https://screen.example.test".into(),
                    model: "test-model".into(),
                }),
                max_outstanding_links: 200,
                show_spec: true,
            });
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

        // The daemon redrafts under them.
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

    #[test]
    fn the_three_headers_spec_18_names_are_all_present() {
        // Rendering model-authored Markdown is an exfiltration path: one
        // hallucinated remote image leaks the page URL via `Referer`.
        let headers = security_headers();
        let named: Vec<&str> = headers.iter().map(|(k, _)| *k).collect();
        for required in [
            "Referrer-Policy",
            "Cache-Control",
            "Content-Security-Policy",
        ] {
            assert!(named.contains(&required), "missing {required}");
        }
        let referrer = headers
            .iter()
            .find(|(k, _)| *k == "Referrer-Policy")
            .unwrap();
        assert_eq!(referrer.1, "no-referrer");
        let cache = headers.iter().find(|(k, _)| *k == "Cache-Control").unwrap();
        assert_eq!(cache.1, "no-store");
    }

    #[test]
    fn the_csp_forbids_remote_subresources() {
        // This is what makes the exfiltration path unreachable rather than
        // merely unreferred.
        let csp = security_headers()
            .iter()
            .find(|(k, _)| *k == "Content-Security-Policy")
            .unwrap()
            .1;
        assert!(csp.starts_with("default-src 'none'"), "{csp}");
        assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
        assert!(
            !csp.contains("https:"),
            "no remote origin is allowed: {csp}"
        );
        assert!(!csp.contains('*'), "no wildcard origin: {csp}");
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
        let named: Vec<&str> = security_headers().iter().map(|(k, _)| *k).collect();
        for required in [
            "Referrer-Policy",
            "Cache-Control",
            "Content-Security-Policy",
        ] {
            assert!(named.contains(&required));
        }
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
