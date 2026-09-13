//! The agent plugin's process entry point (spec 25).
//!
//! Deliberately thin, and **not yet wired to the protocol**: this step moved the agent's
//! four load-bearing pieces out of `sc-win` into a crate that compiles on its own, which
//! is the half that can be proven by the compiler and the existing tests. The panel UI,
//! the `Ask`/`Answered` approval plumbing and the stdin loop come next, against a crate
//! that is already known to build.
//!
//! Splitting it that way is the point: `chat/`, `chat_session.rs`, `session/` and
//! `bridge.rs` moved with no iced types to strip and almost no imports to rewrite, so if
//! this crate compiles and its tests pass, the move itself is correct. Mixing that with a
//! new protocol loop would mean a failure could be either.

fn main() {
    // A placeholder that says so, rather than a silent no-op: a plugin the host spawns
    // and which exits without a handshake is reported as "exited during the handshake"
    // in the Plugins panel, which is exactly right until the loop exists.
    eprintln!(
        "sc-plugin-agent: the agent core has moved here, but the plugin protocol loop \
         is not wired up yet (spec 25). Nothing to run."
    );
}
