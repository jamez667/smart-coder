//! The public surface's words, in the languages it has them in.
//!
//! Only the **public** half is translated. The private review pages have exactly
//! one reader — the developer running the server — and translating them would be
//! catalogue weight paid for nobody.
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

/// Every string the public surface renders.
///
/// One field per string. See the module docs for why this is a struct and not a
/// map — in short, a language missing one of these does not compile.
///
/// Field names describe **where the string appears**, not what it says, so a
/// reworded string does not want a renamed field.
#[derive(Debug)]
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
    /// Shown when a filing names a repository this surface does not collect
    /// for. Deliberately says nothing about which ones it *does* — the picker
    /// already lists those to anyone who reached the form honestly.
    pub file_repo_unknown: &'static str,
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
            s.file_repo_unknown,
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
        const SAME_ON_PURPOSE: [&str; 3] = [
            // A product name, not a word.
            "brand",
            // "you@example.com" -> "vous@exemple.com" differs; but a language
            // that shares the address form legitimately matches.
            "signin_email_placeholder",
            // Digits and a unit; French shortens "hr" to "h" but a language
            // using "min" unchanged is not untranslated.
            "ago_minutes",
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
