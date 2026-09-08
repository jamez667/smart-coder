//! The flame graph, as the desktop client sees it.
//!
//! The parser, the call tree and the layout maths live in **`sc-flame`**, a zero-dependency
//! crate, because they are shared: the same code answers the Profiler panel and the agent's
//! `profile_hotspots` tool ([04](../../../docs/specs/04-tools.md)). Putting them here would
//! have meant `sc-tools` depending on a GUI crate to parse a text file.
//!
//! What stays behind is [`tool`] — finding a profiler and building the command that runs it.
//! That spawns processes and belongs to the client, not to a pure data crate.

pub mod tool;

// The whole pure surface, re-exported so callers in this crate keep saying `flame::…` and the
// split stays an implementation detail rather than a rename that touches every call site.
pub use sc_flame::{
    at_path, hot_frames, layout, matched_percent, matches, parse_folded, short_name, Frame, Placed,
    Profile, ROOT,
};
