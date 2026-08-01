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
//! > file a request · watch it draft · read the spec · approve or send back.
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
//! Approving marks a spec `Ready` and **starts nothing**: the developer picks it
//! up in their IDE when they choose to.

pub mod account;
pub mod auth;
pub mod config;
pub mod i18n;
pub mod mail;
pub mod page;
pub mod ratelimit;
pub mod routes;
pub mod screen;
pub mod screen_eval;
pub mod serve;
pub mod store;

pub use config::Config;
pub use serve::run;
pub use store::{Request, RequestState, Store};
