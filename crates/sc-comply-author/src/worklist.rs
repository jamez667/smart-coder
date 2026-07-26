//! The auditor worklist — guidance for controls a code scan cannot settle.
//!
//! Most of any compliance framework is organizational, and the engine correctly
//! reports those as `Unknown`. But "unknown — obtain documentary evidence" is a
//! dead end for the person holding the report: *what* evidence, from *whom*, and
//! what will the auditor actually ask for?
//!
//! That question is judgment about audit practice, not detection, so a model is
//! the right tool. And critically it is a **safe** use: guidance cannot change a
//! verdict.
//!
//! # The line this module must not cross
//!
//! A model here is asked *"what evidence would settle this control?"* — never
//! *"is this control satisfied?"*. The distinction is the whole safety argument.
//! Pointing a model at an organizational control and asking it to judge would
//! re-introduce exactly the error the engine exists to prevent: it would find
//! `CODE_OF_CONDUCT.md` and conclude the board exercises oversight, confusing
//! *documented* with *operating*.
//!
//! So [`GuidanceItem`] carries no status field. There is no code path by which
//! guidance can flip a control, and [`validate`] rejects any text that tries to
//! reach a verdict anyway.

use sc_comply::evidence::{ControlResult, EvidencePack};
use sc_comply::status::ControlStatus;
use sc_model::{GenerateRequest, Message, ModelBackend};
use sc_proto::Result;

/// Guidance for one undeterminable control.
///
/// Deliberately has **no status field**. Guidance describes what to go and get;
/// it never asserts anything about whether the control is met.
#[derive(Debug, Clone, PartialEq)]
pub struct GuidanceItem {
    /// The control this belongs to, e.g. `"CC1.1"`.
    pub control_id: String,
    /// What to obtain — the concrete artifacts.
    pub evidence: Vec<String>,
    /// Who typically holds it.
    pub owner: String,
    /// What an auditor will probe beyond the document itself.
    pub auditor_asks: String,
}

/// Phrases that would turn guidance into a judgment about the control.
///
/// Guidance says "obtain the board minutes". It must never say "this control is
/// satisfied" or "this appears compliant" — that is the collector's job, and for
/// organizational controls it is nobody's job, because a repository cannot
/// settle it.
const FORBIDDEN: &[&str] = &[
    "is satisfied",
    "is compliant",
    "appears compliant",
    "this control passes",
    "control is met",
    "requirement is met",
    "no action needed",
    "already implemented",
    "evidence exists in the repository",
    "the repository demonstrates",
];

/// Guidance longer than this has stopped being a worklist item.
const MAX_CHARS_PER_FIELD: usize = 400;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// Reaches a verdict rather than describing evidence.
    Judgment(String),
    Empty,
    TooLong,
    /// Names a control that was not asked about.
    UnknownControl(String),
    /// The model could not be reached for one batch. The other batches stand.
    Backend(String),
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rejection::Judgment(p) => {
                write!(f, "reaches a verdict ({p:?}) instead of naming evidence")
            }
            Rejection::Empty => write!(f, "empty"),
            Rejection::TooLong => write!(f, "over the per-field length limit"),
            Rejection::UnknownControl(id) => write!(f, "invented control {id:?}"),
            Rejection::Backend(e) => write!(f, "model unreachable for this batch: {e}"),
        }
    }
}

/// Controls a worklist should cover: everything not determinable from source.
pub fn undeterminable(pack: &EvidencePack) -> Vec<&ControlResult> {
    pack.controls
        .iter()
        .filter(|c| c.status == ControlStatus::Unknown)
        .collect()
}

/// Build the prompt. Public so the brief is reviewable.
pub fn worklist_messages(framework: &str, controls: &[&ControlResult]) -> Vec<Message> {
    let system = "You help a team prepare for a compliance audit.\n\
         \n\
         You are given controls that an automated source-code scan could NOT determine — \
         almost always because they are organizational: policies, training records, vendor \
         contracts, incident procedures, board oversight. A repository cannot evidence any \
         of these.\n\
         \n\
         For each control, say what the team must GO AND OBTAIN.\n\
         \n\
         HARD RULES:\n\
         1. NEVER judge whether a control is met. You have not seen the evidence. Do not \
            write that anything is satisfied, compliant, implemented, or needs no action. \
            Your job is to describe what would settle it, not to settle it.\n\
         2. A document is not the same as a practice. \"We have a policy\" does not \
            evidence that the policy is followed, reviewed, or acknowledged. Name the \
            OPERATING evidence — sign-off records, dated reviews, ticket histories, \
            attendance logs — not just the document.\n\
         3. Be specific to the control. \"Provide documentation\" is useless. Name the \
            artifact an auditor actually asks for.\n\
         4. Name the realistic owner: HR, Legal, the security lead, the cloud provider, \
            the DPO, an external assessor.\n\
         5. If the evidence genuinely lives with a third party (a cloud provider's SOC 2 \
            report, a QSA's ASV scan), say so — that is useful, not a cop-out.\n\
         \n\
         Respond with ONLY a JSON array, one object per control:\n\
         [{\"control_id\":\"CC1.1\",\"evidence\":[\"...\",\"...\"],\"owner\":\"...\",\
         \"auditor_asks\":\"...\"}]\n\
         \n\
         `evidence`: 2-4 concrete artifacts. `owner`: who holds them, a few words. \
         `auditor_asks`: one sentence on what the auditor probes BEYOND the document — \
         the operating-effectiveness question."
        .to_string();

    let mut user = format!("Framework: {framework}\n\nControls the scan could not determine:\n\n");
    for c in controls {
        user.push_str(&format!("- {} — {}\n", c.id, c.title));
        if !c.intent.trim().is_empty() {
            user.push_str(&format!("  Intent: {}\n", one_line(&c.intent)));
        }
        // The pack's own rationale often already says WHY it is undeterminable;
        // that is the most useful hint for what to obtain instead.
        for k in &c.checks {
            if k.status == ControlStatus::Unknown && !k.rationale.trim().is_empty() {
                user.push_str(&format!("  Why undetermined: {}\n", one_line(&k.rationale)));
                break;
            }
        }
        user.push('\n');
    }
    user.push_str("Return the JSON array.");

    vec![Message::system(system), Message::user(user)]
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse and validate a worklist reply.
///
/// `expected` is the set of control ids that were asked about — anything else is
/// an invention and is dropped.
pub fn parse(
    reply: &str,
    expected: &[&ControlResult],
) -> std::result::Result<Vec<GuidanceItem>, Rejection> {
    let Some(arr) = sc_core::extract_json_array(reply) else {
        return Err(Rejection::Empty);
    };
    let items: Vec<serde_json::Value> = serde_json::from_str(arr).map_err(|_| Rejection::Empty)?;

    let ids: Vec<&str> = expected.iter().map(|c| c.id.as_str()).collect();
    let mut out = Vec::new();

    for item in items {
        let control_id = item
            .get("control_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if !ids.contains(&control_id.as_str()) {
            return Err(Rejection::UnknownControl(control_id));
        }

        let evidence: Vec<String> = item
            .get("evidence")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let owner = item
            .get("owner")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        let auditor_asks = item
            .get("auditor_asks")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();

        if evidence.is_empty() || owner.is_empty() {
            continue; // Incomplete item: skip it rather than publish a stub.
        }

        let combined = format!("{} {owner} {auditor_asks}", evidence.join(" "));
        validate(&combined)?;
        if evidence
            .iter()
            .any(|e| e.chars().count() > MAX_CHARS_PER_FIELD)
            || owner.chars().count() > MAX_CHARS_PER_FIELD
            || auditor_asks.chars().count() > MAX_CHARS_PER_FIELD
        {
            return Err(Rejection::TooLong);
        }

        out.push(GuidanceItem {
            control_id,
            evidence,
            owner,
            auditor_asks,
        });
    }

    Ok(out)
}

/// Reject guidance that reaches a verdict.
pub fn validate(text: &str) -> std::result::Result<(), Rejection> {
    let lower = text.to_lowercase();
    for p in FORBIDDEN {
        if lower.contains(p) {
            return Err(Rejection::Judgment((*p).to_string()));
        }
    }
    Ok(())
}

/// How many controls one model call is asked to cover.
///
/// The reply is structured JSON — roughly 120 tokens per item once a reasoning
/// model has spent its thinking budget — so a request must stay well inside
/// `MAX_TOKENS`. Twenty leaves better than 5x headroom.
///
/// The failure this bounds is silent. An over-long reply is cut mid-array,
/// `parse` reports `Empty`, and `generate` returns `Ok(vec![])` — indistinguishable
/// from a framework with nothing to guide on. Without batching, that arrives
/// exactly when the worklist becomes valuable: a large pack with many unknowns.
pub const BATCH_SIZE: usize = 20;

/// Token ceiling for one batch's reply.
const MAX_TOKENS: usize = 16_000;

/// Generate guidance for one framework's undeterminable controls.
///
/// Returns an empty vec rather than an error when the model is unavailable or
/// its output cannot be trusted: the report is complete without guidance, and a
/// failed export would be the worse outcome.
///
/// Controls are sent in batches of [`BATCH_SIZE`]. A batch that is rejected or
/// errors is reported through `on_reject` and skipped — partial guidance over
/// most of a framework beats none over all of it, and each batch's controls are
/// independent of every other's.
pub fn generate(
    backend: &dyn ModelBackend,
    pack: &EvidencePack,
    on_reject: &mut dyn FnMut(&Rejection),
) -> Result<Vec<GuidanceItem>> {
    let controls = undeterminable(pack);
    if controls.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for batch in controls.chunks(BATCH_SIZE) {
        let mut req = GenerateRequest::new(worklist_messages(&pack.framework.name, batch));
        // Generous: a reasoning model spends most of its budget before emitting,
        // and this asks for structured output over a whole batch.
        req.max_tokens = MAX_TOKENS;
        req.temperature = 0.2;

        // A transport failure on one batch must not discard the batches that
        // already succeeded. Reported as a rejection so the caller still sees it.
        let reply = match backend.generate(&req) {
            Ok(r) => r,
            Err(e) => {
                on_reject(&Rejection::Backend(e.to_string()));
                continue;
            }
        };
        match parse(&reply.content, batch) {
            Ok(items) => out.extend(items),
            Err(r) => on_reject(&r),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_comply::evidence::{CheckResult, FrameworkMeta};
    use sc_comply::status::Severity;
    use sc_model::MockBackend;

    fn control(id: &str, status: ControlStatus) -> ControlResult {
        ControlResult {
            id: id.into(),
            title: format!("{id} title"),
            section: Default::default(),
            clause: "c".into(),
            intent: "The entity demonstrates board oversight.".into(),
            severity: Severity::Medium,
            status,
            checks: vec![CheckResult {
                check_id: format!("{id}/doc"),
                kind: "file-exists".into(),
                status,
                weight: 1.0,
                evidence: vec![],
                note: None,
                rationale: "A published document evidences documentation, not operation.".into(),
            }],
            rationale: "r".into(),
            remediation: None,
        }
    }

    fn pack(controls: Vec<ControlResult>) -> EvidencePack {
        EvidencePack::new(
            FrameworkMeta {
                id: "soc2".into(),
                name: "SOC 2".into(),
                version: "1".into(),
                authority: "AICPA".into(),
            },
            "(redacted)".into(),
            "t".into(),
            "scope".into(),
            controls,
            vec![],
        )
    }

    const GOOD: &str = r#"[
      {"control_id":"CC1.1",
       "evidence":["Board meeting minutes showing security oversight discussions",
                   "Signed code of conduct acknowledgements for all staff",
                   "Dated annual policy review records"],
       "owner":"Company secretary and HR",
       "auditor_asks":"Whether oversight actually occurred on a recurring basis, evidenced by dated minutes rather than a policy stating that it should."}
    ]"#;

    #[test]
    fn selects_only_undeterminable_controls() {
        let p = pack(vec![
            control("CC1.1", ControlStatus::Unknown),
            control("CC6.1", ControlStatus::Gap),
            control("CC7.2", ControlStatus::Pass),
        ]);
        let ids: Vec<&str> = undeterminable(&p).iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["CC1.1"]);
    }

    #[test]
    fn the_prompt_forbids_judging_and_teaches_document_vs_practice() {
        let p = pack(vec![control("CC1.1", ControlStatus::Unknown)]);
        let msgs = worklist_messages("SOC 2", &undeterminable(&p));
        let system = &msgs[0].content;
        assert!(system.contains("NEVER judge whether a control is met"));
        assert!(
            system.contains("A document is not the same as a practice"),
            "the documented-vs-operating lesson is the point of this feature"
        );
        assert!(msgs[1].content.contains("CC1.1"));
    }

    #[test]
    fn parses_good_guidance() {
        let p = pack(vec![control("CC1.1", ControlStatus::Unknown)]);
        let items = parse(GOOD, &undeterminable(&p)).expect("parses");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].control_id, "CC1.1");
        assert_eq!(items[0].evidence.len(), 3);
        assert!(items[0].owner.contains("HR"));
    }

    #[test]
    fn rejects_guidance_that_reaches_a_verdict() {
        // The load-bearing guardrail. Guidance must describe evidence, never
        // conclude anything — otherwise it becomes a back door to the exact
        // documented-vs-operating error the engine exists to prevent.
        for bad in [
            "The repository demonstrates this control",
            "This control is satisfied by SECURITY.md",
            "No action needed here",
            "Already implemented in the codebase",
        ] {
            assert!(
                matches!(validate(bad), Err(Rejection::Judgment(_))),
                "accepted a verdict: {bad:?}"
            );
        }
    }

    #[test]
    fn a_judgment_anywhere_in_an_item_rejects_it() {
        let p = pack(vec![control("CC1.1", ControlStatus::Unknown)]);
        let bad = r#"[{"control_id":"CC1.1","evidence":["Board minutes"],
                      "owner":"HR","auditor_asks":"Nothing, this control is satisfied."}]"#;
        assert!(matches!(
            parse(bad, &undeterminable(&p)),
            Err(Rejection::Judgment(_))
        ));
    }

    #[test]
    fn rejects_an_invented_control_id() {
        // A model that guesses at controls it was not given is fabricating.
        let p = pack(vec![control("CC1.1", ControlStatus::Unknown)]);
        let bad = r#"[{"control_id":"CC9.9","evidence":["x"],"owner":"y","auditor_asks":"z"}]"#;
        assert!(matches!(
            parse(bad, &undeterminable(&p)),
            Err(Rejection::UnknownControl(_))
        ));
    }

    #[test]
    fn skips_incomplete_items_rather_than_publishing_stubs() {
        let p = pack(vec![control("CC1.1", ControlStatus::Unknown)]);
        let partial = r#"[{"control_id":"CC1.1","evidence":[],"owner":"","auditor_asks":""}]"#;
        assert!(parse(partial, &undeterminable(&p))
            .expect("parses")
            .is_empty());
    }

    #[test]
    fn guidance_has_no_status_field_by_construction() {
        // Structural, not conventional: there is no code path by which guidance
        // can flip a control, because the type cannot express one.
        let p = pack(vec![control("CC1.1", ControlStatus::Unknown)]);
        let items = parse(GOOD, &undeterminable(&p)).expect("parses");
        let item = &items[0];
        // Compiles only because GuidanceItem carries evidence/owner/asks — if a
        // status field were ever added this test is where the argument lives.
        assert!(!item.evidence.is_empty());
        assert!(!item.owner.is_empty());
    }

    #[test]
    fn generate_returns_items_on_a_good_reply() {
        let p = pack(vec![control("CC1.1", ControlStatus::Unknown)]);
        let backend = MockBackend::new([GOOD]);
        let mut seen = Vec::new();
        let mut on_reject = |r: &Rejection| seen.push(r.clone());
        let items = generate(&backend, &p, &mut on_reject).expect("generate");
        assert_eq!(items.len(), 1);
        assert!(seen.is_empty());
    }

    #[test]
    fn generate_drops_bad_guidance_rather_than_publishing_it() {
        let p = pack(vec![control("CC1.1", ControlStatus::Unknown)]);
        let bad = r#"[{"control_id":"CC1.1","evidence":["nothing"],"owner":"x",
                      "auditor_asks":"This control is compliant already."}]"#;
        let backend = MockBackend::new([bad]);
        let mut seen = Vec::new();
        let mut on_reject = |r: &Rejection| seen.push(r.clone());
        let items = generate(&backend, &p, &mut on_reject).expect("no error");
        assert!(items.is_empty());
        assert_eq!(seen.len(), 1);
    }

    /// A large pack is split across calls rather than sent as one request.
    ///
    /// This is the regression test for a silent failure: a single call covering
    /// 45 controls would truncate mid-array, `parse` would report `Empty`, and
    /// `generate` would return `Ok(vec![])` — the worklist quietly disappearing
    /// exactly when a framework is big enough to need one.
    #[test]
    fn a_large_pack_is_split_into_batches() {
        let controls: Vec<ControlResult> = (0..45)
            .map(|i| control(&format!("CC{i}"), ControlStatus::Unknown))
            .collect();
        let p = pack(controls);

        // One scripted reply per expected batch: 45 controls / 20 = 3.
        // MockBackend errors when the script runs out, so a 4th call would fail
        // the test, and an unused reply is caught by the item count below.
        let replies: Vec<String> = (0..3)
            .map(|b| {
                let ids: Vec<String> = (0..20)
                    .map(|i| b * 20 + i)
                    .filter(|n| *n < 45)
                    .map(|n| format!(
                        r#"{{"control_id":"CC{n}","evidence":["Dated board minutes","Signed acknowledgements"],
                            "owner":"Company secretary","auditor_asks":"Whether the review recurred on schedule."}}"#
                    ))
                    .collect();
                format!("[{}]", ids.join(","))
            })
            .collect();

        let backend = MockBackend::new(replies);
        let mut seen = Vec::new();
        let mut on_reject = |r: &Rejection| seen.push(r.clone());
        let items = generate(&backend, &p, &mut on_reject).expect("generate");

        assert!(seen.is_empty(), "unexpected rejections: {seen:?}");
        assert_eq!(items.len(), 45, "every control got guidance across batches");
    }

    /// One bad batch must not discard the batches that worked.
    ///
    /// Without this, a single malformed reply in the middle of a large framework
    /// would throw away every item already collected.
    #[test]
    fn a_rejected_batch_does_not_discard_the_others() {
        let controls: Vec<ControlResult> = (0..25)
            .map(|i| control(&format!("CC{i}"), ControlStatus::Unknown))
            .collect();
        let p = pack(controls);

        // Batch 1 (20 controls) is fine; batch 2 (5) reaches a verdict.
        let ok: Vec<String> = (0..20)
            .map(|n| format!(
                r#"{{"control_id":"CC{n}","evidence":["Dated board minutes","Signed acknowledgements"],
                    "owner":"Company secretary","auditor_asks":"Whether the review recurred on schedule."}}"#
            ))
            .collect();
        let good = format!("[{}]", ok.join(","));
        let bad = r#"[{"control_id":"CC20","evidence":["none"],"owner":"x",
                      "auditor_asks":"This control is compliant."}]"#;

        let backend = MockBackend::new([good, bad.to_string()]);
        let mut seen = Vec::new();
        let mut on_reject = |r: &Rejection| seen.push(r.clone());
        let items = generate(&backend, &p, &mut on_reject).expect("no error");

        assert_eq!(items.len(), 20, "the good batch survived the bad one");
        assert_eq!(seen.len(), 1, "the bad batch was reported, not swallowed");
    }

    #[test]
    fn a_pack_with_no_unknowns_costs_no_model_call() {
        // MockBackend with an empty script errors if called. This asserts we
        // never spend a call when there is nothing to ask about.
        let p = pack(vec![control("CC6.1", ControlStatus::Pass)]);
        let backend = MockBackend::new(Vec::<String>::new());
        let mut noop = |_: &Rejection| {};
        assert!(generate(&backend, &p, &mut noop)
            .expect("no call made")
            .is_empty());
    }
}
