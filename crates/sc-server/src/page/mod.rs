//! The rendered HTML, split by who sees it.
//!
//! | Module | Audience | Script |
//! |---|---|---|
//! | [`private`] | the developer's enrolled devices | **never** |
//! | [`public`] | anyone with an account | permitted |
//!
//! The split is not organisational tidiness. The two halves have different
//! security postures and the boundary is where that difference lives — see
//! [`Policy`](crate::routes::Policy) for the headers each is served with.
//!
//! ## Everything model-authored is escaped
//!
//! A drafted spec is untrusted text: a model wrote it, and it may contain
//! anything. Both halves render it as **escaped text in a `<pre>`**, never as
//! Markdown and never as HTML. That removes the whole class rather than
//! filtering it — one remote image reference in a rendered spec is an
//! exfiltration path, and a filter that has to be right every time eventually
//! is not.

pub mod private;
pub mod public;

// Re-exported so `crate::page::not_found()` and friends keep resolving exactly
// as they did before the split. Every call site in `routes.rs` is unchanged,
// which is what makes this a mechanical move rather than a rewrite.
pub use private::{
    accounts_page, confirm_approve, detail, enrol_page, enrol_page_with_error, enrolled_page,
    filed, index, message, not_found,
};
pub use public::{
    public_detail, public_file_page, public_filed, signin_confirm_page, signin_failed_page,
    signin_page, signin_sent_page,
};

/// Escape for HTML text content and attributes.
///
/// Applied to **everything** that did not come from this crate. There is no
/// "trusted" path: the request text was typed by a person on the internet and
/// the spec was written by a model.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// The stylesheet. Inline, because a separate file would be one more request on
/// a bad connection — and on the private half the CSP allows no remote
/// subresource at all.
pub(crate) const STYLE: &str = "\
:root { color-scheme: light dark; }
* { box-sizing: border-box; }
body { font: 16px/1.55 system-ui, -apple-system, 'Segoe UI', sans-serif;
       margin: 0; padding: 1rem; max-width: 46rem; margin-inline: auto; }
h1 { font-size: 1.3rem; margin: 0 0 1rem; }
h2 { font-size: 1.05rem; margin: 1.5rem 0 .5rem; }
a { color: inherit; }
form { margin: 0; }
label { display: block; margin: .75rem 0 .25rem; font-weight: 600; font-size: .9rem; }
textarea, input, select, button {
  font: inherit; width: 100%; padding: .6rem; border-radius: .4rem;
  border: 1px solid rgba(128,128,128,.5); background: transparent; color: inherit; }
textarea { min-height: 7rem; resize: vertical; }
button { cursor: pointer; margin-top: .75rem; font-weight: 600; }
.row { display: flex; gap: .5rem; }
.row > form { flex: 1; }
.item { display: block; padding: .7rem; margin: .4rem 0; text-decoration: none;
        border: 1px solid rgba(128,128,128,.35); border-radius: .5rem; }
.meta { font-size: .8rem; opacity: .7; }
.tag { display: inline-block; font-size: .72rem; padding: .1rem .45rem;
       border-radius: .8rem; border: 1px solid currentColor; opacity: .85; }
pre { white-space: pre-wrap; word-wrap: break-word; padding: .8rem;
      border: 1px solid rgba(128,128,128,.35); border-radius: .5rem;
      font: 13px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace; }
.note { padding: .7rem; border-left: 3px solid currentColor; opacity: .85;
        font-size: .9rem; }
.decide { margin-top: 1.5rem; padding-top: 1rem;
          border-top: 1px solid rgba(128,128,128,.35); }
.skip { display: block; font-size: .85rem; opacity: .7; margin: .5rem 0; }
.elided { text-align: center; opacity: .6; font-size: .85rem;
          padding: .4rem; font-style: italic; }
";

/// The document both halves are wrapped in.
///
/// Shared for now. Stage 5 gives the public half its own shell — a masthead, a
/// theme control and a language switcher — at which point this stays as the
/// private one.
pub(crate) fn shell(title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>{}</title><style>{STYLE}</style></head><body>{body}</body></html>",
        esc(title)
    )
}

/// The intake kinds, as a picker.
///
/// Driven by `IntakeKind::ALL`, so a kind added there appears on both surfaces
/// without anyone remembering to update a second list.
pub(crate) fn kind_field() -> String {
    let mut opts = String::new();
    for k in sc_proto::IntakeKind::ALL {
        opts.push_str(&format!(
            "<option value=\"{slug}\">{slug}</option>",
            slug = esc(k.slug())
        ));
    }
    format!(
        "<label for=\"kind\">Kind</label>\
         <select id=\"kind\" name=\"kind\">{opts}</select>"
    )
}

/// A timestamp, as something a human reads.
///
/// Deliberately coarse. The reader's question is "is this fresh, or did it sit
/// overnight?", which a relative age answers and a wall-clock time does not —
/// the server has no idea what timezone the browser is in.
pub(crate) fn ago(then_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(then_ms) / 1000;
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} hr ago", secs / 3600),
        _ => format!("{} days ago", secs / 86_400),
    }
}

#[cfg(test)]
pub(crate) mod corpus {
    //! Every page this crate can render, for the whole-surface checks.
    //!
    //! **Two lists, because the two halves differ**: public pages may carry a
    //! script and private ones may not. A page in neither list is a page nothing
    //! checks — which is exactly how a self-containment test quietly stops
    //! covering the newest thing.

    use super::*;
    use crate::account::Accounts;
    use crate::store::{Request, RequestState};
    use sc_proto::IntakeKind;

    fn req() -> Request {
        Request::new("r-1", "a thing", "alpha", IntakeKind::Feature)
    }

    fn reviewable() -> Request {
        let mut r = req();
        r.state = RequestState::AwaitingReview;
        r.spec = Some("# Spec".to_string());
        r
    }

    pub fn private() -> Vec<(&'static str, String)> {
        vec![
            ("index", index(&[req()])),
            ("detail", detail(&reviewable())),
            ("confirm_approve", confirm_approve(&req(), "# Spec", "d")),
            ("accounts_page", accounts_page(&Accounts::default())),
            ("enrol_page", enrol_page()),
            ("enrol_page_with_error", enrol_page_with_error()),
            ("enrolled_page", enrolled_page()),
            ("filed", filed(&req())),
            ("message", message("nope")),
            ("not_found", not_found()),
        ]
    }

    /// The first element is the **renderer's exact name**, so the coverage test
    /// can compare against `pub fn` in the source. A renderer may appear more
    /// than once where an argument changes the page materially.
    pub fn public() -> Vec<(&'static str, String)> {
        vec![
            ("signin_page", signin_page()),
            ("signin_sent_page", signin_sent_page()),
            ("signin_confirm_page", signin_confirm_page("abc123")),
            ("signin_failed_page", signin_failed_page(true)),
            ("signin_failed_page", signin_failed_page(false)),
            ("public_file_page", public_file_page(&[req()], true)),
            ("public_detail", public_detail(&reviewable(), true)),
            ("public_filed", public_filed(&req())),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_covers_every_character_that_matters() {
        assert_eq!(esc("<&>\"'"), "&lt;&amp;&gt;&quot;&#39;");
        // Ampersand first, or the escapes escape each other.
        assert_eq!(esc("&lt;"), "&amp;lt;");
    }

    #[test]
    fn ages_read_the_way_a_human_would_say_them() {
        let now = 10_000_000_000u64;
        assert_eq!(ago(now, now), "just now");
        assert_eq!(ago(now - 59_000, now), "just now");
        assert_eq!(ago(now - 60_000, now), "1 min ago");
        assert_eq!(ago(now - 3_600_000, now), "1 hr ago");
        assert_eq!(ago(now - 172_800_000, now), "2 days ago");
        // A clock that went backwards must not underflow into "584 million years".
        assert_eq!(ago(now + 5_000, now), "just now");
    }

    #[test]
    fn a_private_page_references_nothing_remote_and_carries_no_script() {
        // The private surface renders specs from *every* filer, so the argument
        // for permitting script on the public half — that a filer only sees
        // their own — does not reach here.
        for (name, html) in corpus::private() {
            for hazard in ["http://", "https://", "<script", "<img", "<link"] {
                assert!(!html.contains(hazard), "{name} contains {hazard}");
            }
        }
    }

    #[test]
    fn a_public_page_references_no_remote_origin() {
        // Script is permitted here; a *remote* subresource is not. One
        // hallucinated remote image in a rendered spec still leaks the page URL
        // through `Referer`, and that argument is unchanged by allowing script.
        for (name, html) in corpus::public() {
            for hazard in [
                "http://", "https://", "<img", "<link", "@import", "url(http",
            ] {
                assert!(!html.contains(hazard), "{name} contains {hazard}");
            }
        }
    }

    #[test]
    fn the_corpus_covers_every_renderer() {
        // A hand-written list under-covers by omission. Reading `pub fn` out of
        // the source is grep-in-a-test and slightly gross, but it is
        // deterministic and it catches the actual failure: somebody adds a page
        // and forgets the list, so the checks above silently stop covering it.
        //
        // Compared by **name**, not by count — some renderers appear twice in
        // the corpus because their argument changes the page, and a count would
        // read that as a missing entry.
        let declared: Vec<&str> = include_str!("public.rs")
            .split("\npub fn ")
            .skip(1)
            .filter_map(|s| s.split('(').next())
            .collect();
        let covered = corpus::public();

        for name in &declared {
            assert!(
                covered.iter().any(|(id, _)| id == name),
                "{name} is missing from the public corpus"
            );
        }
        // And nothing in the corpus names a renderer that no longer exists.
        for (id, _) in &covered {
            assert!(declared.contains(id), "{id} is not a renderer in public.rs");
        }
    }
}
