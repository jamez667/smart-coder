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
/* Warm cream in light, plum-black with coral in dark. Taken from Memosy's own
   palette (web/src/app.css in that repository) so two properties by the same
   owner look like it — reuse rather than imitation. Values are copied rather
   than referenced: that project is a separate repository with a JS toolchain,
   and this one deliberately has neither. */
:root{
--bg:#f5f0e8;--surface:#faf7f2;--surface-2:#ede8df;
--fg:#2c2a25;--dim:#7a7468;--faint:#9a9384;
--line:#d9d2c4;--line-2:#c4bba8;
--link:#8b6914;--accent-ink:#fff;
--shadow:0 1px 2px rgba(44,42,37,.05),0 4px 12px rgba(44,42,37,.04);
--s1:.25rem;--s2:.5rem;--s3:.75rem;--s4:1rem;--s5:1.5rem;--s6:2.5rem;
--radius:.625rem;--pill:999px;
color-scheme:light}
/* Following the OS is the default and stays the default when nothing is chosen.
   `:not(:has(...))` keeps an explicit light choice from being overridden by a
   dark OS. */
@media(prefers-color-scheme:dark){
:root:not(:has(#theme-light:checked)){
--bg:#141218;--surface:#1f1c24;--surface-2:#2a2633;
--fg:#f2eef8;--dim:#9d94ab;--faint:#7a7288;
--line:#3a3445;--line-2:#4a4356;
--link:#e85d6c;--accent-ink:#fff;
--shadow:0 1px 2px rgba(0,0,0,.4),0 4px 14px rgba(0,0,0,.3);
color-scheme:dark}}
/* An explicit choice wins over the OS, in both directions. */
:root:has(#theme-dark:checked){
--bg:#141218;--surface:#1f1c24;--surface-2:#2a2633;
--fg:#f2eef8;--dim:#9d94ab;--faint:#7a7288;
--line:#3a3445;--line-2:#4a4356;
--link:#e85d6c;--accent-ink:#fff;
--shadow:0 1px 2px rgba(0,0,0,.4),0 4px 14px rgba(0,0,0,.3);
color-scheme:dark}
*{box-sizing:border-box}
/* **A local stack, not a web font.** Memosy loads DM Sans and Fraunces from
   Google. This surface is served with `default-src 'none'`, which forbids the
   fetch outright — and that ban is what stops a model-authored spec leaking the
   page URL through a remote reference, so it is not up for trade against
   typography. Georgia is Memosy's own declared fallback for Fraunces, making
   this the degradation that design already anticipated rather than a
   substitution invented here. */
body{margin:0;min-height:100vh;display:flex;flex-direction:column;
background:var(--bg);color:var(--fg);
font:16px/1.6 system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",Helvetica,Arial,sans-serif;
-webkit-font-smoothing:antialiased}
/* The header and footer rules span the viewport; their contents share the
   column width with `main`, so the lines run edge to edge while the text stays
   readable — the arrangement in the reference design. */
.bar{border-block-end:1px solid var(--line);background:var(--surface)}
.bar-inner,main{max-width:60rem;margin-inline:auto;width:100%;
padding-inline:var(--s5)}
main{flex:1;padding-block:var(--s6) 4rem;display:block}
@media(max-width:520px){.bar-inner,main{padding-inline:var(--s4)}}
/* The radios are off-screen rather than `display:none`, which would take them
   out of the tab order and hide them from a screen reader. */
.theme-in{position:absolute;width:1px;height:1px;margin:-1px;padding:0;
overflow:hidden;clip-path:inset(50%);white-space:nowrap}
/* One control, three states, drawn as icons rather than the words "Auto Light
   Dark". A sun and a moon are read faster than their names and do not need
   translating — which matters on the one surface that has a language switcher
   beside them. */
.theme{display:inline-flex;gap:2px;align-items:center;padding:2px;
border:1px solid var(--line);border-radius:999px;background:var(--surface)}
.theme label{width:1.75rem;height:1.75rem;border-radius:999px;cursor:pointer;
display:inline-flex;align-items:center;justify-content:center;color:var(--dim);
transition:background .12s,color .12s}
.theme label:hover{background:var(--surface-2);color:var(--fg)}
.theme svg{width:15px;height:15px;display:block;
/* `currentColor` on a stroke, so the icon follows the label's colour through
   hover and selection without a second rule per state. */
fill:none;stroke:currentColor;stroke-width:1.7;
stroke-linecap:round;stroke-linejoin:round}
#theme-os:checked~.masthead label[for=theme-os],
#theme-light:checked~.masthead label[for=theme-light],
#theme-dark:checked~.masthead label[for=theme-dark]{
background:var(--link);color:var(--accent-ink)}
.theme-in:focus-visible~.masthead label[for]{outline:2px solid var(--link);outline-offset:2px}
.masthead{display:flex;justify-content:space-between;align-items:center;
gap:var(--s4);flex-wrap:wrap;padding-block:var(--s4)}
.masthead .wordmark{font-family:Georgia,"Times New Roman",serif;
font-weight:600;font-size:1.25rem;letter-spacing:-.02em;
text-decoration:none;color:var(--fg);display:inline-flex;align-items:center;
gap:.65rem}
/* The mark, drawn in CSS. An image element would be a subresource, and the CSP
   forbids every one of those — including a same-origin file, since
   `default-src 'none'` leaves no img-src to fall back to.
   (Written without the tag name in angle brackets on purpose: this stylesheet
   ships inside the page, and the self-containment test greps the rendered HTML
   for markup it must not contain. A comment naming the tag would trip it.) */
.masthead .wordmark::before{content:"";width:2.25rem;height:2.25rem;flex:none;
border-radius:var(--radius);box-shadow:var(--shadow);
background:linear-gradient(140deg,var(--link),color-mix(in srgb,var(--link) 55%,#000))}
.controls{display:flex;gap:var(--s2);align-items:center;flex-wrap:wrap}
/* The footer, matching the reference: a copyright line and a dot-separated
   set of links, stacking centred on a narrow screen. */
.footer{margin-top:auto;border-block-start:1px solid var(--line);
border-block-end:0;background:transparent}
.footer .bar-inner{display:flex;justify-content:space-between;align-items:center;
gap:var(--s4);padding-block:var(--s5);font-size:.85rem;color:var(--dim)}
.footer p{margin:0;opacity:.75}
.footer nav{display:flex;align-items:center;gap:var(--s4)}
.footer a{color:var(--dim);text-decoration:none;transition:color .12s}
.footer a:hover{color:var(--fg)}
.footer .sep{opacity:.3}
@media(max-width:520px){
.footer .bar-inner{flex-direction:column;align-items:center;text-align:center}}
/* **Submits on change.** Requiring a second click on "Apply" to change language
   is a step nobody expects, and this surface permits script for exactly this
   kind of thing. The button stays in the markup for readers without script and
   is hidden from those with it, so neither audience sees a dead control. */
.lang{position:relative;display:inline-flex;align-items:center;margin:0}
.lang select{font:inherit;font-size:.8rem;width:auto;
padding:.3rem 1.6rem .3rem var(--s3);
border:1px solid var(--line);border-radius:999px;
background:var(--surface);color:var(--dim);height:1.95rem;
appearance:none;cursor:pointer;transition:color .12s,border-color .12s}
.lang select:hover{color:var(--fg);border-color:var(--line-2)}
/* The chevron, drawn in CSS rather than fetched — `appearance:none` removes the
   native one and a background image would be a subresource the CSP forbids. */
.lang::after{content:"";position:absolute;inset-inline-end:.6rem;
width:.4rem;height:.4rem;pointer-events:none;
border-inline-end:1.6px solid currentColor;border-block-end:1.6px solid currentColor;
transform:translateY(-15%) rotate(45deg);color:var(--dim)}
.lang button{font-size:.78rem;width:auto;margin:0;margin-inline-start:var(--s1);
padding:.3rem var(--s3);height:1.95rem;border-radius:999px}
/* Serif headings against a sans body, which is the pairing that makes the
   reference design read as designed rather than as a default stylesheet. */
h1,h2{font-family:Georgia,"Times New Roman",serif;font-weight:600}
h1{font-size:clamp(1.6rem,1.35rem + 1vw,2.1rem);margin:0 0 var(--s4);
letter-spacing:-.02em;line-height:1.2}
h2{font-size:1.2rem;margin:var(--s6) 0 var(--s3);letter-spacing:-.01em}
/* The paragraph directly after an h1 is the subtitle. */
h1+p{color:var(--dim);font-size:1.02rem;max-width:38rem;margin-block:0 var(--s5)}
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

    // Inline SVG rather than an emoji or an icon font. An emoji renders as a
    // colour picture on some platforms and a black glyph on others, so it cannot
    // be made to follow the text colour; an icon font is a subresource the CSP
    // forbids. These are three paths that inherit `currentColor`.
    const ICON_AUTO: &str = "<svg viewBox=\"0 0 24 24\" aria-hidden=\"true\">\
<rect x=\"2\" y=\"4\" width=\"20\" height=\"13\" rx=\"2\"/><path d=\"M8 21h8M12 17v4\"/></svg>";
    const ICON_LIGHT: &str = "<svg viewBox=\"0 0 24 24\" aria-hidden=\"true\">\
<circle cx=\"12\" cy=\"12\" r=\"4.2\"/>\
<path d=\"M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2\
M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4\"/></svg>";
    const ICON_DARK: &str = "<svg viewBox=\"0 0 24 24\" aria-hidden=\"true\">\
<path d=\"M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z\"/></svg>";

    format!(
        "<input type=\"radio\" name=\"theme\" id=\"theme-os\" class=\"theme-in\" checked>\
<input type=\"radio\" name=\"theme\" id=\"theme-light\" class=\"theme-in\">\
<input type=\"radio\" name=\"theme\" id=\"theme-dark\" class=\"theme-in\">\
<header class=\"bar\"><div class=\"bar-inner masthead\">\
<a class=\"wordmark\" href=\"/public\">{brand}</a>\
<div class=\"controls\">\
<div class=\"theme\" role=\"group\" aria-label=\"{theme_label}\">\
<label for=\"theme-os\" title=\"{auto}\">{ICON_AUTO}<span class=\"theme-in\">{auto}</span></label>\
<label for=\"theme-light\" title=\"{light}\">{ICON_LIGHT}<span class=\"theme-in\">{light}</span></label>\
<label for=\"theme-dark\" title=\"{dark}\">{ICON_DARK}<span class=\"theme-in\">{dark}</span></label>\
</div>\
<form class=\"lang\" method=\"post\" action=\"/public/language\" id=\"langform\">\
<label for=\"lang\" class=\"theme-in\">{lang_label}</label>\
<select id=\"lang\" name=\"lang\">{options}</select>\
<button type=\"submit\">{apply}</button></form>\
</div></div></header>",
        brand = esc(s.brand),
        theme_label = esc(s.theme_label),
        auto = esc(s.theme_auto),
        light = esc(s.theme_light),
        dark = esc(s.theme_dark),
        lang_label = esc(s.language_label),
        apply = esc(s.language_apply),
    )
}

/// The footer, matching the reference design's shape.
///
/// The links are **relative and internal only**. A footer is where an "about"
/// or "privacy" link to somebody else's site would naturally go, and a remote
/// href on this surface would be a subresource the CSP forbids as well as a
/// referrer leak from a page whose URL identifies a request.
fn footer(locale: Locale) -> String {
    let s = locale.strings();
    format!(
        "<footer class=\"bar footer\"><div class=\"bar-inner\">\
<p>{tagline}</p>\
<nav><a href=\"/public\">{file}</a>\
<span class=\"sep\" aria-hidden=\"true\">·</span>\
<a href=\"/public/signout\">{signout}</a></nav>\
</div></footer>",
        tagline = esc(s.footer_tagline),
        file = esc(s.file_title),
        signout = esc(s.file_signout),
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
{masthead}<main>{body}</main>{footer}\
<script src=\"/public/app.js\" defer></script></body></html>",
        lang = esc(locale.code()),
        title = esc(title),
        masthead = masthead(locale),
        footer = footer(locale),
    )
}

/// The public surface's script. **Progressive enhancement only.**
///
/// Served as a file rather than inlined, because the public policy is
/// `script-src 'self'` and deliberately not `'unsafe-inline'` — an inline
/// allowance is also what a successful injection needs, on the surface that
/// renders model-authored text.
///
/// Everything here has a working no-script path: the language form keeps its
/// submit button, and this only hides it once the `change` handler is attached.
/// A reader with script blocked sees the button and it works; a reader with
/// script sees the language change on selection, which is what everyone expects
/// a language picker to do.
pub(crate) const PUBLIC_SCRIPT: &str = r#"(function () {
  var form = document.getElementById('langform');
  if (!form) return;
  var select = form.querySelector('select');
  var button = form.querySelector('button');
  if (!select || !button) return;
  // Hidden only now, so the button is never missing while it is the only way
  // to submit — the failure a `display:none` in the stylesheet would cause for
  // anyone whose script did not load.
  button.hidden = true;
  select.addEventListener('change', function () { form.submit(); });
})();
"#;

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
    fn the_only_script_on_a_public_page_is_a_served_file() {
        // The policy is `script-src 'self'`, never `'unsafe-inline'`. An inline
        // block would be refused by the browser *and* is what a successful
        // injection needs, so a renderer growing one is worth catching here
        // rather than as a silently dead feature in production.
        for (name, html) in corpus::public() {
            for tag in html.split("<script").skip(1) {
                let attrs = tag.split('>').next().unwrap_or("");
                assert!(
                    attrs.contains("src=\"/"),
                    "{name}: a script with no same-origin src: {attrs:?}"
                );
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
