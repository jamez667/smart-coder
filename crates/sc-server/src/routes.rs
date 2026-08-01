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

use sc_daemon::wire::{self, DraftFailed, DraftedSpec, PollResponse, WireError, WorkItem};
use sc_daemon::IntakeKind;

use crate::auth::{self, Caller, Credentials};
use crate::ratelimit::{Bucket, RateLimiter};
use crate::store::{new_id, Request, RequestState, Store};

/// The cookie a browser carries once enrolled.
pub const COOKIE: &str = "sc_device";

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
}

/// Route one request.
pub fn handle(ctx: &mut Ctx<'_>, req: &Req) -> Res {
    let creds = match ctx.store.credentials() {
        Ok(c) => c,
        Err(e) => return error(500, &format!("the credential store is unreadable: {e}")),
    };
    let caller = identify(req, ctx.daemon_key, &creds);

    // Rate limit before anything else touches the store, so a guessing loop costs
    // one hash rather than a disk read per attempt.
    let bucket = match &caller {
        Some(Caller::Daemon) => Bucket::Credential(auth::hash(ctx.daemon_key)),
        Some(Caller::Device { id }) => Bucket::Credential(auth::hash(id)),
        None => Bucket::Anonymous,
    };
    if !ctx.limiter.allow(bucket, ctx.now_ms) {
        return error(429, "too many requests — wait a minute and try again");
    }

    let path = req.path.split('?').next().unwrap_or("").to_string();
    let method = req.method.as_str();

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

/// Who is calling, if anyone.
fn identify(req: &Req, daemon_key: &str, creds: &Credentials) -> Option<Caller> {
    if let Some(bearer) = &req.bearer {
        if auth::matches(bearer, &auth::hash(daemon_key)) {
            return Some(Caller::Daemon);
        }
    }
    if let Some(token) = &req.cookie_token {
        if let Some(device) = creds.device_for(token) {
            return Some(Caller::Device {
                id: device.id.clone(),
            });
        }
    }
    None
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
    // A cap, because the body is stored verbatim and rendered: without one, a
    // single request can fill the volume.
    const MAX: usize = 16 * 1024;
    if text.len() > MAX {
        return Res::html(
            400,
            crate::page::message(&format!(
                "That is {} characters; the limit is {MAX}. Say the essential part \
                 — the spec is drafted from it, not copied from it.",
                text.len()
            )),
        );
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
    use crate::store::now_ms;
    use std::path::PathBuf;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    struct Fixture {
        store: Store,
        limiter: RateLimiter,
        dir: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Fixture {
        fn new(tag: &str) -> Fixture {
            let dir = std::env::temp_dir().join(format!(
                "sc-routes-{tag}-{}-{}",
                std::process::id(),
                now_ms()
            ));
            let store = Store::open(&dir).unwrap();
            Fixture {
                store,
                limiter: RateLimiter::new(),
                dir,
            }
        }

        fn go(&mut self, req: &Req) -> Res {
            let mut ctx = Ctx {
                store: &self.store,
                daemon_key: KEY,
                limiter: &mut self.limiter,
                now_ms: 1_000,
            };
            handle(&mut ctx, req)
        }

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

        let huge = format!("text={}&repo=alpha", "x".repeat(20_000));
        let over = f.go(&Req::post("/file", &huge).with_cookie(&token));
        assert_eq!(over.status, 400);
        assert!(over.body.contains("limit"), "{}", over.body);
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
