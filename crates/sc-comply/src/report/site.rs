//! Static HTML export for publication (GitHub Pages).
//!
//! Every page is self-contained — inline CSS, no JavaScript, no external fetch —
//! so it works from `file://`, from Pages, and offline.
//!
//! # Safety
//!
//! [`framework_page`] takes an *already-redacted* pack and asserts it. That
//! assertion is not decoration: this renderer is the one place where a pack
//! becomes a world-readable artifact, so it refuses rather than trusts the
//! caller to have redacted. See [`crate::redact`].
//!
//! All interpolated text is HTML-escaped. Control intents and rationales are
//! pack-authored rather than user input, but a pack may be third-party, and an
//! escaping renderer costs nothing.

use std::fmt::Write as _;

use crate::evidence::EvidencePack;
use crate::status::ControlStatus;

/// Shared stylesheet. Printable, readable without JS, and identical in shape to
/// the one `sc-server` serves — the two surfaces are the same project and should
/// not look like strangers.
///
/// ## The theme toggle is CSS only
///
/// Three states — follow the OS, force light, force dark — driven by a hidden
/// radio group and `:has()`, with **no JavaScript at all**. That is not
/// asceticism: these pages are published to GitHub Pages and read from
/// `file://` and offline, and a script is one more thing that can fail to load
/// on the least reliable connection somebody reads them over.
///
/// It degrades honestly. Where `:has()` is unsupported the radios simply do
/// nothing and the page follows the OS — the same behaviour as before the
/// toggle existed. Nothing is hidden behind a technique, so nothing becomes
/// unreachable when the technique is missing.
///
/// ## Logical properties throughout
///
/// `margin-inline`, `border-inline-start` and friends rather than `left`/`right`,
/// so a right-to-left language costs a translation rather than a stylesheet
/// rewrite. Free to do now, expensive to retrofit.
const STYLE: &str = r#"
:root{
--bg:#fbfbfd;--surface:#fff;--surface-2:#f4f4f7;
--fg:#16181d;--dim:#5b6270;--faint:#878e9c;
--line:#e3e5ea;--line-2:#cdd1d9;
--link:#2f5fd8;--accent-ink:#fff;
--pass:#1a7f37;--gap:#cf222e;--unknown:#9a6700;--error:#8250df;--na:#59636e;
--shadow:0 1px 2px rgba(16,18,24,.06),0 4px 12px rgba(16,18,24,.05);
--s1:.25rem;--s2:.5rem;--s3:.75rem;--s4:1rem;--s5:1.5rem;--s6:2.5rem;
--radius:.75rem;
color-scheme:light}
/* Following the OS is the default, and stays the default when nothing is
   chosen. `:not(:has(...))` keeps an explicit light choice from being
   overridden by a dark OS. */
@media(prefers-color-scheme:dark){
:root:not(:has(#theme-light:checked)){
--bg:#0e1014;--surface:#161920;--surface-2:#1d212a;
--fg:#e8eaef;--dim:#a2aab8;--faint:#6f7789;
--line:#262b35;--line-2:#333a47;
--link:#7ea2ff;--accent-ink:#0e1014;
--pass:#3fb950;--gap:#f85149;--unknown:#d29922;--error:#d2a8ff;--na:#9198a1;
--shadow:0 1px 2px rgba(0,0,0,.4),0 4px 14px rgba(0,0,0,.3);
color-scheme:dark}}
/* An explicit choice wins over the OS, in both directions. */
:root:has(#theme-dark:checked){
--bg:#0e1014;--surface:#161920;--surface-2:#1d212a;
--fg:#e8eaef;--dim:#a2aab8;--faint:#6f7789;
--line:#262b35;--line-2:#333a47;
--link:#7ea2ff;--accent-ink:#0e1014;
--pass:#3fb950;--gap:#f85149;--unknown:#d29922;--error:#d2a8ff;--na:#9198a1;
--shadow:0 1px 2px rgba(0,0,0,.4),0 4px 14px rgba(0,0,0,.3);
color-scheme:dark}
*{box-sizing:border-box}
/* The radios themselves are off-screen rather than `display:none`, so they stay
   reachable by keyboard and to a screen reader. */
.theme-in{position:absolute;width:1px;height:1px;margin:-1px;padding:0;
overflow:hidden;clip-path:inset(50%);white-space:nowrap}
.theme{display:flex;gap:var(--s1);align-items:center}
.theme label{font-size:.78rem;color:var(--dim);cursor:pointer;
padding:var(--s1) var(--s2);border-radius:.4rem;border:1px solid transparent;
min-height:1.9rem;display:inline-flex;align-items:center;
transition:background .12s,border-color .12s,color .12s}
.theme label:hover{background:var(--surface-2);color:var(--fg)}
/* The radios are siblings of the masthead, and the labels live inside it — so
   the highlight reaches through a descendant step rather than a direct one. */
#theme-os:checked~.masthead label[for=theme-os],
#theme-light:checked~.masthead label[for=theme-light],
#theme-dark:checked~.masthead label[for=theme-dark]{
background:var(--surface-2);border-color:var(--line-2);color:var(--fg);font-weight:600}
.theme-in:focus-visible~.masthead label[for]{outline:2px solid var(--link);outline-offset:2px}
.masthead{display:flex;justify-content:space-between;align-items:center;gap:var(--s4);
flex-wrap:wrap;padding-bottom:var(--s4);margin-bottom:var(--s5);
border-bottom:1px solid var(--line)}
.masthead .wordmark{font-weight:700;font-size:1.05rem;letter-spacing:-.01em}
main{display:block}
body{margin:0 auto;padding:var(--s6) var(--s4) 4rem;max-width:62rem;
background:var(--bg);color:var(--fg);
font:16px/1.6 -apple-system,BlinkMacSystemFont,"Segoe UI",Helvetica,Arial,sans-serif;
-webkit-font-smoothing:antialiased}
h1{font-size:clamp(1.5rem,1.3rem + .9vw,2rem);margin:0 0 var(--s2);letter-spacing:-.02em}
h2{font-size:clamp(1.1rem,1rem + .35vw,1.3rem);margin:var(--s6) 0 var(--s3);
padding-bottom:var(--s2);border-bottom:1px solid var(--line);letter-spacing:-.01em}
h3{font-size:1rem;margin:var(--s5) 0 var(--s2)}
a{color:var(--link);text-underline-offset:2px}
a:hover{text-decoration-thickness:2px}
:focus-visible{outline:2px solid var(--link);outline-offset:2px;border-radius:2px}
code{font:13px/1.5 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;
background:var(--surface-2);padding:.1em .35em;border-radius:4px}
.sub{color:var(--dim);margin:0 0 var(--s5);font-size:1.02rem;max-width:46rem}
.note{background:var(--surface);border:1px solid var(--line);
border-inline-start:3px solid var(--unknown);
border-radius:var(--radius);padding:var(--s3) var(--s4);margin:var(--s5) 0;
box-shadow:var(--shadow)}
.note strong{color:var(--unknown)}
.scope{white-space:pre-wrap;color:var(--dim);font-size:.92rem;overflow-wrap:anywhere}
/* Wide content scrolls inside its own box; the page body never scrolls
   sideways, which on a phone is the difference between readable and not. */
.tablewrap{overflow-x:auto;margin:var(--s4) 0;border:1px solid var(--line);
border-radius:var(--radius);background:var(--surface);box-shadow:var(--shadow)}
table{border-collapse:collapse;width:100%;font-size:.93rem}
th,td{text-align:start;padding:var(--s3);border-bottom:1px solid var(--line);
vertical-align:top}
tr:last-child td{border-bottom:0}
th{color:var(--dim);font-weight:600;font-size:.78rem;text-transform:uppercase;
letter-spacing:.05em;background:var(--surface-2)}
tr:hover td{background:var(--surface-2)}
.pill{display:inline-block;padding:.15em .6em;border-radius:10px;font-size:.8rem;
font-weight:600;max-width:100%;overflow-wrap:anywhere}
.pass{color:var(--pass);background:color-mix(in srgb,var(--pass) 12%,transparent)}
.gap{color:var(--gap);background:color-mix(in srgb,var(--gap) 12%,transparent)}
.unknown{color:var(--unknown);background:color-mix(in srgb,var(--unknown) 12%,transparent)}
.error{color:var(--error);background:color-mix(in srgb,var(--error) 12%,transparent)}
.na{color:var(--na);background:color-mix(in srgb,var(--na) 12%,transparent)}
.counts{display:grid;grid-template-columns:repeat(auto-fit,minmax(7rem,1fr));
gap:var(--s3);margin:var(--s4) 0}
.count{background:var(--surface);border:1px solid var(--line);
border-radius:var(--radius);padding:var(--s3) var(--s4);box-shadow:var(--shadow)}
.count b{display:block;font-size:1.6rem;line-height:1.15;letter-spacing:-.02em}
.count span{color:var(--dim);font-size:.75rem;text-transform:uppercase;letter-spacing:.05em}
.ratios{display:flex;gap:var(--s6);flex-wrap:wrap;margin:var(--s3) 0 0}
.bar{height:6px;background:var(--line);border-radius:3px;width:9rem;overflow:hidden;
margin-top:var(--s1)}
.bar>div{height:100%;background:var(--link)}
.ctl{border:1px solid var(--line);border-radius:var(--radius);padding:var(--s4);
margin:var(--s3) 0;background:var(--surface);
transition:border-color .12s,box-shadow .12s}
.ctl:hover{border-color:var(--line-2);box-shadow:var(--shadow)}
.ctl .hd{display:flex;gap:var(--s2);align-items:baseline;flex-wrap:wrap}
.ctl .id{font-weight:700;overflow-wrap:anywhere}
.ctl .why{color:var(--dim);font-size:.9rem;margin:var(--s2) 0 0}
.ctl .intent{font-size:.93rem;margin:var(--s2) 0 0}
.ctl .rem{font-size:.9rem;margin:var(--s2) 0 0;padding:var(--s2) var(--s3);
background:var(--surface-2);border-radius:.5rem}
.exec{font-size:1.02rem;margin:var(--s5) 0;max-width:46rem}
.exec p{margin:0 0 var(--s3)}
.guide{margin:var(--s3) 0 0;padding:var(--s3);background:var(--surface-2);
border-radius:.5rem;border-inline-start:3px solid var(--link);font-size:.92rem}
.guide strong{color:var(--link)}
.guide ul{margin:var(--s1) 0;padding-inline-start:1.2rem}
.guide p{margin:var(--s1) 0 0}
footer{margin-top:var(--s6);padding-top:var(--s4);border-top:1px solid var(--line);
color:var(--dim);font-size:.85rem}
@media(prefers-reduced-motion:reduce){
*,*::before,*::after{animation-duration:.01ms !important;transition-duration:.01ms !important}}
/* The toggle is a screen affordance; on paper the OS theme is meaningless and
   the ink should be black. */
@media print{
:root{--bg:#fff;--surface:#fff;--surface-2:#fff;--fg:#000;--dim:#333;
--line:#bbb;--shadow:none}
.theme,.theme-in{display:none}
.ctl,.count,.note,.tablewrap{box-shadow:none}}
"#;

/// HTML-escape.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn status_class(s: ControlStatus) -> &'static str {
    match s {
        ControlStatus::Pass => "pass",
        ControlStatus::Gap => "gap",
        ControlStatus::Unknown => "unknown",
        ControlStatus::Error => "error",
        ControlStatus::NotApplicable => "na",
    }
}

fn pill(s: ControlStatus) -> String {
    format!(
        "<span class=\"pill {}\">{}</span>",
        status_class(s),
        esc(s.label())
    )
}

/// The banner every published page carries.
///
/// A published compliance report is exactly the artifact a casual reader
/// mistakes for a compliance claim, so the disclaimer leads and is not
/// negotiable.
fn disclaimer(redacted: bool) -> String {
    let mut s = String::from(
        "<div class=\"note\"><strong>This is not a compliance attestation.</strong> \
         It is the output of an automated scan of source code, showing what could and \
         could not be evidenced from a repository. Most of any framework is organizational \
         and cannot be assessed this way — those controls are reported as <em>unknown</em>, \
         which is a statement about the tool's visibility, not about the organization. \
         Certification requires an accredited auditor.",
    );
    if redacted {
        s.push_str(
            " <br><br>File paths, line numbers and evidence excerpts are <strong>withheld</strong> \
             from this published version.",
        );
    }
    s.push_str("</div>");
    s
}

/// The page header: the wordmark and the theme control.
///
/// Emitted **before** everything it styles, because the sibling selectors that
/// highlight the chosen option (`#theme-dark:checked ~ .theme label`) can only
/// look forwards. `:has()` on `:root` handles the colour tokens and does not
/// care about order, but the highlight does.
///
/// No JavaScript, and nothing hidden behind a technique: where `:has()` is
/// unsupported the radios do nothing and the page follows the OS, which is
/// exactly what it did before the toggle existed.
fn masthead() -> String {
    // One list, walked twice: the inputs must come first for the sibling
    // selectors to reach the labels, but naming the options once means the two
    // halves cannot drift apart.
    const OPTIONS: [(&str, &str); 3] = [
        ("theme-os", "Auto"),
        ("theme-light", "Light"),
        ("theme-dark", "Dark"),
    ];

    let mut s = String::new();
    for (i, (id, _)) in OPTIONS.iter().enumerate() {
        // "Auto" is checked, so an untouched page follows the OS exactly as it
        // did before this control existed.
        let checked = if i == 0 { " checked" } else { "" };
        s.push_str(&format!(
            "<input class=\"theme-in\" type=\"radio\" name=\"theme\" id=\"{id}\"{checked}>"
        ));
    }
    // The masthead is a sibling of the radios rather than a parent, because the
    // `#theme-dark:checked ~ .theme label` highlight can only look forwards.
    s.push_str(
        "<div class=\"masthead\"><span class=\"wordmark\">smart-coder</span>\
         <div class=\"theme\" role=\"group\" aria-label=\"Colour theme\">",
    );
    for (id, label) in OPTIONS {
        s.push_str(&format!("<label for=\"{id}\">{label}</label>"));
    }
    s.push_str("</div></div>");
    s
}

/// What kind of page this is.
///
/// The two differ in two places a reader notices, and both would be wrong if
/// shared: an evidence pack should not be indexed by a search engine and *is*
/// generated by `sc-comply`; the project's own landing page is neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A compliance pack or its index.
    Evidence,
    /// The site root — about the project, not about compliance.
    Landing,
}

fn page_shell(title: &str, body: &str) -> String {
    shell_of(Kind::Evidence, title, body)
}

fn shell_of(kind: Kind, title: &str, body: &str) -> String {
    // Evidence packs stay out of search results: they are a demonstration
    // pointed at one repository, and a search hit on "SOC 2 compliant" pointing
    // here would misrepresent what they say. The landing page is the project's
    // front door and should be findable.
    let robots = match kind {
        Kind::Evidence => "<meta name=\"robots\" content=\"noindex\">\n",
        Kind::Landing => "",
    };
    let footer = match kind {
        Kind::Evidence => {
            "Generated by <code>sc-comply</code> — an evidence pack is an argument, \
             not a verdict."
        }
        Kind::Landing => {
            "smart-coder — MIT licensed. This page is generated from the repository \
             it describes."
        }
    };
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n\
         {robots}\
         <meta name=\"color-scheme\" content=\"light dark\">\n\
         <title>{title_esc}</title>\n<style>{STYLE}</style>\n</head>\n<body>\n\
         {theme}\n\
         <main>\n{body}\n</main>\n\
         <footer>{footer}</footer>\n</body>\n</html>\n",
        title_esc = esc(title),
        theme = masthead(),
    )
}

/// Guidance for one undeterminable control, as the site renders it.
///
/// Deliberately mirrors `sc_comply_author::worklist::GuidanceItem` as a plain
/// data type rather than depending on that crate: `sc-comply` must stay
/// model-free, and this is the seam. It carries no status — guidance describes
/// what to obtain and can never change a verdict.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ControlGuidance {
    pub control_id: String,
    pub evidence: Vec<String>,
    pub owner: String,
    pub auditor_asks: String,
}

/// Render one framework's report as a standalone page.
///
/// `guidance` supplies optional auditor-worklist entries for undeterminable
/// controls. Absent, the page is unchanged — most runs have no model configured.
///
/// # Panics
///
/// If `pack` still contains citations. Publishing an unredacted pack is the one
/// mistake this module exists to prevent, so it fails loudly rather than
/// producing a plausible-looking page that leaks.
pub fn framework_page_with_guidance(
    pack: &EvidencePack,
    back_link: Option<&str>,
    guidance: &[ControlGuidance],
) -> String {
    framework_page_inner(pack, back_link, guidance)
}

/// Render one framework's report as a standalone page.
///
/// # Panics
///
/// If `pack` still contains citations. Publishing an unredacted pack is the one
/// mistake this module exists to prevent, so it fails loudly rather than
/// producing a plausible-looking page that leaks.
pub fn framework_page(pack: &EvidencePack, back_link: Option<&str>) -> String {
    framework_page_inner(pack, back_link, &[])
}

/// Per-evidence-domain scores, as a table.
///
/// The blended figures answer "how much of this framework did we settle?". This
/// answers what a reader can act on: *of the things a repository can evidence,
/// how many does it?* Each row names where its evidence lives and who owns it,
/// so an Organizational row at 0% reads as scope rather than as a failing grade.
///
/// Returns empty for a single-domain pack — one row restating the headline
/// figures is noise.
fn by_section(pack: &EvidencePack) -> String {
    let sections = crate::evidence::Score::by_section(&pack.controls);
    if sections.len() < 2 {
        return String::new();
    }

    let pct = |v: f64| (v * 100.0).round() as u32;
    let mut b = String::with_capacity(2 * 1024);
    b.push_str(
        "<h3>By evidence domain</h3>\
         <div class=\"tablewrap\"><table><thead><tr><th>Domain</th><th>Evidence lives in</th><th>Controls</th>\
         <th>Determinacy</th></tr></thead><tbody>",
    );
    for (section, sc) in &sections {
        let _ = write!(
            b,
            "<tr><td><strong>{}</strong><br><span class=\"scope\">owned by {}</span></td>\
             <td class=\"scope\">{}</td>\
             <td>{} <span class=\"scope\">({} pass · {} gap · {} unknown)</span></td>\
             <td><strong>{}%</strong><div class=\"bar\"><div style=\"width:{}%\"></div></div></td></tr>",
            esc(section.label()),
            esc(section.owner()),
            esc(section.evidence_lives_in()),
            sc.total,
            sc.passed,
            sc.gaps,
            sc.unknown,
            pct(sc.determinacy()),
            pct(sc.determinacy()),
        );
    }
    b.push_str("</tbody></table></div>");
    b.push_str(
        "<p class=\"scope\">These are deliberately not combined into a single figure. A framework \
         is completed mostly by declaring organizational controls, which a repository can never \
         settle — blending them in would make an honest pack look worse than a selective one, and \
         would let a large governance section hide a poor result in code.</p>",
    );
    b
}

fn framework_page_inner(
    pack: &EvidencePack,
    back_link: Option<&str>,
    guidance: &[ControlGuidance],
) -> String {
    assert!(
        !pack.has_citations(),
        "refusing to render a page from an unredacted pack ({}): call EvidencePack::redacted() first",
        pack.framework.id
    );

    let mut b = String::with_capacity(16 * 1024);

    if let Some(href) = back_link {
        let _ = write!(
            b,
            "<p><a href=\"{}\">&larr; All frameworks</a></p>",
            esc(href)
        );
    }
    let _ = write!(b, "<h1>{}</h1>", esc(&pack.framework.name));
    let _ = write!(
        b,
        "<p class=\"sub\">{} &middot; version {} &middot; generated {}</p>",
        esc(&pack.framework.authority),
        esc(&pack.framework.version),
        esc(&pack.generated_at)
    );

    b.push_str(&disclaimer(true));

    // Scope before the score — the same layout rule as every other renderer.
    if !pack.scope_note.trim().is_empty() {
        let _ = write!(
            b,
            "<h2>Scope and limitations</h2><p class=\"scope\">{}</p>",
            esc(pack.scope_note.trim())
        );
    }

    let s = &pack.score;
    let in_scope = s.in_scope();
    let _ = write!(b, "<h2>Summary</h2><div class=\"counts\">");
    for (n, label, class) in [
        (s.passed, "pass", "pass"),
        (s.gaps, "gap", "gap"),
        (s.unknown, "unknown", "unknown"),
        (s.errors, "error", "error"),
        (s.not_applicable, "n/a", "na"),
    ] {
        let _ = write!(
            b,
            "<div class=\"count\"><b class=\"{class}\">{n}</b><span>{label}</span></div>"
        );
    }
    b.push_str("</div>");

    let pct = |v: f64| (v * 100.0).round() as u32;
    let _ = write!(
        b,
        "<div class=\"ratios\">\
         <div><strong>Coverage {}%</strong><br><span class=\"scope\">{} of {} in-scope passed</span>\
         <div class=\"bar\"><div style=\"width:{}%\"></div></div></div>\
         <div><strong>Determinacy {}%</strong><br><span class=\"scope\">{} of {} could be determined</span>\
         <div class=\"bar\"><div style=\"width:{}%\"></div></div></div></div>",
        pct(s.coverage()), s.passed, in_scope, pct(s.coverage()),
        pct(s.determinacy()), s.passed + s.gaps, in_scope, pct(s.determinacy())
    );
    b.push_str(
        "<p class=\"scope\">Coverage without determinacy is meaningless: a high coverage figure \
         at low determinacy means little was verified and what was happened to pass.</p>",
    );

    b.push_str(&by_section(pack));

    if !pack.disabled_capabilities.is_empty() {
        let caps: Vec<String> = pack
            .disabled_capabilities
            .iter()
            .map(|c| format!("<code>{}</code>", esc(c)))
            .collect();
        let _ = write!(
            b,
            "<p class=\"scope\">Capabilities disabled for this run: {}.</p>",
            caps.join(", ")
        );
    }

    // Controls, problems first.
    b.push_str(
        "<h2>Controls</h2><div class=\"tablewrap\"><table><thead><tr><th>Control</th><th>Domain</th><th>Status</th>\
                <th>Severity</th><th>Determination</th></tr></thead><tbody>",
    );
    for c in pack.controls_for_report() {
        let _ = write!(
            b,
            "<tr><td><strong>{}</strong><br><span class=\"scope\">{}</span></td>\
             <td class=\"scope\">{}</td><td>{}</td><td>{}</td><td class=\"scope\">{}</td></tr>",
            esc(&c.id),
            esc(&c.title),
            esc(c.section.label()),
            pill(c.status),
            esc(c.severity.label()),
            esc(&c.rationale)
        );
    }
    b.push_str("</tbody></table></div>");

    // Detail for anything not passing — the worklist.
    let notable: Vec<_> = pack
        .controls
        .iter()
        .filter(|c| c.status != ControlStatus::Pass)
        .collect();
    if !notable.is_empty() {
        b.push_str(
            "<h2>Gaps and manual evidence</h2>\
             <p class=\"scope\">Controls that did not pass. <em>Unknown</em> means the tool could \
             not determine the answer from source — these are tasks for an auditor, not findings \
             against the codebase.</p>",
        );
        let mut sorted = notable;
        sorted.sort_by_key(|c| (c.status.report_order(), std::cmp::Reverse(c.severity)));
        for c in sorted {
            let _ = write!(
                b,
                "<div class=\"ctl\"><div class=\"hd\"><span class=\"id\">{}</span>{}\
                 <span class=\"scope\">{}</span></div>",
                esc(&c.id),
                pill(c.status),
                esc(&c.title)
            );
            if !c.intent.trim().is_empty() {
                let _ = write!(b, "<p class=\"intent\">{}</p>", esc(c.intent.trim()));
            }
            let _ = write!(b, "<p class=\"why\">{}</p>", esc(&c.rationale));

            // Auditor guidance, where a model supplied it. Rendered as a
            // worklist item — what to obtain, from whom, and what will be
            // probed — never as a judgment about the control.
            if let Some(g) = guidance.iter().find(|g| g.control_id == c.id) {
                b.push_str("<div class=\"guide\"><strong>Evidence to obtain</strong><ul>");
                for e in &g.evidence {
                    let _ = write!(b, "<li>{}</li>", esc(e));
                }
                b.push_str("</ul>");
                if !g.owner.trim().is_empty() {
                    let _ = write!(
                        b,
                        "<p><strong>Usually held by:</strong> {}</p>",
                        esc(&g.owner)
                    );
                }
                if !g.auditor_asks.trim().is_empty() {
                    let _ = write!(
                        b,
                        "<p><strong>What an auditor will probe:</strong> {}</p>",
                        esc(&g.auditor_asks)
                    );
                }
                b.push_str("</div>");
            }

            if let Some(r) = &c.remediation {
                let label = if c.status == ControlStatus::Unknown {
                    "Obtain"
                } else {
                    "Remediation"
                };
                let _ = write!(
                    b,
                    "<p class=\"rem\"><strong>{label}:</strong> {}</p>",
                    esc(r.trim())
                );
            }
            b.push_str("</div>");
        }
    }

    page_shell(
        &format!("{} — compliance evidence", pack.framework.name),
        &b,
    )
}

/// The executive summary block: headline numbers, the narrative if there is
/// one, and the cross-framework findings.
///
/// The deterministic part is written to stand alone. A narrative adds judgment
/// about salience that arithmetic cannot produce, but its absence must not leave
/// a hole.
fn executive_summary(rollup: &crate::rollup::Rollup, narrative: Option<&str>) -> String {
    let mut b = String::with_capacity(4 * 1024);
    let pct = |v: f64| (v * 100.0).round() as u32;

    b.push_str("<h2>Summary</h2>");

    // Headline counts — the shape of the result in one line.
    let _ = write!(
        b,
        "<div class=\"counts\">\
         <div class=\"count\"><b>{}</b><span>controls</span></div>\
         <div class=\"count\"><b class=\"pass\">{}</b><span>verified</span></div>\
         <div class=\"count\"><b class=\"gap\">{}</b><span>gaps</span></div>\
         <div class=\"count\"><b class=\"unknown\">{}</b><span>manual</span></div>\
         <div class=\"count\"><b>{}</b><span>frameworks</span></div></div>",
        rollup.controls, rollup.passed, rollup.gaps, rollup.unknown, rollup.frameworks
    );

    if let Some(text) = narrative {
        // Model-written prose. Paragraph breaks are preserved; everything is
        // escaped, because this text did not come from us.
        b.push_str("<div class=\"exec\">");
        for para in text.split("\n\n").filter(|p| !p.trim().is_empty()) {
            let _ = write!(b, "<p>{}</p>", esc(para.trim()));
        }
        b.push_str("</div>");
    }

    // The cross-framework finding — the highest-leverage thing on the page, and
    // the one thing no per-framework report can show.
    let shared = rollup.shared_findings();
    if !shared.is_empty() {
        b.push_str(
            "<h3>Outstanding items affecting several frameworks</h3>\
             <p class=\"scope\">One change here resolves the same finding in every framework \
             listed.</p><div class=\"tablewrap\"><table><thead><tr><th>Item</th><th>Frameworks</th><th>Severity</th>\
             </tr></thead><tbody>",
        );
        for f in shared.iter().take(6) {
            let _ = write!(
                b,
                "<tr><td><code>{}</code><br><span class=\"scope\">{}</span></td>\
                 <td>{}</td><td>{}</td></tr>",
                esc(&f.check),
                esc(&f.rationale),
                f.reach(),
                esc(f.severity.label())
            );
        }
        b.push_str("</tbody></table></div>");
    } else if rollup.has_gaps() {
        b.push_str(
            "<p class=\"scope\">No single finding recurs across frameworks — the outstanding \
             items are specific to individual controls.</p>",
        );
    }

    // Determinacy, stated as the credibility of everything above it.
    let _ = write!(
        b,
        "<div class=\"note\"><strong>{}% of assessed controls could be determined from source.</strong> \
         The remainder are organizational — policies, training records, vendor agreements, \
         incident procedures — and require documentary evidence a code scan cannot reach. \
         That is a limit of this method, not a finding about the project.</div>",
        pct(rollup.determinacy())
    );

    b
}

/// One entry in the docs landing page's spec list.
pub struct SpecLink {
    /// Displayed title, e.g. `"13 — Compliance evidence"`.
    pub title: String,
    /// Target URL. Specs point at github.com, which renders Markdown natively —
    /// this site has Jekyll disabled, so a relative `.md` link would download
    /// raw text rather than render.
    pub href: String,
    /// One line on what the spec covers.
    pub summary: String,
}

/// Render the documentation landing page — the site root.
///
/// GitHub Pages serving from `/docs` looks for `docs/index.html`; without one a
/// visitor to the site root gets a 404 or a bare directory listing.
pub fn landing_page(repo_url: &str, specs: &[SpecLink]) -> String {
    let mut b = String::with_capacity(8 * 1024);

    let _ = write!(b, "<h1>smart-coder</h1>");
    b.push_str(
        "<p class=\"sub\">An agentic coding tool built to run entirely on <strong>small</strong> \
         language models — 12B parameters at the ceiling, and ideally something small enough to \
         run on a phone.</p>",
    );

    // The premise first. A landing page that opens with one subsystem tells a
    // visitor what the project *has* before it tells them what it *is*.
    b.push_str(
        "<p>Most agentic coding tools assume a large frontier model and lean on its raw \
         intelligence. This one assumes the opposite: the model is limited — shallow reasoning, \
         a small context window, shaky at free-form tool calls — and the <em>harness</em> does \
         the heavy lifting. The interesting engineering is the scaffolding that makes a small, \
         cheap, local model behave like a competent coding agent.</p>",
    );

    b.push_str("<h2>The bets</h2>");
    for (title, body) in [
        (
            "Scale out, not up",
            "A swarm of very small workers on one codebase, coordinated by a single larger \
             orchestrator that plans, assigns and integrates. Not one big model.",
        ),
        (
            "A gated workflow",
            "Every non-trivial task moves through staged phases — specs, architecture, layout, \
             stage breakdown, work decomposition — with a human checkpoint between each. The \
             agent works autonomously <em>within</em> a phase and stops at the boundary.",
        ),
        (
            "Tests are the control system",
            "Full TDD, mandatory at the unit level: a failing test defines the work before it is \
             implemented. A test is the machine-checkable oracle a small model lacks — it turns \
             \u{201c}trust the model\u{201d} into \u{201c}trust the test runner\u{201d}.",
        ),
        (
            "Local and private",
            "It runs on a laptop or a homelab box. No code leaves the machine and there is no \
             API bill — which is also what makes the constraints worth designing around.",
        ),
    ] {
        let _ = write!(
            b,
            "<div class=\"ctl\"><div class=\"hd\"><span class=\"id\">{title}</span></div>\
             <p class=\"intent\">{body}</p></div>"
        );
    }

    b.push_str(
        "<h2>What is built</h2>\
         <p>Early implementation, and the specs below are ahead of the code in places — where \
         they are, they say so.</p>",
    );
    for (name, body) in [
        (
            "A staged-workflow desktop client",
            "A native GUI over the core: type intent, watch the agent and swarm work. It drives \
             the design pipeline end to end, pausing at each phase for review, with a code \
             editor, a git diff view and a PR-style review panel whose comments become the \
             send-back notes that regenerate a phase.",
        ),
        (
            "A hosted intake surface",
            "File a request from a phone; come back to a drafted spec you can approve or send \
             back. The developer\u{2019}s machine dials <em>out</em> to it, so nothing there is \
             exposed — the server holds text and has no repository, no model and no filesystem.",
        ),
        (
            "A compliance evidence engine",
            "Audits a repository against ten framework packs and emits an auditor-facing \
             evidence pack. Deterministic: no model runs during an audit, so the same repository \
             produces the same pack.",
        ),
        (
            "Spec traceability",
            "Claims in the specs carry anchors into the code, and a check fails the build when \
             one stops being true. Drift is caught by a machine rather than by remembering to \
             read two documents side by side.",
        ),
    ] {
        let _ = write!(
            b,
            "<div class=\"ctl\"><div class=\"hd\"><span class=\"id\">{name}</span></div>\
             <p class=\"intent\">{body}</p></div>"
        );
    }

    b.push_str(
        "<h2>It audits itself</h2>\
         <p>The compliance engine is pointed at this repository, and the result is published. \
         A live demonstration rather than a claim: most of any framework is organizational and \
         cannot be assessed from source, and the report says so control by control — there is \
         deliberately no headline percentage.</p>\
         <p><a href=\"compliance/index.html\"><strong>Read the evidence packs \u{2192}</strong></a> \
         \u{2014} SOC 2, ISO 27001, NIST SSDF, SLSA, CIS v8, PCI DSS, NIST 800-53, HIPAA, GDPR, \
         and the EU NIS2/DORA/AI Act cluster.</p>",
    );
    b.push_str(
        "<div class=\"note\"><strong>File paths, line numbers and evidence excerpts are \
         withheld</strong> from the published packs. They show what was assessed and what the \
         verdict was, not where to look.</div>",
    );

    if !specs.is_empty() {
        b.push_str(
            "<h2>Design specs</h2>\
             <p>The project is specified before it is built, and the specs are the honest \
             record rather than a sales pitch: they carry what is <em>not</em> built, the \
             deviations taken and why, and the arguments that were lost. Start with the \
             overview.</p>\
             <p class=\"scope\">Rendered on GitHub, where Markdown displays natively.</p>\
             <div class=\"tablewrap\"><table><thead><tr><th>Spec</th><th>Covers</th></tr></thead><tbody>",
        );
        for s in specs {
            let _ = write!(
                b,
                "<tr><td><a href=\"{}\">{}</a></td><td class=\"scope\">{}</td></tr>",
                esc(&s.href),
                esc(&s.title),
                esc(&s.summary)
            );
        }
        b.push_str("</tbody></table></div>");
    }

    let _ = write!(
        b,
        "<h2>Source</h2>\
         <p>Rust, MIT-licensed, and public. The README covers running it against a local model \
         server or a hosted one.</p>\
         <p><a href=\"{url}\"><strong>{url}</strong></a></p>",
        url = esc(repo_url)
    );

    // The title says what the page is about, not what generated it. A visitor
    // searching for the project should not have to know the word "compliance".
    shell_of(
        Kind::Landing,
        "smart-coder — an agentic coding tool for small models",
        &b,
    )
}

/// One framework's row on the index.
pub struct IndexEntry {
    /// Link target, e.g. `"soc2.html"`.
    pub href: String,
    pub pack: EvidencePack,
}

/// Render the index page linking every framework.
///
/// `narrative` is an optional model-written executive summary. When absent the
/// deterministic summary below stands on its own — the page must be complete
/// without a model, because most people running this will not have one
/// configured.
/// Cross-framework scores per evidence domain, for the index.
///
/// The per-framework table below this answers "how complete is each framework?".
/// This answers the question a reader actually has: *of the things a repository
/// can evidence, how many does this project evidence?* Those are different
/// questions, and only the second one is actionable.
///
/// Deliberately no combined figure. A blend is what would let 35 declared
/// governance controls drag down a good result in code — and, worse, make a pack
/// look better the fewer organizational controls it honestly declares.
fn rollup_by_section(rollup: &crate::rollup::Rollup) -> String {
    if rollup.by_section.len() < 2 {
        return String::new();
    }

    let pct = |v: f64| (v * 100.0).round() as u32;
    let mut b = String::with_capacity(2 * 1024);
    b.push_str("<h2>By evidence domain</h2>");
    b.push_str(
        "<p class=\"scope\">Controls grouped by where their evidence physically lives. \
         A domain a repository cannot see is reported as unknown — that is a statement \
         about the evidence, not about the project.</p>",
    );
    b.push_str(
        "<div class=\"tablewrap\"><table><thead><tr><th>Domain</th><th>Evidence lives in</th><th>Controls</th>\
         <th>Determinacy</th></tr></thead><tbody>",
    );
    for (section, sc) in &rollup.by_section {
        let _ = write!(
            b,
            "<tr><td><strong>{}</strong><br><span class=\"scope\">owned by {}</span></td>\
             <td class=\"scope\">{}</td>\
             <td>{} <span class=\"scope\">({} pass · {} gap · {} unknown)</span></td>\
             <td><strong>{}%</strong><div class=\"bar\"><div style=\"width:{}%\"></div></div></td></tr>",
            esc(section.label()),
            esc(section.owner()),
            esc(section.evidence_lives_in()),
            sc.total,
            sc.passed,
            sc.gaps,
            sc.unknown,
            pct(sc.determinacy()),
            pct(sc.determinacy()),
        );
    }
    b.push_str("</tbody></table></div>");
    b.push_str(
        "<p class=\"scope\">These are deliberately not combined into a single figure. A framework \
         is completed mostly by declaring organizational controls, which a repository can never \
         settle — blending them would make an honest report look worse than a selective one, and \
         would let a large governance section hide a poor result in code.</p>",
    );
    b
}

pub fn index_page(
    entries: &[IndexEntry],
    workspace_label: &str,
    rollup: &crate::rollup::Rollup,
    narrative: Option<&str>,
) -> String {
    let mut b = String::with_capacity(12 * 1024);
    let _ = write!(b, "<h1>Compliance evidence</h1>");
    let _ = write!(
        b,
        "<p class=\"sub\">{} frameworks assessed against {}</p>",
        entries.len(),
        esc(workspace_label)
    );

    b.push_str(&disclaimer(true));
    b.push_str(&executive_summary(rollup, narrative));

    // Domains BEFORE the per-framework table. A reader who takes one number away
    // should take away the one for the domain they can act on, not a blend of
    // four domains that answer different questions.
    b.push_str(&rollup_by_section(rollup));

    b.push_str(
        "<h2>Frameworks</h2><div class=\"tablewrap\"><table><thead><tr><th>Framework</th><th>Pass</th><th>Gap</th>\
         <th>Unknown</th><th>Coverage</th><th>Determinacy</th></tr></thead><tbody>",
    );
    for e in entries {
        let s = &e.pack.score;
        let pct = |v: f64| (v * 100.0).round() as u32;
        let _ = write!(
            b,
            "<tr><td><a href=\"{}\">{}</a></td>\
             <td class=\"pass\">{}</td><td class=\"gap\">{}</td><td class=\"unknown\">{}</td>\
             <td>{}%</td><td>{}%</td></tr>",
            esc(&e.href),
            esc(&e.pack.framework.name),
            s.passed,
            s.gaps,
            s.unknown,
            pct(s.coverage()),
            pct(s.determinacy())
        );
    }
    b.push_str("</tbody></table></div>");

    b.push_str(
        "<h2>How to read this</h2>\
         <p><strong>Determinacy</strong> is the number that matters. It is the share of in-scope \
         controls the tool could decide either way. A low figure does not mean poor security — it \
         means most of that framework is organizational (board oversight, vendor contracts, \
         training records, incident procedures) and simply is not visible in source code.</p>\
         <p>Each framework's page states, in its own scope note, exactly what it cannot assess. \
         That honesty is the point: a report claiming broad coverage of a governance framework \
         from a code scan would be misleading.</p>",
    );

    page_shell("Compliance evidence", &b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{CheckResult, ControlResult, Evidence, FrameworkMeta};
    use crate::status::Severity;

    fn pack(with_evidence: bool) -> EvidencePack {
        let evidence = if with_evidence {
            vec![Evidence::new(
                "secret/dir/id_rsa",
                Some(2),
                "-----BEGIN RSA PRIVATE KEY-----",
                "CC6.1/keys",
                "regex",
            )]
        } else {
            vec![]
        };
        EvidencePack::new(
            FrameworkMeta {
                id: "soc2".into(),
                name: "SOC 2 <Trust>".into(),
                version: "1".into(),
                authority: "AICPA".into(),
            },
            "C:/Users/someone/proj".into(),
            "2026-07-25T00:00:00Z".into(),
            "Source-controlled artifacts only.".into(),
            vec![ControlResult {
                id: "CC6.1".into(),
                title: "Logical access".into(),
                section: Default::default(),
                clause: "TSC CC6.1".into(),
                intent: "Credentials must not be committed.".into(),
                severity: Severity::Critical,
                status: ControlStatus::Gap,
                checks: vec![CheckResult {
                    check_id: "CC6.1/keys".into(),
                    kind: "regex-must-not-match".into(),
                    status: ControlStatus::Gap,
                    weight: 1.0,
                    evidence,
                    note: None,
                    rationale: "A committed key is a failure.".into(),
                }],
                rationale: "all-of: worst of 1 check(s) is gap".into(),
                remediation: Some("Rotate the credential.".into()),
            }],
            vec!["command-exit-code".into()],
        )
    }

    #[test]
    fn renders_a_redacted_pack() {
        let html = framework_page(&pack(true).redacted(), Some("index.html"));
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("CC6.1"));
        assert!(html.contains("Rotate the credential."));
    }

    /// A pack spanning several evidence domains reports each one separately.
    #[test]
    fn a_multi_domain_pack_reports_each_domain() {
        let base = pack(false);
        let mut org = base.controls[0].clone();
        org.id = "CC1.1".into();
        org.section = crate::Section::Organizational;
        org.status = ControlStatus::Unknown;
        let mut controls = base.controls.clone();
        controls.push(org);

        let p = EvidencePack::new(
            base.framework.clone(),
            base.workspace.clone(),
            base.generated_at.clone(),
            base.scope_note.clone(),
            controls,
            vec![],
        );
        let html = framework_page(&p.redacted(), None);

        assert!(html.contains("By evidence domain"), "{html}");
        assert!(html.contains("Organizational"), "{html}");
        // Each row says where the evidence is and who owns it, so a 0% row
        // reads as a statement of scope rather than a failing grade.
        assert!(html.contains("HR records"), "{html}");
        assert!(html.contains("owned by"), "{html}");
        assert!(html.contains("not combined into a single figure"), "{html}");
    }

    /// A single-domain pack gets no breakdown table.
    #[test]
    fn a_single_domain_pack_omits_the_breakdown() {
        let html = framework_page(&pack(false).redacted(), None);
        assert!(!html.contains("By evidence domain"), "{html}");
    }

    #[test]
    #[should_panic(expected = "unredacted pack")]
    fn refuses_to_render_an_unredacted_pack() {
        // The safety property: this renderer will not publish citations even if
        // a caller forgets to redact.
        let _ = framework_page(&pack(true), None);
    }

    #[test]
    fn no_path_or_excerpt_reaches_the_html() {
        let html = framework_page(&pack(true).redacted(), None);
        for leak in [
            "secret/dir",
            "id_rsa",
            "BEGIN RSA PRIVATE KEY",
            "someone",
            "proj",
        ] {
            assert!(!html.contains(leak), "page leaked {leak:?}");
        }
    }

    #[test]
    fn guidance_renders_as_a_worklist_not_a_verdict() {
        let mut p = pack(false).redacted();
        p.controls[0].status = ControlStatus::Unknown;
        let guidance = vec![ControlGuidance {
            control_id: "CC6.1".into(),
            evidence: vec![
                "Board meeting minutes".into(),
                "Signed acknowledgements".into(),
            ],
            owner: "HR and the company secretary".into(),
            auditor_asks: "Whether oversight recurred, evidenced by dated minutes.".into(),
        }];
        let html = framework_page_with_guidance(&p, None, &guidance);

        assert!(html.contains("Evidence to obtain"));
        assert!(html.contains("Board meeting minutes"));
        assert!(html.contains("Usually held by:"));
        assert!(html.contains("What an auditor will probe:"));
        // It must never restate the control as decided.
        assert!(!html.contains("is satisfied"));
    }

    #[test]
    fn guidance_is_escaped_and_matched_by_control_id() {
        let mut p = pack(false).redacted();
        p.controls[0].status = ControlStatus::Unknown;
        // Guidance for a DIFFERENT control must not attach to this one.
        let other = vec![ControlGuidance {
            control_id: "ZZ9.9".into(),
            evidence: vec!["<script>alert(1)</script>".into()],
            owner: "x".into(),
            auditor_asks: "y".into(),
        }];
        let html = framework_page_with_guidance(&p, None, &other);
        assert!(
            !html.contains("Evidence to obtain"),
            "wrong control got guidance"
        );
        assert!(!html.contains("<script>alert"));
    }

    #[test]
    fn a_page_without_guidance_is_unchanged() {
        // The no-model path, which is what most runs use.
        let p = pack(false).redacted();
        assert_eq!(
            framework_page(&p, None),
            framework_page_with_guidance(&p, None, &[])
        );
    }

    #[test]
    fn the_disclaimer_precedes_the_score() {
        // Same honesty-of-layout rule as the Markdown and web renderers.
        let html = framework_page(&pack(false).redacted(), None);
        let disc = html
            .find("not a compliance attestation")
            .expect("disclaimer");
        let score = html.find("Summary").expect("summary");
        assert!(disc < score);
    }

    #[test]
    fn html_is_escaped() {
        // The framework name contains angle brackets in the fixture.
        let html = framework_page(&pack(false).redacted(), None);
        assert!(html.contains("SOC 2 &lt;Trust&gt;"));
        assert!(!html.contains("SOC 2 <Trust>"));
    }

    #[test]
    fn pages_are_self_contained() {
        // Must work from file:// and offline: no external CSS, JS or fetches.
        let html = framework_page(&pack(false).redacted(), None);
        assert!(!html.contains("<script"));
        assert!(!html.contains("http://"));
        assert!(!html.contains("https://"));
        assert!(html.contains("<style>"));
    }

    #[test]
    fn the_landing_page_is_about_the_project_not_about_compliance() {
        // It is the repository's front door. A visitor should learn what the
        // tool *is* before they learn what one of its subsystems produces —
        // leading with compliance describes the project by its most auditable
        // part rather than its point.
        let html = landing_page("https://github.com/x/y", &[]);

        let premise = html.find("small").expect("the premise is stated");
        let compliance = html.find("evidence packs").expect("still linked");
        assert!(premise < compliance, "the tool comes before the audit");

        assert!(html.contains("The bets"), "{html}");
        assert!(html.contains("What is built"), "{html}");
        assert!(html.contains("harness"), "the actual argument: {html}");
        // And compliance is still there, as one thing among several.
        assert!(html.contains("compliance/index.html"));
    }

    #[test]
    fn the_landing_page_is_findable_but_the_packs_are_not() {
        // An evidence pack is a demonstration pointed at one repository. A
        // search hit on "SOC 2 compliant" landing there would misrepresent what
        // it says — the packs are explicit that most of a framework cannot be
        // assessed from source at all.
        let landing = landing_page("https://github.com/x/y", &[]);
        assert!(!landing.contains("noindex"), "the front door is findable");

        let pack = framework_page(&pack(false).redacted(), None);
        assert!(pack.contains("noindex"), "a pack is not");
    }

    #[test]
    fn the_landing_page_does_not_claim_to_be_a_compliance_artifact() {
        // The shared footer says "Generated by sc-comply — an evidence pack is
        // an argument, not a verdict", which is true of a pack and misleading
        // as the last line of the project's own page.
        let html = landing_page("https://github.com/x/y", &[]);
        assert!(!html.contains("an evidence pack is an argument"), "{html}");
        assert!(html.contains("MIT licensed"), "{html}");
    }

    #[test]
    fn the_title_says_what_the_project_is() {
        // Somebody searching for the project should not have to know the word
        // "compliance" to recognise the result.
        let html = landing_page("https://github.com/x/y", &[]);
        assert!(html.contains("<title>smart-coder"), "{html}");
        assert!(html.contains("agentic coding tool"), "{html}");
    }

    #[test]
    fn the_theme_toggle_needs_no_javascript() {
        // These pages are read from file://, offline, and over whatever
        // connection somebody has. A script is one more thing that can fail to
        // load, and the toggle does not need one.
        let html = framework_page(&pack(false).redacted(), None);
        assert!(!html.contains("<script"), "no script anywhere");
        assert!(html.contains("id=\"theme-dark\""), "{html}");
        assert!(html.contains(":has(#theme-dark:checked)"), "CSS drives it");
    }

    #[test]
    fn an_untouched_page_still_follows_the_operating_system() {
        // The toggle is an override, not a replacement. Before it existed the
        // page followed `prefers-color-scheme`, and with nothing chosen it
        // still does — so the default behaviour is unchanged.
        let html = framework_page(&pack(false).redacted(), None);
        assert!(
            html.contains("id=\"theme-os\" checked"),
            "auto is the default"
        );
        assert!(html.contains("prefers-color-scheme:dark"), "{html}");
    }

    /// CSS with `/* … */` comments removed.
    ///
    /// So a rule check reads rules. This stylesheet explains its own decisions
    /// in comments — including why the radios avoid `display:none` — and a
    /// substring check would otherwise fail on the sentence saying the thing is
    /// not done.
    fn strip_comments(css: &str) -> String {
        let mut out = String::with_capacity(css.len());
        let mut rest = css;
        while let Some(open) = rest.find("/*") {
            out.push_str(&rest[..open]);
            match rest[open..].find("*/") {
                Some(close) => rest = &rest[open + close + 2..],
                // Unterminated: drop the remainder rather than loop forever.
                None => return out,
            }
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn no_content_is_hidden_behind_a_technique() {
        // Where `:has()` is unsupported the radios do nothing and the page
        // follows the OS — the same behaviour as before. A control hidden until
        // a technique fires would instead be permanently unreachable there.
        //
        // Checked over the *screen* rules only: the print block deliberately
        // hides the theme toggle, because a colour control means nothing on
        // paper. Asserting over the whole sheet would either fail on that or
        // have to be loosened until it proved nothing.
        // Comments are stripped first: this file *discusses* `display:none` in
        // the comment explaining why the radios do not use it, and a substring
        // check cannot tell prose from a rule.
        let screen = strip_comments(
            STYLE
                .split("@media print")
                .next()
                .expect("the print block is last"),
        );

        for hazard in ["display:none", "visibility:hidden", "opacity:0"] {
            assert!(!screen.contains(hazard), "{hazard} in the screen rules");
        }
        // And the print block hides only the toggle, never content.
        let print = STYLE.split("@media print").nth(1).unwrap_or("");
        assert!(print.contains(".theme,.theme-in{display:none}"), "{print}");
    }

    #[test]
    fn the_radios_stay_reachable_by_keyboard() {
        // Off-screen rather than `display:none`, which would remove them from
        // the tab order and from a screen reader.
        let html = framework_page(&pack(false).redacted(), None);
        assert!(html.contains("clip-path:inset(50%)"), "{html}");
        assert!(html.contains("aria-label=\"Colour theme\""), "{html}");
    }

    #[test]
    fn the_stylesheet_uses_logical_properties() {
        // So a right-to-left language costs a translation rather than a
        // stylesheet rewrite. Fifteen seconds now, a full rewrite later.
        for physical in [
            "margin-left:",
            "margin-right:",
            "padding-left:",
            "padding-right:",
            "border-left:",
            "border-right:",
            "text-align:left",
        ] {
            assert!(!STYLE.contains(physical), "{physical} in the stylesheet");
        }
    }

    #[test]
    fn a_wide_table_scrolls_inside_its_own_box() {
        // The page body must never scroll sideways — on a phone that is the
        // difference between readable and not.
        let html = framework_page(&pack(false).redacted(), None);
        assert!(html.contains("class=\"tablewrap\""), "{html}");
        assert!(STYLE.contains("overflow-x:auto"), "{STYLE}");
    }

    #[test]
    fn the_stylesheet_references_nothing_remote() {
        // The page-level test catches `https://`, but a remote font sneaks in
        // as a protocol-relative `url(//…)` or an `@import`, which it would not.
        for remote in ["@import", "url(http", "url(//", "//fonts"] {
            assert!(!STYLE.contains(remote), "{remote} in the stylesheet");
        }
    }

    fn sample_rollup() -> crate::rollup::Rollup {
        crate::rollup::Rollup {
            frameworks: 2,
            controls: 20,
            passed: 8,
            gaps: 2,
            unknown: 10,
            errors: 0,
            recurring: vec![crate::rollup::RecurringFinding {
                check: "secret-scanning-configured".into(),
                frameworks: vec!["SOC 2".into(), "ISO 27001".into()],
                severity: Severity::Critical,
                rationale: "Automated secret detection prevents recurrence.".into(),
                remediation: Some("Add gitleaks to CI.".into()),
            }],
            weakest_coverage: vec![("ISO 27001".into(), 0.3)],
            by_section: [
                (
                    crate::Section::Code,
                    crate::evidence::Score {
                        total: 10,
                        passed: 8,
                        gaps: 2,
                        ..Default::default()
                    },
                ),
                (
                    crate::Section::Organizational,
                    crate::evidence::Score {
                        total: 10,
                        unknown: 10,
                        ..Default::default()
                    },
                ),
            ]
            .into_iter()
            .collect(),
            disabled_capabilities: vec!["command-exit-code".into()],
        }
    }

    fn two_entries() -> Vec<IndexEntry> {
        vec![
            IndexEntry {
                href: "soc2.html".into(),
                pack: pack(false).redacted(),
            },
            IndexEntry {
                href: "iso.html".into(),
                pack: pack(false).redacted(),
            },
        ]
    }

    #[test]
    fn the_index_links_every_framework() {
        let html = index_page(&two_entries(), "this repository", &sample_rollup(), None);
        assert!(html.contains("soc2.html"));
        assert!(html.contains("iso.html"));
        assert!(html.contains("Determinacy"));
        assert!(html.contains("not a compliance attestation"));
    }

    #[test]
    fn the_summary_is_complete_without_a_narrative() {
        // Most people running this will have no model configured. The page must
        // be good anyway.
        let html = index_page(&two_entries(), "repo", &sample_rollup(), None);
        assert!(html.contains("<h2>Summary</h2>"));
        assert!(
            html.contains("secret-scanning-configured"),
            "the shared finding"
        );
        assert!(html.contains("Outstanding items affecting several frameworks"));
        assert!(html.contains("could be determined from source"));
    }

    #[test]
    fn a_narrative_is_rendered_above_the_findings() {
        let html = index_page(
            &two_entries(),
            "repo",
            &sample_rollup(),
            Some("First paragraph.\n\nSecond paragraph."),
        );
        assert!(html.contains("<p>First paragraph.</p>"));
        assert!(html.contains("<p>Second paragraph.</p>"));
        let narrative = html.find("First paragraph").expect("narrative");
        let findings = html.find("Outstanding items").expect("findings");
        assert!(narrative < findings, "prose leads, detail follows");
    }

    #[test]
    fn a_narrative_is_escaped() {
        // It came from a model, not from us.
        let html = index_page(
            &two_entries(),
            "repo",
            &sample_rollup(),
            Some("<script>alert(1)</script>"),
        );
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn the_summary_leads_with_counts_not_a_score() {
        // No single headline percentage — the same rule as every other renderer.
        let html = index_page(&two_entries(), "repo", &sample_rollup(), None);
        let counts = html.find("<span>controls</span>").expect("counts");
        let table = html.find("<h2>Frameworks</h2>").unwrap_or(html.len());
        assert!(counts < table);
    }

    /// The index carries the cross-framework domain breakdown.
    ///
    /// This shipped missing once: `by_section` was wired into the framework
    /// pages and the narrative prompt but not into `index_page`, so regenerating
    /// the site faithfully re-rendered a summary page with no breakdown on it.
    /// The prose mentioned domains the tables never showed.
    #[test]
    fn the_index_reports_each_evidence_domain() {
        let html = index_page(&two_entries(), "repo", &sample_rollup(), None);

        assert!(html.contains("<h2>By evidence domain</h2>"), "{html}");
        assert!(html.contains("Organizational"), "{html}");
        // Each row says where the evidence is and who owns it, so a 0% row reads
        // as scope rather than as a failing grade.
        assert!(html.contains("HR records"), "{html}");
        assert!(html.contains("owned by"), "{html}");
        assert!(html.contains("not combined into a single figure"), "{html}");
    }

    /// Domains come before the per-framework table.
    ///
    /// The per-framework table answers "how complete is each framework?"; the
    /// domain table answers what a reader can act on. Order is the whole point.
    #[test]
    fn the_domain_breakdown_precedes_the_framework_table() {
        let html = index_page(&two_entries(), "repo", &sample_rollup(), None);
        let domains = html.find("<h2>By evidence domain</h2>").expect("domains");
        let frameworks = html.find("<h2>Frameworks</h2>").expect("frameworks");
        assert!(
            domains < frameworks,
            "the actionable split must come before the per-framework totals"
        );
    }

    /// An empty rollup renders without a stray empty table.
    #[test]
    fn an_index_with_no_domain_data_omits_the_breakdown() {
        let html = index_page(&[], "x", &crate::rollup::Rollup::default(), None);
        assert!(!html.contains("By evidence domain"), "{html}");
    }

    #[test]
    fn the_landing_page_links_the_compliance_site_and_specs() {
        let specs = vec![SpecLink {
            title: "13 — Compliance evidence".into(),
            href: "https://github.com/x/y/blob/main/docs/specs/13-compliance-evidence.md".into(),
            summary: "An evidence pack is an argument, not a verdict.".into(),
        }];
        let html = landing_page("https://github.com/x/y", &specs);
        assert!(html.starts_with("<!doctype html>"));
        assert!(
            html.contains("compliance/index.html"),
            "must link the report"
        );
        assert!(
            html.contains("13-compliance-evidence.md"),
            "must link the specs"
        );
        assert!(html.contains("github.com/x/y"));
    }

    #[test]
    fn the_landing_page_states_the_redaction() {
        // A reader arriving at the site root should learn the packs are redacted
        // before they open one.
        let html = landing_page("https://github.com/x/y", &[]);
        assert!(html.contains("withheld"), "{html}");
    }

    #[test]
    fn the_landing_page_omits_the_spec_table_when_empty() {
        let html = landing_page("https://github.com/x/y", &[]);
        assert!(!html.contains("Design specs"));
    }

    #[test]
    fn the_landing_page_carries_no_script_and_inlines_its_css() {
        // Unlike the framework pages it DOES contain https:// links (to GitHub,
        // which renders Markdown natively — this site has Jekyll disabled). It
        // must still ship no script and no external stylesheet.
        let html = landing_page("https://github.com/x/y", &[]);
        assert!(!html.contains("<script"));
        assert!(!html.contains("rel=\"stylesheet\""));
        assert!(html.contains("<style>"));
    }

    #[test]
    fn the_index_explains_what_low_determinacy_means() {
        // Without this a reader reads 20% as "bad security" rather than
        // "mostly a governance framework".
        let html = index_page(&[], "x", &crate::rollup::Rollup::default(), None);
        assert!(html.contains("organizational"));
        assert!(html.contains("not visible in source code"));
    }
}
