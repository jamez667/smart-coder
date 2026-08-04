//! The hosted intake surface (spec 18).
//!
//! A small server the developer deploys wherever they already run things — a
//! Docker image installed in Portainer. It holds **text and nothing else**: a
//! request in, a drafted spec out.
//!
//! ## What this is not
//!
//! It has no repository, no model, no filesystem access to anything the daemon
//! owns, and no route that builds. That is not a policy the handlers enforce —
//! there is simply nothing here that could reach them. The whole vocabulary is:
//!
//! > file a request · watch it draft · read the spec · accept or send back.
//!
//! ## The inversion that makes it safe to expose
//!
//! The **daemon dials out**; this server never calls it. So the developer's
//! machine needs no inbound port, no tunnel, and no address anyone has to guard —
//! the funnel-and-bind-address risk class is removed rather than mitigated.
//!
//! ```text
//!   phone ──HTTPS──▶ sc-server ◀──long-poll── sc-daemon ──▶ the repository
//!                    (public)                 (the developer's machine)
//! ```
//!
//! Accepting marks a spec `Accepted` and **starts nothing**: the developer picks it
//! up in their IDE when they choose to.

pub mod account;
pub mod admin;
pub mod auth;
pub mod config;
pub mod daemons;
pub mod i18n;
pub mod log;
pub mod mail;
pub mod oauth;
pub mod page;
pub mod query;
pub mod ratelimit;
pub mod roster;
pub mod routes;
pub mod screen;
pub mod screen_eval;
pub mod seal;
pub mod serve;
pub mod settings;
pub mod store;

pub use config::Config;
pub use serve::run;
pub use store::{Request, RequestState, Store};

/// Is this server answering on its own port?
///
/// The container's healthcheck, run as `sc-server --health`. **A TCP connect,
/// not an HTTP request**: every route needs a credential or a configured public
/// surface, so any URL worth probing returns 401 or 404 on a perfectly healthy
/// server — and a check that has to enumerate which non-200 responses are fine
/// is a check that eventually calls a broken server healthy. "Is anybody
/// listening" is the question a proxy actually cares about, and it has one
/// unambiguous answer.
///
/// Reads the same environment the server does, so it probes the port the server
/// binds rather than a hardcoded one an operator may have changed.
pub fn health() -> std::result::Result<(), String> {
    let port = Config::from_env().map(|c| c.port).unwrap_or(8420);
    // Loopback rather than the configured bind: this runs *inside* the
    // container, where `0.0.0.0` is not an address to connect to.
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2))
        .map(|_| ())
        .map_err(|e| format!("nothing listening on {port}: {e}"))
}
