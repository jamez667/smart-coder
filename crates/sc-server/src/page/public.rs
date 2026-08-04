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
//!
//! What actually grants the permission is
//! [`Policy::PublicScript`](crate::routes::Policy::PublicScript), applied to
//! everything the public routes return. It is **not** a property of the
//! renderers in this module: a page defined here and served from a private route
//! would be served strict, which is the safe direction for that mistake to fall.
//! The permission is `'self'`, so an inline `<script>` reaching a page through
//! model output still does not run.
//!
//! ## Every page here takes a [`Locale`]
//!
//! Not a default parameter and not a thread-local: the language is threaded
//! through as an argument, so a page cannot be rendered without one having been
//! decided. Text comes from `locale.strings()` and is escaped on the way in like
//! any other text — a catalogue is data, and treating it as trusted markup is
//! how a translation becomes an injection.

use sc_proto::IntakeKind;

use super::{esc, kind_field_in, public_shell, public_shell_as, relative_time, Signed};
use crate::i18n::Locale;
use crate::store::{Request, RequestState};

/// The landing page: what this is, for somebody who has never seen it.
///
/// **The only page that explains anything.** Every other page assumes the reader
/// knows why they are here — they followed a sign-in link, or they came back to
/// check on something they filed. This one is what a bare address shows a
/// stranger, so it leads with the premise rather than with a form.
///
/// Signed-in filers never see it: the route sends them to their own page
/// instead, because they have read the pitch and came back for their requests.
pub fn landing_page(locale: Locale) -> String {
    let s = locale.strings();
    let point = |title: &str, body: &str| {
        format!(
            "<section class=\"point\"><h2>{}</h2><p>{}</p></section>",
            esc(title),
            esc(body)
        )
    };
    public_shell(
        locale,
        s.landing_headline,
        &format!(
            // **No call-to-action button.** The masthead's Sign in is the way
            // in, and a second button competing with it — going to a page that
            // immediately asks for the same sign-in — was a step that added
            // nothing. The reference has no CTA here either.
            "<h1>{headline}</h1><p>{sub}</p>{p1}{p2}{p3}",
            headline = esc(s.landing_headline),
            sub = esc(s.landing_sub),
            p1 = point(s.landing_point_1_title, s.landing_point_1_body),
            p2 = point(s.landing_point_2_title, s.landing_point_2_body),
            p3 = point(s.landing_point_3_title, s.landing_point_3_body),
        ),
    )
}

/// Ask for a sign-in link.
pub fn signin_page() -> String {
    signin_page_in(Locale::default())
}

/// Ask for a sign-in link, in a chosen language.
///
/// Split from [`signin_page`] because the language route renders this one *after*
/// the choice and before the cookie has been read back — the only caller that
/// knows the locale from something other than the request.
pub fn signin_page_in(locale: Locale) -> String {
    let s = locale.strings();
    public_shell(
        locale,
        s.signin_title,
        &format!(
            "<h1>{title}</h1><p>{intro}</p>\
             <form method=\"post\" action=\"/public/signin\">\
             <label for=\"email\">{email}</label>\
             <input id=\"email\" name=\"email\" type=\"email\" required \
             autocapitalize=\"off\" autocorrect=\"off\" spellcheck=\"false\" \
             placeholder=\"{placeholder}\">\
             <button type=\"submit\">{submit}</button></form>\
             <p class=\"meta\">{note}</p>",
            title = esc(s.signin_title),
            intro = esc(s.signin_intro),
            email = esc(s.signin_email_label),
            placeholder = esc(s.signin_email_placeholder),
            submit = esc(s.signin_submit),
            note = esc(s.signin_no_password),
        ),
    )
}

/// An owner's list: everything filed against the repositories they own.
///
/// **Every state**, because "why has nothing happened to this" is a question an
/// owner asks, and a page showing only what awaits a decision cannot answer it.
///
/// **No filer identity.** The email hint exists for the developer's revoke list;
/// showing it here would turn a repository allowlist into a contact list, and
/// nothing about declining work needs to know who asked for it.
pub fn owner_page(all: &[Request], login: &str, repos: &[String], locale: Locale) -> String {
    let s = locale.strings();
    let ordered = crate::routes::listing_order(all.to_vec());

    let mut items = String::new();
    for r in &ordered {
        items.push_str(&format!(
            "<a class=\"item\" href=\"/public/request/{id}\">{summary}\
             <div class=\"meta\"><span class=\"tag\">{state}</span> {repo} · {kind}</div></a>",
            id = esc(&r.id),
            summary = esc(r.summary()),
            // The developer's own vocabulary, not the filer's softened one: an
            // owner is deciding, so "awaiting review" is the useful word.
            state = esc(r.state.label()),
            repo = esc(&r.repo),
            kind = esc(r.kind.slug()),
        ));
    }
    if ordered.is_empty() {
        items.push_str(&format!("<p class=\"meta\">{}</p>", esc(s.owner_nothing)));
    }

    public_shell_as(
        locale,
        s.owner_title,
        &format!(
            "<h1>{title}</h1>\
             <p class=\"meta\">{who} · {list}</p>\
             <p class=\"note\">{note}</p>{items}\
             <p class=\"meta\"><a href=\"/public/signout\">{signout}</a></p>",
            title = esc(s.owner_title),
            who = esc(login),
            list = esc(&repos.join(", ")),
            note = esc(s.owner_note),
            items = items,
            signout = esc(s.file_signout),
        ),
        Signed::In,
    )
}

/// One request, as its repository's owner sees it.
///
/// Carries the spec and the two verbs that decline it. **No approve**, and not
/// because it is hidden: the route that would accept one is on the developer's
/// surface, behind a match no owner satisfies. Absent from the page because
/// there is nothing for it to post to.
pub fn owner_detail(r: &Request, locale: Locale) -> String {
    let s = locale.strings();
    let mut body = format!(
        "<h1>{summary}</h1>\
         <p class=\"meta\"><span class=\"tag\">{state}</span> {repo} · {kind}</p>\
         <h2>{asked}</h2><pre>{text}</pre>",
        summary = esc(r.summary()),
        state = esc(r.state.label()),
        repo = esc(&r.repo),
        kind = esc(r.kind.slug()),
        asked = esc(s.detail_asked_heading),
        text = esc(&r.text),
    );

    if let Some(spec) = &r.spec {
        body.push_str(&format!(
            "<h2>{}</h2><pre>{}</pre>",
            esc(s.detail_spec_heading),
            esc(spec)
        ));
    }

    // The verbs, offered only where the state admits them — `send_back` refuses
    // anything not awaiting review, and a button that always errors is worse
    // than no button.
    if r.state == RequestState::AwaitingReview {
        body.push_str(&format!(
            "<div class=\"decide\">\
             <form method=\"post\" action=\"/public/request/{id}/send-back\">\
             <label for=\"note\">{note_label}</label>\
             <textarea id=\"note\" name=\"note\" placeholder=\"{note_hint}\"></textarea>\
             <button type=\"submit\">{send_back}</button></form>\
             <form method=\"post\" action=\"/public/request/{id}/discard\">\
             <button type=\"submit\">{discard}</button></form></div>",
            id = esc(&r.id),
            note_label = esc(s.owner_note_label),
            note_hint = esc(s.owner_note_hint),
            send_back = esc(s.owner_send_back),
            discard = esc(s.owner_discard),
        ));
    }

    body.push_str(&format!(
        "<p class=\"meta\"><a href=\"/public\">{}</a></p>",
        esc(s.back)
    ));
    public_shell_as(locale, s.owner_title, &body, Signed::In)
}

/// The step before GitHub: what is about to happen, and a link.
///
/// **A link, not a redirect.** The reader sees where they are going before they
/// go, which is the honest version of an OAuth hop — and it needs no `Location`
/// header, which this server's response type deliberately does not carry.
///
/// The URL is built by this crate from configuration, never from anything a
/// caller sent, so there is no open redirect here to find.
pub fn github_start_page(authorize_url: &str, locale: Locale) -> String {
    let s = locale.strings();
    public_shell(
        locale,
        s.github_title,
        &format!(
            "<h1>{title}</h1><p>{intro}</p>\
             <p><a class=\"cta\" href=\"{url}\">{go}</a></p>\
             <p class=\"meta\">{note}</p>",
            title = esc(s.github_title),
            intro = esc(s.github_intro),
            url = esc(authorize_url),
            go = esc(s.github_go),
            note = esc(s.github_note),
        ),
    )
}

/// Shown after asking for a link — **identical whatever actually happened**.
///
/// New address, existing account, revoked account, malformed input, over the
/// outstanding cap: all land here. Only what gets *sent* differs, so the page
/// cannot be used to discover whether an address has an account.
pub fn signin_sent_page(locale: Locale) -> String {
    let s = locale.strings();
    public_shell(
        locale,
        s.sent_title,
        &format!(
            "<h1>{title}</h1><p>{body}</p><p class=\"meta\">{note}</p>",
            title = esc(s.sent_title),
            body = esc(s.sent_body),
            note = esc(s.sent_nothing_yet),
        ),
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
pub fn signin_confirm_page(token: &str, locale: Locale) -> String {
    let s = locale.strings();
    public_shell(
        locale,
        s.confirm_title,
        &format!(
            "<h1>{title}</h1><p>{intro}</p>\
             <form method=\"post\" action=\"/public/signin/{token}\">\
             <button type=\"submit\">{submit}</button></form>\
             <p class=\"meta\">{note}</p>",
            title = esc(s.confirm_title),
            intro = esc(s.confirm_intro),
            token = esc(token),
            submit = esc(s.confirm_submit),
            note = esc(s.confirm_not_you),
        ),
    )
}

/// A link that could not be spent.
pub fn signin_failed_page(already_used: bool, locale: Locale) -> String {
    let s = locale.strings();
    // "Invalid link" to somebody whose sign-in just worked reads as a bug, so a
    // second click is told apart from a forgery. That leaks only that a token
    // once existed — and it was theirs.
    let body = if already_used {
        format!(
            "<p class=\"note\">{lead}<a href=\"/public\">{link}</a>.</p>",
            lead = esc(s.link_already_used),
            link = esc(s.link_already_used_link),
        )
    } else {
        format!("<p class=\"note\">{}</p>", esc(s.link_expired))
    };
    public_shell(
        locale,
        s.link_failed_title,
        &format!(
            "<h1>{title}</h1>{body}\
             <p><a href=\"/public/signin\">{again}</a></p>",
            title = esc(s.link_failed_title),
            again = esc(s.link_ask_again),
        ),
    )
}

/// The public filing form, plus what this filer has already sent.
///
/// **A repository is chosen from a closed set, never typed.** The picker is
/// rendered from the operator's own configuration and the submitted name is
/// checked against the same set, so a stranger can choose *among* nominated
/// repositories and cannot name one that was never nominated. A surface serving
/// one repository shows no field at all — there is nothing to decide.
///
/// Still **names, never paths**. The server holds no path to any repository and
/// this field does not change that: a name is resolved by the daemon against its
/// own configured set, one hop later, which is where the closed set is really
/// enforced.
pub fn public_file_page(
    mine: &[Request],
    repos: &crate::config::Repos,
    show_spec: bool,
    locale: Locale,
) -> String {
    let s = locale.strings();
    let mut items = String::new();
    for r in crate::routes::listing_order(mine.to_vec()) {
        items.push_str(&format!(
            "<a class=\"item\" href=\"/public/request/{id}\">{summary}\
             <div class=\"meta\"><span class=\"tag\">{state}</span> {kind}</div></a>",
            id = esc(&r.id),
            summary = esc(r.summary()),
            state = esc(public_state_label(r.state, locale)),
            kind = esc(r.kind.slug()),
        ));
    }
    if mine.is_empty() {
        items.push_str(&format!(
            "<p class=\"meta\">{}</p>",
            esc(s.file_nothing_yet)
        ));
    }

    public_shell_as(
        locale,
        s.file_title,
        &format!(
            "<h1>{title}</h1>\
             <form method=\"post\" action=\"/public\">\
             <label for=\"text\">{prompt}</label>\
             <textarea id=\"text\" name=\"text\" required maxlength=\"{bytes}\" \
             placeholder=\"{placeholder}\"></textarea>\
             {repo}{kind}\
             <button type=\"submit\">{submit}</button></form>\
             <p class=\"meta\">{cap_before}{words}{cap_after}{spec_note}</p>\
             <h2>{mine_heading}</h2>{items}\
             <p class=\"meta\"><a href=\"/public/signout\">{signout}</a></p>",
            title = esc(s.file_title),
            prompt = esc(s.file_prompt),
            bytes = crate::routes::MAX_BYTES,
            placeholder = esc(s.file_placeholder),
            repo = super::repo_field_in(repos, locale),
            kind = kind_field_in(locale),
            submit = esc(s.file_submit),
            cap_before = esc(s.file_cap_before),
            words = crate::routes::MAX_WORDS,
            cap_after = esc(s.file_cap_after),
            spec_note = if show_spec {
                esc(s.file_spec_note)
            } else {
                String::new()
            },
            mine_heading = esc(s.file_mine_heading),
            items = items,
            signout = esc(s.file_signout),
        ),
        Signed::In,
    )
}

/// What a filer is told about a state.
///
/// Deliberately coarser than [`RequestState::label`]. A filer does not need to
/// know that their request is being screened for spam — saying so invites
/// gaming, and "queued" is true in the sense they care about. `Quarantined`
/// likewise reads as waiting rather than as an accusation, since a human may yet
/// release it.
fn public_state_label(state: RequestState, locale: Locale) -> &'static str {
    let s = locale.strings();
    match state {
        RequestState::Screening | RequestState::Quarantined | RequestState::Queued => {
            s.state_received
        }
        RequestState::Claimed => s.state_writing,
        RequestState::AwaitingReview => s.state_reviewing,
        RequestState::Accepted => s.state_accepted,
        RequestState::Discarded | RequestState::Failed => s.state_closed,
    }
}

/// One of a filer's own requests.
///
/// Renders **only** what is theirs to see: their own text, a coarse state, and —
/// when the operator allows it — the drafted spec. Never `artifact_dir` (a path
/// on the developer's machine), never `note` (daemon failure text naming
/// repositories), never the repository name.
pub fn public_detail(r: &Request, show_spec: bool, locale: Locale) -> String {
    let s = locale.strings();
    let mut body = format!(
        "<h1>{summary}</h1>\
         <p class=\"meta\"><span class=\"tag\">{state}</span> {kind} · {filed}{when}</p>\
         <h2>{asked}</h2><pre>{text}</pre>",
        summary = esc(r.summary()),
        state = esc(public_state_label(r.state, locale)),
        kind = esc(r.kind.slug()),
        filed = esc(s.detail_filed_prefix),
        when = esc(&relative_time(r.filed_ms, crate::store::now_ms(), locale)),
        asked = esc(s.detail_asked_heading),
        text = esc(&r.text),
    );

    match (&r.spec, show_spec) {
        (Some(spec), true) => body.push_str(&format!(
            "<h2>{}</h2><pre>{}</pre>",
            esc(s.detail_spec_heading),
            esc(spec)
        )),
        (Some(_), false) => body.push_str(&format!(
            "<p class=\"meta\">{}</p>",
            esc(s.detail_spec_withheld)
        )),
        (None, _) => {}
    }

    body.push_str(&format!("<p><a href=\"/public\">{}</a></p>", esc(s.back)));
    public_shell(locale, r.summary(), &body)
}

/// Confirmation that a public request was filed.
pub fn public_filed(r: &Request, locale: Locale) -> String {
    let s = locale.strings();
    let body = if r.kind == IntakeKind::Feedback {
        esc(s.filed_feedback_body)
    } else {
        esc(s.filed_body)
    };
    public_shell_as(
        locale,
        s.filed_title,
        &format!(
            "<h1>{title}</h1><p>{body}</p><p><a href=\"/public\">{back}</a></p>",
            title = esc(s.filed_title),
            back = esc(s.back),
        ),
        Signed::In,
    )
}

/// The public surface's 404.
///
/// Separate from the private one so it carries the masthead, the theme control
/// and the language switcher — a filer who mistypes a URL should not be dropped
/// onto a page that looks like a different site.
pub fn public_not_found(locale: Locale) -> String {
    let s = locale.strings();
    public_shell(
        locale,
        s.not_found_title,
        &format!(
            "<h1>{title}</h1><p>{body}</p><p><a href=\"/public\">{back}</a></p>",
            title = esc(s.not_found_title),
            body = esc(s.not_found_body),
            back = esc(s.back),
        ),
    )
}

/// Something the filer did that the server would not take.
pub fn public_message(text: &str, locale: Locale) -> String {
    let s = locale.strings();
    public_shell(
        locale,
        s.not_found_title,
        &format!(
            "<p class=\"note\">{text}</p><p><a href=\"/public\">{back}</a></p>",
            text = esc(text),
            back = esc(s.back),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(text: &str) -> Request {
        Request::new("r-1", text, "alpha", IntakeKind::Feature)
    }

    #[test]
    fn the_public_form_offers_only_nominated_repositories() {
        // **The property is unchanged; the mechanism is not.** This used to
        // assert the form had no repository field at all, which was how a single
        // -repository surface kept a stranger from aiming work anywhere. With a
        // set, the field exists — and what protects the property is that its
        // options come from the operator's configuration and the submitted name
        // is checked against the same set.
        let repos = crate::config::Repos::new(&["alpha", "beta"]);
        let html = public_file_page(&[], &repos, true, Locale::En);

        assert!(html.contains("name=\"repo\""), "the picker renders: {html}");
        assert!(html.contains("<option value=\"alpha\">"), "{html}");
        assert!(html.contains("<option value=\"beta\">"), "{html}");
        assert!(!html.contains("gamma"), "only what was nominated: {html}");

        // And **still no path**, which is the half of the old assertion that
        // never stopped mattering: the server holds names, never locations.
        for path_ish in ["name=\"path\"", "name=\"workspace\"", "name=\"dir\""] {
            assert!(!html.contains(path_ish), "{path_ish}: {html}");
        }
        assert!(html.contains("name=\"text\""), "{html}");
        assert!(html.contains("name=\"kind\""), "{html}");
    }

    #[test]
    fn one_repository_needs_no_picker() {
        // A select with one option is a question with one answer. The filer has
        // nothing to decide, so the field is not rendered at all — which is also
        // what keeps a single-repository deployment's form exactly as it was.
        let repos = crate::config::Repos::new(&["alpha"]);
        let html = public_file_page(&[], &repos, true, Locale::En);
        assert!(!html.contains("name=\"repo\""), "{html}");
        assert!(html.contains("name=\"text\""), "{html}");
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

        let html = public_detail(&r, true, Locale::En);
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

        let html = public_detail(&r, false, Locale::En);
        assert!(!html.contains("Private details"), "{html}");
        assert!(html.contains("with a reviewer"), "but they know it moved");
    }

    #[test]
    fn a_filer_is_not_told_their_request_was_screened_for_spam() {
        // Saying so invites gaming, and "received" is true in the sense the
        // filer cares about — a human may yet release it.
        //
        // Checked in **every** language: a catalogue is where a translator
        // helpfully renders "quarantined" literally, and the reason for the
        // euphemism does not stop at the language boundary.
        for locale in Locale::ALL {
            for state in [RequestState::Screening, RequestState::Quarantined] {
                let mut r = req("a thing");
                r.state = state;
                let html = public_detail(&r, true, locale).to_lowercase();
                for leak in ["spam", "quarantin", "pourriel", "quarantaine"] {
                    assert!(!html.contains(leak), "{locale} {state:?} leaks {leak}");
                }
                assert!(
                    html.contains(&locale.strings().state_received.to_lowercase()),
                    "{locale} {state:?}: {html}"
                );
            }
        }
    }

    #[test]
    fn every_page_declares_the_language_it_is_written_in() {
        // Without a correct `lang`, a screen reader pronounces French with
        // English phonemes — which is the accessibility failure this whole
        // feature is supposed to fix, not cause.
        for locale in Locale::ALL {
            let html = signin_page_in(locale);
            assert!(
                html.contains(&format!("<html lang=\"{}\"", locale.code())),
                "{locale}: {html}"
            );
        }
    }

    #[test]
    fn a_translated_page_carries_no_english_left_over() {
        // The catalogue drift tests prove the *strings* were translated. This
        // proves the **renderers** actually use them: a hardcoded English
        // fragment in a format string passes every catalogue check and still
        // renders English into a French page.
        let mut r = req("a thing");
        r.state = RequestState::AwaitingReview;
        r.spec = Some("# Spec".to_string());

        let pages = [
            signin_page_in(Locale::Fr),
            signin_sent_page(Locale::Fr),
            signin_confirm_page("t", Locale::Fr),
            signin_failed_page(true, Locale::Fr),
            signin_failed_page(false, Locale::Fr),
            public_file_page(
                &[r.clone()],
                &crate::config::Repos::new(&["alpha"]),
                true,
                Locale::Fr,
            ),
            public_detail(&r, true, Locale::Fr),
            public_filed(&r, Locale::Fr),
            public_not_found(Locale::Fr),
        ];

        // Distinctive English the renderers used to hardcode. Each would have
        // survived the catalogue tests untouched.
        let english = [
            "Sign in",
            "Check your email",
            "File a request",
            "What needs doing",
            "Back",
            "Sign out",
            "with a reviewer",
            "Not found",
            "already been used",
            "You have not filed",
        ];
        // **Text only, with tags stripped.** Element ids and class names are
        // English by necessity — `id="signin-dialog"` is markup, not copy, and
        // is not translated in any codebase. Grepping the raw HTML flagged those
        // as untranslated text, which is a false positive that would push
        // somebody towards renaming ids to satisfy a test.
        // The stylesheet and the script go too. Their *contents* are prose in a
        // sense — the CSS carries long English comments — but none of it is copy
        // a reader sees, and a comment explaining the sign-in dialog naturally
        // contains the words "Sign in".
        let visible = |html: &str| {
            let body = html
                .rsplit("</style>")
                .next()
                .unwrap_or(html)
                .split("<script")
                .next()
                .unwrap_or("");
            let mut out = String::with_capacity(body.len());
            let mut inside_tag = false;
            for c in body.chars() {
                match c {
                    '<' => inside_tag = true,
                    '>' => inside_tag = false,
                    _ if !inside_tag => out.push(c),
                    _ => {}
                }
            }
            out
        };
        for html in &pages {
            let text = visible(html);
            for word in english {
                assert!(
                    !text.contains(word),
                    "English {word:?} survived in visible text: {text}"
                );
            }
        }
    }
}
