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
    accounts_page, confirm_accept, detail, enrol_page, enrol_page_with_error, enrolled_page, filed,
    index, message, not_found, owners_page, repos_page, Who,
};
pub use public::{
    github_start_page, landing_page, owner_detail, owner_page, public_detail, public_file_page,
    public_filed, public_message, public_not_found, signin_confirm_page, signin_failed_page,
    signin_page, signin_page_in, signin_sent_page,
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
/* The real faces, served from this origin. Memosy loads these from Google;
   here they are compiled into the binary and served from `/public/`, so the page
   looks the same while `default-src 'none'` still forbids every remote fetch —
   `font-src 'self'` permits exactly these two and nothing else.
   `display:swap` so text is readable in the fallback face while they load,
   rather than invisible: on the bad connection this surface is designed for,
   the alternative is a blank page holding real content. */
@font-face{font-family:"DM Sans";src:url(/public/dm-sans.woff2)format("woff2");
font-weight:100 1000;font-style:normal;font-display:swap}
@font-face{font-family:"Fraunces";src:url(/public/fraunces.woff2)format("woff2");
font-weight:100 1000;font-style:normal;font-display:swap}
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
/* Following the OS is the default: with the box unticked, these two blocks are
   the only thing deciding, and they read `prefers-color-scheme` directly.
   Ticking `#theme-invert` means "the other one", so a dark OS goes light and a
   light OS goes dark. One box, two rules, and no state it cannot leave. */
@media(prefers-color-scheme:dark){
:root:not(:has(#theme-invert:checked)){
--bg:#141218;--surface:#1f1c24;--surface-2:#2a2633;
--fg:#f2eef8;--dim:#9d94ab;--faint:#7a7288;
--line:#3a3445;--line-2:#4a4356;
--link:#e85d6c;--accent-ink:#fff;
--shadow:0 1px 2px rgba(0,0,0,.4),0 4px 14px rgba(0,0,0,.3);
color-scheme:dark}}
/* A light OS with the box ticked: dark. */
@media(prefers-color-scheme:light){
:root:has(#theme-invert:checked){
--bg:#141218;--surface:#1f1c24;--surface-2:#2a2633;
--fg:#f2eef8;--dim:#9d94ab;--faint:#7a7288;
--line:#3a3445;--line-2:#4a4356;
--link:#e85d6c;--accent-ink:#fff;
--shadow:0 1px 2px rgba(0,0,0,.4),0 4px 14px rgba(0,0,0,.3);
color-scheme:dark}}
*{box-sizing:border-box}
/* **A local stack, not a web font.** Memosy loads DM Sans and Fraunces from
   Google. This surface is served with `default-src 'none'`, which forbids the
   fetch outright — and that ban is what stops a model-authored spec leaking the
   page URL through a remote reference, so it is not up for trade against
   typography. Georgia is Memosy's own declared fallback for Fraunces, making
   this the degradation that design already anticipated rather than a
   substitution invented here. */
/* The coral glow behind the top of the page, from the reference. A gradient
   rather than an image, so it costs no subresource — which the CSP forbids. */
body{margin:0;min-height:100vh;display:flex;flex-direction:column;
background:radial-gradient(ellipse 120% 80% at 50% -20%,
color-mix(in srgb,var(--link) 18%,transparent),transparent 55%),var(--bg);
color:var(--fg);
font:16px/1.6 "DM Sans",system-ui,-apple-system,"Segoe UI",Helvetica,Arial,sans-serif;
-webkit-font-smoothing:antialiased}
/* The header and footer rules span the viewport; their contents share the
   column width with `main`, so the lines run edge to edge while the text stays
   readable — the arrangement in the reference design. */
.bar{border-block-end:1px solid var(--line);background:var(--surface)}
/* **The bars are 960px and the text column is 720px** — the reference's own
   two widths, and the thing I had wrong. Making them equal put the header's
   controls directly above body text that then stopped short of them, which is
   what read as "nothing lines up". A narrower measure is also simply easier to
   read; 60rem of prose is too wide a line.
   Padding is 2rem here against the reference's px-8, so the two columns still
   share a left edge on a wide screen. */
.bar-inner{max-width:60rem;margin-inline:auto;width:100%;padding-inline:2rem}
main{flex:1;max-width:45rem;margin-inline:auto;width:100%;
padding:3rem 2rem;display:block}
@media(max-width:520px){.bar-inner{padding-inline:var(--s4)}
main{padding-inline:var(--s4)}}
/* The radios are off-screen rather than `display:none`, which would take them
   out of the tab order and hide them from a screen reader. */
.theme-in{position:absolute;width:1px;height:1px;margin:-1px;padding:0;
overflow:hidden;clip-path:inset(50%);white-space:nowrap}
/* A sliding pill: a 44x24 track with a knob that moves and carries one glyph.
   Copied from Memosy's ThemeToggle, down to the measurements — 1.15rem knob,
   2px inset, 200ms — because "roughly like theirs" is what produced the last
   version, and it did not look like theirs.
   Two states rather than three. Theirs has two, and the third (follow the OS)
   is what the page does before anything is clicked, so nothing is lost that a
   reader can reach. */
/* `inline-flex`, not `inline-block`. An inline-block sits on the **text
   baseline**, so it hangs below the buttons beside it by the depth of a
   descender — which reads as "not aligned" and is not fixed by centring the
   knob inside it. A flex item aligns to the row's centre line instead. */
/* The reference's 44x24. The neighbouring controls are 1.95rem tall, and two
   boxes of different heights in a centred row read as misaligned even when both
   are centred — so the pill gets the same box height and holds its 24px track
   inside it. That is what "align it" actually meant; centring the knob within a
   short box could never fix it. */
/* The reference's 44x24 track, and **the box IS the track** — an earlier
   attempt made the box 1.95rem to match its neighbours and drew the track as a
   `::before`, which put the knob's `top:50%` and its `calc(100% - ...)` on the
   box while the visible track was somewhere else. The knob ended up off-centre
   and hanging past the right edge.
   Matching the neighbours' height is done by `align-self:center` in the flex
   row instead, which is what centres a shorter item without lying about its
   size. */
/* Transcribed from the reference's ThemeToggle, class for class:
     w-11 h-6 rounded-full border border-border bg-secondary p-0 flex-shrink-0
   which is 44x24 under `border-box` — Tailwind's reset, and this file's global
   default, so the border sits inside the 24px exactly as it does there. */
.theme{position:relative;display:block;flex:none;align-self:center;
width:2.75rem;height:1.5rem;padding:0;cursor:pointer;
border-radius:var(--pill);border:1px solid var(--line);
background:var(--surface-2);transition:border-color .12s}
.theme:hover{border-color:var(--dim)}
/* The knob, also transcribed rather than derived:
     absolute top-0.5 w-[1.15rem] h-[1.15rem] rounded-full bg-primary
     transition-all duration-200 flex items-center justify-center
     text-[0.7rem] leading-none
   **`top:2px`, not `top:50%` with a transform.** Every attempt to centre this
   arithmetically landed a pixel out, because the offset is measured from the
   border box while a 50% translate centres on the padding box. The reference
   just says 2px from the top, and 2px from the top is correct. */
.theme .knob{position:absolute;top:2px;
inset-inline-start:2px;
width:1.15rem;height:1.15rem;border-radius:var(--pill);
background:var(--link);color:var(--accent-ink);
display:flex;align-items:center;justify-content:center;
font-size:.7rem;line-height:1;transition:inset-inline-start .2s}
/* **One checkbox, meaning "the opposite of what the OS says".**
   Two checkboxes could both end up ticked, and then no press could get back:
   ticking `light` and then `dark` left both set, an explicit-light rule kept
   winning, and dark became unreachable without script to untick the other. A
   single box cannot reach a state it cannot leave.
   So there are two pills — one for each OS setting — and exactly one is in the
   document flow at a time. Under a dark OS the sun pill shows and ticking the
   box means light; under a light OS the moon pill shows and ticking means dark. */
.to-light{display:none}
@media(prefers-color-scheme:dark){
.to-light{display:inline-flex}
.to-dark{display:none}}
/* Whichever pill is showing, ticking the box slides its knob across and swaps
   the glyph — so the control always animates towards the state it just entered. */
.to-light .moon,.to-dark .sun{display:none}
:root:has(#theme-invert:checked) .to-light .knob,
:root:not(:has(#theme-invert:checked)) .to-dark .knob{inset-inline-start:2px}
:root:not(:has(#theme-invert:checked)) .to-light .knob,
:root:has(#theme-invert:checked) .to-dark .knob{
inset-inline-start:calc(100% - 1.15rem - 2px)}
.theme-in:focus-visible~.masthead .theme{outline:2px solid var(--link);outline-offset:2px}
.masthead{display:flex;justify-content:space-between;align-items:center;
gap:var(--s4);flex-wrap:wrap;padding-block:var(--s4)}
.masthead .wordmark{font-family:"Fraunces",Georgia,"Times New Roman",serif;
font-weight:600;font-size:1.25rem;letter-spacing:-.02em;
text-decoration:none;color:var(--fg);display:inline-flex;align-items:center;
gap:.65rem}
/* The mark, drawn in CSS. An image element would be a subresource, and the CSP
   forbids every one of those — including a same-origin file, since
   `default-src 'none'` leaves no img-src to fall back to.
   (Written without the tag name in angle brackets on purpose: this stylesheet
   ships inside the page, and the self-containment test greps the rendered HTML
   for markup it must not contain. A comment naming the tag would trip it.) */
.masthead .logo{width:2.25rem;height:2.25rem;flex:none;
border-radius:var(--radius);box-shadow:var(--shadow);
background:linear-gradient(140deg,var(--link),color-mix(in srgb,var(--link) 55%,#000));
color:var(--accent-ink);display:inline-flex;align-items:center;justify-content:center;
font-family:"Fraunces",Georgia,serif;font-size:.95rem;font-weight:600;letter-spacing:.02em}
/* The nav row, as the reference has it: `flex items-center gap-2` and nothing
   else. **No `flex-wrap`** — wrapping is what put the sign-in button on its own
   line as soon as the row got tight, and a nav that reflows into two rows is
   not the design. The items are small and `white-space:nowrap`, so the row
   fits; the masthead above still wraps, which is where a narrow screen should
   break.
   The dialog is excluded because a closed `<dialog>` is still a child, and as a
   flex item it would take a slot in this row and push the buttons along. */
/* `flex items-center gap-2` from the reference's PublicHeader nav — 8px.
   **The margin reset is the whole fix.** The form rules below give every
   `label` a 16px top margin and every `button` a 16px one, which is right in a
   form and wrong in a nav — and the pill IS a label and Sign in IS a button, so
   each was being pushed down by 16px independently. That is what four rounds of
   adjusting heights, box-sizing and baselines never touched: the boxes were the
   right size all along and the margins were moving them.
   Measured, not guessed: `scripts/layout-check.js` reported the pill at y=34,
   the select at y=24 and the button at y=32, with computed margins of
   `16px 0 4px` and `16px 0 0`. */
.controls{display:flex;align-items:center;gap:8px}
.controls label,.controls button,.controls select,.controls a{margin:0}
.controls>dialog{position:fixed}
/* The footer, matching the reference: a copyright line and a dot-separated
   set of links, stacking centred on a narrow screen. */
.footer{margin-top:auto;border-block-start:1px solid var(--line);
border-block-end:0;background:transparent}
/* One line. `space-between` and the nav rules went with the link list — with a
   single child, `space-between` is just `flex-start` with extra words. */
.footer .bar-inner{padding-block:var(--s5);font-size:.85rem;color:var(--dim)}
.footer p{margin:0;opacity:.75}
@media(max-width:520px){.footer .bar-inner{text-align:center}}
/* **Submits on change.** Requiring a second click on "Apply" to change language
   is a step nobody expects, and this surface permits script for exactly this
   kind of thing. The button stays in the markup for readers without script and
   is hidden from those with it, so neither audience sees a dead control. */
.lang{position:relative;display:inline-flex;align-items:center;margin:0}
/* The same 32px box as the buttons it sits between. */
.lang select{font:inherit;font-size:.8rem;width:auto;
padding:0 1.6rem 0 10px;height:32px;
border:1px solid var(--line);border-radius:var(--pill);
background:var(--surface);color:var(--dim);
appearance:none;cursor:pointer;transition:color .12s,border-color .12s}
.lang select:hover{color:var(--fg);border-color:var(--line-2)}
/* The chevron, drawn in CSS rather than fetched — `appearance:none` removes the
   native one and a background image would be a subresource the CSP forbids. */
.lang::after{content:"";position:absolute;inset-inline-end:.6rem;
width:.4rem;height:.4rem;pointer-events:none;
border-inline-end:1.6px solid currentColor;border-block-end:1.6px solid currentColor;
transform:translateY(-15%) rotate(45deg);color:var(--dim)}
/* The submit button exists only inside `<noscript>`, so with script it is not
   in the document at all — no flash before a handler attaches, and nothing to
   hide. Without script it is the only way to change language, and it works. */
.lang button{font-size:.78rem;width:auto;margin:0;margin-inline-start:6px;
padding:0 10px;height:32px;border-radius:var(--pill)}
/* The masthead's buttons and the landing page's call to action. */
/* The reference's `size="sm"`: h-8 (32px), px-2.5 (10px). Transcribed rather
   than eyeballed — the heights I had picked were within a pixel of these and
   still visibly wrong beside a 24px pill. */
.btn{display:inline-flex;align-items:center;justify-content:center;gap:6px;
height:32px;padding:0 10px;border-radius:var(--pill);
border:1px solid var(--line);background:var(--surface);color:var(--fg);
font-size:.82rem;font-weight:600;text-decoration:none;cursor:pointer;
white-space:nowrap;transition:border-color .12s,background .12s}
.btn:hover{border-color:var(--line-2);background:var(--surface-2)}
/* The account menu: `<details>`, so it opens with no script at all. */
.acct{position:relative}
.acct summary{list-style:none}
.acct summary::-webkit-details-marker{display:none}
.acct summary::after{content:"";width:.35rem;height:.35rem;
border-inline-end:1.5px solid currentColor;border-block-end:1.5px solid currentColor;
transform:translateY(-25%) rotate(45deg)}
.acct[open] summary{border-color:var(--line-2);background:var(--surface-2)}
.acct .menu{position:absolute;inset-inline-end:0;top:calc(100% + var(--s1));
min-width:11rem;padding:var(--s1);z-index:20;
background:var(--surface);border:1px solid var(--line);
border-radius:var(--radius);box-shadow:var(--shadow)}
.acct .menu a,.acct .menu button{display:block;width:100%;text-align:start;
padding:var(--s2) var(--s3);border:0;border-radius:.4rem;margin:0;
background:transparent;color:var(--fg);font:inherit;font-size:.85rem;
font-weight:500;text-decoration:none;cursor:pointer}
.acct .menu a:hover,.acct .menu button:hover{background:var(--surface-2)}
/* The landing page's three points. */
.point{margin-block:3rem}
.point h2{margin:0 0 var(--s2);border:0;padding:0;font-size:1.25rem}
.point p{margin:0;color:var(--dim);font-size:.95rem;line-height:1.65}
/* The sign-in dialog. Measurements from the reference: 380px, 1.75rem padding,
   a 55% black backdrop with a 4px blur.
   **A real `<dialog>`**, not a div positioned over the page. The element brings
   focus trapping, Escape-to-close, inertness of the content behind it and a
   `::backdrop` — every one of which a hand-rolled overlay has to reimplement,
   and the accessibility half of that list is what such overlays usually skip. */
dialog{max-width:380px;width:calc(100% - 2rem);padding:1.75rem;
border:1px solid var(--line);border-radius:var(--radius);
background:var(--surface);color:var(--fg);box-shadow:var(--shadow)}
dialog::backdrop{background:rgba(0,0,0,.55);backdrop-filter:blur(4px)}
dialog h2{margin:0 0 var(--s2);border:0;padding:0;font-size:1.25rem}
dialog p{color:var(--dim);font-size:.9rem}
dialog .close{position:absolute;inset-block-start:var(--s3);
inset-inline-end:var(--s3);width:1.75rem;height:1.75rem;padding:0;margin:0;
display:inline-flex;align-items:center;justify-content:center;
border:0;border-radius:var(--pill);background:transparent;color:var(--dim);
font-size:1.1rem;line-height:1;cursor:pointer}
dialog .close:hover{background:var(--surface-2);color:var(--fg)}
/* Without script the dialog cannot be opened, so the page must not pretend it
   can: the trigger is a plain link to the sign-in page, and this only turns it
   into a dialog opener once the script is there to do it.
   Its **own** class rather than the document-wide `.js`, because the fallback
   is also needed on a browser that has script but no `showModal` — and undoing
   `.js` to express that would drag the language form's button back with it. */
.modal{display:none}
.has-dialog .modal{display:inline-flex}
.has-dialog .modal-fallback{display:none}
/* Serif headings against a sans body, which is the pairing that makes the
   reference design read as designed rather than as a default stylesheet. */
h1,h2{font-family:"Fraunces",Georgia,"Times New Roman",serif;font-weight:600}
h1{font-size:clamp(1.75rem,4vw,2.35rem);margin:0 0 var(--s4);
letter-spacing:-.02em;line-height:1.2}
h2{font-size:1.2rem;margin:var(--s6) 0 var(--s3);letter-spacing:-.01em}
/* The paragraph directly after an h1 is the subtitle. */
/* No `max-width` on the prose. A 38rem cap inside a 60rem column left every
   paragraph stopping two-thirds of the way across while the header ran the full
   width — the mismatch that reads as "nothing lines up". Measure is controlled
   by the column, in one place. */
h1+p{color:var(--dim);font-size:1rem;line-height:1.65;margin-block:0 3rem}
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
fn masthead(locale: Locale, signed: Signed) -> String {
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

    // **Two checkboxes, not three radios.** The OS default needs no control of
    // its own: with neither box ticked the `prefers-color-scheme` rules apply,
    // which *is* following the system. Ticking one overrides it in that
    // direction, and the pair is what makes the override reversible — a lone
    // checkbox cannot express "the other one", and a radio cannot be un-ticked.
    //
    // Only one is ever reachable at a time: the label sits over whichever
    // direction the page is not currently showing, so the reader always presses
    // the same pill and it always means "switch".
    format!(
        "<input type=\"checkbox\" id=\"theme-invert\" class=\"theme-in\">\
<header class=\"bar\"><div class=\"bar-inner masthead\">\
<a class=\"wordmark\" href=\"/\"><span class=\"logo\" aria-hidden=\"true\">SC</span><span>{brand}</span></a>\
<div class=\"controls\">\
<label class=\"theme to-dark\" for=\"theme-invert\" title=\"{dark}\">\
<span class=\"theme-in\">{dark}</span>\
<span class=\"knob\" aria-hidden=\"true\"><span class=\"sun\">\u{2600}</span>\
<span class=\"moon\">\u{263e}</span></span></label>\
<label class=\"theme to-light\" for=\"theme-invert\" title=\"{light}\">\
<span class=\"theme-in\">{light}</span>\
<span class=\"knob\" aria-hidden=\"true\"><span class=\"sun\">\u{2600}</span>\
<span class=\"moon\">\u{263e}</span></span></label>\
<form class=\"lang\" method=\"post\" action=\"/public/language\" id=\"langform\">\
<label for=\"lang\" class=\"theme-in\">{lang_label}</label>\
<select id=\"lang\" name=\"lang\">{options}</select>\
<noscript><button type=\"submit\">{apply}</button></noscript></form>\
{account}\
</div></div></header>",
        brand = esc(&site::name(s.brand)),
        account = account_nav(locale, signed == Signed::In),
        light = esc(s.theme_light),
        dark = esc(s.theme_dark),
        lang_label = esc(s.language_label),
        apply = esc(s.language_apply),
    )
}

/// The footer.
///
/// The links are **relative and internal only**. A footer is where an "about"
/// or "privacy" link to somebody else's site would naturally go, and a remote
/// href on this surface would be a subresource the CSP forbids as well as a
/// referrer leak from a page whose URL identifies a request.
///
/// **Sign out is not here.** It moved to the account menu, which is where a
/// reader looks for it — and a destructive action sitting in the footer next to
/// navigation links is one somebody eventually presses by accident.
fn footer(locale: Locale) -> String {
    let s = locale.strings();
    // **One line, no link list.** The right-hand nav duplicated the masthead:
    // the wordmark already goes home and Sign in already leads to filing, so it
    // was a second copy of the same two destinations at the bottom of a page
    // short enough that nobody scrolls to find them.
    format!(
        "<footer class=\"bar footer\"><div class=\"bar-inner\">\
<p>{name}{tagline}</p></div></footer>",
        name = esc(&site::name(s.brand)),
        tagline = esc(s.footer_tagline),
    )
}

/// The masthead's account control.
///
/// Signed out it is one button, and the wording is deliberate: this surface has
/// no separate "register", because a first sign-in *is* the signup. Offering
/// both would promise a difference that does not exist — and the two paths must
/// stay indistinguishable anyway, since that is what stops the page revealing
/// whether an address already has an account.
///
/// Signed in it is a details/summary menu holding sign-out. No script: a menu
/// that needs JavaScript to open is a menu that does not open for a reader whose
/// script failed, and `<details>` is the same interaction with none.
fn account_nav(locale: Locale, signed_in: bool) -> String {
    let s = locale.strings();
    if !signed_in {
        // **Two triggers, one of which is always the wrong one — and the CSS
        // picks.** Without script the dialog cannot open, so the page must not
        // offer a button that does nothing: `.modal-fallback` is an ordinary
        // link to the sign-in page, and `.modal` is the dialog opener, with only
        // the second shown once the script has marked the document.
        //
        // The dialog holds the *same form* posting to the *same route*, so the
        // two paths differ in presentation and nothing else.
        return format!(
            "<a class=\"btn modal-fallback\" href=\"/public/signin\">{signin}</a>\
<button class=\"btn modal\" id=\"signin-open\" type=\"button\">{signin}</button>\
<dialog id=\"signin-dialog\">\
<button class=\"close\" id=\"signin-close\" type=\"button\" \
aria-label=\"{close}\">\u{00d7}</button>\
<h2>{title}</h2><p>{intro}</p>\
<form method=\"post\" action=\"/public/signin\">\
<label for=\"dlg-email\">{email}</label>\
<input id=\"dlg-email\" name=\"email\" type=\"email\" required \
autocapitalize=\"off\" autocorrect=\"off\" spellcheck=\"false\" \
placeholder=\"{placeholder}\">\
<button type=\"submit\">{submit}</button></form>\
<p class=\"meta\">{note}</p></dialog>",
            signin = esc(s.nav_signin),
            close = esc(s.dialog_close),
            title = esc(s.signin_title),
            intro = esc(s.signin_intro),
            email = esc(s.signin_email_label),
            placeholder = esc(s.signin_email_placeholder),
            submit = esc(s.signin_submit),
            note = esc(s.signin_no_password),
        );
    }
    format!(
        "<details class=\"acct\"><summary class=\"btn\">{account}</summary>\
<div class=\"menu\">\
<a href=\"/public\">{mine}</a>\
<form method=\"post\" action=\"/public/signout\">\
<button type=\"submit\">{signout}</button></form>\
</div></details>",
        account = esc(s.nav_account),
        mine = esc(s.file_mine_heading),
        signout = esc(s.file_signout),
    )
}

/// The document a **public** page is wrapped in.
///
/// Carries the `lang` attribute, without which a screen reader pronounces French
/// with English phonemes — the accessibility failure a language switcher exists
/// to fix rather than to cause.
pub(crate) fn public_shell(locale: Locale, title: &str, body: &str) -> String {
    public_shell_as(locale, title, body, Signed::Out)
}

/// What the masthead calls this site.
///
/// **The configured repository name**, set per request from `PublicConfig`
/// rather than compiled in: one binary serves whichever repository an operator
/// nominated, and a masthead reading "Smart Coder" on a surface collecting
/// requests for *their* project is the wrong name on every page.
///
/// A thread-local because the alternative is threading it through ten renderers
/// that have no other reason to know it. Set once per request in `routes`, and
/// falling back to the product name when nothing set it — which is what the
/// private surface and the render-to-file example both do.
pub(crate) mod site {
    use std::cell::RefCell;

    thread_local! {
        static NAME: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    /// Name this request's site. Cleared by [`clear`] when the request ends.
    pub fn set(name: &str) {
        NAME.with(|n| *n.borrow_mut() = Some(name.to_string()));
    }

    /// Forget it. **A thread serves many requests**, so a name left set would
    /// leak one repository's name onto the next request's page.
    pub fn clear() {
        NAME.with(|n| *n.borrow_mut() = None);
    }

    pub fn name(fallback: &str) -> String {
        NAME.with(|n| n.borrow().clone())
            .unwrap_or_else(|| fallback.to_string())
    }
}

/// Whether the masthead shows a sign-in button or an account menu.
///
/// An explicit two-variant enum rather than a `bool`, because `public_shell(l,
/// t, b, true)` at a call site says nothing about what is true — and the wrong
/// value here shows a stranger an account menu, or a signed-in filer a button
/// telling them to sign in again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Signed {
    In,
    Out,
}

/// The document, with the masthead told who is reading.
pub(crate) fn public_shell_as(locale: Locale, title: &str, body: &str, signed: Signed) -> String {
    shell_inner(locale, title, body, signed)
}

fn shell_inner(locale: Locale, title: &str, body: &str, signed: Signed) -> String {
    format!(
        "<!doctype html>\n<html lang=\"{lang}\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>{title}</title><style>{PUBLIC_STYLE}</style></head><body>\
{masthead}<main>{body}</main>{footer}\
<script src=\"/public/app.js\" defer></script></body></html>",
        lang = esc(locale.code()),
        title = esc(title),
        masthead = masthead(locale, signed),
        footer = footer(locale),
    )
}

/// The body face — DM Sans, variable weight, Latin subset.
///
/// Compiled in rather than read from disk, like the eval corpus: it travels with
/// the binary, so the container has no asset directory to mount and no file that
/// can go missing. **SIL Open Font License 1.1**, whose text ships beside it in
/// `assets/` — the licence requires the notice to travel with the font.
pub const FONT_BODY: &[u8] = include_bytes!("../../assets/dm-sans.woff2");

/// The display face — Fraunces, variable weight, Latin subset. Same licence.
pub const FONT_DISPLAY: &[u8] = include_bytes!("../../assets/fraunces.woff2");

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
  // Marks the document as scripted, which is how the stylesheet decides between
  // the sign-in *link* (works without script) and the sign-in *dialog* (needs
  // it). Set first, so the swap happens before anything below can fail.
  document.documentElement.classList.add('js');

  // The sign-in dialog. `showModal` is what makes it a real modal: focus is
  // trapped inside it, Escape closes it, and the page behind goes inert —
  // behaviour a div-with-an-overlay has to reimplement and usually does not.
  var open = document.getElementById('signin-open');
  var dialog = document.getElementById('signin-dialog');
  if (open && dialog && typeof dialog.showModal === 'function') {
    // Only now is the dialog trigger shown and the plain link hidden, so a
    // browser without `showModal` keeps a link that works.
    document.documentElement.classList.add('has-dialog');
    open.addEventListener('click', function () {
      dialog.showModal();
      var email = dialog.querySelector('input[type=email]');
      if (email) email.focus();
    });
    var close = document.getElementById('signin-close');
    if (close) close.addEventListener('click', function () { dialog.close(); });
    // Clicking the backdrop closes it. The dialog fills its own box, so a click
    // whose target IS the dialog element landed outside the content.
    dialog.addEventListener('click', function (e) {
      if (e.target === dialog) dialog.close();
    });
  }

  // The language switcher submits on change. The submit button lives inside
  // `<noscript>`, so there is nothing to hide here — the two paths are exclusive
  // by construction rather than by a class this has to remember to set.
  var form = document.getElementById('langform');
  if (form) {
    var select = form.querySelector('select');
    if (select) {
      select.addEventListener('change', function () { form.submit(); });
    }
  }

  // **The theme control needs no script at all.** It was two checkboxes, and
  // this reset the other one — which meant that without script, a third press
  // could reach a state it could not leave: light, then dark, left both ticked
  // and the light rule kept winning, so dark became unreachable. One checkbox
  // meaning "invert the OS" has no such state, so the script that papered over
  // it is gone rather than kept as a safety net for a bug that no longer exists.
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

/// The repository picker, when the surface serves more than one.
///
/// **Rendered from the configured set, never from anything the caller sent** —
/// which is what lets a public form carry a repository field at all. The server
/// checks the submitted name against the same set, so the field is a *choice
/// among* nominated repositories rather than a place to name one.
///
/// Returns nothing for a single-repository surface: a select with one option is
/// a question with one answer, and the filer has nothing to decide.
///
/// The option text is the **untranslated name**, for the same reason
/// [`kind_field_in`] gives: it is what the form submits and what the developer
/// reads on the review page, so translating it would have a filer and a reviewer
/// naming the same repository differently. Only the label is translated.
pub(crate) fn repo_field_in(repos: &crate::config::Repos, locale: Locale) -> String {
    if repos.is_single() {
        return String::new();
    }
    let mut opts = String::new();
    for name in repos.names() {
        opts.push_str(&format!(
            "<option value=\"{name}\">{name}</option>",
            name = esc(name)
        ));
    }
    format!(
        "<label for=\"repo\">{label}</label>\
         <select id=\"repo\" name=\"repo\">{opts}</select>",
        label = esc(locale.strings().file_repo_label),
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
            (
                "detail",
                detail(&reviewable(), &crate::page::Who::default()),
            ),
            ("confirm_accept", confirm_accept(&req(), "# Spec", "d")),
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
                ("landing_page", landing_page(l)),
                ("signin_page", signin_page()),
                ("signin_page_in", signin_page_in(l)),
                ("signin_sent_page", signin_sent_page(l)),
                ("signin_confirm_page", signin_confirm_page("abc123", l)),
                ("signin_failed_page", signin_failed_page(true, l)),
                ("signin_failed_page", signin_failed_page(false, l)),
                (
                    "github_start_page",
                    github_start_page("https://github.test/login/oauth/authorize?x=1", l),
                ),
                (
                    "owner_page",
                    owner_page(&[req()], "jamez667", &["alpha".to_string()], l),
                ),
                ("owner_detail", owner_detail(&reviewable(), l)),
                (
                    "public_file_page",
                    public_file_page(&[req()], &crate::config::Repos::new(&["alpha"]), true, l),
                ),
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
        //
        // **`github_start_page` is the one exception, and only for the URL.** Its
        // whole purpose is to send a reader to GitHub, and a link they click is
        // not a subresource: nothing is fetched while rendering, so there is no
        // silent request and nothing leaks unless they choose to go. The
        // subresource hazards below still apply to it, which is why the
        // exemption is one hazard rather than the whole page — a remote `<img>`
        // appearing on it would still fail here.
        for (name, html) in corpus::public() {
            let leaving_on_purpose = name == "github_start_page";
            for hazard in [
                "http://", "https://", "<img", "<link", "@import", "url(http",
            ] {
                let is_the_url =
                    leaving_on_purpose && (hazard == "https://" || hazard == "http://");
                if is_the_url {
                    continue;
                }
                assert!(!html.contains(hazard), "{name} contains {hazard}");
            }
        }
    }

    #[test]
    fn the_only_page_that_links_off_site_links_to_one_place() {
        // The exemption above is narrow, and this is what keeps it honest: the
        // page may carry a remote URL, and it must be the authorize link and
        // nothing else. A second remote reference appearing here — an icon, a
        // stylesheet — would show up as a count greater than one.
        let html = github_start_page("https://github.test/login/oauth/authorize?x=1", Locale::En);
        assert_eq!(
            html.matches("https://").count(),
            1,
            "exactly one off-site reference: {html}"
        );
        assert!(
            html.contains("<a class=\"cta\" href=\"https://github.test/"),
            "{html}"
        );
    }

    #[test]
    fn the_theme_toggle_is_one_control_that_reverses() {
        // **A toggle that only goes one way**, which is what shipped: two
        // checkboxes meant light-then-dark left both ticked, an explicit-light
        // rule kept winning, and dark was unreachable without script.
        //
        // The fix is structural — one checkbox meaning "invert the OS" has no
        // state it cannot leave — so this asserts the structure rather than the
        // symptom. Counting inputs is crude, but it is exactly the thing that
        // went wrong, and a second one cannot reappear unnoticed.
        let html = signin_page_in(Locale::En);
        assert_eq!(
            html.matches("type=\"checkbox\"").count(),
            1,
            "the theme control is one checkbox: {html}"
        );
        assert!(html.contains("id=\"theme-invert\""), "{html}");
        // Both pills drive that one box, so whichever is visible reverses it.
        assert_eq!(
            html.matches("for=\"theme-invert\"").count(),
            2,
            "both pills point at the single checkbox: {html}"
        );

        // And the stylesheet decides from the OS preference plus that one box,
        // never from a second one that could disagree with it.
        assert!(
            !PUBLIC_STYLE.contains("theme-light"),
            "a second box returned"
        );
        assert!(
            !PUBLIC_STYLE.contains("theme-dark"),
            "a second box returned"
        );
    }

    #[test]
    fn the_language_form_needs_no_button_when_there_is_script() {
        // The button lives inside `<noscript>`, so with script it is not in the
        // document at all — no flash before a handler attaches, and nothing for
        // a stylesheet rule to hide. Without script it is the only way to change
        // language, and it is present and working.
        let html = signin_page_in(Locale::En);
        let form = html
            .split("id=\"langform\"")
            .nth(1)
            .and_then(|s| s.split("</form>").next())
            .expect("the language form is rendered");
        assert!(form.contains("<noscript><button"), "{form}");
        assert_eq!(
            form.matches("<button").count(),
            1,
            "one button, and it is the noscript one: {form}"
        );
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
