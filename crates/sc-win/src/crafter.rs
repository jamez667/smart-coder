//! `smart-coder-crafter` — the editor, and only the editor (spec 21 / spec 25 step 6).
//!
//! Ten lines, and that is the whole point. It runs the same [`app`](crate::app) as
//! `smart-coder`; the difference is one call to `set_product` before anything reads
//! state, and everything downstream that already branches on it:
//!
//! * **Its own state directory** — `%APPDATA%\smart-coder-crafter\`, so `config.json`,
//!   `layout.json`, the recents list and the logs never mix with the agent build's.
//! * **Its own default layout** — `Layout::craft_default()`: git and files beside the
//!   editor, over the bottom strip. No chat column, because there is no chat.
//! * **Its own plugin directory** — `<state dir>/plugins/`. A plugin installed for the
//!   agent build is a process this one would otherwise start, which is a stronger reason
//!   for the split than the config files were.
//! * **Its own title** — `Product::display_name()`, the one place the name is spelled.
//!
//! All of that was written and unit-tested when the products split; none of it had ever
//! run, because nothing called `set_product` and the atomic defaults to `SmartCoder`.
//! This binary is what turns it on.
//!
//! # Why a second `[[bin]]` and not a second crate
//!
//! The manifest used to say a bin target here would link "every model crate", so the
//! Crafter had to be its own crate. **That stopped being true when the agent left.**
//! `sc-win`'s dependency tree is now six crates — `sc-craft-ui`, `sc-editor`, `sc-flame`,
//! `sc-fsutil`, `sc-index`, `sc-plugin-proto` — and not one of them can reach a model.
//! `scripts/check.*` asserts exactly that, now against this binary as well as the crate.
//!
//! So the reason for a separate crate was a dependency argument that has been won. A
//! second crate would mean duplicating `app/` or inventing an indirection to share it;
//! this is ten lines.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;

fn main() -> iced::Result {
    // FIRST, before any state is read. `state_dir()` resolves through `product()`, so a
    // path resolved before this call would point at the agent build's directory — which
    // is why `set_product`'s own doc says to call it once, first thing in `main`.
    sc_craft_ui::config::set_product(sc_craft_ui::config::Product::Crafter);
    app::run()
}
