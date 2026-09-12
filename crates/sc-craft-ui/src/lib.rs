//! **The editor half of Smart Coder** (spec 21) — shared by both products, dependent on
//! no model code.
//!
//! # The split
//!
//! Smart Coder ships as two executables:
//!
//! * **`smart-coder`** — the agent app. `sc-win`, which depends on this crate and adds
//!   chat, the swarm, review gates, the remote mirror, and the model backends.
//! * **`smart-coder-crafter`** — the editor. `sc-crafter`, which depends on this crate
//!   and adds nothing that talks to a model, because there is nothing to add.
//!
//! Spec 21 always described Craft mode as "the app's other half, which happens to have
//! been built second" — Assistant mode being Craft mode *plus* an agent. This crate is
//! that other half, made structural. It was a runtime predicate (`cfg.craft()`, checked
//! at every backend builder) and a Cargo feature that pinned the predicate true; both are
//! gone. The editor no longer *refuses* to contact a model. It has no way to.
//!
//! # The guarantee, and how it is kept
//!
//! `cargo tree -p sc-crafter` names no crate that can reach a model. That is the whole
//! claim, it is checked by `scripts/check.*`, and it is stronger than the ~1,600 lines of
//! tests it replaced: a test proves that a path was refused on the day it ran, while a
//! dependency tree proves the path does not exist.
//!
//! What this costs, stated plainly: those tests (`craft_mode_refuses_to_send_chat`,
//! `craft_mode_never_spawns_the_health_probe`, and their siblings) are gone, and most
//! could not be rewritten here even in principle — you cannot assert that the Crafter
//! refuses to send a chat message when `Message::ChatSend` is not a variant it has.
//!
//! **So the rule for this crate is a dependency rule, not a code rule:** adding
//! `sc-core`, `sc-model`, `sc-swarm`, `sc-workflow`, `sc-iterate`, `sc-verify`,
//! `sc-tools` or `sc-web` to `Cargo.toml` breaks the product silently. It would still
//! compile. It would still run. It would no longer be what it says it is.
//!
//! # What lives here
//!
//! Everything an editor needs and an agent also uses: buffers and save rules
//! ([`editbuf`]), the panel tree ([`layout`]), git ([`gitdiff`]), the file tree
//! ([`filetree`]), the command runner ([`terminal`]), compile diagnostics
//! ([`diagnostics`]), the profiler ([`flame`], [`flamecanvas`]), markdown, the project
//! model, inline review comments ([`comments`]), and per-product paths ([`config`]).
//!
//! Two modules were split rather than moved, and each names its other half: "follow the
//! agent" left [`codeview`] for `sc_win::follow`, and workflow send-back left [`comments`]
//! for `sc_win::sendback`. Both halves read `AgentEvent`/`Phase`; neither could come here.

pub mod codeview;
pub mod comments;
pub mod config;
pub mod diagnostics;
pub mod editbuf;
pub mod filetree;
pub mod flame;
pub mod flamecanvas;
pub mod gitdiff;
pub mod layout;
pub mod markdown;
pub mod minimap;
pub mod persist;
pub mod plugin;
pub mod proc;
pub mod project;
pub mod splits;
pub mod terminal;
pub mod welcome;

pub use codeview::CodeView;
pub use config::{state_dir, CraftConfig, Product};
pub use diagnostics::{CompileReport, Diagnostic, Severity};
pub use editbuf::{Classified, DiskStamp, Ending, NoEdit, SaveVerdict};
pub use filetree::{build_rows, is_noise_dir, TreeRow};
pub use flame::{layout as flame_layout, parse_folded, Frame, Placed, Profile};
pub use layout::{Axis, Layout, LayoutStore, PanelKind, PanelSlot, Side};
pub use project::{CompileCommand, ProjectKind};
