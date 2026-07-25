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

/// Shared stylesheet. Light, printable, and readable without JS.
const STYLE: &str = r#"
:root{--bg:#fff;--fg:#1f2328;--dim:#59636e;--line:#d1d9e0;--panel:#f6f8fa;
--pass:#1a7f37;--gap:#cf222e;--unknown:#9a6700;--error:#8250df;--na:#59636e;--link:#0969da}
@media(prefers-color-scheme:dark){:root{--bg:#0d1117;--fg:#e6edf3;--dim:#9198a1;
--line:#3d444d;--panel:#151b23;--pass:#3fb950;--gap:#f85149;--unknown:#d29922;
--error:#d2a8ff;--na:#9198a1;--link:#4493f8}}
*{box-sizing:border-box}
body{margin:0 auto;padding:2rem 1.25rem 4rem;max-width:60rem;background:var(--bg);color:var(--fg);
font:16px/1.6 -apple-system,BlinkMacSystemFont,"Segoe UI",Helvetica,Arial,sans-serif}
h1{font-size:1.7rem;margin:0 0 .3rem}h2{font-size:1.25rem;margin:2.2rem 0 .8rem;
padding-bottom:.3rem;border-bottom:1px solid var(--line)}
h3{font-size:1rem;margin:1.4rem 0 .4rem}
a{color:var(--link)}code{font:13px/1.5 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;
background:var(--panel);padding:.1em .35em;border-radius:4px}
.sub{color:var(--dim);margin:0 0 1.5rem}
.note{background:var(--panel);border:1px solid var(--line);border-left:3px solid var(--unknown);
border-radius:6px;padding:.85rem 1rem;margin:1.2rem 0}
.note strong{color:var(--unknown)}
.scope{white-space:pre-wrap;color:var(--dim);font-size:.92rem}
table{border-collapse:collapse;width:100%;margin:1rem 0;font-size:.93rem}
th,td{text-align:left;padding:.5rem .6rem;border-bottom:1px solid var(--line);vertical-align:top}
th{color:var(--dim);font-weight:600;font-size:.82rem;text-transform:uppercase;letter-spacing:.04em}
tr:hover td{background:var(--panel)}
.pill{display:inline-block;padding:.1em .6em;border-radius:10px;font-size:.8rem;font-weight:600;
white-space:nowrap}
.pass{color:var(--pass);background:color-mix(in srgb,var(--pass) 12%,transparent)}
.gap{color:var(--gap);background:color-mix(in srgb,var(--gap) 12%,transparent)}
.unknown{color:var(--unknown);background:color-mix(in srgb,var(--unknown) 12%,transparent)}
.error{color:var(--error);background:color-mix(in srgb,var(--error) 12%,transparent)}
.na{color:var(--na);background:color-mix(in srgb,var(--na) 12%,transparent)}
.counts{display:flex;gap:.6rem;flex-wrap:wrap;margin:1rem 0}
.count{background:var(--panel);border:1px solid var(--line);border-radius:8px;
padding:.55rem .9rem;min-width:5.5rem}
.count b{display:block;font-size:1.45rem;line-height:1.2}
.count span{color:var(--dim);font-size:.78rem;text-transform:uppercase;letter-spacing:.04em}
.ratios{display:flex;gap:2rem;flex-wrap:wrap;margin:.6rem 0 0}
.bar{height:6px;background:var(--line);border-radius:3px;width:9rem;overflow:hidden;margin-top:.3rem}
.bar>div{height:100%;background:var(--link)}
.ctl{border:1px solid var(--line);border-radius:8px;padding:.9rem 1.1rem;margin:.9rem 0}
.ctl .hd{display:flex;gap:.6rem;align-items:baseline;flex-wrap:wrap}
.ctl .id{font-weight:700}.ctl .why{color:var(--dim);font-size:.9rem;margin:.5rem 0 0}
.ctl .intent{font-size:.93rem;margin:.5rem 0 0}
.ctl .rem{font-size:.9rem;margin:.6rem 0 0;padding:.5rem .7rem;background:var(--panel);border-radius:6px}
footer{margin-top:3rem;padding-top:1rem;border-top:1px solid var(--line);color:var(--dim);font-size:.85rem}
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

fn page_shell(title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n\
         <meta name=\"robots\" content=\"noindex\">\n\
         <title>{}</title>\n<style>{STYLE}</style>\n</head>\n<body>\n{body}\n\
         <footer>Generated by <code>sc-comply</code> — an evidence pack is an argument, \
         not a verdict.</footer>\n</body>\n</html>\n",
        esc(title)
    )
}

/// Render one framework's report as a standalone page.
///
/// # Panics
///
/// If `pack` still contains citations. Publishing an unredacted pack is the one
/// mistake this module exists to prevent, so it fails loudly rather than
/// producing a plausible-looking page that leaks.
pub fn framework_page(pack: &EvidencePack, back_link: Option<&str>) -> String {
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
        "<h2>Controls</h2><table><thead><tr><th>Control</th><th>Status</th>\
                <th>Severity</th><th>Determination</th></tr></thead><tbody>",
    );
    for c in pack.controls_for_report() {
        let _ = write!(
            b,
            "<tr><td><strong>{}</strong><br><span class=\"scope\">{}</span></td>\
             <td>{}</td><td>{}</td><td class=\"scope\">{}</td></tr>",
            esc(&c.id),
            esc(&c.title),
            pill(c.status),
            esc(c.severity.label()),
            esc(&c.rationale)
        );
    }
    b.push_str("</tbody></table>");

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

/// One framework's row on the index.
pub struct IndexEntry {
    /// Link target, e.g. `"soc2.html"`.
    pub href: String,
    pub pack: EvidencePack,
}

/// Render the index page linking every framework.
pub fn index_page(entries: &[IndexEntry], workspace_label: &str) -> String {
    let mut b = String::with_capacity(8 * 1024);
    let _ = write!(b, "<h1>Compliance evidence</h1>");
    let _ = write!(
        b,
        "<p class=\"sub\">{} frameworks assessed against {}</p>",
        entries.len(),
        esc(workspace_label)
    );

    b.push_str(&disclaimer(true));

    b.push_str(
        "<h2>Frameworks</h2><table><thead><tr><th>Framework</th><th>Pass</th><th>Gap</th>\
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
    b.push_str("</tbody></table>");

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
    fn the_index_links_every_framework() {
        let entries = vec![
            IndexEntry {
                href: "soc2.html".into(),
                pack: pack(false).redacted(),
            },
            IndexEntry {
                href: "iso.html".into(),
                pack: pack(false).redacted(),
            },
        ];
        let html = index_page(&entries, "this repository");
        assert!(html.contains("soc2.html"));
        assert!(html.contains("iso.html"));
        assert!(html.contains("Determinacy"));
        assert!(html.contains("not a compliance attestation"));
    }

    #[test]
    fn the_index_explains_what_low_determinacy_means() {
        // Without this a reader reads 20% as "bad security" rather than
        // "mostly a governance framework".
        let html = index_page(&[], "x");
        assert!(html.contains("organizational"));
        assert!(html.contains("not visible in source code"));
    }
}
