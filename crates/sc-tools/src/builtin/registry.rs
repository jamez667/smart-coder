//! The tool schemas: what the model is offered, and in what order.
//!
//! Two registries — the full v1 surface ([`default_registry`]) and the
//! three-tool worker surface ([`minimal_worker_registry`]). Descriptions are
//! written *at* a small model: they say which tool to prefer for which shape of
//! change, because the choice between twelve options is itself a failure mode.

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
            description: "Read a UTF-8 text file. Optionally pass `start` (1-based line) and \
                          `limit` (line count) to read just a window of it rather than the \
                          whole file.",
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
            description: "Search files with a REGEX (e.g. `match .*ShipRole`, `fn \\w+`, \
                          `enum \\w+`); returns file:line hits. A plain string with no regex \
                          metacharacters works as a literal substring. Use `.*` to match across a \
                          line and `\\.` for a literal dot.",
            params: vec![ParamSpec::new(
                "query",
                ParamType::String,
                "the literal text to search for",
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
            description: "Append content to the END of a file (creating it if absent). Use this \
                          to build a large file in several turns: write the first part with \
                          write_file, then append the rest in chunks so no single reply is too long.",
            params: vec![
                ParamSpec::new(
                    "path",
                    ParamType::String,
                    "file path relative to the project root",
                ),
                ParamSpec::new("content", ParamType::String, "text to append at the end of the file"),
            ],
            side_effect: SideEffect::Mutating,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "edit_file",
            description: "Replace an EXACT snippet in a file: old_str must occur exactly once.",
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
            description: "Replace lines start..=end (1-based, inclusive) of a file with new_text. \
                          BEST for a large file: no snippet to copy exactly — just give the line \
                          numbers shown in the file view. Use start==end+1 form? No: to INSERT \
                          without deleting, set start = the line to insert BEFORE and end = \
                          start-1 (an empty range inserts).",
            params: vec![
                ParamSpec::new(
                    "path",
                    ParamType::String,
                    "file path relative to the project root",
                ),
                ParamSpec::new("start", ParamType::Integer, "first line to replace (1-based)"),
                ParamSpec::new(
                    "end",
                    ParamType::Integer,
                    "last line to replace (1-based, inclusive); use start-1 to INSERT before start",
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
            description: "Read a SINGLE function/method by NAME (Rust/Python/C#) — its whole \
                          body, line-numbered. PREFER this over read_file for a big file: you get \
                          just the function you care about, not hundreds of unrelated lines.",
            params: vec![
                ParamSpec::new(
                    "path",
                    ParamType::String,
                    "file path relative to the project root",
                ),
                ParamSpec::new("name", ParamType::String, "the function/method name to read"),
            ],
            side_effect: SideEffect::ReadOnly,
            permission: Permission::Auto,
        },
        ToolSpec {
            name: "edit_function",
            description: "Replace a whole function/method by NAME (Rust/Python/C#) with new_body. \
                          BEST for changing a function: no snippet to copy exactly and no line \
                          numbers to get right — name the function, give its full new text. Use \
                          this to add a match arm, change a signature, or rewrite a body. (If the \
                          function is very large, it suggests using edit_lines for a targeted \
                          change instead.)",
            params: vec![
                ParamSpec::new(
                    "path",
                    ParamType::String,
                    "file path relative to the project root",
                ),
                ParamSpec::new("name", ParamType::String, "the function/method name to replace"),
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
    const EXCLUDE: [&str; 2] = ["update_plan", "ask_user"];
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
