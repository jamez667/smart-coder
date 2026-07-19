//! End-to-end HTTP surface test for the remote-drive server: drive a real
//! `serve()` over a TCP socket and assert the token gate and event wire.
//!
//! The agent itself just finishes on turn one (a `CallbackBackend` that emits a
//! `finish` call), so the test is about the HTTP layer — auth, routing, the
//! `/events` feed — not the loop. The RemoteConfirmer round-trip (block → resolve)
//! is unit-tested with real threads in `remote_confirm.rs`; here we prove the
//! server binds, gates on the token, and serves the stream.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use sc_model::{Capabilities, CallbackBackend, GenerateResponse, ToolCalling};
use sc_web::{serve, WebRun};

/// A backend that finishes immediately: one turn, one `finish` tool call.
fn finishing_backend() -> CallbackBackend<impl Fn(&sc_model::GenerateRequest) -> sc_proto::Result<GenerateResponse>>
{
    let caps = Capabilities {
        max_context_tokens: 8_192,
        tool_calling: ToolCalling::None,
        on_device: false,
    };
    CallbackBackend::new("finisher", caps, |_req| {
        Ok(GenerateResponse {
            content: "{\"tool\":\"finish\",\"reason\":\"done\"}".to_string(),
        })
    })
}

/// Minimal blocking HTTP GET; returns (status_code, body).
fn http_get(addr: &str, path: &str) -> (u16, String) {
    let host = addr.trim_start_matches("http://");
    let mut stream = TcpStream::connect(host).expect("connect");
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes())
        .expect("write");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("read");
    let status = raw
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = raw.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    (status, body)
}

#[test]
fn events_route_requires_the_token() {
    let token = "test-token-abc";
    let spec = WebRun {
        backend: finishing_backend(),
        advisor: None::<CallbackBackend<fn(&sc_model::GenerateRequest) -> sc_proto::Result<GenerateResponse>>>,
        registry: sc_tools::default_registry(),
        strategy: sc_core::select_strategy(&Capabilities {
            max_context_tokens: 8_192,
            tool_calling: ToolCalling::None,
            on_device: false,
        }),
        instruction: "noop".to_string(),
        workspace: std::env::temp_dir(),
        config: sc_core::AgentConfig::default(),
    };

    let (tx, rx) = mpsc::channel();
    // serve() blocks until the run finishes AND a client drains /events, so run it
    // on a worker and talk to it from the test thread.
    let handle = thread::spawn(move || {
        let _ = serve(spec, "127.0.0.1:0", token, |url| {
            tx.send(url).unwrap();
        });
    });
    let addr = rx.recv_timeout(Duration::from_secs(5)).expect("server bound");

    // No token → 401 on both the page and the feed.
    assert_eq!(http_get(&addr, "/events?from=0").0, 401, "events without token must be 401");
    assert_eq!(http_get(&addr, "/").0, 401, "page without token must be 401");
    // Wrong token → 401.
    assert_eq!(http_get(&addr, "/events?from=0&k=wrong").0, 401);

    // The page WITH the token in the query string must be 200 — the phone opens
    // `/?k=<token>`, so the `/` route has to match despite the trailing query.
    let (page_status, page_body) = http_get(&addr, &format!("/?k={token}"));
    assert_eq!(page_status, 200, "page with token must be 200");
    assert!(page_body.contains("<!doctype html>"), "page body is the dashboard");

    // Correct token → 200, a valid feed carrying real events, that reaches done.
    // Advance `from` by the feed's `next` each poll; serve()'s exit condition is
    // `done && parse_from(url) >= hub.len()`, so we MUST catch up to the tail for the
    // server to shut down (a poll parked at from=0 would keep it alive forever).
    let mut done = false;
    let mut saw_run_started = false;
    let mut from = 0usize;
    for _ in 0..1000 {
        let (status, body) = http_get(&addr, &format!("/events?from={from}&k={token}"));
        assert_eq!(status, 200, "events with token must be 200: {body}");
        assert!(body.contains("\"events\""), "feed shape: {body}");
        if body.contains("\"RunStarted\"") {
            saw_run_started = true;
        }
        if let Some(n) = body.split_once("\"next\":").and_then(|(_, r)| {
            r.split(|c: char| !c.is_ascii_digit())
                .find(|s| !s.is_empty())
                .and_then(|d| d.parse::<usize>().ok())
        }) {
            from = n;
        }
        if body.contains("\"done\":true") {
            done = true;
            // One more poll at the tail so serve()'s exit condition trips.
            let _ = http_get(&addr, &format!("/events?from={from}&k={token}"));
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(saw_run_started, "feed should carry the RunStarted event");
    assert!(done, "run should reach done (Stopped drains the feed)");
    handle.join().unwrap();
}
