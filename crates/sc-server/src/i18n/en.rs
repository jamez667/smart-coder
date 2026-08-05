//! English — the source language.
//!
//! Where a string is reworded, it is reworded **here first** and the other
//! catalogues follow. The drift test in [`super::super::i18n`]'s sibling module
//! cannot tell a stale translation from a deliberate one, so the ordering
//! convention is what keeps that honest.

use super::Strings;

pub static STRINGS: Strings = Strings {
    brand: "Smart Coder",
    theme_light: "Light",
    theme_dark: "Dark",
    language_label: "Language",
    language_apply: "Apply",
    footer_tagline: " — file a request, read the spec it becomes.",

    nav_signin: "Sign in",
    nav_account: "Account",
    nav_admin_heading: "Admin",
    nav_admin_review: "Requests",
    nav_admin_settings: "Settings",
    nav_admin_repos: "Repositories",
    nav_admin_owners: "Owners",
    nav_admin_daemons: "Machines",
    nav_admin_accounts: "Who can file",
    dialog_close: "Close",

    // **The client's wording, which replaced the catalogue's.** The three points
    // were on entirely different topics in each — the catalogue argued "say it
    // plainly / a person decides / no password to forget", and the interface
    // argued "say it plainly / a spec not a ticket / somebody reads it". The
    // interface is what a reader actually sees, so it won and this followed.
    landing_headline: "Ask for a change — get a spec back.",
    landing_sub: "Describe what needs doing in your own words. It comes back as a \
                  written specification for the developer to read, approve, or send \
                  back for another pass.",
    landing_point_1_title: "Say it plainly",
    landing_point_1_body: "No issue templates and no jargon. A sentence or two about what \
                           is wrong or what you want is enough to start.",
    landing_point_2_title: "A spec, not a ticket",
    landing_point_2_body: "What comes back is a written specification grounded in the \
                           actual code, not a restatement of what you asked for.",
    landing_point_3_title: "Somebody reads it",
    landing_point_3_body: "Nothing is built until a person approves the spec. The gate is \
                           a human one, and it stays that way.",

    signin_title: "Sign in",
    signin_intro: "Filing a request needs an email address — it is how you find your way back \
                   to what you filed, and it keeps this form from being a free-for-all.",
    signin_email_label: "Email",
    signin_email_placeholder: "you@example.com",
    signin_submit: "Email me a link",
    signin_user_label: "Email",
    signin_password_label: "Password",
    signin_wrong: "That did not work. Check the username and password, and try again in a moment.",
    signin_password_heading: "Admin login",
    signin_password_submit: "Sign in",
    signin_register: "Create an account",
    signin_no_mail: "This site cannot send sign-in links just now, so there is no way to sign in. Try again later.",
    signin_no_password: "No password. We send a link that works once, for fifteen minutes.",
    signin_no_password_note: "No password. We send a link that works once, for fifteen \
                              minutes. Filing for the first time creates the account.",
    signin_sent: "If that address can sign in here, a link is on its way. It works once, \
                  for fifteen minutes.",

    sent_title: "Check your email",
    sent_body: "If that address can receive mail, a sign-in link is on its way. \
                It expires in fifteen minutes.",
    sent_nothing_yet: "Nothing else has happened yet — the link is what signs you in.",

    confirm_title: "Confirm sign-in",
    confirm_intro: "Press the button to finish signing in on this device.",
    confirm_submit: "Sign me in",
    confirm_not_you: "If you did not ask for this, close the page — nothing happens \
                      until you press it.",

    link_failed_title: "That link did not work",
    link_already_used: "That link has already been used. You are probably signed in already — ",
    link_already_used_link: "try filing something",
    link_expired: "That link is not valid any more. They expire after fifteen minutes.",
    link_ask_again: "Ask for a new one",

    file_title: "File a request",
    file_prompt: "What needs doing?",
    file_placeholder: "Describe it the way you would to a colleague.",
    file_submit: "File it",
    file_cap_before: "Up to ",
    file_cap_after: " words. Short is better — a spec is drafted from what you write, \
                     not copied from it.",
    file_spec_note: " You will be able to read the spec that comes back.",
    file_kind_label: "Kind",
    file_repo_label: "Project",
    owner_title: "Review",
    owner_nothing: "Nothing has been filed against your projects yet.",
    owner_note: "You can send a spec back for another pass, or discard it. Accepting one is the developer's decision.",
    owner_note_label: "Why is it not right?",
    owner_note_hint: "The next draft is written from this.",
    owner_send_back: "Send it back",
    owner_discard: "Discard it",
    owner_release: "Release it — this is not spam",
    owner_release_note: "Screening held this back. Read it and decide; leaving it here decides nothing.",
    file_no_repos: "This site is not collecting requests just now — no project is open for them. Try again later.",
    file_repo_unknown: "That project does not take requests here.",
    file_mine_heading: "What you have filed",
    file_nothing_yet: "You have not filed anything yet.",
    file_signout: "Sign out",

    filed_title: "Filed",
    filed_body: "Filed. Come back to this page to see what happens to it.",
    filed_feedback_body: "Thanks — that is recorded. Feedback is kept for the developer \
                          to read; it does not become a spec.",
    back: "Back",

    detail_asked_heading: "What you asked for",
    detail_spec_heading: "The spec that came back",
    detail_spec_withheld: "A spec has been written and is with a reviewer.",
    detail_filed_prefix: "filed ",

    state_received: "received",
    state_writing: "being written up",
    state_reviewing: "with a reviewer",
    state_accepted: "accepted",
    state_closed: "closed",

    ago_just_now: "just now",
    ago_prefix: "",
    ago_minutes: " min ago",
    ago_hours: " hr ago",
    ago_days: " days ago",

    error_empty: "A request needs some text.",
    error_too_long: "That is longer than this form takes.",
    not_found_title: "Not found",
    not_found_body: "There is nothing here.",

    // -- the client's own strings, transcribed from `web/src/` --------------
    //
    // These were written in the interface first and are copied here **as they
    // read on screen**, not reworded on the way. Where one of them and an older
    // field above disagreed, the field above was changed to match this — see
    // the landing points, which were on different topics entirely.
    nav_mine: "What you have filed",
    nav_review: "Requests to review",
    nav_signout: "Sign out",
    theme_to_light: "Light",
    theme_to_dark: "Dark",
    footer_tagline_app: " — ask for a change, get a spec back.",

    filing_heading: "What needs doing?",
    filing_text_label: "What do you need?",
    filing_text_placeholder: "Describe the change in your own words.",
    filing_kind_label: "Is it broken, or missing?",
    kind_feature: "Something is missing",
    kind_bug: "Something is broken",
    filing_repo_label: "Which project?",
    filing_submit: "File it",
    filing_done: "Filed. Somebody writes it up, and it appears below when there is a \
                  spec to read.",
    filing_none_title: "Nothing to file against",
    filing_none_body: "This server is not offering any repository for requests right now.",

    review_heading: "Requests",
    review_empty_title: "Nothing to review",
    review_empty_body: "When something is filed and drafted, it appears here.",
    review_no_daemon: "Nothing has polled this server recently, so nothing will pick this \
                       up. Start a daemon.",
    review_unserved: "Daemons are polling, but none offers this repository. Check the name \
                      matches what queue add-repo was given.",
    review_skip_to_decision: "Skip to the decision",
    review_asked_heading: "What was asked for",
    review_spec_heading: "The drafted spec",
    review_note_heading: "Note",
    review_landed_before: "It landed in ",
    review_decide_heading: "Your call",
    review_send_back_label: "Send it back — what should change?",
    review_send_back_placeholder: "The redraft grounds on this, so be specific.",
    review_send_back_submit: "Send back",
    review_accept: "Approve this spec",
    review_discard: "Discard",
    review_leaving_decides_nothing: "Leaving this page decides nothing — it will still be here.",
    review_quarantine_leaving: "Leaving it here decides nothing.",

    // The raw wire states, given faces. A reviewer decides on the difference
    // between these; a filer is deliberately shown the coarse `state_*` set.
    review_state_screening: "screening",
    review_state_quarantined: "held back",
    review_state_queued: "queued",
    review_state_claimed: "being written up",
    review_state_awaiting_review: "awaiting review",
    review_state_accepted: "accepted",
    review_state_discarded: "discarded",
    review_state_failed: "failed",

    app_not_found_title: "Not found",
    app_not_found_body: "There is nothing at this address.",
    app_not_found_link: "Back to the start",

    admin_saved: "Saved.",
    admin_save: "Save",
    admin_add: "Add",
    admin_revoke: "Revoke",
    admin_revoked_tag: "revoked",

    settings_heading: "Settings",
    settings_public_heading: "The public site",
    settings_public_note: "Whether strangers can file requests here at all. A freshly \
                           claimed server starts with this off.",
    settings_public_on: "Turn the public site on",
    settings_public_off: "Turn the public site off",
    settings_filers_heading: "What filers see",
    settings_show_spec: "Let a filer read the spec drafted from their own request",
    settings_stack_heading: "Set in the stack, not here",
    settings_stack_note: "The address, the site name, the mail settings and the spam \
                          screener are environment variables. Change them where the \
                          container is configured, then redeploy. They used to be \
                          editable here and seeded from the stack, which meant a variable \
                          could be set, correct, and silently ignored.",
    settings_ceilings_heading: "Ceilings",
    settings_ceilings_note: "What this server will spend in a day. Blank means the \
                             built-in default, which is not the same as zero.",
    settings_max_filings: "Filings a day",
    settings_max_drafts: "Drafts a day",
    settings_max_accounts: "Accounts",
    settings_max_links: "Outstanding sign-in links",

    owners_heading: "Owners",
    owners_note: "An owner signs in with a username and password, and reviews requests for \
                  the repositories you name here. They can send work back, release it and \
                  discard it — they cannot accept it.",
    owners_add_heading: "Add an owner",
    owners_email_label: "Email",
    owners_repos_label: "Repositories, separated by commas",

    repos_heading: "Repositories",
    repos_unserved_before: "No machine has offered ",
    repos_unserved_after: ". Enabling it anyway means requests filed against it will wait \
                           until one does.",
    repos_enable_anyway: "Enable it anyway",
    repos_no_machine: "no machine has offered it",
    repos_off_tag: "off",
    repos_turn_off: "Turn it off",
    repos_add_heading: "Enable a repository",
    repos_name_label: "Repository name",
    repos_enable: "Enable",

    daemons_heading: "Machines",
    daemons_minted_after: " — this key is shown once and cannot be recovered. Put it in \
                           that machine's configuration now.",
    daemons_add_heading: "Add a machine",
    daemons_label_label: "A name for it",
    daemons_mint: "Mint a key",

    accounts_heading: "Who can file",
    accounts_note: "Revoked accounts stay listed. A list that silently shrinks cannot \
                    answer \"did I already deal with that?\".",
    accounts_password_tag: "password",

    setup_code_heading: "Set up this server",
    setup_code_intro: "This server has not been claimed. Whoever claims it administers it, \
                       so the code below is printed in the container's log — being able to \
                       read that log is the proof.",
    setup_code_label: "The claim code from the log",
    setup_base_url_before: "This server answers at ",
    setup_base_url_after: ", which is set where the container is configured.",
    setup_continue: "Continue",
    setup_admin_heading: "Who administers this?",
    setup_admin_intro: "Choose an email address and a password. This account administers \
                        this server: it reviews requests, decides what the public site \
                        collects, and holds the keys. ",
    setup_admin_intro_strong: "There is no second one.",
    setup_email_label: "Email",
    setup_password_label: "Password",
    setup_min_password_before: "At least ",
    setup_min_password_chars: " characters",
    setup_min_password_chars_one: " character",
    setup_min_password_after: ". It is stored hashed and cannot be recovered — if you lose \
                               it, delete ",
    setup_min_password_tail: " from the volume and claim the server again.",
    setup_claim: "Claim it",
};
