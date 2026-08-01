//! The public surface: what a signed-in filer sees.
//!
//! Rendered by functions of their own rather than by reusing the private pages
//! with fields hidden. Hoping every future edit remembers which fields are
//! public is the mistake `security_headers` was factored out to avoid — and the
//! fields that must never appear here are exactly the ones a careless edit would
//! add: `artifact_dir` is a path on the developer's machine, `note` carries
//! daemon failure text that names repositories, and `id` is enumerable.
//!
//! ## Script is permitted here, and not on the private half
//!
//! A filer only ever sees specs from their **own** requests, so a filer who
//! crafts a request to make the model emit something hostile is attacking
//! themselves — they already control the input and can already read the output.
//! The private review pages render specs from *every* filer, where that
//! argument does not reach, and they stay scriptless.
//!
//! The residual risk here is privacy rather than attack: a model can emit a
//! hallucinated remote reference nobody asked for. `default-src 'none'` still
//! blocks the fetch, so permitting script does not reopen it.

use sc_proto::IntakeKind;

use super::{ago, esc, kind_field, shell};
use crate::store::{Request, RequestState};

/// Ask for a sign-in link.
pub fn signin_page() -> String {
    shell(
        "Sign in",
        "<h1>Sign in</h1>\
         <p>Filing a request needs an email address — it is how you find your \
         way back to what you filed, and it keeps this form from being a \
         free-for-all.</p>\
         <form method=\"post\" action=\"/public/signin\">\
         <label for=\"email\">Email</label>\
         <input id=\"email\" name=\"email\" type=\"email\" required \
         autocapitalize=\"off\" autocorrect=\"off\" spellcheck=\"false\" \
         placeholder=\"you@example.com\">\
         <button type=\"submit\">Email me a link</button></form>\
         <p class=\"meta\">No password. We send a link that works once, for \
         fifteen minutes.</p>",
    )
}

/// Shown after asking for a link — **identical whatever actually happened**.
///
/// New address, existing account, revoked account, malformed input, over the
/// outstanding cap: all land here. Only what gets *sent* differs, so the page
/// cannot be used to discover whether an address has an account.
pub fn signin_sent_page() -> String {
    shell(
        "Check your email",
        "<h1>Check your email</h1>\
         <p>If that address can receive mail, a sign-in link is on its way. It \
         expires in fifteen minutes.</p>\
         <p class=\"meta\">Nothing else has happened yet — the link is what \
         signs you in.</p>",
    )
}

/// The landing page a sign-in link opens. **Changes nothing.**
///
/// A GET here must be inert: mail scanners (Outlook Safe Links and friends)
/// fetch every URL in a message, often within seconds, so a GET that spent the
/// token would burn it before the human opened their inbox.
///
/// It renders the same form whether the token is valid, expired or fabricated.
/// A 404 on an invalid one would be a free validity oracle — an attacker could
/// test candidate tokens with a GET, which costs less budget than the POST.
pub fn signin_confirm_page(token: &str) -> String {
    shell(
        "Confirm sign-in",
        &format!(
            "<h1>Confirm sign-in</h1>\
             <p>Press the button to finish signing in on this device.</p>\
             <form method=\"post\" action=\"/public/signin/{}\">\
             <button type=\"submit\">Sign me in</button></form>\
             <p class=\"meta\">If you did not ask for this, close the page — \
             nothing happens until you press it.</p>",
            esc(token)
        ),
    )
}

/// A link that could not be spent.
pub fn signin_failed_page(already_used: bool) -> String {
    // "Invalid link" to somebody whose sign-in just worked reads as a bug, so a
    // second click is told apart from a forgery. That leaks only that a token
    // once existed — and it was theirs.
    let body = if already_used {
        "<p class=\"note\">That link has already been used. You are probably \
         signed in already — <a href=\"/public\">try filing something</a>.</p>"
    } else {
        "<p class=\"note\">That link is not valid any more. They expire after \
         fifteen minutes.</p>"
    };
    shell(
        "That link did not work",
        &format!(
            "<h1>That link did not work</h1>{body}\
             <p><a href=\"/public/signin\">Ask for a new one</a></p>"
        ),
    )
}

/// The public filing form, plus what this filer has already sent.
///
/// **No repository field.** Public filings go to the repository the operator
/// configured, so a stranger cannot aim work at one that was never nominated for
/// public intake. Absent from the form *and* ignored in the body.
pub fn public_file_page(mine: &[Request], show_spec: bool) -> String {
    let mut items = String::new();
    for r in crate::routes::listing_order(mine.to_vec()) {
        items.push_str(&format!(
            "<a class=\"item\" href=\"/public/request/{id}\">{summary}\
             <div class=\"meta\"><span class=\"tag\">{state}</span> {kind}</div></a>",
            id = esc(&r.id),
            summary = esc(r.summary()),
            state = esc(public_state_label(r.state)),
            kind = esc(r.kind.slug()),
        ));
    }
    if mine.is_empty() {
        items.push_str("<p class=\"meta\">You have not filed anything yet.</p>");
    }

    shell(
        "File a request",
        &format!(
            "<h1>File a request</h1>\
             <form method=\"post\" action=\"/public\">\
             <label for=\"text\">What needs doing?</label>\
             <textarea id=\"text\" name=\"text\" required maxlength=\"{bytes}\" \
             placeholder=\"Describe it the way you would to a colleague.\"></textarea>\
             {kind}\
             <button type=\"submit\">File it</button></form>\
             <p class=\"meta\">Up to {words} words. Short is better — a spec is \
             drafted from what you write, not copied from it.{spec_note}</p>\
             <h2>What you have filed</h2>{items}\
             <p class=\"meta\"><a href=\"/public/signout\">Sign out</a></p>",
            bytes = crate::routes::MAX_BYTES,
            words = crate::routes::MAX_WORDS,
            kind = kind_field(),
            spec_note = if show_spec {
                " You will be able to read the spec that comes back."
            } else {
                ""
            },
            items = items,
        ),
    )
}

/// What a filer is told about a state.
///
/// Deliberately coarser than [`RequestState::label`]. A filer does not need to
/// know that their request is being screened for spam — saying so invites
/// gaming, and "queued" is true in the sense they care about. `Quarantined`
/// likewise reads as waiting rather than as an accusation, since a human may yet
/// release it.
fn public_state_label(state: RequestState) -> &'static str {
    match state {
        RequestState::Screening | RequestState::Quarantined | RequestState::Queued => "received",
        RequestState::Claimed => "being written up",
        RequestState::AwaitingReview => "with a reviewer",
        RequestState::Ready => "accepted",
        RequestState::Discarded | RequestState::Failed => "closed",
    }
}

/// One of a filer's own requests.
///
/// Renders **only** what is theirs to see: their own text, a coarse state, and —
/// when the operator allows it — the drafted spec. Never `artifact_dir` (a path
/// on the developer's machine), never `note` (daemon failure text naming
/// repositories), never the repository name.
pub fn public_detail(r: &Request, show_spec: bool) -> String {
    let mut body = format!(
        "<h1>{summary}</h1>\
         <p class=\"meta\"><span class=\"tag\">{state}</span> {kind} · filed {when}</p>\
         <h2>What you asked for</h2><pre>{text}</pre>",
        summary = esc(r.summary()),
        state = esc(public_state_label(r.state)),
        kind = esc(r.kind.slug()),
        when = esc(&ago(r.filed_ms, crate::store::now_ms())),
        text = esc(&r.text),
    );

    match (&r.spec, show_spec) {
        (Some(spec), true) => body.push_str(&format!(
            "<h2>The spec that came back</h2><pre>{}</pre>",
            esc(spec)
        )),
        (Some(_), false) => {
            body.push_str("<p class=\"meta\">A spec has been written and is with a reviewer.</p>")
        }
        (None, _) => {}
    }

    body.push_str("<p><a href=\"/public\">Back</a></p>");
    shell(r.summary(), &body)
}

/// Confirmation that a public request was filed.
pub fn public_filed(r: &Request) -> String {
    let body = if r.kind == IntakeKind::Feedback {
        "<p>Thanks — that is recorded. Feedback is kept for the developer to \
         read; it does not become a spec.</p>"
            .to_string()
    } else {
        "<p>Filed. Come back to this page to see what happens to it.</p>".to_string()
    };
    shell(
        "Filed",
        &format!("<h1>Filed</h1>{body}<p><a href=\"/public\">Back</a></p>"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(text: &str) -> Request {
        Request::new("r-1", text, "alpha", IntakeKind::Feature)
    }

    #[test]
    fn the_public_form_offers_no_repository_field() {
        // A stranger must not be able to aim work at a repository the operator
        // did not nominate. Absent from the form, and ignored in the body.
        let html = public_file_page(&[], true);
        for path_ish in ["name=\"repo\"", "name=\"path\"", "name=\"workspace\""] {
            assert!(!html.contains(path_ish), "{path_ish}: {html}");
        }
        assert!(html.contains("name=\"text\""), "{html}");
        assert!(html.contains("name=\"kind\""), "{html}");
    }

    #[test]
    fn a_filers_page_shows_nothing_about_the_developers_machine() {
        // `artifact_dir` is a path on their machine; `note` carries daemon
        // failure text that names repositories. Neither is the filer's business,
        // and a shared renderer would eventually leak one.
        let mut r = req("a thing");
        r.state = RequestState::AwaitingReview;
        r.spec = Some("# The spec".to_string());
        r.artifact_dir = Some("specs/a-thing".to_string());
        r.note = Some("could not draft: /home/dev/secret-repo is mid-rebase".to_string());

        let html = public_detail(&r, true);
        assert!(html.contains("# The spec"), "the spec is shown");
        assert!(!html.contains("specs/a-thing"), "{html}");
        assert!(!html.contains("secret-repo"), "{html}");
        assert!(!html.contains("alpha"), "not even the repo name: {html}");
    }

    #[test]
    fn a_filer_sees_no_spec_when_the_operator_turns_it_off() {
        let mut r = req("a thing");
        r.state = RequestState::AwaitingReview;
        r.spec = Some("# Private details".to_string());

        let html = public_detail(&r, false);
        assert!(!html.contains("Private details"), "{html}");
        assert!(html.contains("with a reviewer"), "but they know it moved");
    }

    #[test]
    fn a_filer_is_not_told_their_request_was_screened_for_spam() {
        // Saying so invites gaming, and "received" is true in the sense the
        // filer cares about — a human may yet release it.
        for state in [RequestState::Screening, RequestState::Quarantined] {
            let mut r = req("a thing");
            r.state = state;
            let html = public_detail(&r, true);
            assert!(!html.to_lowercase().contains("spam"), "{state:?}: {html}");
            assert!(!html.contains("quarantin"), "{state:?}: {html}");
            assert!(html.contains("received"), "{state:?}: {html}");
        }
    }
}
