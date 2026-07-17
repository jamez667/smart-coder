# 06 — CLI & UX

## Scope

v1 is **terminal-only**. No TUI, no editor extension. A clean line-oriented CLI
that's pleasant interactively and scriptable in one-shot mode.

## Invocation modes

```bash
# Interactive session (REPL) in the current repo
smart-coder

# One-shot task, non-interactive (good for scripts / CI experiments)
smart-coder run "add input validation to parse_config and a test for it"

# Pick a backend/model ad hoc
smart-coder --backend ollama --model gemma4:e4b

# Replay / inspect a previous session log
smart-coder replay <session-id>
```

### Interactive REPL
The default. The user types a task; the agent loop runs, streaming its
reasoning, tool calls, and results live; control returns to the user when the
task stops. Supports follow-ups in the same session (history carried via the
Context Manager's compaction, [05](05-context-management.md)).

### One-shot (`run`)
Runs a single task to completion (or budget) and exits with a status code. No
interactive confirmation prompts — instead governed by the configured permission
policy (e.g. an allowlist or `--yolo`); if a Confirm-gated action is hit without
pre-approval, it stops and reports rather than blocking.

## What the user sees (live)

The CLI renders the harness event stream ([01](01-architecture.md)) as readable,
color-coded output:

```
● plan
  1. locate parse_config                                      [done]
  2. add validation for missing keys                          [active]
  3. add a unit test                                          [pending]

▸ search_code  "fn parse_config"
  └ src/config.rs:42

▸ read_file  src/config.rs:40-78
  └ 39 lines

▸ edit_file  src/config.rs   (+8 −1)
  └ applied ✓

▸ run_verification  cargo test
  └ ✓ 14 passed

✔ done — 3 files read, 1 edited, tests green   (steps 6 · 4.1k tok · 12s)
```

Design choices:
- **Streaming tokens** while the model thinks (where the backend supports it).
- **Tool calls shown before they run**; results summarized after.
- **Plan panel** kept visible so the user tracks progress against the steps.
- **Honest stop line** — reports partial/failed outcomes plainly, never claims
  success it didn't verify.

## Confirmations & safety

Per the permission layer ([04](04-tools.md)):

```
▸ run_command  "rm -rf build/"   [destructive]
  Allow this command? [y]es / [n]o / [a]lways for this session / [v]iew  ›
```

- Destructive/arbitrary shell prompts by default.
- `--yolo` pre-approves; an allowlist config can auto-approve known-safe
  commands.
- `Ctrl-C` interrupts the loop cleanly at the next turn boundary (no half-applied
  edits left dangling — edits are atomic per call).

## Workflow checkpoints

Beyond per-tool confirmations, the CLI gates the **phase boundaries** of the
staged workflow ([09](09-workflow-and-checkpoints.md)). At each checkpoint the
agent presents the phase artifact and waits for a decision:

```
⛳ checkpoint — phase 3/6: LAYOUT   (specs ✓  architecture ✓)
   artifact written to docs/plan/03-layout.md   (review the diff)

   [a]pprove · [r]evise (edit the file yourself) · [s]end back · [v]iew · [q]uit ›
```

- **Approve** advances to the next phase; **revise** lets you edit the artifact
  file and accept your version; **send back** regenerates with your notes (and
  may target an earlier phase); **quit** stops but keeps approved artifacts.
- The current phase and which gates are passed are always shown.
- In `run` / `--json` mode, the configured gate policy decides whether the
  workflow auto-advances or stops at the first un-approved gate and reports.

## Inspection & debugging

- `--verbose` / `-v` — show the full assembled prompt per turn (what the model
  *actually saw*, [05](05-context-management.md)).
- `--log <path>` — write the structured session log; default to a per-session
  file under the config dir.
- `smart-coder replay <id>` — step through a recorded session
  ([03](03-agent-loop.md)) to understand a past run.
- `--dry-run` — plan and show intended actions without mutating anything.

## Configuration & discovery

- Config precedence: **CLI flags > env vars > project `.smart-coder.toml` > user
  config > defaults.** ([02](02-model-backends.md) shows the model section.)
- A **project file** (`.smart-coder.toml`) can pin the verification command,
  ignore globs for indexing, and permission policy for that repo.
- `smart-coder doctor` — check that the configured backend is reachable, the
  model is pulled, and the tokenizer is available; print effective context
  budget. (First thing to run when setup is off.)

## Output for humans *and* machines

- Default: rich human output as above.
- `--json` (or in `run` mode): emit the event stream as JSON lines, so
  `smart-coder` can be driven by scripts or other tools.

## Explicitly out of scope for v1

- Full-screen TUI (panes, mouse) — candidate for v2 ([07](07-roadmap.md)).
- Editor/IDE integration.
- Remote/daemon mode or web UI.

## Note: swarm rendering (later)

When the worker swarm lands ([08](08-orchestration-and-swarm.md), M7 in
[07](07-roadmap.md)), the CLI gains a view of swarm state — active workers and
their subtasks, the task board, and integration/merge progress — rendered from
the same event stream. The line-oriented model above is designed to extend to
this (one block per worker); a full-screen TUI for it remains a v2 consideration.
