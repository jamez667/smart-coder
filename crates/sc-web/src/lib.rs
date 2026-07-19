//! `sc-web` — a local web dashboard that visualizes a live agent run in the
//! browser.
//!
//! It consumes the same `sc_core` event stream the TUI does (spec 01): the agent
//! runs on a worker thread feeding a [`Hub`]; a small blocking HTTP server
//! ([`serve`]) serves an embedded dashboard page and an incremental `/events`
//! JSON feed the browser polls. No async runtime, no separate frontend build —
//! open the printed `localhost` URL and watch.

mod hub;
mod iterate_server;
mod mirror_server;
mod remote_confirm;
mod server;
mod swarm_server;

pub use hub::{sse_frame, FnHubSink, Hub, HubSink};
pub use iterate_server::{serve_iterate, IterateServer};
pub use mirror_server::{serve_mirror, InboundCmd, RemoteMirror};
pub use remote_confirm::RemoteConfirmer;
pub use server::{events_body, parse_from, serve, WebRun};

/// Mint a fresh 256-bit session token as a 64-char lowercase hex string. Used as the
/// per-run bearer secret the dashboard requires on every request (defense-in-depth
/// behind the Tailscale tunnel). Falls back to a time+pid seed only if the OS RNG is
/// somehow unavailable — never panics the run.
pub fn mint_token() -> String {
    let mut bytes = [0u8; 32];
    if getrandom::fill(&mut bytes).is_err() {
        // Extremely unlikely; keep the run alive with a weaker seed rather than crash.
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id() as u128;
        let mix = t ^ (pid << 64);
        bytes[..16].copy_from_slice(&mix.to_le_bytes());
        bytes[16..].copy_from_slice(&mix.rotate_left(33).to_le_bytes());
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub use swarm_server::{serve_swarm, SwarmHub, WebSwarm};
