//! Coverage: which crates no spec describes.
//!
//! The reverse direction from anchors, and it needs none — a crate is *governed*
//! when some spec names it, in prose or in an anchor. This catches the second
//! failure mode: not a false document, but an **incomplete** one (spec 17).
//!
//! ## Granularity, and why it is crates only
//!
//! Spec 17 says "crate and top-level module". Measured against this repo: crates
//! yield 2 findings; top-level modules yield dozens (`sc-comply` alone has 13).
//! The spec's own warning — "pitched finer, the check produces noise, and a noisy
//! check is one that gets `--no-verify`'d and then deleted" — argues against the
//! finer pitch, so v1 is crates. Module granularity drops in trivially once crate
//! coverage is clean.
//!
//! ## The matching rule
//!
//! Whole-token only. The trap is real and lives in this repo:
//! `docs/specs/00-overview.md` contains "run tests, iterate" — the English verb.
//! A substring match would silently mark `sc-iterate` as governed by an accident
//! of prose, and the check would report clean while a crate went undescribed.

use crate::manifest::{Crate, Workspace};
use crate::scan::SpecDoc;
use crate::status::ClaimStatus;

/// Why a crate counts as governed — kept so the report shows its receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimKind {
    /// An anchor targets this crate. The strongest form: a machine-checkable
    /// claim, not just a mention.
    Anchor,
    /// The crate name appears as a whole token in prose.
    Prose,
}

/// One crate and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrateCoverage {
    pub name: String,
    pub status: ClaimStatus,
    /// The spec that claims it, and how. `None` when ungoverned.
    pub claimed_by: Option<(String, usize, ClaimKind)>,
}

/// Which crates are described by which specs.
///
/// `anchor_targets` are the crate lib-names every *resolvable* anchor pointed at,
/// so an anchor governs its crate even when the spec never spells the name in
/// prose.
pub fn coverage(
    ws: &Workspace,
    specs: &[SpecDoc],
    anchor_targets: &[(String, String, usize)],
) -> Vec<CrateCoverage> {
    ws.crates
        .iter()
        .map(|krate| {
            // An anchor is the stronger claim, so it is checked first.
            if let Some((_, spec, line)) = anchor_targets
                .iter()
                .find(|(lib, _, _)| *lib == krate.lib_name)
            {
                return CrateCoverage {
                    name: krate.name.clone(),
                    status: ClaimStatus::Ok,
                    claimed_by: Some((spec.clone(), *line, ClaimKind::Anchor)),
                };
            }
            match find_prose_mention(krate, specs, ws) {
                Some((spec, line)) => CrateCoverage {
                    name: krate.name.clone(),
                    status: ClaimStatus::Ok,
                    claimed_by: Some((spec, line, ClaimKind::Prose)),
                },
                None => CrateCoverage {
                    name: krate.name.clone(),
                    status: ClaimStatus::Ungoverned,
                    claimed_by: None,
                },
            }
        })
        .collect()
}

/// The first spec mentioning `krate` by name, as a whole token.
fn find_prose_mention(krate: &Crate, specs: &[SpecDoc], ws: &Workspace) -> Option<(String, usize)> {
    // A crate whose name is a prefix of a longer crate's name must not be
    // governed by a mention of that longer one: `sc-comply-author` in prose says
    // nothing about `sc-comply`.
    let longer: Vec<&str> = ws
        .crates
        .iter()
        .map(|c| c.name.as_str())
        .filter(|n| n.len() > krate.name.len() && n.starts_with(&krate.name))
        .collect();

    // Both spellings: `sc-comply` in prose, `sc_comply` in an anchor or code.
    let names = [krate.name.clone(), krate.lib_name.clone()];

    for spec in specs {
        for (i, line) in spec.contents.lines().enumerate() {
            // Anchors are attributed as `Anchor`, so they are stripped here
            // rather than double-counted as prose.
            let prose = strip_anchors(line);
            for name in &names {
                if mentions_token(&prose, name, &longer) {
                    return Some((spec.path.clone(), i + 1));
                }
            }
        }
    }
    None
}

/// Remove `<!--@ … -->` spans so an anchor is not also read as prose.
fn strip_anchors(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(start) = rest.find("<!--@") {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        match after.find("-->") {
            Some(end) => rest = &after[end + 3..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Does `haystack` contain `name` as a whole token?
///
/// The boundary must exclude `-` and `_` as well as alphanumerics, or `sc-comply`
/// would match inside `sc-comply-author`. `longer` names are additionally
/// subtracted so a line mentioning only the longer crate never governs the
/// shorter one.
fn mentions_token(haystack: &str, name: &str, longer: &[&str]) -> bool {
    let mut from = 0usize;
    while let Some(rel) = haystack[from..].find(name) {
        let start = from + rel;
        let end = start + name.len();
        let before_ok =
            start == 0 || !is_token_char(haystack[..start].chars().next_back().unwrap());
        let after_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|c| !is_token_char(c));
        if before_ok && after_ok && !inside_longer(haystack, start, longer) {
            return true;
        }
        from = end;
    }
    false
}

/// Is this occurrence actually part of a longer crate's name?
fn inside_longer(haystack: &str, at: usize, longer: &[&str]) -> bool {
    longer
        .iter()
        .any(|l| haystack[at..].starts_with(l) || haystack[..at + 1].ends_with(l))
}

/// Characters that continue an identifier, for boundary purposes. `-` counts, so
/// `sc-comply` does not match inside `sc-comply-author`.
fn is_token_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Crate;
    use crate::scan::read_specs;
    use crate::test_support::repo_root;

    fn krate(name: &str) -> Crate {
        Crate {
            name: name.to_string(),
            lib_name: name.replace('-', "_"),
            dir: format!("crates/{name}"),
        }
    }

    fn spec(path: &str, contents: &str) -> SpecDoc {
        SpecDoc {
            path: path.to_string(),
            contents: contents.to_string(),
        }
    }

    fn status_of(cov: &[CrateCoverage], name: &str) -> ClaimStatus {
        cov.iter().find(|c| c.name == name).unwrap().status
    }

    #[test]
    fn a_crate_named_in_prose_is_governed() {
        let ws = Workspace {
            crates: vec![krate("sc-web")],
        };
        let specs = vec![spec(
            "docs/specs/01.md",
            "The `sc-web` dashboard streams.\n",
        )];
        let cov = coverage(&ws, &specs, &[]);
        assert_eq!(cov[0].status, ClaimStatus::Ok);
        let (spec_path, line, kind) = cov[0].claimed_by.clone().unwrap();
        assert_eq!(spec_path, "docs/specs/01.md");
        assert_eq!(line, 1);
        assert_eq!(kind, ClaimKind::Prose);
    }

    #[test]
    fn a_crate_named_only_by_an_anchor_is_governed_and_says_so() {
        let ws = Workspace {
            crates: vec![krate("sc-web")],
        };
        let specs = vec![spec(
            "docs/specs/18.md",
            "See <!--@ sc_web::mint_token -->\n",
        )];
        let targets = vec![("sc_web".to_string(), "docs/specs/18.md".to_string(), 1)];
        let cov = coverage(&ws, &specs, &targets);
        assert_eq!(cov[0].status, ClaimStatus::Ok);
        // The stronger claim wins the attribution: an anchor is machine-checked.
        assert_eq!(cov[0].claimed_by.clone().unwrap().2, ClaimKind::Anchor);
    }

    #[test]
    fn an_unmentioned_crate_is_ungoverned() {
        let ws = Workspace {
            crates: vec![krate("sc-ghost")],
        };
        let specs = vec![spec("docs/specs/01.md", "Nothing relevant here.\n")];
        let cov = coverage(&ws, &specs, &[]);
        assert_eq!(cov[0].status, ClaimStatus::Ungoverned);
        assert!(cov[0].claimed_by.is_none());
    }

    #[test]
    fn the_english_verb_iterate_does_not_govern_sc_iterate() {
        // THE false-positive guard, verbatim from docs/specs/00-overview.md:32.
        // A substring match would report this crate governed by an accident of
        // prose, and the check would read clean while a crate went undescribed.
        let ws = Workspace {
            crates: vec![krate("sc-iterate")],
        };
        let specs = vec![spec(
            "docs/specs/00-overview.md",
            "   repo, make a focused change across a few files, run tests, iterate.\n",
        )];
        assert_eq!(
            coverage(&ws, &specs, &[])[0].status,
            ClaimStatus::Ungoverned
        );
    }

    #[test]
    fn a_longer_crate_name_does_not_govern_the_shorter_one() {
        // `sc-comply-author` in prose says nothing about `sc-comply`.
        let ws = Workspace {
            crates: vec![krate("sc-comply"), krate("sc-comply-author")],
        };
        let specs = vec![spec(
            "docs/specs/14.md",
            "The `sc-comply-author` lints run at authoring time.\n",
        )];
        let cov = coverage(&ws, &specs, &[]);
        assert_eq!(status_of(&cov, "sc-comply-author"), ClaimStatus::Ok);
        assert_eq!(
            status_of(&cov, "sc-comply"),
            ClaimStatus::Ungoverned,
            "the prefix must not be governed by the longer name"
        );
    }

    #[test]
    fn an_anchor_is_not_also_counted_as_a_prose_mention() {
        // Attribution must be the strongest form, once — otherwise the receipt
        // in the report points at a comment rather than the sentence.
        let stripped = strip_anchors("Text <!--@ sc_web::mint_token --> more text");
        assert!(!stripped.contains("sc_web"), "{stripped}");
        assert!(stripped.contains("Text") && stripped.contains("more text"));
    }

    #[test]
    fn a_crate_mentioned_inside_a_code_fence_is_still_governed() {
        // A crate drawn in an architecture diagram is genuinely described.
        // Excluding fences would flip it ungoverned on a technicality.
        let ws = Workspace {
            crates: vec![krate("sc-swarm")],
        };
        let specs = vec![spec("docs/specs/08.md", "```text\n sc-swarm \n```\n")];
        assert_eq!(coverage(&ws, &specs, &[])[0].status, ClaimStatus::Ok);
    }

    #[test]
    fn both_spellings_count() {
        let ws = Workspace {
            crates: vec![krate("sc-workflow")],
        };
        // The Rust path form, as it appears in code and prose about code.
        let specs = vec![spec("docs/specs/09.md", "the `sc_workflow` runner\n")];
        assert_eq!(coverage(&ws, &specs, &[])[0].status, ClaimStatus::Ok);
    }

    #[test]
    fn token_matching_respects_boundaries() {
        assert!(mentions_token("uses sc-web today", "sc-web", &[]));
        assert!(mentions_token("`sc-web`", "sc-web", &[]));
        assert!(mentions_token("sc-web", "sc-web", &[]));
        // Not a whole token.
        assert!(!mentions_token("sc-webby", "sc-web", &[]));
        assert!(!mentions_token("my-sc-web", "sc-web", &[]));
    }

    #[test]
    fn the_real_repo_reports_only_the_genuinely_undescribed_crates() {
        // Against the actual workspace: 17 of 19 crates are described. Asserting
        // the SHAPE (a small number, and specifically not everything) rather than
        // an exact list, so adding a crate does not break this test.
        let root = repo_root();
        let ws = Workspace::load(&root);
        let specs = read_specs(&root);
        let cov = coverage(&ws, &specs, &[]);

        let ungoverned: Vec<&str> = cov
            .iter()
            .filter(|c| c.status == ClaimStatus::Ungoverned)
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            ungoverned.len() < 5,
            "coverage should be signal, not noise: {ungoverned:?}"
        );
        // The crates this spec exists to describe are all claimed.
        for named in ["sc-comply", "sc-swarm", "sc-workflow", "sc-index", "sc-web"] {
            assert_eq!(status_of(&cov, named), ClaimStatus::Ok, "{named}");
        }
    }
}
