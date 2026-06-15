//! `dc-win` entry point. The iced application lives in [`app`]; this just launches it.

mod app;
mod canvas;

fn main() -> iced::Result {
    app::run()
}
