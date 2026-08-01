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

use sc_proto::IntakeKind;

use crate::account::Accounts;
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
.decide { margin-top: 1.5rem; padding-top: 1rem;
          border-top: 1px solid rgba(128,128,128,.35); }
.skip { display: block; font-size: .85rem; opacity: .7; margin: .5rem 0; }
.elided { text-align: center; opacity: .6; font-size: .85rem;
          padding: .4rem; font-style: italic; }
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
             <h2>Filed</h2>{items}\
             <p class=\"meta\"><a href=\"/accounts\">Who can file</a></p>",
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
    // Half of spec 20's "provenance": *when*. The other half — which agent
    // profile produced it — has nothing to read from yet, and is recorded as
    // unbuilt rather than approximated with a backend name that reads
    // "openai-compat" for every model alike.
    let now = crate::store::now_ms();
    let mut when = format!("filed {}", ago(r.filed_ms, now));
    if let Some(drafted) = r.drafted_ms {
        when.push_str(&format!(" · drafted {}", ago(drafted, now)));
    }

    let mut body = format!(
        "<h1>{summary}</h1>\
         <p class=\"meta\"><span class=\"tag\">{state}</span> {repo} · {kind}<br>\
         {when}</p>\
         <h2>The request</h2><pre>{text}</pre>",
        summary = esc(r.summary()),
        state = esc(r.state.label()),
        repo = esc(&r.repo),
        kind = esc(r.kind.slug()),
        when = esc(&when),
        text = esc(&r.text),
    );

    if let Some(note) = &r.note {
        body.push_str(&format!("<p class=\"note\">{}</p>", esc(note)));
    }

    match (&r.spec, r.state) {
        (Some(spec), RequestState::AwaitingReview) => {
            // The skip link is deliberately visible. Hiding the bypass does not
            // remove it — flicking to the bottom is the bypass and is always
            // available — it only lets the system believe nobody used one. Naming
            // the fast path makes taking it a small act of self-awareness.
            body.push_str(&format!(
                "<h2>The drafted spec</h2>\
                 <a class=\"skip\" href=\"#decide\">Skip to the decision ↓</a>\
                 <pre>{}</pre>{}",
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
        (None, RequestState::Screening) => {
            body.push_str("<p class=\"meta\">Being screened before it is queued.</p>");
        }
        (None, RequestState::Quarantined) => {
            body.push_str(&format!(
                "<p class=\"note\">Held before reaching the queue. Nothing has run \
                 on your machine. Read it and decide.</p>{}",
                release_action(&r.id)
            ));
        }
        // Explicit rather than a `_` arm: a state added later must be a compile
        // error here, not a page that renders its header and then silently
        // stops. That is exactly how the two states above were nearly missed.
        (None, RequestState::AwaitingReview)
        | (None, RequestState::Ready)
        | (None, RequestState::Discarded)
        | (None, RequestState::Failed) => {}
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

/// How many lines of a long spec the confirmation restates at each end.
const EXTRACT_LINES: usize = 40;

/// Split a spec into its opening and closing lines, and how many were elided.
///
/// The confirmation restates *both* ends rather than a summary: a summary of an
/// artifact is a second artifact nobody verified, and approving it means
/// approving something the developer did not read (spec 20).
///
/// The tail matters most. It is the part a flick-to-the-bottom reviewer nominally
/// "reached", so reprinting it puts the end of the document in front of them a
/// second time, in a different context.
fn head_and_tail(spec: &str, n: usize) -> (String, String, usize) {
    let lines: Vec<&str> = spec.lines().collect();
    if lines.len() <= n * 2 {
        return (spec.to_string(), String::new(), 0);
    }
    (
        lines[..n].join("\n"),
        lines[lines.len() - n..].join("\n"),
        lines.len() - n * 2,
    )
}

/// The confirmation page: restate what is being approved, and bind to its bytes.
///
/// The `digest` is carried in a hidden field and re-checked on submit by
/// [`Store::approve`](crate::store::Store::approve). That is what turns a second
/// tap from ceremony into a real guarantee: the approval attaches to the exact
/// text shown here, so a redraft landing mid-review is refused rather than
/// silently approved on the strength of reading the previous one.
pub fn confirm_approve(r: &Request, spec: &str, digest: &str) -> String {
    let (head, tail, elided) = head_and_tail(spec, EXTRACT_LINES);
    let mut extract = format!("<pre>{}</pre>", esc(&head));
    if elided > 0 {
        extract.push_str(&format!(
            "<p class=\"elided\">… {elided} lines not shown — they are in full on \
             the page you came from …</p><pre>{}</pre>",
            esc(&tail)
        ));
    }

    shell(
        "Confirm approval",
        &format!(
            "<h1>Approve this spec?</h1>\
             <p>Approving settles <strong>{summary}</strong> for \
             <strong>{repo}</strong>. The spec stays in the repository as the \
             record of what was agreed. <strong>Nothing is built</strong> — you \
             pick it up in your IDE when you choose to.</p>\
             <h2>What you are approving</h2>{extract}\
             <div class=\"decide\">\
             <form method=\"post\" action=\"/request/{id}/approve/confirm\">\
             <input type=\"hidden\" name=\"digest\" value=\"{digest}\">\
             <button type=\"submit\">Yes — approve this spec</button></form>\
             <form method=\"get\" action=\"/request/{id}\">\
             <button type=\"submit\">No — take me back to read it</button></form>\
             </div>\
             <p class=\"meta\">Closing this page decides nothing.</p>",
            summary = esc(r.summary()),
            repo = esc(&r.repo),
            id = esc(&r.id),
            digest = esc(digest),
        ),
    )
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
        "<div class=\"decide\" id=\"decide\"><h2>Your call</h2>\
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
         here.</p></div>"
    )
}

/// Release a quarantined request into the queue.
///
/// The developer overruling the screener, which is the reason quarantine is not
/// deletion. Deliberately a plain button with no "looks fine to me" framing: the
/// screener held it for a reason, and the point is that a human reads the text
/// and decides rather than clearing a nag.
fn release_action(id: &str) -> String {
    let id = esc(id);
    format!(
        "<div class=\"decide\"><form method=\"post\" action=\"/request/{id}/release\">\
         <button type=\"submit\">Release it — this is not spam</button></form>\
         <form method=\"post\" action=\"/request/{id}/discard\">\
         <button type=\"submit\">Discard</button></form>\
         <p class=\"meta\">Leaving it here decides nothing.</p></div>"
    )
}

/// A timestamp, as something a human reads.
///
/// Deliberately coarse. The reviewer's question is "is this fresh, or did it sit
/// overnight?", which a relative age answers and a wall-clock time does not —
/// the server has no idea what timezone the phone is in.
fn ago(then_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(then_ms) / 1000;
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} hr ago", secs / 3600),
        _ => format!("{} days ago", secs / 86_400),
    }
}

// ---------------------------------------------------------------------------
// The public surface
//
// Rendered by functions of their own rather than by reusing the private pages
// with fields hidden. Hoping every future edit remembers which fields are
// public is the mistake `security_headers` was factored out to avoid — and the
// fields that must never appear here are exactly the ones a careless edit would
// add: `artifact_dir` is a path on the developer's machine, `note` carries
// daemon failure text that names repositories, and `id` is enumerable.
// ---------------------------------------------------------------------------

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

/// Who can file, and the switch that stops them.
///
/// Revoked accounts are **listed, not hidden** — a list that silently shrinks
/// cannot answer "did I already deal with that?", so the developer revokes twice
/// or worries they never did.
pub fn accounts_page(accounts: &Accounts) -> String {
    let mut rows = String::new();
    for a in &accounts.accounts {
        let action = if a.revoked {
            "<span class=\"meta\">revoked</span>".to_string()
        } else {
            format!(
                "<form method=\"post\" action=\"/accounts/{}/revoke\">\
                 <button type=\"submit\">Revoke</button></form>",
                esc(&a.id)
            )
        };
        rows.push_str(&format!(
            "<div class=\"item\"><strong>{hint}</strong>\
             <div class=\"meta\">{id} · joined {when}</div>{action}</div>",
            // The hint, never the address: this page is the reason it exists.
            hint = esc(&a.email_hint),
            id = esc(&a.id),
            when = esc(&ago(a.created_ms, crate::store::now_ms())),
            action = action,
        ));
    }
    if accounts.accounts.is_empty() {
        rows.push_str("<p class=\"meta\">Nobody has signed up.</p>");
    }

    shell(
        "Accounts",
        &format!(
            "<h1>Accounts</h1>\
             <p class=\"meta\">Anyone with a working email address can sign up and \
             file requests. Revoking one stops it filing immediately and ends every \
             session it has open.</p>{rows}\
             <p><a href=\"/\">Back to the queue</a></p>"
        ),
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
        //
        // Every page belongs in this list. The public ones especially: they are
        // the surface strangers see, and an omission here means the check
        // silently stops covering the newest thing.
        let pages = [
            index(&[req("a thing")]),
            detail(&req("a thing")),
            enrol_page(),
            enrolled_page(),
            not_found(),
            message("nope"),
            signin_page(),
            signin_sent_page(),
            signin_confirm_page("abc123"),
            signin_failed_page(true),
            signin_failed_page(false),
            public_file_page(&[req("a thing")], true),
            public_detail(&req("a thing"), true),
            public_filed(&req("a thing")),
            accounts_page(&Accounts::default()),
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
    fn the_decision_comes_after_the_whole_artifact() {
        // The one property document order actually gives: on a phone the
        // controls are physically below the spec. Anchored on the *close* of the
        // spec block — "after the opening tag" would pass with the buttons
        // sitting in the middle of the document.
        let mut r = req("a thing");
        r.state = RequestState::AwaitingReview;
        r.spec = Some("# Spec\nline\nline".to_string());
        let html = detail(&r);

        let spec_ends = html.rfind("</pre>").unwrap();
        assert!(spec_ends < html.find("/send-back").unwrap(), "{html}");
        assert!(spec_ends < html.find("/approve").unwrap(), "{html}");
    }

    #[test]
    fn the_detail_pages_approve_button_asks_rather_than_decides() {
        // The mechanism is that /approve renders a confirmation. If the detail
        // page ever posted straight to the committing route, the second step is
        // gone and nothing in the routing tests would notice.
        let mut r = req("a thing");
        r.state = RequestState::AwaitingReview;
        r.spec = Some("# Spec".to_string());
        let html = detail(&r);

        assert!(html.contains("action=\"/request/r-1/approve\""), "{html}");
        assert!(!html.contains("/approve/confirm"), "{html}");
    }

    #[test]
    fn the_confirmation_restates_the_artifact_rather_than_only_asking_again() {
        // A confirm page that says just "are you sure?" is ceremony: it adds a
        // tap and no evidence. It has to put the text back in front of the
        // reviewer.
        let r = req("a thing");
        let spec = "# Spec\nthe first line\nthe last line";
        let html = confirm_approve(&r, spec, "deadbeef");

        assert!(html.contains("the first line"), "{html}");
        assert!(html.contains("the last line"), "{html}");
        assert!(html.contains("a thing"), "names what is approved: {html}");
        assert!(html.contains("alpha"), "names the repository: {html}");
        assert!(html.contains("Nothing is built"), "{html}");
    }

    #[test]
    fn the_confirmation_binds_the_approval_to_the_text_it_showed() {
        // Without the digest the reviewer consents to "whatever is on disk when
        // the POST lands", which a redraft arriving mid-review silently changes.
        let r = req("a thing");
        let html = confirm_approve(&r, "# Spec", "deadbeef");
        assert!(
            html.contains("name=\"digest\" value=\"deadbeef\""),
            "{html}"
        );
        assert!(html.contains("/request/r-1/approve/confirm"), "{html}");
    }

    #[test]
    fn the_confirmation_offers_a_way_out_that_weighs_the_same() {
        // If "yes" is a button and "no" is a text link, the confirm page is a
        // funnel rather than a decision.
        let r = req("a thing");
        let html = confirm_approve(&r, "# Spec", "deadbeef");

        let buttons: Vec<&str> = html
            .match_indices("<button")
            .map(|(i, _)| &html[i..])
            .collect();
        assert_eq!(buttons.len(), 2, "confirm and go back: {html}");
        for b in buttons {
            let tag = &b[..b.find('>').unwrap()];
            assert_eq!(tag, "<button type=\"submit\"", "styled differently: {tag}");
        }
        assert!(html.contains("decides nothing"), "{html}");
    }

    #[test]
    fn a_long_spec_is_restated_at_both_ends_with_the_gap_named() {
        // The tail matters most: it is what a flick-to-the-bottom reviewer
        // nominally reached, so reprinting it is the point. And the elision is
        // stated rather than silent — a truncation nobody mentions reads as the
        // whole document.
        let spec: String = (1..=200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let r = req("a thing");
        let html = confirm_approve(&r, &spec, "d");

        assert!(html.contains("line 1\n"), "the head: {html}");
        assert!(html.contains("line 200"), "the tail: {html}");
        assert!(!html.contains("line 100"), "the middle IS elided");
        assert!(html.contains("120 lines not shown"), "{html}");
        // And the elision points at where the whole text still is, so the
        // reviewer is not left thinking this extract was all there was.
        assert!(html.contains("page you came from"), "{html}");
    }

    #[test]
    fn a_short_spec_is_shown_whole_with_no_elision_marker() {
        let r = req("a thing");
        let html = confirm_approve(&r, "# Spec\nshort", "d");
        assert!(html.contains("short"), "{html}");
        assert!(!html.contains("not shown"), "nothing was elided: {html}");
    }

    #[test]
    fn head_and_tail_never_drops_or_duplicates_a_line() {
        // An off-by-one here would either hide a line the reviewer needs or show
        // one twice, and the elided count would lie about which.
        for len in [0usize, 1, 5, 79, 80, 81, 200] {
            let spec: String = (0..len)
                .map(|i| format!("l{i}"))
                .collect::<Vec<_>>()
                .join("\n");
            let (head, tail, elided) = head_and_tail(&spec, EXTRACT_LINES);
            let shown = head.lines().count() + tail.lines().count();
            assert_eq!(shown + elided, len, "len {len}");
        }
    }

    #[test]
    fn nothing_in_the_review_path_can_fail_closed() {
        // Why a CSS scroll-reveal was rejected: a control hidden until some
        // technique fires is a control that is permanently unreachable where the
        // technique is unsupported — `animation-timeline` is Chromium-only, so on
        // every iOS Safari the gate would be bricked and the run parked forever.
        // Unreachable is strictly worse than reachable-without-ceremony.
        let mut r = req("a thing");
        r.state = RequestState::AwaitingReview;
        r.spec = Some("# Spec".to_string());

        for html in [detail(&r), confirm_approve(&r, "# Spec", "d")] {
            for hazard in [
                "animation-timeline",
                "scroll-timeline",
                "pointer-events: none",
                "opacity: 0",
                "opacity:0",
                "display: none",
                "visibility: hidden",
                ":target",
            ] {
                assert!(
                    !html.contains(hazard),
                    "{hazard}: a control depending on this is unreachable where \
                     it is unsupported"
                );
            }
        }
    }

    #[test]
    fn the_page_says_when_it_was_filed_and_drafted() {
        // Half of spec 20's provenance. The reviewer's question is "is this
        // fresh, or did it sit overnight?" — which a relative age answers and a
        // wall-clock time does not, since the server has no idea what timezone
        // the phone is in.
        let mut r = req("a thing");
        r.state = RequestState::AwaitingReview;
        r.spec = Some("# Spec".to_string());
        r.filed_ms = crate::store::now_ms() - 7_200_000;
        r.drafted_ms = Some(crate::store::now_ms() - 120_000);

        let html = detail(&r);
        assert!(html.contains("filed 2 hr ago"), "{html}");
        assert!(html.contains("drafted 2 min ago"), "{html}");
    }

    #[test]
    fn an_undrafted_request_shows_no_drafted_time() {
        let r = req("a thing");
        let html = detail(&r);
        assert!(html.contains("filed "), "{html}");
        assert!(!html.contains("drafted "), "there is no draft yet: {html}");
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
    fn the_skip_link_is_visible_rather_than_hidden() {
        // Hiding the bypass does not remove it — flicking to the bottom is the
        // bypass — it only lets the system believe nobody used one.
        let mut r = req("a thing");
        r.state = RequestState::AwaitingReview;
        r.spec = Some("# Spec".to_string());
        let html = detail(&r);
        assert!(html.contains("href=\"#decide\""), "{html}");
        assert!(html.contains("id=\"decide\""), "the anchor exists: {html}");
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
    fn the_accounts_page_shows_a_hint_never_an_address() {
        // This page is the reason `email_hint` exists: enough to recognise the
        // account you meant to revoke, not enough to be a contact list.
        let mut accounts = Accounts::default();
        let a = accounts.create(
            &crate::auth::hash("jonathan.smith@example.com"),
            "jo***@example.com",
            crate::store::now_ms(),
        );

        let html = accounts_page(&accounts);
        assert!(html.contains("jo***@example.com"), "{html}");
        assert!(!html.contains("jonathan.smith"), "{html}");
        assert!(
            html.contains(&format!("/accounts/{}/revoke", a.id)),
            "{html}"
        );
    }

    #[test]
    fn a_revoked_account_is_still_listed_and_offers_no_second_revoke() {
        // A list that silently shrinks cannot answer "did I already deal with
        // that?", so the developer revokes twice or worries they never did.
        let mut accounts = Accounts::default();
        let a = accounts.create(&crate::auth::hash("jo@x.com"), "jo***@x.com", 1);
        accounts.revoke(&a.id);

        let html = accounts_page(&accounts);
        assert!(html.contains("jo***@x.com"), "still listed: {html}");
        assert!(html.contains("revoked"), "{html}");
        assert!(!html.contains("/revoke\">"), "no button for it: {html}");
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

    #[test]
    fn a_quarantined_request_offers_release_to_the_reviewer() {
        // The developer overruling the screener — the reason quarantine is not
        // deletion.
        let mut r = req("a thing");
        r.state = RequestState::Quarantined;
        let html = detail(&r);
        assert!(html.contains("/request/r-1/release"), "{html}");
        assert!(html.contains("Nothing has run on your machine"), "{html}");
    }

    #[test]
    fn a_screening_request_says_what_it_is_waiting_for() {
        // The state that used to render nothing at all, because `detail` had a
        // catch-all arm.
        let mut r = req("a thing");
        r.state = RequestState::Screening;
        let html = detail(&r);
        assert!(html.contains("screened"), "{html}");
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
