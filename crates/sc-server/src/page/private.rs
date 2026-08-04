//! The private surface: the developer's own review pages.
//!
//! **Forms, not fetch, and no script at all.** These pages render drafted specs
//! from *every* filer, so the argument that permits script on the public half —
//! that a filer only ever sees their own — does not reach here. The CSP stays
//! `default-src 'none'` with no `script-src` at all
//! ([`Policy::Strict`](crate::routes::Policy::Strict)), and permitting script is
//! what would make a rendered model-authored spec dangerous.
//!
//! It also makes the surface work on a phone with a bad connection on a train,
//! which is the situation this whole feature exists for.

use sc_proto::IntakeKind;

use super::{ago, esc, kind_field, shell};

use crate::account::Accounts;
use crate::store::{Request, RequestState};

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

/// What the server knows about who could draft this, for a request that is
/// waiting.
///
/// Passed in rather than looked up here, because the register lives behind a
/// lock and a page renderer that takes locks is one that can deadlock a request
/// while producing HTML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Who {
    pub coverage: crate::daemons::Coverage,
    /// What live daemons *do* offer — shown when the wanted repository is not
    /// among them, because "add it" is only actionable next to the names that
    /// are already there.
    pub offered: Vec<String>,
}

impl Default for Who {
    /// Assumes a daemon is out there. The fallback for callers that have no
    /// register to consult, and it produces the message this page showed before
    /// any of this existed.
    fn default() -> Self {
        Who {
            coverage: crate::daemons::Coverage::Served,
            offered: Vec::new(),
        }
    }
}

/// One request, and its spec if there is one.
pub fn detail(r: &Request, who: &Who) -> String {
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
            // "Waiting for a daemon to pick it up" is true of all three of
            // these, and useless for two of them: a request nothing serves waits
            // for ever, and the operator can only act if the page says which
            // case it is. The two answers send them to different places.
            match who.coverage {
                crate::daemons::Coverage::Served => {
                    body.push_str("<p class=\"meta\">Waiting for a daemon to pick it up.</p>");
                }
                crate::daemons::Coverage::NoDaemonSeen => {
                    body.push_str(
                        "<p class=\"note\">No daemon has connected, so nothing will pick \
                         this up. Start one with <code>smart-coder queue serve</code>.</p>",
                    );
                }
                crate::daemons::Coverage::Unserved => {
                    // Each name escaped *before* the markup is joined around
                    // them — escaping afterwards would escape this file's own
                    // tags and print them at the reader.
                    let offered = if who.offered.is_empty() {
                        String::new()
                    } else {
                        let names: Vec<String> = who
                            .offered
                            .iter()
                            .map(|n| format!("<code>{}</code>", esc(n)))
                            .collect();
                        format!(
                            " The daemons that are connected serve {}.",
                            names.join(", ")
                        )
                    };
                    body.push_str(&format!(
                        "<p class=\"note\">No connected daemon serves <code>{}</code>, so \
                         nothing will pick this up.{offered} Add it with \
                         <code>smart-coder queue add-repo {} &lt;path&gt;</code>, or discard \
                         this request.</p>",
                        esc(&r.repo),
                        esc(&r.repo)
                    ));
                }
            }
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
        | (None, RequestState::Accepted)
        | (None, RequestState::Discarded)
        | (None, RequestState::Failed) => {}
    }

    if r.state == RequestState::Accepted {
        // Saying so plainly. The old state was called "ready", which read as
        // "built" — the rename to "accepted" is most of the fix, and this line
        // says the rest of it out loud.
        body.push_str(
            "<p class=\"note\">Accepted. The spec is settled in the repository — \
             nothing has been built. Open your IDE and run the pipeline when you \
             choose to.</p>",
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
/// artifact is a second artifact nobody verified, and accepting it means
/// accepting something the developer did not read (spec 20).
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

/// The confirmation page: restate what is being accepted, and bind to its bytes.
///
/// The `digest` is carried in a hidden field and re-checked on submit by
/// [`Store::accept`](crate::store::Store::accept). That is what turns a second
/// tap from ceremony into a real guarantee: the acceptance attaches to the exact
/// text shown here, so a redraft landing mid-review is refused rather than
/// silently accepted on the strength of reading the previous one.
pub fn confirm_accept(r: &Request, spec: &str, digest: &str) -> String {
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
        "Confirm",
        &format!(
            "<h1>Accept this spec?</h1>\
             <p>Accepting records that <strong>{summary}</strong> for \
             <strong>{repo}</strong> is settled. The spec is already in the \
             repository — this marks it done here so it drops out of your review \
             list. <strong>To build it, open your IDE and run the \
             pipeline.</strong></p>\
             <h2>What you are accepting</h2>{extract}\
             <div class=\"decide\">\
             <form method=\"post\" action=\"/request/{id}/accept/confirm\">\
             <input type=\"hidden\" name=\"digest\" value=\"{digest}\">\
             <button type=\"submit\">Yes — accept this spec</button></form>\
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
         <form method=\"post\" action=\"/request/{id}/accept\">\
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

/// Who may review, and for what.
///
/// **Shows an owner's repositories the surface no longer serves**, marked as
/// such. `identify` intersects the two and would simply grant less, and a
/// permission that looks applied and reaches nothing is the failure a setting
/// used to refuse at boot. A record cannot refuse, so the page has to say it.
pub fn owners_page(roster: &crate::roster::Roster, served: &crate::config::Repos) -> String {
    let mut rows = String::new();
    for o in &roster.owners {
        let action = if o.revoked {
            "<span class=\"meta\">revoked</span>".to_string()
        } else {
            format!(
                "<form method=\"post\" action=\"/owners/{}/revoke\">\
                 <button type=\"submit\">Revoke</button></form>",
                esc(&o.login)
            )
        };
        let repos = o
            .repos
            .iter()
            .map(|r| {
                if served.accepts(r) {
                    esc(r)
                } else {
                    format!("{} <span class=\"meta\">(not served here)</span>", esc(r))
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let repos = if repos.is_empty() {
            "<span class=\"meta\">nothing</span>".to_string()
        } else {
            repos
        };
        rows.push_str(&format!(
            "<div class=\"item\"><strong>{login}</strong>\
             <div class=\"meta\">{repos} · added {when}</div>{action}</div>",
            login = esc(&o.login),
            repos = repos,
            when = esc(&ago(o.added_ms, crate::store::now_ms())),
            action = action,
        ));
    }
    if roster.owners.is_empty() {
        rows.push_str("<p class=\"meta\">Nobody reviews but you.</p>");
    }

    let mut opts = String::new();
    for name in served.names() {
        opts.push_str(&format!(
            "<label class=\"check\"><input type=\"checkbox\" name=\"repos\" \
             value=\"{name}\"> {name}</label>",
            name = esc(name)
        ));
    }

    shell(
        "Owners",
        &format!(
            "<h1>Owners</h1>\
             <p class=\"meta\">An owner signs in with GitHub and reviews requests \
             for the repositories you name here. They can send work back, release \
             it and discard it — they cannot accept it, and they cannot add an \
             owner. Revoking one takes effect on their very next request.</p>{rows}\
             <h2>Add an owner</h2>\
             <form method=\"post\" action=\"/owners\">\
             <label for=\"login\">GitHub username</label>\
             <input id=\"login\" name=\"login\" required autocapitalize=\"off\" \
             autocorrect=\"off\" spellcheck=\"false\">\
             {opts}\
             <button type=\"submit\">Add</button></form>\
             <p class=\"meta\">Adding somebody already listed replaces their \
             repositories rather than adding a second entry.</p>\
             <p><a href=\"/\">Back to the queue</a></p>"
        ),
    )
}

/// Which repositories the public surface collects for.
///
/// `unconfirmed` is a name no polling daemon declared. The page **questions it
/// rather than refusing**: `Seen` is empty for the first half-minute after a
/// restart and an older daemon declares nothing, so a `None` is "cannot
/// confirm" and not "wrong". It names the case, says what *is* on offer, and
/// offers to proceed anyway.
pub fn repos_page(
    roster: &crate::roster::Roster,
    offered: &[String],
    unconfirmed: Option<&str>,
) -> String {
    let mut rows = String::new();
    for r in &roster.repos {
        let action = if r.disabled {
            "<span class=\"meta\">not collecting</span>".to_string()
        } else {
            format!(
                "<form method=\"post\" action=\"/repos/{}/disable\">\
                 <button type=\"submit\">Stop collecting</button></form>",
                esc(&r.name)
            )
        };
        // The two claims kept apart: a machine said it serves this, or the
        // developer asserted it. A page that showed them alike could not
        // explain why nothing is being drafted.
        let who = match &r.served_by {
            Some(label) => format!("confirmed by {}", esc(label)),
            None => "enabled without confirmation".to_string(),
        };
        rows.push_str(&format!(
            "<div class=\"item\"><strong>{name}</strong>\
             <div class=\"meta\">{who} · added {when}</div>{action}</div>",
            name = esc(&r.name),
            who = who,
            when = esc(&ago(r.added_ms, crate::store::now_ms())),
            action = action,
        ));
    }
    if roster.repos.is_empty() {
        rows.push_str(
            "<p class=\"meta\">Nothing is being collected. The public form says \
             so rather than taking filings nothing will pick up.</p>",
        );
    }

    let on_offer = if offered.is_empty() {
        "<p class=\"meta\">No daemon is polling right now, so nothing can be \
         confirmed. That is also the state for the first half-minute after a \
         restart.</p>"
            .to_string()
    } else {
        format!(
            "<p class=\"meta\">Daemons are currently offering: {}.</p>",
            esc(&offered.join(", "))
        )
    };

    // The override, offered only where it is the answer to a question just
    // asked. A permanent "enable anyway" checkbox would be ticked out of habit,
    // which is exactly the reflex the check exists to interrupt.
    let form = match unconfirmed {
        Some(name) => format!(
            "<h2>Nothing is serving {name}</h2>\
             <p class=\"meta\">No daemon that is polling right now declared \
             <strong>{name}</strong>. That usually means a typo, and it is much \
             cheaper to fix here than after filings pile up against a name \
             nothing will claim.</p>{on_offer}\
             <form method=\"post\" action=\"/repos\">\
             <input type=\"hidden\" name=\"name\" value=\"{name}\">\
             <input type=\"hidden\" name=\"anyway\" value=\"yes\">\
             <button type=\"submit\">Collect for {name} anyway</button></form>\
             <p class=\"meta\">Enabling it without confirmation is recorded as \
             such, so this page can still tell the two apart later.</p>\
             <h2>Or collect for something else</h2>{blank}",
            name = esc(name),
            on_offer = on_offer,
            blank = blank_repo_form(),
        ),
        None => format!(
            "<h2>Collect for a repository</h2>{on_offer}{}",
            blank_repo_form()
        ),
    };

    shell(
        "Repositories",
        &format!(
            "<h1>Repositories</h1>\
             <p class=\"meta\">Which projects the public site takes requests \
             for. Stopping one closes the door and keeps what already came \
             through it.</p>{rows}{form}\
             <p><a href=\"/\">Back to the queue</a></p>"
        ),
    )
}

/// What this server does — the settings that used to be environment variables.
///
/// **Secrets render as presence and a date, never as values.** There is no read
/// path for a stored secret anywhere in this server, which removes the class of
/// leak rather than gating it: no page, cache or screenshot can hold a live key.
///
/// `fresh` says whether this browser proved itself against GitHub recently
/// enough to change one. The fields render either way and are disabled when it
/// has not — being told to prove it again *after* typing a key is worse than
/// being told before.
pub fn settings_page(
    s: &crate::settings::Settings,
    fresh: bool,
    saved: Option<&str>,
    error: Option<&str>,
) -> String {
    let note = match (saved, error) {
        (_, Some(e)) => format!("<p class=\"note\">{}</p>", esc(e)),
        (Some(what), None) => format!("<p class=\"note\">{} saved.</p>", esc(what)),
        _ => String::new(),
    };

    let now = crate::store::now_ms();
    let secret = |label: &str, field: &str, sealed: &crate::seal::Sealed| {
        let state = if sealed.is_set() {
            format!(
                "<span class=\"tag\">set</span> \
                 <span class=\"meta\">changed {}</span>",
                esc(&ago(sealed.set_ms, now))
            )
        } else {
            "<span class=\"meta\">not set</span>".to_string()
        };
        format!(
            "<label for=\"{field}\">{label}</label>{state}\
             <input id=\"{field}\" name=\"{field}\" type=\"password\" \
             autocapitalize=\"off\" autocorrect=\"off\" spellcheck=\"false\" \
             placeholder=\"leave blank to keep\"{disabled}>",
            field = esc(field),
            label = esc(label),
            state = state,
            disabled = if fresh { "" } else { " disabled" },
        )
    };

    let gate = if fresh {
        "<p class=\"meta\">This browser proved itself recently, so these can be \
         changed. Blank means keep what is there.</p>"
            .to_string()
    } else {
        format!(
            "<p class=\"note\">Changing a secret needs a fresh sign-in, and it has \
             been more than five minutes. <a href=\"{}\">Prove it again</a>, then \
             come back.</p>",
            esc(crate::routes::public_route::AUTH_GITHUB)
        )
    };

    let disabled = if fresh { "" } else { " disabled" };

    shell(
        "Settings",
        &format!(
            "<h1>Settings</h1>{note}\
             <h2>The public site</h2>\
             <p class=\"meta\">Whether strangers can file requests at all. \
             Currently {public_state}.</p>\
             <form method=\"post\" action=\"/settings/public\">\
             <button type=\"submit\">{toggle}</button></form>\
             <form method=\"post\" action=\"/settings/site\">\
             <label for=\"site_name\">What the masthead calls it</label>\
             <input id=\"site_name\" name=\"site_name\" value=\"{site}\" \
             placeholder=\"{host}\">\
             <label class=\"check\"><input type=\"checkbox\" name=\"show_spec\" \
             value=\"yes\"{spec}> Let a filer read the spec drafted from their \
             own request</label>\
             <button type=\"submit\">Save</button></form>\
             <h2>Address and secrets</h2>\
             <p class=\"meta\">Sign-in links are built from the address. Changing \
             it <strong>signs everybody out</strong>, including you: the cookies \
             belong to the old one.</p>{gate}\
             <form method=\"post\" action=\"/settings/secret\">\
             <label for=\"base_url\">This address</label>\
             <input id=\"base_url\" name=\"base_url\" value=\"{base}\"{disabled}>\
             {mail_key}{gh_secret}{screen_key}\
             <button type=\"submit\"{disabled}>Save</button></form>\
             <h2>Sending email</h2>\
             <form method=\"post\" action=\"/settings/mail\">\
             <label for=\"mail_provider\">Provider</label>\
             <input id=\"mail_provider\" name=\"mail_provider\" value=\"{provider}\" \
             placeholder=\"brevo, resend or postmark\">\
             <label for=\"mail_from\">From address</label>\
             <input id=\"mail_from\" name=\"mail_from\" value=\"{from}\">\
             <label for=\"mail_from_name\">From name</label>\
             <input id=\"mail_from_name\" name=\"mail_from_name\" value=\"{from_name}\">\
             <button type=\"submit\">Save</button></form>\
             <h2>Screening</h2>\
             <p class=\"meta\">{screening_state}</p>\
             <form method=\"post\" action=\"/settings/screen\">\
             <label for=\"screen_url\">Endpoint</label>\
             <input id=\"screen_url\" name=\"screen_url\" value=\"{screen_url}\" \
             placeholder=\"{screen_url_default}\">\
             <label for=\"screen_model\">Model</label>\
             <input id=\"screen_model\" name=\"screen_model\" value=\"{screen_model}\" \
             placeholder=\"{screen_model_default}\">\
             <button type=\"submit\">Save</button></form>\
             <h2>What it may spend</h2>\
             <p class=\"meta\">Blank means the built-in default.</p>\
             <form method=\"post\" action=\"/settings/caps\">\
             <label for=\"max_daily_filings\">Filings per person per day</label>\
             <input id=\"max_daily_filings\" name=\"max_daily_filings\" value=\"{f}\">\
             <label for=\"max_daily_drafts\">Drafting runs per repository per day</label>\
             <input id=\"max_daily_drafts\" name=\"max_daily_drafts\" value=\"{d}\">\
             <label for=\"max_accounts\">Accounts</label>\
             <input id=\"max_accounts\" name=\"max_accounts\" value=\"{a}\">\
             <label for=\"max_outstanding_links\">Unspent sign-in links</label>\
             <input id=\"max_outstanding_links\" name=\"max_outstanding_links\" value=\"{l}\">\
             <button type=\"submit\">Save</button></form>\
             <p><a href=\"/\">Back to the queue</a></p>",
            note = note,
            public_state = if s.public {
                "<span class=\"tag\">on</span>"
            } else {
                "<span class=\"meta\">off, so the public site serves nothing</span>"
            },
            toggle = if s.public {
                "Turn it off"
            } else {
                "Turn it on"
            },
            site = esc(&s.site_name),
            host = esc(&crate::config::host_label(&s.base_url)),
            spec = if s.show_spec.unwrap_or(true) {
                " checked"
            } else {
                ""
            },
            base = esc(&s.base_url),
            disabled = disabled,
            gate = gate,
            mail_key = secret("Mail API key", "mail_key", &s.mail_key),
            gh_secret = secret(
                "GitHub client secret",
                "github_client_secret",
                &s.github_client_secret
            ),
            screen_key = secret("Screening API key", "screen_key", &s.screen_key),
            provider = esc(&s.mail_provider),
            from = esc(&s.mail_from),
            from_name = esc(&s.mail_from_name),
            screening_state = if s.has_screening() {
                "Filings are screened before a daemon may claim them."
            } else {
                "No key, so filings are NOT screened — they queue exactly as filed."
            },
            screen_url = esc(&s.screen_url),
            screen_url_default = esc(crate::config::DEFAULT_SCREEN_URL),
            screen_model = esc(&s.screen_model),
            screen_model_default = esc(crate::config::DEFAULT_SCREEN_MODEL),
            f = s
                .max_daily_filings
                .map(|v| v.to_string())
                .unwrap_or_default(),
            d = s
                .max_daily_drafts
                .map(|v| v.to_string())
                .unwrap_or_default(),
            a = s.max_accounts.map(|v| v.to_string()).unwrap_or_default(),
            l = s
                .max_outstanding_links
                .map(|v| v.to_string())
                .unwrap_or_default(),
        ),
    )
}

fn blank_repo_form() -> String {
    "<form method=\"post\" action=\"/repos\">\
     <label for=\"name\">Repository name</label>\
     <input id=\"name\" name=\"name\" required autocapitalize=\"off\" \
     autocorrect=\"off\" spellcheck=\"false\">\
     <button type=\"submit\">Collect for it</button></form>\
     <p class=\"meta\">Exactly as the daemon knows it — the name is matched \
     character for character.</p>"
        .to_string()
}

/// Not found, with the way back in named.
///
/// **A bare 404 is a confusing answer to the developer whose cookie used to
/// work.** Device enrolment is gone, so an enrolled browser now matches nothing
/// and lands here — and without this line the only signal would be a page that
/// says there is nothing at an address they used yesterday.
///
/// It leaks nothing: the sign-in link is already on the public landing page for
/// anyone to see.
pub fn not_found_for_admin() -> String {
    shell(
        "Not found",
        "<h1>Not found</h1><p>There is nothing here.</p>         <p class=\"meta\">If you administer this server,          <a href=\"/public/auth/github\">sign in with GitHub</a>.</p>",
    )
}

/// Claim an unclaimed server.
///
/// **Three steps in dependency order**, and the order is forced rather than
/// chosen: the GitHub callback URL is absolute, so it cannot be shown until the
/// base address is known, and the sign-in cannot run until an application
/// exists. Asking for everything on one screen would show a callback URL that
/// changes as somebody types.
///
/// `error` is a message from a rejected submission, re-rendered above the form
/// with what was typed still in it.
pub fn setup_page(base_url: &str, error: Option<&str>) -> String {
    let note = match error {
        Some(e) => format!("<p class=\"note\">{}</p>", esc(e)),
        None => String::new(),
    };

    // **Says what it decided rather than asking.** Whether cookies carry
    // `Secure` is derived from the address — "is this a private network" is a
    // question people answer wrong, and answering it wrong drops `Secure` from
    // every session cookie without a word. Shown live is impossible without
    // script, so it is shown for whatever has been entered so far.
    let decided = if base_url.trim().is_empty() {
        String::new()
    } else if crate::config::secure_for(base_url) {
        "<p class=\"meta\">Cookies will be marked <code>Secure</code>.</p>".to_string()
    } else {
        "<p class=\"meta\">That looks like a private address, so cookies will \
         <strong>not</strong> be marked <code>Secure</code>. Right for a LAN or \
         localhost, wrong for anything reachable from the internet.</p>"
            .to_string()
    };

    shell(
        "Set up this server",
        &format!(
            "<h1>Set up this server</h1>\
             <p class=\"meta\">Nobody administers this server yet. The code below \
             was printed to the container's log when it started — reading it is \
             how this server knows you are the person who deployed it.</p>{note}\
             <form method=\"post\" action=\"/setup\">\
             <label for=\"code\">Claim code</label>\
             <input id=\"code\" name=\"code\" required autocapitalize=\"characters\" \
             autocorrect=\"off\" spellcheck=\"false\" placeholder=\"XXX-XXX\">\
             <label for=\"base_url\">This server's address</label>\
             <input id=\"base_url\" name=\"base_url\" required autocapitalize=\"off\" \
             autocorrect=\"off\" spellcheck=\"false\" \
             placeholder=\"https://requests.example.com\" value=\"{base}\">\
             <button type=\"submit\">Continue</button></form>{decided}\
             <p class=\"meta\">The address is where people reach this server. \
             Sign-in links are built from it, so it has to be the real one and it \
             has to be <code>https://</code> unless you are on a private network.</p>",
            base = esc(base_url),
        ),
    )
}

/// Step two: the GitHub application, now that the callback URL is known.
pub fn setup_github_page(base_url: &str, error: Option<&str>) -> String {
    let note = match error {
        Some(e) => format!("<p class=\"note\">{}</p>", esc(e)),
        None => String::new(),
    };
    let callback = format!(
        "{}/public/auth/github/callback",
        base_url.trim_end_matches('/')
    );

    shell(
        "Set up this server",
        &format!(
            "<h1>Sign-in</h1>\
             <p class=\"meta\">You sign in to this server with GitHub, which means \
             it needs an OAuth application of its own. Create one at GitHub → \
             Settings → Developer settings → OAuth Apps.</p>{note}\
             <p>Paste this as the <strong>Authorization callback URL</strong>:</p>\
             <pre>{callback}</pre>\
             <form method=\"post\" action=\"/setup/github\">\
             <label for=\"client_id\">Client ID</label>\
             <input id=\"client_id\" name=\"client_id\" required autocapitalize=\"off\" \
             autocorrect=\"off\" spellcheck=\"false\">\
             <label for=\"client_secret\">Client secret</label>\
             <input id=\"client_secret\" name=\"client_secret\" type=\"password\" required \
             autocapitalize=\"off\" autocorrect=\"off\" spellcheck=\"false\">\
             <button type=\"submit\">Save and sign in</button></form>\
             <p class=\"meta\">The secret is stored encrypted and is never shown \
             again. The next step signs you in, and the account you sign in with \
             becomes this server's administrator.</p>",
            callback = esc(&callback),
        ),
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
    fn a_drafted_spec_is_escaped_rather_than_rendered() {
        // A model wrote it and it may contain anything. Escaping removes the
        // whole class; a filter that has to be right every time eventually is not.
        let mut r = req("a thing");
        r.state = RequestState::AwaitingReview;
        r.spec = Some(
            "# Spec\n<script>fetch('http://evil')</script>\n![x](http://evil/pixel)".to_string(),
        );

        let html = detail(&r, &Who::default());
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        // The Markdown image is inert text, not a request.
        assert!(!html.contains("<img"), "{html}");
    }

    #[test]
    fn a_request_typed_by_a_person_is_escaped_too() {
        // There is no trusted path: the text came off the public internet.
        let r = req("<b>bold</b> & \"quoted\"");
        let html = detail(&r, &Who::default());
        assert!(html.contains("&lt;b&gt;"), "{html}");
        assert!(!html.contains("<b>bold</b>"), "{html}");
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
        assert!(actions.contains("/request/r-1/accept"), "{actions}");
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
        let approve = actions.find("accept").unwrap();
        assert!(send_back < approve, "{actions}");

        // And deferring is free, and says so.
        assert!(detail(&r, &Who::default()).contains("decides nothing"));
    }

    #[test]
    fn a_send_back_demands_a_reason_in_the_form_itself() {
        // Without one the redraft has nothing to go on and produces the same
        // spec, which reads to the developer as being ignored.
        let mut r = req("a thing");
        r.state = RequestState::AwaitingReview;
        r.spec = Some("# Spec".to_string());
        let html = detail(&r, &Who::default());
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
        let html = detail(&r, &Who::default());

        let spec_ends = html.rfind("</pre>").unwrap();
        assert!(spec_ends < html.find("/send-back").unwrap(), "{html}");
        assert!(spec_ends < html.find("/accept").unwrap(), "{html}");
    }

    #[test]
    fn the_detail_pages_approve_button_asks_rather_than_decides() {
        // The mechanism is that /accept renders a confirmation. If the detail
        // page ever posted straight to the committing route, the second step is
        // gone and nothing in the routing tests would notice.
        let mut r = req("a thing");
        r.state = RequestState::AwaitingReview;
        r.spec = Some("# Spec".to_string());
        let html = detail(&r, &Who::default());

        assert!(html.contains("action=\"/request/r-1/accept\""), "{html}");
        assert!(!html.contains("/accept/confirm"), "{html}");
    }

    #[test]
    fn the_confirmation_restates_the_artifact_rather_than_only_asking_again() {
        // A confirm page that says just "are you sure?" is ceremony: it adds a
        // tap and no evidence. It has to put the text back in front of the
        // reviewer.
        let r = req("a thing");
        let spec = "# Spec\nthe first line\nthe last line";
        let html = confirm_accept(&r, spec, "deadbeef");

        assert!(html.contains("the first line"), "{html}");
        assert!(html.contains("the last line"), "{html}");
        assert!(html.contains("a thing"), "names what is accepted: {html}");
        assert!(html.contains("alpha"), "names the repository: {html}");
        // **Says where building actually happens.** The old copy said "nothing
        // is built", which left the reader to wonder what would build it; the
        // page now names the IDE, which is the whole point of the rename.
        assert!(html.contains("run the pipeline"), "{html}");
        assert!(html.contains("IDE"), "{html}");
    }

    #[test]
    fn the_confirmation_binds_the_approval_to_the_text_it_showed() {
        // Without the digest the reviewer consents to "whatever is on disk when
        // the POST lands", which a redraft arriving mid-review silently changes.
        let r = req("a thing");
        let html = confirm_accept(&r, "# Spec", "deadbeef");
        assert!(
            html.contains("name=\"digest\" value=\"deadbeef\""),
            "{html}"
        );
        assert!(html.contains("/request/r-1/accept/confirm"), "{html}");
    }

    #[test]
    fn the_confirmation_offers_a_way_out_that_weighs_the_same() {
        // If "yes" is a button and "no" is a text link, the confirm page is a
        // funnel rather than a decision.
        let r = req("a thing");
        let html = confirm_accept(&r, "# Spec", "deadbeef");

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
        let html = confirm_accept(&r, &spec, "d");

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
        let html = confirm_accept(&r, "# Spec\nshort", "d");
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

        for html in [
            detail(&r, &Who::default()),
            confirm_accept(&r, "# Spec", "d"),
        ] {
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

        let html = detail(&r, &Who::default());
        assert!(html.contains("filed 2 hr ago"), "{html}");
        assert!(html.contains("drafted 2 min ago"), "{html}");
    }

    #[test]
    fn an_undrafted_request_shows_no_drafted_time() {
        let r = req("a thing");
        let html = detail(&r, &Who::default());
        assert!(html.contains("filed "), "{html}");
        assert!(!html.contains("drafted "), "there is no draft yet: {html}");
    }

    #[test]
    fn the_skip_link_is_visible_rather_than_hidden() {
        // Hiding the bypass does not remove it — flicking to the bottom is the
        // bypass — it only lets the system believe nobody used one.
        let mut r = req("a thing");
        r.state = RequestState::AwaitingReview;
        r.spec = Some("# Spec".to_string());
        let html = detail(&r, &Who::default());
        assert!(html.contains("href=\"#decide\""), "{html}");
        assert!(html.contains("id=\"decide\""), "the anchor exists: {html}");
    }

    #[test]
    fn review_actions_appear_only_when_there_is_something_to_review() {
        // Approving a queued request would be signing off a spec that does not
        // exist.
        let r = req("a thing");
        let html = detail(&r, &Who::default());
        assert!(!html.contains("/accept"), "{html}");
        assert!(html.contains("Waiting for a daemon"), "{html}");
    }

    #[test]
    fn ready_says_plainly_that_nothing_was_built() {
        // "Ready" reads as "built" unless the page says otherwise.
        let mut r = req("a thing");
        r.state = RequestState::Accepted;
        r.spec = Some("# Spec".to_string());
        r.artifact_dir = Some("specs/a-thing".to_string());
        let html = detail(&r, &Who::default());
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
    fn a_quarantined_request_offers_release_to_the_reviewer() {
        // The developer overruling the screener — the reason quarantine is not
        // deletion.
        let mut r = req("a thing");
        r.state = RequestState::Quarantined;
        let html = detail(&r, &Who::default());
        assert!(html.contains("/request/r-1/release"), "{html}");
        assert!(html.contains("Nothing has run on your machine"), "{html}");
    }

    #[test]
    fn a_screening_request_says_what_it_is_waiting_for() {
        // The state that used to render nothing at all, because `detail` had a
        // catch-all arm.
        let mut r = req("a thing");
        r.state = RequestState::Screening;
        let html = detail(&r, &Who::default());
        assert!(html.contains("screened"), "{html}");
    }

    #[test]
    fn a_failed_claim_reveals_nothing_about_why() {
        // The same property the enrolment page had, moved to the page that now
        // takes a one-time code: distinguishing "expired" from "wrong" from
        // "already spent" tells a guesser which half they got right.
        let html = setup_page(
            "",
            Some(
                "That code did not work. It is printed in the container's log                  at startup, is single-use, and expires after thirty minutes.",
            ),
        );
        assert!(html.contains("did not work"), "{html}");
        for leak in ["already spent", "no code is armed", "wrong code"] {
            assert!(!html.contains(leak), "{leak} leaks which half was right");
        }
    }
}
