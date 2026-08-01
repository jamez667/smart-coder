//! The HTTP layer: bytes off a wire, and the long poll.
//!
//! Everything that decides anything lives in [`routes`](crate::routes) as a pure
//! function. This module reads a request, calls it, and writes the response — so
//! the untested surface is as small as it can be.
//!
//! **No TLS in-process.** A reverse proxy terminates it, which is how this is
//! deployed anyway (Portainer, behind whatever the developer already runs).
//! Certificates, renewal and a private key inside the container are three failure
//! modes solving a problem that is already solved outside it.

use std::sync::{Arc, Mutex};

use sc_daemon::wire;
use sc_proto::{DcError, Result};

use crate::config::Config;
use crate::ratelimit::RateLimiter;
use crate::routes::{self, Ctx, Req, Res};
use crate::store::{now_ms, Store};

/// How long a single idle poll sleeps before re-checking for work.
///
/// Small relative to [`wire::POLL_TIMEOUT`], so a request filed while a daemon is
/// mid-poll is picked up in well under a second rather than waiting out the hold.
const POLL_TICK: std::time::Duration = std::time::Duration::from_millis(250);

/// Run the server until the process is killed.
pub fn run(cfg: &Config) -> Result<()> {
    let store = Store::open(&cfg.data_dir)?;

    // Arm enrolment on first start, so a fresh container is usable without
    // pre-configuration but is never *open*.
    let code = arm_enrolment(&store, cfg)?;
    let limiter = Arc::new(Mutex::new(RateLimiter::new()));

    let server = tiny_http::Server::http(cfg.addr())
        .map_err(|e| DcError::Eval(format!("could not bind {}: {e}", cfg.addr())))?;

    println!("sc-server listening on {}", cfg.addr());
    println!("state in {}", cfg.data_dir.display());
    if let Some(code) = &code {
        // Printed once, at startup, because it is the only way in on a fresh
        // install and nothing else will show it — it is stored hashed.
        println!("\n  enrolment code: {code}\n  (single use; open the site and type it)\n");
    }

    for request in server.incoming_requests() {
        let store = store.clone();
        let limiter = Arc::clone(&limiter);
        let key = cfg.daemon_key.clone();
        // A thread per request: this serves one developer and a handful of
        // daemons, so a thread pool would be machinery without a load to justify
        // it. The long poll needs a blocking thread regardless.
        std::thread::spawn(move || {
            if let Err(e) = serve_one(request, &store, &key, &limiter) {
                eprintln!("request failed: {e}");
            }
        });
    }
    Ok(())
}

/// Make sure there is a way in, and return a code if one was freshly minted.
fn arm_enrolment(store: &Store, cfg: &Config) -> Result<Option<String>> {
    let mut creds = store.credentials()?;
    if !creds.live().is_empty() {
        // Somebody is already enrolled; minting a code every restart would leave
        // a standing way in that the developer never asked for.
        return Ok(None);
    }
    let code = cfg
        .enrol_code
        .clone()
        .unwrap_or_else(crate::auth::mint_enrol_code);
    creds.set_enrol_code(&code);
    store.put_credentials(&creds)?;
    Ok(Some(code))
}

fn serve_one(
    mut request: tiny_http::Request,
    store: &Store,
    daemon_key: &str,
    limiter: &Arc<Mutex<RateLimiter>>,
) -> Result<()> {
    let req = read(&mut request)?;
    let is_poll = req.method == "GET" && req.path.split('?').next() == Some(wire::route::WORK);

    let mut res = dispatch(store, daemon_key, limiter, &req);

    // The long poll: hold the connection open rather than answering "nothing"
    // immediately, so a request filed on a train is picked up in under a second
    // with almost no idle traffic.
    if is_poll && res.hold_for_work {
        let deadline = std::time::Instant::now() + wire::POLL_TIMEOUT;
        while std::time::Instant::now() < deadline {
            std::thread::sleep(POLL_TICK);
            let again = dispatch(store, daemon_key, limiter, &req);
            if !again.hold_for_work {
                res = again;
                break;
            }
        }
    }

    write(request, res)
}

fn dispatch(store: &Store, daemon_key: &str, limiter: &Arc<Mutex<RateLimiter>>, req: &Req) -> Res {
    let now = now_ms();
    let mut guard = match limiter.lock() {
        Ok(g) => g,
        // A poisoned lock means another thread panicked mid-request. Recovering
        // the guard is right here: the limiter is a counter, so the worst a
        // partial update costs is one request counted twice.
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.sweep(now);
    let mut ctx = Ctx {
        store,
        daemon_key,
        limiter: &mut guard,
        now_ms: now,
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
            "cookie" => cookie_token = cookie_value(value, routes::COOKIE),
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
    // eventually missing from one.
    for (name, value) in routes::security_headers() {
        if let Ok(h) = format!("{name}: {value}").parse() {
            headers.push(h);
        }
    }
    if let Some(cookie) = &res.set_cookie {
        if let Ok(h) = format!("Set-Cookie: {cookie}").parse() {
            headers.push(h);
        }
    }

    let mut response = tiny_http::Response::from_string(res.body).with_status_code(res.status);
    for h in headers {
        response.add_header(h);
    }
    request
        .respond(response)
        .map_err(|e| DcError::Eval(format!("could not respond: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

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
