//! The compliance plugin's process entry point (spec 13 / 25).
//!
//! Not yet wired to the protocol. This step moved the audit and its optional prose path
//! out of `sc-win` into a crate that compiles on its own — the half the compiler and the
//! existing tests can prove. The panel, the run command and the stdin loop come next,
//! against a crate already known to build.
//!
//! Splitting it that way is deliberate: if this crate compiles and its tests pass, the
//! move itself is correct. Mixing a new protocol loop into the same step would mean a
//! failure could be either.

fn main() {
    // Says so rather than exiting silently: a plugin the host spawns which never
    // handshakes is reported as "exited during the handshake" in the Plugins panel, which
    // is exactly the right message until the loop exists.
    eprintln!(
        "sc-plugin-comply: the compliance audit has moved here, but the plugin protocol \
         loop is not wired up yet (spec 25). Nothing to run."
    );
}
