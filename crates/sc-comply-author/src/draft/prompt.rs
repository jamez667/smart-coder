//! The drafting prompt.
//!
//! The model's whole job is to map a control's prose onto a **closed** check
//! vocabulary and, critically, to reason about what it means when the evidence
//! cannot be seen at all. Everything else in this crate exists to catch it
//! getting that second part wrong, so the prompt spends most of its length on it.
//!
//! Two deliberate choices:
//!
//! - **Ask for JSON, not TOML.** `sc-model` has no structured-output mode, so
//!   either way this is prompt-and-parse — but a model that invents a check kind
//!   fails at *our* deserialization with a precise error we can feed back,
//!   rather than emitting plausible TOML that breaks much later. We render the
//!   TOML ourselves, which also keeps drafts in the house style.
//! - **Teach the model to say "unknown".** The most common failure mode for a
//!   drafting model is eagerness: it will invent a check for a control that no
//!   repository can evidence. The prompt makes declaring a control undeterminable
//!   an explicitly correct answer, with a worked example.

use sc_model::Message;

/// The check vocabulary, rendered for the prompt. Kept in one place so it cannot
/// drift from `sc_comply::pack::CheckKind`.
const VOCABULARY: &str = r#"
Every check MUST use exactly one of these eight kinds. There are no others.
Inventing a kind is the single most common failure; if none of these fits, the
control is not evidenceable from source and you must say so (see below).

1. file-exists      {"kind":"file-exists","paths":["CONTRIBUTING.md","docs/CONTRIBUTING.md"]}
   True if ANY listed path exists. Directories count. Literal paths, not globs.

2. file-absent      {"kind":"file-absent","path":".env"}
   True if the path EXISTS. Note the inversion: "matching" means the unwanted
   file was found, so it pairs with on_match="gap".

3. regex-match-in-glob   {"kind":"regex-match-in-glob","glob":"**/*.yml","pattern":"cargo test"}
   True if at least one line in a glob-selected file matches.

4. regex-must-not-match  {"kind":"regex-must-not-match","glob":"**/*.rs","pattern":"api_key\\s*="}
   True if at least one line matches — i.e. "matching" is the BAD outcome.

5. symbol-exists    {"kind":"symbol-exists","name_pattern":"init_tracing","languages":["rust","python","csharp"]}
   A parsed function/class/struct DEFINITION whose name matches. Rust, Python
   and C# only — no other language can be parsed.

6. toml-path        {"kind":"toml-path","path":"Cargo.toml","key_path":"package.edition","assert":{"kind":"equals","value":"2021"}}
7. json-path        {"kind":"json-path","path":"pkg.json","key_path":"a.0.b","assert":{"kind":"gte","value":1}}
   Assert kinds: exists | non-empty | equals | not-equals | gte | lte | matches
   (matches takes {"kind":"matches","pattern":"..."}).

8. command-exit-code {"kind":"command-exit-code","command":"cargo audit","expect_codes":[0]}
   DISABLED by default at audit time and reported unknown. Only propose this if
   nothing else can express the control.
"#;

/// The `on_no_files` teaching block — the highest-value part of the prompt.
const OUTCOME_SEMANTICS: &str = r#"
Every check declares three outcomes. Each is one of: "pass", "gap", "unknown".

  on_match     — the condition held
  on_no_match  — we looked, and it did not hold
  on_no_files  — WE COULD NOT LOOK AT ALL

on_no_files is the one that matters and the one that is always got wrong.
It defaults to on_no_match, and that default is usually a lie.

"We looked and found nothing" and "we never looked" are different claims with
different legal weight. Conflating them makes the report state a confident
falsehood.

WORKED EXAMPLE — branch protection:
  A json-path check reads .github/settings.yml for a required-review count.
  That file is normally ABSENT, because branch protection lives in the VCS
  provider's API, not the repository. So:
      on_match     = "pass"      (the setting is there and satisfies the control)
      on_no_match  = "gap"       (the file exists and review is NOT required)
      on_no_files  = "unknown"   (no file: we cannot see the provider's config)
  Leaving on_no_files unset would report "code review is not enforced" for every
  repository on earth that configures it server-side. That is the failure mode.

WORKED EXAMPLE — a secret scan:
      on_match     = "gap"       (a key is committed)
      on_no_match  = "pass"      (we searched real files and found none — genuine)
      on_no_files  = "unknown"   (the glob matched nothing; we searched NOTHING)
  Here on_no_match="pass" is correct. Only the on_no_files slot needs care.

HARD RULES:
  * on_no_files must NEVER be "pass". Nothing that was never observed can be
    evidence that a control is satisfied.
  * Set on_no_files explicitly on every regex-*, symbol-exists, toml-path and
    json-path check. Do not rely on the default.
  * symbol-exists must always use on_no_files="unknown": only Rust, Python and
    C# can be parsed, so any other codebase is a blind spot, not a finding.
"#;

/// The escape hatch that keeps the model honest about organizational controls.
const UNDETERMINABLE: &str = r#"
MOST CONTROLS ARE NOT EVIDENCEABLE FROM SOURCE. In a framework like SOC 2 that
is roughly 85% of them: board oversight, vendor contracts, incident records,
access reviews, background checks, physical security, restore testing. A
repository contains none of that.

For such a control, DO NOT invent a check that can pass. Emit a check that maps
every outcome to "unknown" and explain in the rationale what the auditor must
obtain instead:

  {"id":"code-of-conduct-published","kind":"file-exists",
   "paths":["CODE_OF_CONDUCT.md"],
   "on_match":"unknown","on_no_match":"unknown","on_no_files":"unknown",
   "rationale":"A published code of conduct evidences that this control is DOCUMENTED, never that it OPERATES. The auditor must obtain acknowledgement records and evidence of board oversight."}

Declaring a control undeterminable is a CORRECT and valuable answer. Omitting it
would imply the framework was fully covered when it was not. Finding a Markdown
file is never evidence that a process operates.
"#;

/// Build the messages for one drafting request.
pub fn draft_messages(framework: &str, control_id: &str, control_text: &str) -> Vec<Message> {
    let system = format!(
        "You author checks for a compliance evidence engine. Given one control from a \
         framework, propose the checks that would evidence it from a source repository.\n\
         \n\
         Respond with ONLY a JSON array of check objects. No prose, no explanation \
         outside the JSON.\n\
         {VOCABULARY}\n\
         {OUTCOME_SEMANTICS}\n\
         {UNDETERMINABLE}\n\
         Each check object also needs:\n\
           \"id\"        — short kebab-case, unique within the control\n\
           \"rationale\" — one or two sentences on why this is evidence for the control,\n\
                        and what it does NOT establish\n\
         Optionally:\n\
           \"weight\"        — a positive number (default 1.0), for weighted controls\n\
           \"exclude_globs\" — paths to skip. A secret-detection pattern matches its own\n\
                            definition, so exclude test fixtures and vendored trees.\n\
         \n\
         Prefer 2-5 checks. Fewer, well-reasoned checks beat many weak ones."
    );

    let user = format!(
        "Framework: {framework}\n\
         Control: {control_id}\n\
         \n\
         Control text:\n{control_text}\n\
         \n\
         Return a JSON array of check objects for this control."
    );

    vec![Message::system(system), Message::user(user)]
}

/// Build a retry request that feeds the previous failure back.
///
/// Kept as its own function so the error text is verbatim: a model corrects a
/// concrete "kind `grep-file` is not in the vocabulary" far more reliably than a
/// paraphrase.
pub fn retry_messages(
    framework: &str,
    control_id: &str,
    control_text: &str,
    previous: &str,
    errors: &[String],
) -> Vec<Message> {
    let mut messages = draft_messages(framework, control_id, control_text);
    messages.push(Message::assistant(previous.to_string()));
    messages.push(Message::user(format!(
        "That response was rejected:\n\n{}\n\nFix these specific problems and return \
         the corrected JSON array. Use only the eight check kinds listed above.",
        errors
            .iter()
            .map(|e| format!("  - {e}"))
            .collect::<Vec<_>>()
            .join("\n")
    )));
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_model::Role;

    #[test]
    fn the_prompt_teaches_the_on_no_files_rule() {
        let msgs = draft_messages("SOC 2", "CC8.1", "Change management.");
        let system = &msgs[0].content;
        assert!(system.contains("on_no_files"));
        assert!(
            system.contains("must NEVER be \"pass\""),
            "the one hard rule must be stated"
        );
        assert!(
            system.contains("settings.yml"),
            "the branch-protection worked example carries the lesson"
        );
    }

    #[test]
    fn the_prompt_lists_every_check_kind() {
        let system = &draft_messages("F", "C", "t")[0].content;
        for kind in [
            "file-exists",
            "file-absent",
            "regex-match-in-glob",
            "regex-must-not-match",
            "symbol-exists",
            "toml-path",
            "json-path",
            "command-exit-code",
        ] {
            assert!(system.contains(kind), "vocabulary missing {kind}");
        }
    }

    #[test]
    fn the_prompt_makes_undeterminable_an_acceptable_answer() {
        // Without this the model invents a passing check for board oversight.
        let system = &draft_messages("F", "C", "t")[0].content;
        assert!(system.contains("CORRECT and valuable answer"));
        assert!(system.contains("DOCUMENTED, never that it OPERATES"));
    }

    #[test]
    fn the_prompt_states_the_symbol_language_limit() {
        // Stated twice: in the vocabulary entry and again as a hard rule, since
        // a symbol check without on_no_files="unknown" reports a false negative
        // on every codebase sc-index cannot parse.
        let system = &draft_messages("F", "C", "t")[0].content;
        assert!(
            system.contains("only Rust, Python and\n    C# can be parsed"),
            "the hard rule must name the supported languages"
        );
        assert!(
            system.contains("no other language can be parsed"),
            "the vocabulary entry must state the limit too"
        );
    }

    #[test]
    fn messages_are_system_then_user() {
        let msgs = draft_messages("SOC 2", "CC1.1", "Integrity and ethics.");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, Role::System);
        assert_eq!(msgs[1].role, Role::User);
        assert!(msgs[1].content.contains("CC1.1"));
        assert!(msgs[1].content.contains("Integrity and ethics."));
    }

    #[test]
    fn retry_replays_the_conversation_and_names_the_errors() {
        let msgs = retry_messages(
            "SOC 2",
            "CC8.1",
            "Change management.",
            "[{\"kind\":\"grep-file\"}]",
            &["unknown check kind `grep-file`".to_string()],
        );
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[2].role, Role::Assistant);
        assert_eq!(msgs[3].role, Role::User);
        assert!(
            msgs[3].content.contains("grep-file"),
            "the error must be verbatim so the model can act on it"
        );
    }
}
