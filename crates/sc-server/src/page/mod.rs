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

use crate::i18n::Locale;

// Re-exported so `crate::page::not_found()` and friends keep resolving exactly
// as they did before the split. Every call site in `routes.rs` is unchanged,
// which is what makes this a mechanical move rather than a rewrite.
pub use private::{
    accounts_page, confirm_approve, detail, enrol_page, enrol_page_with_error, enrolled_page,
    filed, index, message, not_found,
};
pub use public::{
    public_detail, public_file_page, public_filed, public_message, public_not_found,
    signin_confirm_page, signin_failed_page, signin_page, signin_page_in, signin_sent_page,
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

/// The **public** surface's stylesheet.
///
/// Shares its token names and values with the GitHub Pages site
/// <!--@ crates/sc-comply/src/report/site.rs -->, so the two read as one product.
/// Copied rather than imported, and that is a deliberate trade: the alternative
/// is a shared crate between the compliance reporter and the intake server,
/// which couples two things that have nothing else to do with each other. The
/// drift this risks is cosmetic and visible.
///
/// ## Logical properties throughout
///
/// `margin-inline`, `border-inline-start` and `text-align:start` rather than
/// `left`/`right`, so a right-to-left language costs a translation rather than a
/// stylesheet rewrite. Free now, expensive to retrofit — and this surface has a
/// language switcher, which is what makes it a real prospect rather than a
/// hypothetical one.
pub(crate) const PUBLIC_STYLE: &str = r#"
:root{
--bg:#fbfbfd;--surface:#fff;--surface-2:#f4f4f7;
--fg:#16181d;--dim:#5b6270;--faint:#878e9c;
--line:#e3e5ea;--line-2:#cdd1d9;
--link:#2f5fd8;--accent-ink:#fff;
--shadow:0 1px 2px rgba(16,18,24,.06),0 4px 12px rgba(16,18,24,.05);
--s1:.25rem;--s2:.5rem;--s3:.75rem;--s4:1rem;--s5:1.5rem;--s6:2.5rem;
--radius:.75rem;
color-scheme:light}
/* Following the OS is the default and stays the default when nothing is chosen.
   `:not(:has(...))` keeps an explicit light choice from being overridden by a
   dark OS. */
@media(prefers-color-scheme:dark){
:root:not(:has(#theme-light:checked)){
--bg:#0e1014;--surface:#161920;--surface-2:#1d212a;
--fg:#e8eaef;--dim:#a2aab8;--faint:#6f7789;
--line:#262b35;--line-2:#333a47;
--link:#7ea2ff;--accent-ink:#0e1014;
--shadow:0 1px 2px rgba(0,0,0,.4),0 4px 14px rgba(0,0,0,.3);
color-scheme:dark}}
/* An explicit choice wins over the OS, in both directions. */
:root:has(#theme-dark:checked){
--bg:#0e1014;--surface:#161920;--surface-2:#1d212a;
--fg:#e8eaef;--dim:#a2aab8;--faint:#6f7789;
--line:#262b35;--line-2:#333a47;
--link:#7ea2ff;--accent-ink:#0e1014;
--shadow:0 1px 2px rgba(0,0,0,.4),0 4px 14px rgba(0,0,0,.3);
color-scheme:dark}
*{box-sizing:border-box}
body{margin:0 auto;padding:var(--s5) var(--s4) 4rem;max-width:46rem;
background:var(--bg);color:var(--fg);
font:16px/1.6 -apple-system,BlinkMacSystemFont,"Segoe UI",Helvetica,Arial,sans-serif;
-webkit-font-smoothing:antialiased}
/* The radios are off-screen rather than `display:none`, which would take them
   out of the tab order and hide them from a screen reader. */
.theme-in{position:absolute;width:1px;height:1px;margin:-1px;padding:0;
overflow:hidden;clip-path:inset(50%);white-space:nowrap}
.theme{display:flex;gap:var(--s1);align-items:center}
.theme label{font-size:.78rem;color:var(--dim);cursor:pointer;
padding:var(--s1) var(--s2);border-radius:.4rem;border:1px solid transparent;
min-height:1.9rem;display:inline-flex;align-items:center;
transition:background .12s,border-color .12s,color .12s}
.theme label:hover{background:var(--surface-2);color:var(--fg)}
#theme-os:checked~.masthead label[for=theme-os],
#theme-light:checked~.masthead label[for=theme-light],
#theme-dark:checked~.masthead label[for=theme-dark]{
background:var(--surface-2);border-color:var(--line-2);color:var(--fg);font-weight:600}
.theme-in:focus-visible~.masthead label[for]{outline:2px solid var(--link);outline-offset:2px}
.masthead{display:flex;justify-content:space-between;align-items:center;gap:var(--s3);
flex-wrap:wrap;padding-bottom:var(--s3);margin-bottom:var(--s5);
border-bottom:1px solid var(--line)}
.masthead .wordmark{font-weight:700;font-size:1.05rem;letter-spacing:-.01em;
text-decoration:none;color:var(--fg)}
.controls{display:flex;gap:var(--s3);align-items:center;flex-wrap:wrap}
/* The switcher submits on change where script is available, and falls back to
   its own button where it is not — so it is a plain form either way. */
.lang{display:flex;gap:var(--s1);align-items:center;margin:0}
.lang select{font:inherit;font-size:.78rem;width:auto;padding:var(--s1) var(--s2);
border:1px solid var(--line);border-radius:.4rem;
background:var(--surface);color:var(--fg);min-height:1.9rem}
.lang button{font-size:.78rem;width:auto;margin:0;padding:var(--s1) var(--s2);
min-height:1.9rem}
main{display:block}
h1{font-size:clamp(1.4rem,1.25rem + .7vw,1.8rem);margin:0 0 var(--s3);
letter-spacing:-.02em}
h2{font-size:1.05rem;margin:var(--s6) 0 var(--s3);padding-bottom:var(--s2);
border-bottom:1px solid var(--line);letter-spacing:-.01em}
a{color:var(--link);text-underline-offset:2px}
a:hover{text-decoration-thickness:2px}
:focus-visible{outline:2px solid var(--link);outline-offset:2px;border-radius:2px}
form{margin:0}
label{display:block;margin:var(--s4) 0 var(--s1);font-weight:600;font-size:.88rem}
textarea,input,select,button{font:inherit;width:100%;padding:var(--s3);
border-radius:.5rem;border:1px solid var(--line-2);
background:var(--surface);color:var(--fg)}
textarea{min-height:8rem;resize:vertical;line-height:1.55}
button{cursor:pointer;margin-top:var(--s4);font-weight:600;
background:var(--link);color:var(--accent-ink);border-color:transparent}
button:hover{filter:brightness(1.08)}
.item{display:block;padding:var(--s3) var(--s4);margin:var(--s2) 0;
text-decoration:none;color:inherit;background:var(--surface);
border:1px solid var(--line);border-radius:var(--radius);
box-shadow:var(--shadow);transition:border-color .12s,box-shadow .12s}
.item:hover{border-color:var(--line-2)}
.meta{font-size:.82rem;color:var(--dim)}
.tag{display:inline-block;font-size:.74rem;padding:.1em .6em;border-radius:10px;
font-weight:600;background:var(--surface-2);color:var(--dim)}
/* Model-authored text. It scrolls inside its own box rather than pushing the
   page sideways — on a phone that is the difference between readable and not. */
pre{white-space:pre-wrap;overflow-wrap:anywhere;overflow-x:auto;
padding:var(--s4);background:var(--surface);border:1px solid var(--line);
border-radius:var(--radius);box-shadow:var(--shadow);
font:13px/1.55 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}
.note{background:var(--surface);border:1px solid var(--line);
border-inline-start:3px solid var(--link);border-radius:var(--radius);
padding:var(--s3) var(--s4);margin:var(--s5) 0;box-shadow:var(--shadow)}
"#;

/// The public surface's masthead: wordmark, theme control, language switcher.
///
/// The theme control is **three radio inputs and CSS**, with no script at all.
/// Script is permitted on this surface now, but a theme that needs it flashes
/// the wrong colours before it runs, and this approach has no flash by
/// construction. The radios sit before `.masthead` because the sibling
/// combinator only reaches forwards.
///
/// **Three, not two.** A radio cannot be un-checked, so light/dark alone would be
/// a one-way door: pick either and there is no way back to following the system.
/// The third option is what makes the control reversible, and it starts checked
/// so the page opens on the reader's own setting.
///
/// The choice does not survive a page load — there is no cookie for it. That is
/// the honest limit of doing this without script, and it is the right trade
/// here: the alternative is a third cookie and a route to set it, on a surface
/// whose pages a reader passes through two or three at a time.
fn masthead(locale: Locale) -> String {
    let s = locale.strings();
    let mut options = String::new();
    for l in Locale::ALL {
        options.push_str(&format!(
            "<option value=\"{code}\"{sel}>{name}</option>",
            code = esc(l.code()),
            sel = if l == locale { " selected" } else { "" },
            name = esc(l.endonym()),
        ));
    }

    format!(
        "<input type=\"radio\" name=\"theme\" id=\"theme-os\" class=\"theme-in\" checked>\
<input type=\"radio\" name=\"theme\" id=\"theme-light\" class=\"theme-in\">\
<input type=\"radio\" name=\"theme\" id=\"theme-dark\" class=\"theme-in\">\
<header class=\"masthead\">\
<a class=\"wordmark\" href=\"/public\">{brand}</a>\
<div class=\"controls\">\
<div class=\"theme\" role=\"group\" aria-label=\"{theme_label}\">\
<label for=\"theme-os\">{auto}</label>\
<label for=\"theme-light\">{light}</label>\
<label for=\"theme-dark\">{dark}</label></div>\
<form class=\"lang\" method=\"post\" action=\"/public/language\">\
<label for=\"lang\" class=\"theme-in\">{lang_label}</label>\
<select id=\"lang\" name=\"lang\">{options}</select>\
<button type=\"submit\">{apply}</button></form>\
</div></header>",
        brand = esc(s.brand),
        theme_label = esc(s.theme_label),
        auto = esc(s.theme_auto),
        light = esc(s.theme_light),
        dark = esc(s.theme_dark),
        lang_label = esc(s.language_label),
        apply = esc(s.language_apply),
    )
}

/// The document a **public** page is wrapped in.
///
/// Carries the `lang` attribute, without which a screen reader pronounces French
/// with English phonemes — the accessibility failure a language switcher exists
/// to fix rather than to cause.
pub(crate) fn public_shell(locale: Locale, title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"{lang}\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>{title}</title><style>{PUBLIC_STYLE}</style></head><body>\
{masthead}<main>{body}</main></body></html>",
        lang = esc(locale.code()),
        title = esc(title),
        masthead = masthead(locale),
    )
}

/// The document a **private** page is wrapped in.
///
/// Kept plain on purpose. It has one reader, who is the developer, and it is
/// served with a CSP that permits no script — so it gets none of the controls
/// the public shell carries, and needs none of them.
pub(crate) fn shell(title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>{}</title><style>{STYLE}</style></head><body>{body}</body></html>",
        esc(title)
    )
}

/// The intake kinds, as a picker, for the private surface.
///
/// Driven by `IntakeKind::ALL`, so a kind added there appears on both surfaces
/// without anyone remembering to update a second list.
pub(crate) fn kind_field() -> String {
    kind_field_in(Locale::En)
}

/// The intake kinds, as a picker, labelled in a language.
///
/// **The option text is the slug, untranslated**, and that is deliberate: the
/// slug is what the form submits and what the developer reads on the review
/// page, so translating the visible text would mean a filer and a reviewer
/// naming the same kind differently. Only the field's label is translated.
pub(crate) fn kind_field_in(locale: Locale) -> String {
    let mut opts = String::new();
    for k in sc_proto::IntakeKind::ALL {
        opts.push_str(&format!(
            "<option value=\"{slug}\">{slug}</option>",
            slug = esc(k.slug())
        ));
    }
    format!(
        "<label for=\"kind\">{label}</label>\
         <select id=\"kind\" name=\"kind\">{opts}</select>",
        label = esc(locale.strings().file_kind_label),
    )
}

/// A timestamp, as something a human reads, on the private surface.
///
/// Deliberately coarse. The reader's question is "is this fresh, or did it sit
/// overnight?", which a relative age answers and a wall-clock time does not —
/// the server has no idea what timezone the browser is in.
pub(crate) fn ago(then_ms: u64, now_ms: u64) -> String {
    relative_time(then_ms, now_ms, Locale::En)
}

/// The same, in a language.
///
/// Built from a **prefix and a suffix** around the number rather than a format
/// template, because English puts the marker after ("5 min ago") and French
/// before ("il y a 5 min"). See [`crate::i18n`] on why the catalogue holds no
/// placeholders.
pub(crate) fn relative_time(then_ms: u64, now_ms: u64, locale: Locale) -> String {
    let s = locale.strings();
    let secs = now_ms.saturating_sub(then_ms) / 1000;
    let (n, unit) = match secs {
        0..=59 => return s.ago_just_now.to_string(),
        60..=3599 => (secs / 60, s.ago_minutes),
        3600..=86_399 => (secs / 3600, s.ago_hours),
        _ => (secs / 86_400, s.ago_days),
    };
    format!("{}{n}{unit}", s.ago_prefix)
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
    ///
    /// Rendered in **every** language, because the self-containment properties
    /// are properties of the surface and not of English. A catalogue is the
    /// obvious place for a stray `<a href="http://…">` to enter, and covering
    /// one language would not see it.
    pub fn public() -> Vec<(&'static str, String)> {
        let mut all = Vec::new();
        for l in Locale::ALL {
            all.extend([
                ("signin_page", signin_page()),
                ("signin_page_in", signin_page_in(l)),
                ("signin_sent_page", signin_sent_page(l)),
                ("signin_confirm_page", signin_confirm_page("abc123", l)),
                ("signin_failed_page", signin_failed_page(true, l)),
                ("signin_failed_page", signin_failed_page(false, l)),
                ("public_file_page", public_file_page(&[req()], true, l)),
                ("public_detail", public_detail(&reviewable(), true, l)),
                ("public_filed", public_filed(&req(), l)),
                ("public_not_found", public_not_found(l)),
                ("public_message", public_message("nope", l)),
            ]);
        }
        all
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
