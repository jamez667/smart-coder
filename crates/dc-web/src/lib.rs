//! `dc-web` — a local web dashboard that visualizes a live agent run in the
//! browser.
//!
//! It consumes the same `dc_core` event stream the TUI does (spec 01): the agent
//! runs on a worker thread feeding a [`Hub`]; a small blocking HTTP server
//! ([`serve`]) serves an embedded dashboard page and an incremental `/events`
//! JSON feed the browser polls. No async runtime, no separate frontend build —
//! open the printed `localhost` URL and watch.

mod hub;
mod server;

pub use hub::{sse_frame, Hub, HubSink};
pub use server::{events_body, parse_from, serve, WebRun};
