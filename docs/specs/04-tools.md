# 04 — Tools

## Principle

Tools are how the model affects the world. For a small model, the tool design
*is* the safety and reliability story. Two rules drive everything here:

1. **Narrow & explicit beats broad & flexible.** A few sharply-scoped tools with
   strict schemas outperform a handful of powerful, open-ended ones. Small
   models pick the wrong tool and fill args poorly when the surface is large or
   ambiguous. This is SWE-agent's **Agent-Computer Interface** principle — design
   the tool surface *for* the model ([10](10-prior-art.md)).
2. **Structure is enforced, not requested.** Wherever the backend allows
   ([02](02-model-backends.md)), tool-call output is constrained by grammar/JSON
   schema so it's valid by construction. Otherwise, parse-and-repair.

## Tool contract

Every tool declares:

```rust
// Illustrative.
pub struct ToolSpec {
    pub name: &'static str,            // short, unambiguous
    pub description: &'static str,     // one line, action-oriented
    pub params_schema: JsonSchema,     // strict; no free-form blobs
    pub side_effect: SideEffect,       // ReadOnly | Mutating | Destructive
    pub permission: Permission,        // Auto | Confirm | Deny-by-default
}
```

- **Strict schemas.** Required fields, enums over free strings where possible,
  no "kitchen-sink" object params. Bad args fail validation *before* execution
  and feed a precise error back to the model.
- **Side-effect class** drives the permission layer (below).
- **Structured results.** Tools return typed results (status, payload,
  truncation flag), not raw blobs, so the Context Manager can budget and
  summarize them.

## Built-in tool set (v1)

Kept deliberately small. Each does one thing.

### Read / navigate (ReadOnly)
- `read_file` — read a file (or a line range). Returns bytes + line numbers.
- `list_dir` — list a directory (non-recursive by default).
- `search_code` — ripgrep-style regex/literal search; returns file:line hits.
- `find_symbol` — locate a definition/usages via the retrieval index
  ([05](05-context-management.md), [01](01-architecture.md)). Lets a small model
  jump to the right place instead of scanning.

### Edit (Mutating)
- `edit_file` — apply a **precise, anchored edit** (exact old → new string, or a
  unified-diff hunk). No "rewrite the whole file" mode by default — anchored
  edits are easier for a small model to get right and easier to verify/revert.
- `create_file` — write a new file (fails if it exists).

> All edits go through a single apply-and-record path so every change can be
> diffed, shown to the user, and rolled back.

### Execute (Mutating / Destructive)
- `run_command` — run a shell command in the workspace. **Confirm-gated by
  default.** Returns exit code + captured stdout/stderr (truncated).
- `run_verification` — run the project's configured build/test/lint command.
  This is the verify gate from [03](03-agent-loop.md); separated from
  `run_command` so the harness can call it on its own and parse results
  predictably. Returns **structured per-test pass/fail + failure messages**, not
  a raw log — the spine of the TDD loop ([11](11-testing-and-tdd.md)).

### Version control (Mutating)
- `git_status` / `git_diff` (ReadOnly) — ground the model in actual repo state.
- `git_commit` — Confirm-gated; never auto-pushes in v1.

### Meta (ReadOnly, harness-facing)
- `update_plan` — let the model revise the step list (the harness validates and
  owns the result; see [03](03-agent-loop.md)).
- `ask_user` — escalate a genuine ambiguity to the human instead of guessing.
- `finish` — declare the task complete (triggers final verification + summary).

## Permission layer

Every Mutating/Destructive call passes through a permission gate before
execution:

| Class | Default policy |
| --- | --- |
| ReadOnly | Auto-allow |
| Mutating (edits, commits) | Auto-allow within the workspace; **never** writes outside it |
| Destructive / arbitrary shell | **Confirm** each call (interactive prompt) |

- Policies are configurable (e.g. `--yolo` to pre-approve, or an allowlist of
  safe commands). Defaults are conservative.
- The gate is enforced by the harness, **outside** the model's control — the
  model can't grant itself permission.
- All actions are **sandboxed to the workspace root**; path traversal outside it
  is rejected.
- **Approved contract tests are frozen.** `edit_file`/`delete` on a human-approved
  test path is **denied** for worker models and flagged to the orchestrator/human
  — a worker must make tests pass, never weaken them ([11](11-testing-and-tdd.md)).

## Tool results & the model

- Results are **truncated/summarized to a budget** by the Context Manager before
  re-entering the prompt (a 5k-line test log can't go back verbatim to an 8k
  window). Truncation is flagged so the model knows output was cut.
- Errors are returned as **structured, actionable messages** ("file X not found;
  did you mean Y?") so the model can self-correct in one turn.

## Extensibility (post-v1)

The registry is designed so tools can be added without touching the loop. Future
directions ([07](07-roadmap.md)): user-defined tools via config. Out of scope for
v1.
