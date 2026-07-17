//! `sc-win` entry point. The iced application lives in [`app`]; this just launches it.

// Release builds are a pure GUI app — suppress the console window Windows would
// otherwise allocate (the stray blank terminal). Debug builds keep the console so
// panics and diagnostics stay visible while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod canvas;
mod minimap;

fn main() -> iced::Result {
    app::run()
}
