//! The interface's words, in the languages it has them in.
//!
//! ## Everything is translated, and that reverses what this module used to say
//!
//! This module doc read "only the **public** half is translated": the private
//! review pages had one reader, and a catalogue for them was weight paid for
//! nobody. **That is overturned.** The whole interface is translated — the
//! landing page, the sign-in dialog, the review pages, the administrative lists
//! and the setup wizard.
//!
//! The argument that won is already written a few hundred lines below, on
//! `nav_admin_*`: those menu entries were translated because the one
//! administrator per server *may not read English*, and the masthead around them
//! already was. That argument was never about the menu. A menu in French whose
//! every entry opens an English page is worse than an English menu — it promises
//! a translated surface and then withdraws it one click later, and the reader
//! who needed the translation is the one who finds out. The pages the menu
//! points at get the same treatment as the menu.
//!
//! The setup wizard is included for a sharper reason. It is the **first** thing
//! anybody sees on a fresh server, before there is an account, a cookie or a
//! preference — so `Accept-Language` is the only signal there will ever be, and
//! it is the one screen where getting the language wrong means the reader cannot
//! claim the server at all. The recovery is deleting a file off a volume.
//!
//! What is still deliberately **untranslated** is narrow and mechanical:
//! repository names, email addresses, machine labels, a minted key, the `SC`
//! monogram, decorative glyphs, wire values a form submits, and the intake
//! *kinds* as slugs. Those are identifiers or data, not prose — see
//! `kind_feature`/`kind_bug` for the one place a slug gains a translated label
//! while the wire value stays put.
//!
//! ## Who reads a catalogue
//!
//! Two readers, and they want the same catalogue for different reasons:
//!
//! - the **server itself**, for the handful of strings it sends already
//!   translated — the coarse state labels, the filing refusals, and the
//!   magic-link landing, which is the one document this server still renders;
//! - the **client**, which fetches the whole negotiated catalogue once from
//!   `GET /api/v1/ui/strings` and renders every screen out of it.
//!
//! The client is why the four fifths of this module that had no reader now have
//! one. Nothing here was deleted while it was waiting, and this is what it was
//! waiting for.
//!
//! ## A missing translation does not compile
//!
//! The catalogue is a `struct` with one field per string, not a map keyed on a
//! name. That choice is the whole design:
//!
//! ```text
//! HashMap<&str, &str>   a missing key is a runtime `None` — and the fallback
//!                       renders English into the middle of a French page,
//!                       which nobody notices until a user says so
//!
//! struct Strings        a missing field is a compile error naming the
//!                       language and the string, at the moment it goes missing
//! ```
//!
//! So adding a string to the surface breaks the build of every language until
//! each has one. That is the point: the failure arrives while the person adding
//! it is still holding the context, rather than months later in production.
//!
//! ## Formatting stays out
//!
//! Fields are plain `&'static str` and never format templates. A translator
//! reordering `{0}` and `{1}`, or dropping one, is a runtime panic in a formatter
//! — and it is the kind of mistake translation *invites*. Where a string needs a
//! value in the middle, the renderer splits it into two fields and puts the value
//! between them. Clumsier to write, impossible to get wrong.
//!
//! ## No markup in the catalogue
//!
//! Strings are text, never HTML fragments. A translated `<a href=…>` is an
//! injection point maintained by whoever last edited the catalogue, and it would
//! defeat the escaping the rest of this crate is careful about. Renderers build
//! the markup and put escaped catalogue text inside it.

use std::fmt;

mod en;
mod fr;

/// A language the public surface can be read in.
///
/// Deliberately a closed enum rather than a BCP-47 string. The set of languages
/// this server has *catalogues* for is a compile-time fact, and treating it as
/// one means [`Locale::ALL`] cannot drift from what exists — the language
/// switcher, the `Accept-Language` match and the cookie parser all read from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, PartialOrd, Ord)]
pub enum Locale {
    #[default]
    En,
    Fr,
}

impl Locale {
    /// Every language, in the order the switcher lists them.
    pub const ALL: [Locale; 2] = [Locale::En, Locale::Fr];

    /// The `lang` attribute, the cookie value, and the switcher's form value —
    /// one code for all three, so they cannot disagree.
    pub fn code(self) -> &'static str {
        match self {
            Locale::En => "en",
            Locale::Fr => "fr",
        }
    }

    /// The language's name **in that language**, for the switcher.
    ///
    /// "Français", never "French": somebody who cannot read the current page is
    /// exactly the person using this control, so listing the options in a
    /// language they may not read defeats it.
    pub fn endonym(self) -> &'static str {
        match self {
            Locale::En => "English",
            Locale::Fr => "Français",
        }
    }

    /// Parse a language code. Case-insensitive, and tolerant of a region suffix
    /// (`fr-CA`, `en_GB`) since both cookies and `Accept-Language` carry them.
    pub fn parse(code: &str) -> Option<Locale> {
        let base = code
            .split(['-', '_'])
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        Locale::ALL.into_iter().find(|l| l.code() == base)
    }

    /// The catalogue for this language.
    pub fn strings(self) -> &'static Strings {
        match self {
            Locale::En => &en::STRINGS,
            Locale::Fr => &fr::STRINGS,
        }
    }
}

impl fmt::Display for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// Pick a language for a request.
///
/// **The cookie wins**, because it is the only signal the reader chose
/// deliberately — `Accept-Language` is whatever their browser was installed
/// with, and overriding an explicit choice with it makes the switcher look
/// broken. An unparseable or unknown cookie value falls through to the header
/// rather than erroring; a bad cookie is a stale one, not an attack.
pub fn negotiate(cookie: Option<&str>, accept_language: Option<&str>) -> Locale {
    cookie
        .and_then(Locale::parse)
        .or_else(|| accept_language.and_then(from_accept_language))
        .unwrap_or_default()
}

/// Best match from an `Accept-Language` header.
///
/// Honours `q=` weights, since a browser sending `fr;q=0.9, en;q=1.0` is stating
/// a preference and ignoring it would hand them their second choice. Entries this
/// server has no catalogue for are skipped rather than failing the parse — a
/// header listing six languages must still match on the one that is present.
fn from_accept_language(header: &str) -> Option<Locale> {
    let mut best: Option<(Locale, f32)> = None;
    for part in header.split(',') {
        let mut bits = part.split(';');
        let Some(tag) = bits.next().map(str::trim) else {
            continue;
        };
        // A malformed weight is treated as *absent* (q=1), not as zero. Reading
        // "q=abc" as "does not want this at all" is the wrong way to be wrong.
        let q = bits
            .find_map(|b| b.trim().strip_prefix("q=")?.trim().parse::<f32>().ok())
            .unwrap_or(1.0);
        let Some(locale) = Locale::parse(tag) else {
            continue;
        };
        // Strictly greater, so an earlier entry wins a tie — the header is
        // ordered by preference where weights are equal.
        if best.is_none_or(|(_, bq)| q > bq) {
            best = Some((locale, q));
        }
    }
    best.map(|(l, _)| l)
}

/// Every string the interface renders.
///
/// One field per string. See the module docs for why this is a struct and not a
/// map — in short, a language missing one of these does not compile.
///
/// Field names describe **where the string appears**, not what it says, so a
/// reworded string does not want a renamed field.
///
/// **`Serialize` is what the client eats.** `GET /api/v1/ui/strings` sends this
/// derived form — a flat object of field name to text — so the wire shape is the
/// struct definition rather than a second list somebody maintains beside it. A
/// field added here reaches the client with nothing else edited, which is the
/// same reason `fields()` in the tests reads the struct out of its own source.
#[derive(Debug, serde::Serialize)]
pub struct Strings {
    // -- the shell ----------------------------------------------------------
    /// The product name in the masthead. Not translated in any catalogue — it is
    /// a name — but present as a field so a language that *does* transliterate it
    /// has somewhere to put that.
    pub brand: &'static str,
    pub theme_light: &'static str,
    pub theme_dark: &'static str,
    pub language_label: &'static str,
    /// The switcher's submit button, for readers with script disabled.
    pub language_apply: &'static str,
    /// The footer line, **after** the site's name. Says what this surface is,
    /// since a filer may arrive on it from a link with no other context.
    ///
    /// The name is interpolated rather than written into the string, so the
    /// footer and the masthead cannot call the same site two different things —
    /// which they did when this read "Smart Coder ..." against a masthead
    /// showing the configured repository.
    pub footer_tagline: &'static str,
    /// The masthead's sign-in action, and the account menu.
    pub nav_signin: &'static str,
    pub nav_account: &'static str,
    /// The administrative pages, as they appear in the account menu.
    ///
    /// **Translated, unlike the route slugs they point at.** These are read by
    /// exactly one person per server, but that person may not read English, and
    /// the rest of the masthead around them is already translated.
    pub nav_admin_heading: &'static str,
    pub nav_admin_review: &'static str,
    pub nav_admin_settings: &'static str,
    pub nav_admin_repos: &'static str,
    pub nav_admin_owners: &'static str,
    pub nav_admin_daemons: &'static str,
    pub nav_admin_accounts: &'static str,
    /// The dialog's close button, for a screen reader — the glyph itself is
    /// decorative and hidden from one.
    pub dialog_close: &'static str,

    // -- the landing page ----------------------------------------------------
    //
    // The only page that says what this is for. Everything else assumes the
    // reader already knows.
    pub landing_headline: &'static str,
    pub landing_sub: &'static str,
    pub landing_point_1_title: &'static str,
    pub landing_point_1_body: &'static str,
    pub landing_point_2_title: &'static str,
    pub landing_point_2_body: &'static str,
    pub landing_point_3_title: &'static str,
    pub landing_point_3_body: &'static str,

    // -- signing in ---------------------------------------------------------
    pub signin_title: &'static str,
    pub signin_intro: &'static str,
    pub signin_email_label: &'static str,
    pub signin_email_placeholder: &'static str,
    pub signin_submit: &'static str,
    pub signin_no_password: &'static str,
    /// Shown instead of the email form when no mail provider is configured.
    ///
    /// Names nobody and blames nothing: a filer cannot act on it and does not
    /// need a culprit, only to know that coming back later is the answer.
    pub signin_no_mail: &'static str,
    /// Names the other role, so somebody who is an owner knows this page has
    /// Heads the password form, naming who it is for.
    ///
    /// **Both named roles, not just owners.** The administrator signs in here
    /// too, and a heading that named only owners would send the one person who
    /// can fix a broken server looking for a door that does not exist.
    pub signin_password_heading: &'static str,
    /// One message for every sign-in failure.
    ///
    /// Wrong password, no such account, still backing off — all one answer,
    /// because distinguishing them tells a guesser which half they got right.
    pub signin_wrong: &'static str,
    pub signin_user_label: &'static str,
    pub signin_password_label: &'static str,
    pub signin_password_submit: &'static str,
    /// The register button, under the email form.
    ///
    /// **The same route as signing in**, because on this surface a first
    /// sign-in *is* the signup — there is no separate registration to perform.
    /// It exists because "Email me a link" does not read as a way to *start*,
    /// and somebody with no account needs to see a door rather than infer one.
    pub signin_register: &'static str,

    pub sent_title: &'static str,
    pub sent_body: &'static str,
    pub sent_nothing_yet: &'static str,

    pub confirm_title: &'static str,
    pub confirm_intro: &'static str,
    pub confirm_submit: &'static str,
    pub confirm_not_you: &'static str,

    pub link_failed_title: &'static str,
    pub link_already_used: &'static str,
    pub link_already_used_link: &'static str,
    pub link_expired: &'static str,
    pub link_ask_again: &'static str,

    // -- filing -------------------------------------------------------------
    pub file_title: &'static str,
    pub file_prompt: &'static str,
    pub file_placeholder: &'static str,
    pub file_submit: &'static str,
    /// Split around the word count: `"Up to "` + `500` + `" words. …"`. See the
    /// module docs on why there is no `{}` here.
    pub file_cap_before: &'static str,
    pub file_cap_after: &'static str,
    pub file_spec_note: &'static str,
    pub file_kind_label: &'static str,
    /// The repository picker's label. Shown only when the surface serves more
    /// than one — the names themselves are never translated.
    pub file_repo_label: &'static str,
    pub owner_title: &'static str,
    pub owner_nothing: &'static str,
    pub owner_note: &'static str,
    pub owner_note_label: &'static str,
    pub owner_note_hint: &'static str,
    pub owner_send_back: &'static str,
    pub owner_discard: &'static str,
    /// Overruling the screener, which is why quarantine is not deletion.
    ///
    /// Deliberately not "looks fine to me": the screener held it for a reason,
    /// and the point is that a person reads the text and decides rather than
    /// clearing a nag.
    pub owner_release: &'static str,
    /// Why the request is sitting here at all, above the release button.
    pub owner_release_note: &'static str,
    /// Shown when a filing names a repository this surface does not collect
    /// for. Deliberately says nothing about which ones it *does* — the picker
    /// already lists those to anyone who reached the form honestly.
    pub file_repo_unknown: &'static str,
    /// Shown instead of the form when no repository is enabled.
    ///
    /// A real state, reachable the moment a developer disables the last one.
    /// It says the site is between configurations rather than broken, and
    /// names nobody: a filer cannot act on it and does not need a culprit.
    pub file_no_repos: &'static str,
    pub file_mine_heading: &'static str,
    pub file_nothing_yet: &'static str,
    pub file_signout: &'static str,

    pub filed_title: &'static str,
    pub filed_body: &'static str,
    pub filed_feedback_body: &'static str,
    pub back: &'static str,

    // -- a filer's own request ----------------------------------------------
    pub detail_asked_heading: &'static str,
    pub detail_spec_heading: &'static str,
    pub detail_spec_withheld: &'static str,
    pub detail_filed_prefix: &'static str,

    // -- states, as a filer is told them ------------------------------------
    //
    // Coarser than the internal states on purpose: a filer is not told their
    // request is being screened for spam. See `public_state_label`.
    pub state_received: &'static str,
    pub state_writing: &'static str,
    pub state_reviewing: &'static str,
    pub state_accepted: &'static str,
    pub state_closed: &'static str,

    // -- relative times -----------------------------------------------------
    //
    // A **prefix and a suffix** around the number, not a suffix alone: English
    // puts the marker after ("5 min ago") and French puts it before ("il y a
    // 5 min"). A suffix-only field would have forced the French catalogue to
    // write English word order, which is the kind of thing that ships because
    // the type still compiled.
    pub ago_just_now: &'static str,
    pub ago_prefix: &'static str,
    pub ago_minutes: &'static str,
    pub ago_hours: &'static str,
    pub ago_days: &'static str,

    // -- what goes wrong ----------------------------------------------------
    pub error_empty: &'static str,
    pub error_too_long: &'static str,
    pub not_found_title: &'static str,
    pub not_found_body: &'static str,

    // =======================================================================
    // Everything below is read by the **client** rather than by a renderer in
    // this crate, and arrives there through `GET /api/v1/ui/strings`.
    //
    // Grouped by the component that draws them, because that is the unit
    // somebody edits. Where the client's English and an older field above
    // disagreed, the client's wording won and the field above was rewritten to
    // match it — the interface is what a reader actually sees, and a catalogue
    // that quietly says something else is a second source of truth.
    // =======================================================================

    // -- the masthead --------------------------------------------------------
    /// The account menu's own entry for a filer's own requests. Distinct from
    /// `file_mine_heading`, which heads the page it opens: a menu entry and a
    /// page title are edited independently and one is often shorter.
    pub nav_mine: &'static str,
    /// A reviewer who is *not* the administrator gets one extra entry, since the
    /// administrative block above it does not exist for them.
    pub nav_review: &'static str,
    /// The account menu's sign-out. Separate from `file_signout`, which was the
    /// rendered filing page's button — same words today, different callers.
    pub nav_signout: &'static str,
    /// The theme toggle's `title`, and its screen-reader text. The glyphs beside
    /// it are decorative and hidden from one, so these carry the whole meaning.
    pub theme_to_light: &'static str,
    pub theme_to_dark: &'static str,

    // -- the landing page's footer -------------------------------------------
    /// The footer says the brand and then the tagline. **The client used to
    /// hardcode both into one sentence**, which put the product name back into a
    /// translatable string after `footer_tagline` was deliberately split to keep
    /// it out — so the renderer joins `brand` and this, and neither catalogue
    /// holds the other's half.
    ///
    /// Distinct from `footer_tagline` above, which the magic-link landing uses:
    /// the client's wording is "ask for a change, get a spec back" and the
    /// rendered page's is "file a request, read the spec it becomes". Two
    /// readers arriving from two places; kept separate rather than unified,
    /// because unifying them is a wording decision and not a translation one.
    pub footer_tagline_app: &'static str,

    // -- the sign-in dialog --------------------------------------------------
    /// What the dialog says once a link has been asked for.
    ///
    /// **The same words whether or not an account existed**, because the server
    /// answers identically — saying "check your email" only when there was a
    /// mailbox to send to would give back everything that identical answer buys.
    pub signin_sent: &'static str,
    /// Under the email form. The client's version adds the sentence the
    /// catalogue's `signin_no_password` lacked: that filing for the first time
    /// is what creates the account. That is the whole reason a stranger can use
    /// this at all, so it stays.
    pub signin_no_password_note: &'static str,

    // -- filing --------------------------------------------------------------
    /// The filing page's heading, which is a question rather than a title.
    pub filing_heading: &'static str,
    /// The textarea's label. Deliberately not the same string as the heading
    /// above it — a label repeating its own heading reads to a screen reader as
    /// the question asked twice.
    pub filing_text_label: &'static str,
    pub filing_text_placeholder: &'static str,
    /// The kind picker's label, and the two options.
    ///
    /// **The options are translated; the values they submit are not.** `feature`
    /// and `bug` are wire values the server matches on and the developer reads
    /// on the review page — translating those would have a filer and a reviewer
    /// naming the same kind differently.
    pub filing_kind_label: &'static str,
    pub kind_feature: &'static str,
    pub kind_bug: &'static str,
    /// The repository picker's label. Shown only when the surface serves more
    /// than one; the names in it are never translated.
    pub filing_repo_label: &'static str,
    pub filing_submit: &'static str,
    /// Shown after a successful filing, in place of a navigation.
    pub filing_done: &'static str,
    /// Shown instead of the form when this surface serves no repository. A real
    /// state, reachable the moment the last one is disabled — and it says the
    /// site is between configurations rather than broken.
    pub filing_none_title: &'static str,
    pub filing_none_body: &'static str,

    // -- the review list -----------------------------------------------------
    pub review_heading: &'static str,
    pub review_empty_title: &'static str,
    pub review_empty_body: &'static str,

    // -- one request, and the decision about it ------------------------------
    /// The two diagnostics under a request nothing has picked up.
    ///
    /// **Two messages rather than one**, because "no machine has polled at all"
    /// and "machines are polling and none offers this repository" send an
    /// operator to two different places, and one message for both sends half of
    /// them to the wrong one.
    pub review_no_daemon: &'static str,
    pub review_unserved: &'static str,
    /// The bypass link. Visible rather than hidden: hiding it does not remove
    /// it, it only lets the system believe nobody used one.
    pub review_skip_to_decision: &'static str,
    pub review_asked_heading: &'static str,
    pub review_spec_heading: &'static str,
    pub review_note_heading: &'static str,
    /// Precedes the artifact directory: `"It landed in "` + the path. See the
    /// module docs on why there is no `{}` here. The path is on the
    /// administrator's own machine and is never translated.
    ///
    /// **A prefix alone, with no matching suffix**, because the sentence ends at
    /// the value in both catalogues. An empty `_after` field would have been a
    /// field every future language has to fill with nothing, and the
    /// no-empty-string test would need an exception naming it — a permanent hole
    /// in a check, bought to keep a symmetry no reader benefits from.
    pub review_landed_before: &'static str,
    /// Heads the decision controls.
    pub review_decide_heading: &'static str,
    /// The send-back form: its label, its placeholder, and its button.
    pub review_send_back_label: &'static str,
    pub review_send_back_placeholder: &'static str,
    pub review_send_back_submit: &'static str,
    pub review_accept: &'static str,
    pub review_discard: &'static str,
    /// Under the controls. Says that closing the page is not a decision, which
    /// is the thing a reviewer most needs permission to do.
    pub review_leaving_decides_nothing: &'static str,
    pub review_quarantine_leaving: &'static str,

    // -- raw states and kinds, as a reviewer is shown them --------------------
    //
    // **These are the wire values, given faces.** `ReviewRequest.state` and
    // `kind` arrive as `queued`, `quarantined`, `feature` — code identifiers,
    // rendered straight into the page. A reviewer is a person, and a person
    // reading "awaiting-review" is reading a variable name.
    //
    // Distinct from `state_*` above, which are the **coarse** labels a filer
    // sees and the server has already translated. A reviewer decides on the
    // difference between `Quarantined` and `Queued`; a filer is deliberately not
    // told there is one. Two sets of words for two audiences, and merging them
    // would leak the screening states to the audience they are hidden from.
    pub review_state_screening: &'static str,
    pub review_state_quarantined: &'static str,
    pub review_state_queued: &'static str,
    pub review_state_claimed: &'static str,
    pub review_state_awaiting_review: &'static str,
    pub review_state_accepted: &'static str,
    pub review_state_discarded: &'static str,
    pub review_state_failed: &'static str,

    // -- the interface's own 404 ---------------------------------------------
    //
    // Says the same thing the server's rendered 404 says, because a reader who
    // followed a stale link should not be able to tell which of the two answered
    // — the distinction is about who routed, and that is not their problem.
    pub app_not_found_title: &'static str,
    pub app_not_found_body: &'static str,
    pub app_not_found_link: &'static str,

    // -- the administrative pages --------------------------------------------
    //
    // One reader per server, who may not read English. See the module doc.
    pub admin_saved: &'static str,
    pub admin_save: &'static str,
    pub admin_add: &'static str,
    pub admin_revoke: &'static str,
    pub admin_revoked_tag: &'static str,

    /// Settings.
    pub settings_heading: &'static str,
    pub settings_public_heading: &'static str,
    pub settings_public_note: &'static str,
    pub settings_public_on: &'static str,
    pub settings_public_off: &'static str,
    pub settings_filers_heading: &'static str,
    pub settings_show_spec: &'static str,
    pub settings_stack_heading: &'static str,
    pub settings_stack_note: &'static str,
    pub settings_ceilings_heading: &'static str,
    pub settings_ceilings_note: &'static str,
    pub settings_max_filings: &'static str,
    pub settings_max_drafts: &'static str,
    pub settings_max_accounts: &'static str,
    pub settings_max_links: &'static str,

    /// Owners.
    pub owners_heading: &'static str,
    pub owners_note: &'static str,
    pub owners_add_heading: &'static str,
    pub owners_email_label: &'static str,
    pub owners_repos_label: &'static str,

    /// Repositories.
    pub repos_heading: &'static str,
    /// Split around the repository name, which is never translated:
    /// `"No machine has offered "` + `intake` + `". Enabling it anyway …"`.
    pub repos_unserved_before: &'static str,
    pub repos_unserved_after: &'static str,
    pub repos_enable_anyway: &'static str,
    /// Shown in place of a machine's label when nothing has offered a
    /// repository. A phrase rather than a dash, because a blank column reads as
    /// a rendering fault.
    pub repos_no_machine: &'static str,
    pub repos_off_tag: &'static str,
    pub repos_turn_off: &'static str,
    pub repos_add_heading: &'static str,
    pub repos_name_label: &'static str,
    pub repos_enable: &'static str,

    /// Machines.
    pub daemons_heading: &'static str,
    /// Split around the machine's label, which is the operator's own word and is
    /// never translated: `label` + `" — this key is shown once …"`.
    pub daemons_minted_after: &'static str,
    pub daemons_add_heading: &'static str,
    pub daemons_label_label: &'static str,
    pub daemons_mint: &'static str,

    /// Who can file.
    pub accounts_heading: &'static str,
    pub accounts_note: &'static str,
    pub accounts_password_tag: &'static str,

    // -- the setup wizard ----------------------------------------------------
    //
    // **The first screen on a fresh server**, before there is an account or a
    // preference — so `Accept-Language` is the only signal, and it is the one
    // screen where the wrong language means the server cannot be claimed at all.
    pub setup_code_heading: &'static str,
    pub setup_code_intro: &'static str,
    pub setup_code_label: &'static str,
    /// Split around the address: `"This server answers at "` + the URL + `",
    /// which is set where the container is configured."` The URL is never
    /// translated.
    pub setup_base_url_before: &'static str,
    pub setup_base_url_after: &'static str,
    pub setup_continue: &'static str,
    pub setup_admin_heading: &'static str,
    pub setup_admin_intro: &'static str,
    pub setup_admin_intro_strong: &'static str,
    pub setup_email_label: &'static str,
    pub setup_password_label: &'static str,
    /// Split around **two** values: the minimum length, and the filename.
    ///
    /// `"At least "` + `12` + `" characters. …"` + `admin.json` + `" from the
    /// volume …"`. `admin.json` is a filename and is never translated.
    ///
    /// The middle piece carries a hazard the English does not have: French
    /// "caractère" agrees in number with the count, and `min_password` is
    /// configurable — so it can legitimately be 1. `min_password_chars_one`
    /// exists for that case and the renderer picks between them, which is the
    /// only plural rule this catalogue has and the only one it needs.
    pub setup_min_password_before: &'static str,
    pub setup_min_password_chars: &'static str,
    /// The singular of the word above, for a minimum of one. See its docs.
    pub setup_min_password_chars_one: &'static str,
    /// Between the count and the filename.
    pub setup_min_password_after: &'static str,
    /// After the filename, closing the sentence.
    pub setup_min_password_tail: &'static str,
    pub setup_claim: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cookie_beats_the_browsers_preference() {
        // The only signal the reader chose deliberately. If the header could
        // override it, the switcher would appear not to work.
        assert_eq!(negotiate(Some("fr"), Some("en")), Locale::Fr);
        assert_eq!(negotiate(Some("en"), Some("fr")), Locale::En);
    }

    #[test]
    fn a_stale_cookie_falls_through_rather_than_failing() {
        // A language that was once offered and is not any more leaves cookies in
        // the wild. That is a stale cookie, not an attack.
        assert_eq!(negotiate(Some("de"), Some("fr")), Locale::Fr);
        assert_eq!(negotiate(Some(""), Some("fr")), Locale::Fr);
        assert_eq!(negotiate(Some("../../etc/passwd"), None), Locale::En);
    }

    #[test]
    fn the_default_is_english_when_nothing_says_otherwise() {
        assert_eq!(negotiate(None, None), Locale::En);
        assert_eq!(negotiate(None, Some("de,ja;q=0.8")), Locale::En);
    }

    #[test]
    fn accept_language_weights_are_honoured() {
        // A browser sending weights is stating a preference; ignoring them hands
        // the reader their second choice.
        assert_eq!(negotiate(None, Some("en;q=0.2, fr;q=0.9")), Locale::Fr);
        assert_eq!(negotiate(None, Some("fr;q=0.1, en;q=0.8")), Locale::En);
        // Unweighted entries are q=1 and beat a weighted one.
        assert_eq!(negotiate(None, Some("fr, en;q=0.9")), Locale::Fr);
    }

    #[test]
    fn an_earlier_entry_wins_a_tie() {
        // The header is ordered by preference where weights are equal.
        assert_eq!(negotiate(None, Some("fr,en")), Locale::Fr);
        assert_eq!(negotiate(None, Some("en,fr")), Locale::En);
    }

    #[test]
    fn languages_this_server_lacks_are_skipped_not_fatal() {
        // A real header lists several. Matching must not stop at the first
        // unknown one.
        assert_eq!(
            negotiate(None, Some("de-DE,de;q=0.9,fr;q=0.8,en;q=0.7")),
            Locale::Fr
        );
    }

    #[test]
    fn a_region_suffix_still_matches_its_language() {
        // `fr-CA` is French. Both separators appear in the wild.
        assert_eq!(Locale::parse("fr-CA"), Some(Locale::Fr));
        assert_eq!(Locale::parse("en_GB"), Some(Locale::En));
        assert_eq!(Locale::parse("FR"), Some(Locale::Fr));
        assert_eq!(Locale::parse(" fr "), Some(Locale::Fr));
    }

    #[test]
    fn a_malformed_weight_reads_as_absent_rather_than_zero() {
        // "q=abc" means the browser sent something odd, not that the reader
        // does not want the language at all.
        assert_eq!(negotiate(None, Some("fr;q=abc")), Locale::Fr);
    }

    /// Every string in a catalogue, paired with its field name.
    ///
    /// Read out of this file's own source rather than hand-listed, so a field
    /// added to `Strings` is covered by the drift tests below without anyone
    /// remembering to add it here. The compiler already guarantees each language
    /// *has* the field; what these tests are for is the part it cannot see —
    /// whether the field was actually translated.
    fn fields(s: &'static Strings) -> Vec<(&'static str, &'static str)> {
        // The field list comes from the struct definition above, which is in
        // this same file. Anything between `pub struct Strings {` and its close.
        let src = include_str!("i18n.rs");
        let body = src
            .split("pub struct Strings {")
            .nth(1)
            .expect("the struct is declared in this file");
        let names: Vec<&str> = body
            .lines()
            .take_while(|l| !l.starts_with('}'))
            .filter_map(|l| l.trim().strip_prefix("pub "))
            .filter_map(|l| l.split(':').next())
            .collect();

        let values = [
            s.brand,
            s.theme_light,
            s.theme_dark,
            s.language_label,
            s.language_apply,
            s.footer_tagline,
            s.nav_signin,
            s.nav_account,
            s.nav_admin_heading,
            s.nav_admin_review,
            s.nav_admin_settings,
            s.nav_admin_repos,
            s.nav_admin_owners,
            s.nav_admin_daemons,
            s.nav_admin_accounts,
            s.dialog_close,
            s.landing_headline,
            s.landing_sub,
            s.landing_point_1_title,
            s.landing_point_1_body,
            s.landing_point_2_title,
            s.landing_point_2_body,
            s.landing_point_3_title,
            s.landing_point_3_body,
            s.signin_title,
            s.signin_intro,
            s.signin_email_label,
            s.signin_email_placeholder,
            s.signin_submit,
            s.signin_no_password,
            s.signin_no_mail,
            s.signin_password_heading,
            s.signin_wrong,
            s.signin_user_label,
            s.signin_password_label,
            s.signin_password_submit,
            s.signin_register,
            s.sent_title,
            s.sent_body,
            s.sent_nothing_yet,
            s.confirm_title,
            s.confirm_intro,
            s.confirm_submit,
            s.confirm_not_you,
            s.link_failed_title,
            s.link_already_used,
            s.link_already_used_link,
            s.link_expired,
            s.link_ask_again,
            s.file_title,
            s.file_prompt,
            s.file_placeholder,
            s.file_submit,
            s.file_cap_before,
            s.file_cap_after,
            s.file_spec_note,
            s.file_kind_label,
            s.file_repo_label,
            s.owner_title,
            s.owner_nothing,
            s.owner_note,
            s.owner_note_label,
            s.owner_note_hint,
            s.owner_send_back,
            s.owner_discard,
            s.owner_release,
            s.owner_release_note,
            s.file_repo_unknown,
            s.file_no_repos,
            s.file_mine_heading,
            s.file_nothing_yet,
            s.file_signout,
            s.filed_title,
            s.filed_body,
            s.filed_feedback_body,
            s.back,
            s.detail_asked_heading,
            s.detail_spec_heading,
            s.detail_spec_withheld,
            s.detail_filed_prefix,
            s.state_received,
            s.state_writing,
            s.state_reviewing,
            s.state_accepted,
            s.state_closed,
            s.ago_just_now,
            s.ago_prefix,
            s.ago_minutes,
            s.ago_hours,
            s.ago_days,
            s.error_empty,
            s.error_too_long,
            s.not_found_title,
            s.not_found_body,
            s.nav_mine,
            s.nav_review,
            s.nav_signout,
            s.theme_to_light,
            s.theme_to_dark,
            s.footer_tagline_app,
            s.signin_sent,
            s.signin_no_password_note,
            s.filing_heading,
            s.filing_text_label,
            s.filing_text_placeholder,
            s.filing_kind_label,
            s.kind_feature,
            s.kind_bug,
            s.filing_repo_label,
            s.filing_submit,
            s.filing_done,
            s.filing_none_title,
            s.filing_none_body,
            s.review_heading,
            s.review_empty_title,
            s.review_empty_body,
            s.review_no_daemon,
            s.review_unserved,
            s.review_skip_to_decision,
            s.review_asked_heading,
            s.review_spec_heading,
            s.review_note_heading,
            s.review_landed_before,
            s.review_decide_heading,
            s.review_send_back_label,
            s.review_send_back_placeholder,
            s.review_send_back_submit,
            s.review_accept,
            s.review_discard,
            s.review_leaving_decides_nothing,
            s.review_quarantine_leaving,
            s.review_state_screening,
            s.review_state_quarantined,
            s.review_state_queued,
            s.review_state_claimed,
            s.review_state_awaiting_review,
            s.review_state_accepted,
            s.review_state_discarded,
            s.review_state_failed,
            s.app_not_found_title,
            s.app_not_found_body,
            s.app_not_found_link,
            s.admin_saved,
            s.admin_save,
            s.admin_add,
            s.admin_revoke,
            s.admin_revoked_tag,
            s.settings_heading,
            s.settings_public_heading,
            s.settings_public_note,
            s.settings_public_on,
            s.settings_public_off,
            s.settings_filers_heading,
            s.settings_show_spec,
            s.settings_stack_heading,
            s.settings_stack_note,
            s.settings_ceilings_heading,
            s.settings_ceilings_note,
            s.settings_max_filings,
            s.settings_max_drafts,
            s.settings_max_accounts,
            s.settings_max_links,
            s.owners_heading,
            s.owners_note,
            s.owners_add_heading,
            s.owners_email_label,
            s.owners_repos_label,
            s.repos_heading,
            s.repos_unserved_before,
            s.repos_unserved_after,
            s.repos_enable_anyway,
            s.repos_no_machine,
            s.repos_off_tag,
            s.repos_turn_off,
            s.repos_add_heading,
            s.repos_name_label,
            s.repos_enable,
            s.daemons_heading,
            s.daemons_minted_after,
            s.daemons_add_heading,
            s.daemons_label_label,
            s.daemons_mint,
            s.accounts_heading,
            s.accounts_note,
            s.accounts_password_tag,
            s.setup_code_heading,
            s.setup_code_intro,
            s.setup_code_label,
            s.setup_base_url_before,
            s.setup_base_url_after,
            s.setup_continue,
            s.setup_admin_heading,
            s.setup_admin_intro,
            s.setup_admin_intro_strong,
            s.setup_email_label,
            s.setup_password_label,
            s.setup_min_password_before,
            s.setup_min_password_chars,
            s.setup_min_password_chars_one,
            s.setup_min_password_after,
            s.setup_min_password_tail,
            s.setup_claim,
        ];

        // The check that keeps this honest: if a field was added to the struct
        // and not to the array above, the counts disagree and this fails rather
        // than silently covering one fewer string.
        assert_eq!(
            names.len(),
            values.len(),
            "the field list in `fields()` is out of step with `struct Strings`"
        );
        names.into_iter().zip(values).collect()
    }

    #[test]
    fn no_string_is_empty_in_any_language() {
        // A blank field compiles and renders a gap in the page. The two
        // deliberate exceptions are the relative-time prefix, which English does
        // not use, and nothing else.
        for locale in Locale::ALL {
            for (name, value) in fields(locale.strings()) {
                if name == "ago_prefix" {
                    continue;
                }
                assert!(!value.trim().is_empty(), "{locale}.{name} is empty");
            }
        }
    }

    #[test]
    fn no_translation_is_left_at_its_english_text() {
        // What the compiler cannot see. A field filled in by copying the English
        // and meaning to come back to it looks exactly like a finished one, and
        // this is the only thing that will ever notice.
        //
        // The exceptions are strings that are *correctly* identical, and each is
        // named with its reason rather than skipped in bulk.
        const SAME_ON_PURPOSE: [&str; 6] = [
            // A product name, not a word.
            "brand",
            // "Machines" is the same word in French.
            "nav_admin_daemons",
            // The page that menu entry opens. Same word, same reason — and
            // named separately rather than folded into the one above, so a
            // language where they legitimately diverge is not silently excused
            // in both places at once.
            "daemons_heading",
            // "you@example.com" -> "vous@exemple.com" differs; but a language
            // that shares the address form legitimately matches.
            "signin_email_placeholder",
            // Digits and a unit; French shortens "hr" to "h" but a language
            // using "min" unchanged is not untranslated.
            "ago_minutes",
            // "Note" is the same word in French, and is the right one — the
            // alternatives ("Remarque", "Commentaire") are longer and say less.
            "review_note_heading",
        ];

        let english = fields(Locale::En.strings());
        for locale in Locale::ALL {
            if locale == Locale::En {
                continue;
            }
            let mut identical = Vec::new();
            for ((name, theirs), (en_name, ours)) in
                fields(locale.strings()).into_iter().zip(&english)
            {
                assert_eq!(name, *en_name, "the field lists disagree");
                if theirs == *ours && !SAME_ON_PURPOSE.contains(&name) {
                    identical.push(name);
                }
            }
            assert!(
                identical.is_empty(),
                "{locale} is still English in: {identical:?}"
            );
        }
    }

    #[test]
    fn no_catalogue_string_carries_markup_or_a_format_placeholder() {
        // Two failure modes the catalogue must not admit, both of which arrive
        // through translation rather than through code:
        //
        // - markup, because a translated `<a href=…>` is an injection point
        //   maintained by whoever last edited the catalogue;
        // - `{}`/`{0}`, because a translator dropping or reordering one is a
        //   runtime panic in a formatter. Strings that need a value in the
        //   middle are split into two fields instead.
        for locale in Locale::ALL {
            for (name, value) in fields(locale.strings()) {
                for bad in ['<', '>', '{', '}'] {
                    assert!(
                        !value.contains(bad),
                        "{locale}.{name} contains {bad:?}: {value}"
                    );
                }
            }
        }
    }

    #[test]
    fn every_locale_is_listed_and_has_a_distinct_code() {
        // `ALL` drives the switcher, the cookie parser and this test. A locale
        // added to the enum but not to `ALL` would be unreachable in the UI.
        let mut seen: Vec<&str> = Locale::ALL.iter().map(|l| l.code()).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(before, seen.len(), "two locales share a code");

        for l in Locale::ALL {
            assert_eq!(Locale::parse(l.code()), Some(l), "{l} does not round-trip");
            assert!(!l.endonym().is_empty());
        }
    }
}
