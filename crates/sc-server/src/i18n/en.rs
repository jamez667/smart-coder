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
    footer_tagline: "Smart Coder — file a request, read the spec it becomes.",

    signin_title: "Sign in",
    signin_intro: "Filing a request needs an email address — it is how you find your way back \
                   to what you filed, and it keeps this form from being a free-for-all.",
    signin_email_label: "Email",
    signin_email_placeholder: "you@example.com",
    signin_submit: "Email me a link",
    signin_no_password: "No password. We send a link that works once, for fifteen minutes.",

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
};
