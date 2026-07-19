//! `sc-win` entry point. The iced application lives in [`app`]; this just launches it.

// Release builds are a pure GUI app — suppress the console window Windows would
// otherwise allocate (the stray blank terminal). Debug builds keep the console so
// panics and diagnostics stay visible while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod canvas;
mod minimap;

fn main() -> iced::Result {
    // Default the model-call transcript log into %APPDATA%\smart-coder\logs (next to config +
    // recents), unless the user already set SC_LOG_DIR or disabled logging with SC_NO_LOG.
    // sc-model writes one transcript-<ts>-<pid>.jsonl per launch there.
    if std::env::var_os("SC_LOG_DIR").is_none() && std::env::var_os("SC_NO_LOG").is_none() {
        if let Some(dir) = sc_win::config::log_dir() {
            std::env::set_var("SC_LOG_DIR", dir);
        }
    }

    // `sc-win --remote-history` (or `--sessions`): print the remote-mirror session history
    // (current/active URLs first) and exit, instead of launching the GUI.
    if std::env::args().any(|a| a == "--remote-history" || a == "--sessions") {
        app::print_remote_history();
        return Ok(());
    }
    app::run()
}
