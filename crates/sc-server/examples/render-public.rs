//! Render every public page to HTML files, for looking at.
//!
//! ```text
//! cargo run -p sc-server --example render-public -- <out-dir>
//! ```
//!
//! Exists because the alternative is worse. Seeing these pages otherwise means
//! configuring a mail provider, standing the server up, and completing a magic
//! link — a lot of setup to answer "does the dark theme look right", and setup
//! that needs an API key for a third party. This renders through the **real**
//! renderers, so what you look at is what gets served.
//!
//! Not a test and not part of the gate: it asserts nothing. The properties worth
//! asserting are asserted in `page::tests`, which run every commit.

use sc_server::i18n::Locale;
use sc_server::page;
use sc_server::store::{Request, RequestState};

use sc_proto::IntakeKind;

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let dir = std::path::Path::new(&out);
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("could not create {out}: {e}");
        std::process::exit(1);
    }

    // Realistic content, because the design questions are about real text: does
    // a long summary wrap, does a spec overflow its box, does a list of mixed
    // states read clearly. Lorem ipsum answers none of those.
    let mut bug = Request::new(
        "r-0001",
        "The export button on the reports page does nothing when more than about \
         fifty rows are selected. Small selections export fine.",
        "alpha",
        IntakeKind::Bug,
    );
    bug.state = RequestState::AwaitingReview;
    bug.spec = Some(
        "# Fix the export on large selections\n\n\
         ## Context\n\n\
         Selecting more than roughly fifty rows makes the export silently fail: \
         no file, no error. The threshold is not exact, which points at a size \
         limit rather than a count.\n\n\
         ## What to change\n\n\
         - Stream the export rather than buffering the whole set\n\
         - Surface the failure instead of swallowing it\n"
            .to_string(),
    );

    let mut feature = Request::new(
        "r-0002",
        "Could the dashboard remember which filters I had set last time?",
        "alpha",
        IntakeKind::Feature,
    );
    feature.state = RequestState::Claimed;

    let mut done = Request::new(
        "r-0003",
        "Typo on the settings page: 'recieve'",
        "alpha",
        IntakeKind::Improvement,
    );
    done.state = RequestState::Accepted;

    let mine = [bug.clone(), feature, done];

    let mut wrote = 0;
    for locale in Locale::ALL {
        let code = locale.code();
        let pages: [(String, String); 8] = [
            (format!("landing-{code}.html"), page::landing_page(locale)),
            (
                format!("signin-{code}.html"),
                page::signin_page_in(locale, true),
            ),
            (format!("sent-{code}.html"), page::signin_sent_page(locale)),
            (
                format!("confirm-{code}.html"),
                page::signin_confirm_page("a-token", locale),
            ),
            (
                format!("link-failed-{code}.html"),
                page::signin_failed_page(true, locale),
            ),
            (
                format!("file-{code}.html"),
                page::public_file_page(
                    &mine,
                    &sc_server::config::Repos::new(&["alpha", "memosy"]),
                    true,
                    locale,
                ),
            ),
            (
                format!("detail-{code}.html"),
                page::public_detail(&bug, true, locale),
            ),
            (
                format!("filed-{code}.html"),
                page::public_filed(&bug, locale),
            ),
        ];
        for (name, html) in pages {
            let path = dir.join(&name);
            match std::fs::write(&path, html) {
                Ok(()) => wrote += 1,
                Err(e) => eprintln!("could not write {name}: {e}"),
            }
        }
    }
    println!("wrote {wrote} pages to {}", dir.display());
}
