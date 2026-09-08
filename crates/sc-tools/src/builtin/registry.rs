//! The tool schemas: what the model is offered, and in what order.
//!
//! The full v1 surface ([`default_registry`]), the six-tool build menu it is
//! trimmed to ([`six_tool_registry`]), the read-only investigate menu
//! ([`read_only_registry`]) and the worker surface ([`minimal_worker_registry`]).
//!
//! Every description is ONE neutral sentence saying what the tool does. Which tool
//! to prefer for which shape of change is policy, and policy lives in the task
//! prefix the agent loop writes (sc-core's config), where it can be tuned per run;
//! a schema that says "PREFER this" argues with that prefix and, on a trimmed
//! registry, argues for a tool the model may not even have.

use crate::spec::{ParamSpec, ParamType, Permission, SideEffect, ToolRegistry, ToolSpec};

/// The default registry: the v1 built-in tools, in a stable order.
pub fn default_registry() -> ToolRegistry {
    ToolRegistry::new(vec![
        ToolSpec {
            name: "read_file",
            // Names no OTHER tool. The registry may be trimmed (a task run offers six
            // of these, not all sixteen), and steering toward a tool the model does not
            // have wastes the turn and teaches it to distrust the harness. This used to
            // point at `search_code`, which a trimmed run has no way to call.
            description: "Read a UTF-8 text file with every line numbered, or just a window of \
                          it given `start` (1-based line) and `limit` (line count).",
            params: vec![
                ParamSpec::new(
                    "path",
                    ParamType::String,
                    "file path relative to the project root",
                ),
                ParamSpec::new(
                    "start",
                    ParamType::OptionalInteger,
                    "1-based line to start reading from (omit to read from the top)",
                ),
                ParamSpec::new(
                    "limit",
                    ParamType::OptionalInteger,
                    "how many lines to read from `start` (omit for a capped default)",
                ),
            ],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "list_dir",
            description: "List the entries of a directory (non-recursive).",
            params: vec![ParamSpec::new(
                "path",
                ParamType::String,
                "directory path relative to the project root ('.' for root)",
            )],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "search_code",
            // The description and the parameter say the SAME thing: a pattern gets a regex
            // grep (plain text with no metacharacters matches literally), a question gets
            // the ranked index. They used to disagree -- "REGEX" above, "the literal text"
            // below -- and a model reads both.
            description: "Search the project: a regex or code pattern (e.g. `fn \\w+`, \
                          `ShipRole::`) returns file:line hits, and a plain-English question \
                          returns the functions most relevant to it.",
            params: vec![ParamSpec::new(
                "query",
                ParamType::String,
                "a regex or code pattern (plain text with no metacharacters matches \
                 literally), or a plain-English question",
            )],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "find_symbol",
            description: "Locate where a function/type/class is defined; returns path:line.",
            params: vec![ParamSpec::new(
                "name",
                ParamType::String,
                "the symbol name to locate (exact)",
            )],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "cargo_info",
            // Names no other tool, per the rule below. "crate" rather than "package"
            // because that is the word the manifests and the directory names use.
            description: "Describe this Rust workspace's crates from their Cargo.toml \
                          manifests (what a crate is for, what it depends on, and what \
                          depends on it), for the named `crate` or for every crate when it \
                          is omitted.",
            params: vec![ParamSpec::new(
                "crate",
                ParamType::OptionalString,
                "the crate to describe, e.g. 'sc-proto' (omit to list every crate)",
            )],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "profile_hotspots",
            // Names no other tool, per the rule above. Says READ, not "profile", because the
            // tool cannot record one -- a model that thinks it can will ask for something the
            // harness has no way to deliver.
            description: "Read an existing recorded CPU profile (a folded-stack file) and list \
                          the functions that cost the most time, hottest first; it does not \
                          run a profiler.",
            params: vec![
                ParamSpec::new(
                    "path",
                    ParamType::String,
                    "path to the folded-stack file, relative to the project root (e.g. \
                     'target/sc-profile.folded')",
                ),
                ParamSpec::new(
                    "limit",
                    ParamType::OptionalInteger,
                    "how many frames to list (omit for a sensible default)",
                ),
            ],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "write_file",
            description: "Create or overwrite a file with the given full contents.",
            params: vec![
                ParamSpec::new(
                    "path",
                    ParamType::String,
                    "file path relative to the project root",
                ),
                ParamSpec::new("content", ParamType::String, "the full new file contents"),
            ],
            side_effect: SideEffect::Mutating,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "create_file",
            description: "Create a NEW file with the given contents; fails if it already exists.",
            params: vec![
                ParamSpec::new(
                    "path",
                    ParamType::String,
                    "file path relative to the project root",
                ),
                ParamSpec::new("content", ParamType::String, "the full file contents"),
            ],
            side_effect: SideEffect::Mutating,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "append_file",
            description: "Append content to the end of a file, creating it if absent.",
            params: vec![
                ParamSpec::new(
                    "path",
                    ParamType::String,
                    "file path relative to the project root",
                ),
                ParamSpec::new(
                    "content",
                    ParamType::String,
                    "text to append at the end of the file",
                ),
            ],
            side_effect: SideEffect::Mutating,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "edit_file",
            description: "Replace an exact snippet in a file: old_str must occur exactly once.",
            params: vec![
                ParamSpec::new(
                    "path",
                    ParamType::String,
                    "file path relative to the project root",
                ),
                ParamSpec::new(
                    "old_str",
                    ParamType::String,
                    "the exact text to replace (must appear exactly once)",
                ),
                ParamSpec::new("new_str", ParamType::String, "the replacement text"),
            ],
            side_effect: SideEffect::Mutating,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "edit_lines",
            description: "Replace lines start..=end (1-based, inclusive) of a file with \
                          new_text; to insert before line N without deleting anything, pass \
                          start=N and end=N-1.",
            params: vec![
                ParamSpec::new(
                    "path",
                    ParamType::String,
                    "file path relative to the project root",
                ),
                ParamSpec::new(
                    "start",
                    ParamType::Integer,
                    "first line to replace (1-based)",
                ),
                ParamSpec::new(
                    "end",
                    ParamType::Integer,
                    "last line to replace (1-based, inclusive); start-1 inserts before start",
                ),
                ParamSpec::new(
                    "new_text",
                    ParamType::String,
                    "the replacement text for those lines (may be multiple lines)",
                ),
            ],
            side_effect: SideEffect::Mutating,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "read_function",
            description: "Read one function or method by name (Rust/Python/C#), its whole body \
                          with every line numbered.",
            params: vec![
                ParamSpec::new(
                    "path",
                    ParamType::String,
                    "file path relative to the project root",
                ),
                ParamSpec::new(
                    "name",
                    ParamType::String,
                    "the function/method name to read",
                ),
            ],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "edit_function",
            description: "Replace a whole function or method by name (Rust/Python/C#) with \
                          new_body, the full new text of the function.",
            params: vec![
                ParamSpec::new(
                    "path",
                    ParamType::String,
                    "file path relative to the project root",
                ),
                ParamSpec::new(
                    "name",
                    ParamType::String,
                    "the function/method name to replace",
                ),
                ParamSpec::new(
                    "new_body",
                    ParamType::String,
                    "the FULL new text of the function (signature + body), replacing the old one",
                ),
            ],
            side_effect: SideEffect::Mutating,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "run_command",
            description: "Run a shell command in the workspace; returns exit code + output.",
            params: vec![ParamSpec::new(
                "command",
                ParamType::String,
                "the shell command line to run",
            )],
            side_effect: SideEffect::Destructive,
            permission: Permission::Confirm,
        },
        ToolSpec {
            name: "run_verification",
            description: "Run the project's configured test command; returns per-test results.",
            params: vec![],
            side_effect: SideEffect::Mutating,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "update_plan",
            description: "Replace your step plan with a new ordered list of short steps.",
            params: vec![ParamSpec::new(
                "steps",
                ParamType::String,
                "the new plan as a JSON array of short step strings",
            )],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "ask_user",
            description: "Escalate a genuine blocker for advice instead of guessing.",
            params: vec![ParamSpec::new(
                "question",
                ParamType::String,
                "the specific question or blocker",
            )],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "finish",
            description: "Declare the task complete.",
            params: vec![],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        },
    ])
}

/// The six-tool build menu: `read_file`, `edit_file`, `write_file`, `run_command`,
/// `run_verification`, `finish`.
///
/// Measured on the SWE-bench path: six tools got `run_command` 12/12; the full
/// sixteen got 3/12. A big menu makes a small model deliberate instead of act --
/// it reads and re-reads instead of editing. This is the menu a scored task run
/// and the desktop iterate run offer; [`default_registry`] keeps every tool for
/// the flows that want them.
///
/// Filtered from [`default_registry`] by name so each spec has exactly one
/// definition; the order is the default registry's.
pub fn six_tool_registry() -> ToolRegistry {
    const KEEP: [&str; 6] = [
        "read_file",
        "edit_file",
        "write_file",
        "run_command",
        "run_verification",
        "finish",
    ];
    let specs: Vec<ToolSpec> = default_registry()
        .specs()
        .iter()
        .filter(|s| KEEP.contains(&s.name))
        .cloned()
        .collect();
    debug_assert_eq!(specs.len(), KEEP.len(), "a kept tool is missing by name");
    ToolRegistry::new(specs)
}

/// A READ-ONLY registry: every built-in tool that cannot change the workspace, plus
/// `finish`.
///
/// For answering a question ABOUT the code — "why is the star trail thin before it gets
/// thick?" — where the answer requires reading the source but nothing should be edited.
/// Without it that question reached a model holding no tools at all, which could only
/// reason from the README/TODO and the one open file; it correctly said "I can't see the
/// rendering code" and guessed, and the guess read as the model being stupid.
///
/// Derived from [`SideEffect::ReadOnly`] rather than a hardcoded name list, so a tool
/// added later is classified by what it DOES. A hand-written list is a second place to
/// remember, and the one that silently goes stale.
pub fn read_only_registry() -> ToolRegistry {
    // ReadOnly is the safety property, but it is not the whole selection: `update_plan`
    // and `ask_user` are read-only and still wrong here. They are workflow tools for a
    // build run, and offering them to a question-answering loop invites a model to
    // update a plan or bounce the question back instead of opening the file. Six tools
    // beat sixteen for exactly this reason -- a big menu makes a small model deliberate
    // rather than act.
    //
    // `cargo_info` is excluded on that last ground alone, not because it is unsafe or
    // useless here -- a question about which crate owns a behaviour is exactly what it
    // answers. But this menu is the one thing in the project with a measurement behind
    // it (six tools: `run_command` 12/12; sixteen: 3/12), and that measurement compared
    // six against sixteen -- it says nothing about seven. Growing it on a hunch would
    // spend the only frozen model-facing contract here to find out. It joins the day a
    // probe says it earns the slot, the way `SC_INVESTIGATE_LEADS` was decided.
    //
    // `profile_hotspots` is excluded on that same ground. It is read-only and genuinely
    // useful -- "which function is slow" is a question this loop could be asked -- but it
    // needs a profile file that usually does not exist, so on most repositories it is a
    // seventh menu entry that can only answer "could not read". It joins when a probe says
    // it earns the slot, not before.
    const EXCLUDE: [&str; 4] = ["update_plan", "ask_user", "cargo_info", "profile_hotspots"];
    let specs: Vec<ToolSpec> = default_registry()
        .specs()
        .iter()
        .filter(|s| s.side_effect == SideEffect::ReadOnly && !EXCLUDE.contains(&s.name))
        .cloned()
        .collect();
    debug_assert!(
        specs.iter().any(|s| s.name == "read_file"),
        "a read-only registry without read_file cannot investigate anything"
    );
    debug_assert!(
        specs.iter().any(|s| s.name == "finish"),
        "without finish the loop cannot terminate cleanly"
    );
    // `finish` needs somewhere to PUT the answer.
    //
    // The build registry's `finish` takes no parameters -- it is a signal that work is done,
    // and the work is the edits on disk. For a question there are no edits: the answer is the
    // only deliverable, and with a parameterless `finish` there was nowhere to put it.
    // Measured live: the model wrote a correct, complete diagnosis as prose, the harness
    // rejected it as "no JSON tool object", and the next turn called `{"tool":"finish"}` with
    // nothing in it. The run reported success and returned an empty answer.
    let specs = specs
        .into_iter()
        .map(|mut s| {
            if s.name == "finish" {
                s.description = "Give your final answer and end the investigation. Put the COMPLETE answer in `summary` -- it is the only thing the user sees.";
                s.params = vec![ParamSpec::new(
                    "summary",
                    ParamType::String,
                    "the full answer: the file and line, the cause, and the exact fix",
                )];
            }
            s
        })
        .collect();
    ToolRegistry::new(specs)
}

/// A deliberately tiny registry for a focus-scoped worker (spec 04/08): just the
/// three tools it ever needs — `edit_file`, `run_verification`, `finish`. The
/// worker is already shown the file's current contents every turn, so it never
/// needs to read/search/list/plan/ask. Fewer choices = a dumb model that acts
/// instead of dithering between twelve options.
pub fn minimal_worker_registry() -> ToolRegistry {
    ToolRegistry::new(vec![
        ToolSpec {
            name: "edit_file",
            description: "Replace an exact snippet: old_str must match the shown file once.",
            params: vec![
                ParamSpec::new("path", ParamType::String, "the file to edit"),
                ParamSpec::new(
                    "old_str",
                    ParamType::String,
                    "exact text to replace, copied from the shown file",
                ),
                ParamSpec::new("new_str", ParamType::String, "the replacement text"),
            ],
            side_effect: SideEffect::Mutating,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "edit_lines",
            description: "Replace lines start..=end (1-based) with new_text — address by line \
                          NUMBER, no snippet to copy. Best for a large file. end=start-1 inserts.",
            params: vec![
                ParamSpec::new("path", ParamType::String, "the file to edit"),
                ParamSpec::new(
                    "start",
                    ParamType::Integer,
                    "first line to replace (1-based)",
                ),
                ParamSpec::new(
                    "end",
                    ParamType::Integer,
                    "last line (inclusive); start-1 to insert",
                ),
                ParamSpec::new("new_text", ParamType::String, "the replacement text"),
            ],
            side_effect: SideEffect::Mutating,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "run_verification",
            description: "Run the tests and see which pass or fail.",
            params: vec![],
            side_effect: SideEffect::Mutating,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "finish",
            description: "Stop — only once the tests pass.",
            params: vec![],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        },
    ])
}

#[cfg(test)]
mod read_only_registry_tests {
    use super::*;

    /// **A read-only registry must be able to investigate, and must not be able to edit.**
    ///
    /// Both halves matter. Without `read_file` it answers questions about code it cannot
    /// see — the failure this registry exists to fix. With `write_file` it is not read-only
    /// at all, and a question about the code could silently change it.
    #[test]
    fn it_can_read_and_search_but_never_write() {
        let r = read_only_registry();
        let names: Vec<&str> = r.specs().iter().map(|s| s.name).collect();

        for needed in ["read_file", "list_dir", "search_code", "finish"] {
            assert!(names.contains(&needed), "missing {needed}, got {names:?}");
        }
        for forbidden in [
            "write_file",
            "edit_file",
            "edit_lines",
            "run_command",
            // Read-only, but workflow tools rather than investigation ones.
            "update_plan",
            "ask_user",
        ] {
            assert!(
                !names.contains(&forbidden),
                "{forbidden} can change the workspace and must not be offered"
            );
        }
        // The real invariant, not just the named cases above: nothing mutating gets through.
        assert!(
            r.specs()
                .iter()
                .all(|s| s.side_effect == SideEffect::ReadOnly),
            "every tool in a read-only registry must be ReadOnly"
        );
    }
}

#[cfg(test)]
mod investigate_shape {
    use super::*;

    /// **The investigate registry must be small.**
    ///
    /// Sixteen tools measured 3/12 where six measured 12/12: a big menu makes a small
    /// model deliberate instead of act. This one exists to read code and stop, so it
    /// should stay close to that size — a regression that quietly re-adds tools would
    /// show up here rather than as a model that suddenly dithers.
    #[test]
    fn it_stays_a_short_menu() {
        let n = read_only_registry().specs().len();
        assert!(
            (4..=7).contains(&n),
            "expected a short investigation menu, got {n} tools"
        );
    }
}

#[cfg(test)]
mod finish_carries_the_answer {
    use super::*;

    /// **On a read-only run the answer IS the deliverable, so `finish` must have room for it.**
    ///
    /// The build registry's `finish` takes no parameters: work is done, and the work is the
    /// edits on disk. Reused for a question that leaves nowhere to put the answer — measured
    /// live, the model wrote a correct diagnosis as prose, the harness rejected it as "no
    /// JSON tool object", and the next turn sent `{"tool":"finish"}` with nothing in it.
    #[test]
    fn the_read_only_finish_takes_a_summary() {
        let r = read_only_registry();
        let finish = r
            .specs()
            .iter()
            .find(|s| s.name == "finish")
            .expect("finish must exist or the loop cannot end");
        assert!(
            finish.params.iter().any(|p| p.name == "summary"),
            "read-only finish needs a summary to carry the answer, got {:?}",
            finish.params.iter().map(|p| p.name).collect::<Vec<_>>()
        );
        // The build registry's finish stays parameterless — this must not leak into it.
        let build_finish = default_registry()
            .specs()
            .iter()
            .find(|s| s.name == "finish")
            .cloned()
            .expect("finish");
        assert!(
            build_finish.params.is_empty(),
            "the build finish is a signal, not a report"
        );
    }
}
