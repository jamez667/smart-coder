# smart-coder

An agentic coding tool built to run entirely on **small** language models — 12B
parameters at the absolute ceiling, and ideally something tiny like
**Gemma 4 E4B** (~4B-class, small enough to run on a phone).

The bet behind `smart-coder`: most agentic coding tools assume a large frontier
model and lean on its raw intelligence. `smart-coder` assumes the opposite. The
model is "dumb" — limited reasoning depth, small context window, shaky at
free-form tool calls — and the *harness* does the heavy lifting. The interesting
engineering is in the scaffolding that makes a small, cheap, local model behave
like a competent coding agent.

The second bet: **scale out, not up.** Instead of one big model, run a *swarm*
of very small workers (Gemma 4 E4B class) on the same codebase, coordinated by a
single larger orchestrator (up to the 12B ceiling) that plans, assigns, and
integrates their work. See [08 — Orchestration & the worker swarm](docs/specs/08-orchestration-and-swarm.md).

The third bet: **structured, gated workflow.** Every non-trivial task moves
through staged phases — specs → architecture → layout → stage breakdown → work
decomposition — with a **human checkpoint between each**. The agent works
autonomously within a phase and stops for sign-off at the boundary, where you
Approve, Send-back with notes, or Abort. See [09 — Workflow & human checkpoints](docs/specs/09-workflow-and-checkpoints.md).

The fourth bet: **tests are the control system.** Full TDD, mandatory at the unit
level — every unit of work is defined by a failing test *before* it's
implemented, and "done" means the test goes green. A test is the unambiguous,
machine-checkable oracle a dumb model lacks: it turns "trust the model" into
"trust the test runner." See [11 — Testing & TDD](docs/specs/11-testing-and-tdd.md).

## Why small models?

- **Local & private** — runs on a laptop or a homelab box. No code leaves the
  machine, no API bill.
- **Fast & cheap** — small models are fast to load and fast per token; tight
  loops feel interactive.
- **Forces good design** — constraints that make a small model usable
  (decomposition, narrow tools, disciplined context) also make the agent more
  predictable and auditable.

## Status

🚧 **Early implementation.** Specs are in [`docs/specs/`](docs/specs/) (start with
the [overview](docs/specs/00-overview.md)). Landed so far (`crates/`, ~700 tests):

- **M8 native Windows client** (`sc-win`, the vibe-coding desktop app — spec 12) —
  an [iced](https://iced.rs) GUI over the proven core: type intent, watch the agent
  and swarm work. It drives the full **staged workflow** end to end. **Breakdown**
  runs the design pipeline (specs → architecture → layout → stage breakdown → work
  decomposition) and **pauses at each phase for review**; **Build** carries the same
  design through a **compiler-driven executor** that applies the change and loops
  cargo-check → fix-each-diagnostic to green. Each phase **streams into the chat live**
  (token by token) and its artifact opens in a **code editor** with tabs, a git diff
  view, and a PR-style **review panel** — drag-select lines and comment, and those
  comments become the **Send-back notes** that regenerate the phase. Approve /
  Send-back / Abort live both in the plan list and in the editor header. A built-in
  git panel (stage/unstage/discard, multi-select, commit/push/pull) and a
  strict-sandbox integrated terminal round it out. A **phone mirror** (`sc-web` +
  `sc-iterate`) lets a phone attach to the live desktop session over Tailscale.
- **M7 worker swarm core** (`sc-swarm`) — the "scale out, not up" thesis (spec 08):
  a larger **orchestrator** model decomposes a task into a dependency-DAG **task
  board** of independent subtasks; a **bounded pool of tiny workers** runs the
  ready ones in parallel — each worker is the unchanged M0–M4 agent loop in an
  isolated scratch copy, returning a *proposed* diff. The orchestrator then
  **integrates proposals one at a time**, re-running verification after each
  (parallel intelligence, serialized writes); a change that breaks the suite is
  reverted and the subtask failed. A derailed worker damages only its own copy.
  Proven: multiple subtasks (incl. a dependency) run by parallel workers and
  integrated green; a suite-breaker is rejected.
- **Event stream + two live UIs** (`sc-core` event hub → `sc-tui` *and* `sc-web`).
  Every phase of a run emits a typed `AgentEvent` (RunStarted / Planned / ToolCall
  / ToolResult / Verification / Stalled / Advice / Stopped) through an `EventSink`
  (spec 01's event-stream architecture). Two renderers consume the same stream:
  - **`sc-web`** — a **local web dashboard**: a small `tiny_http` server streams
    the run to your browser (incremental JSON feed), rendered as a plan panel, a
    color-coded live activity feed, and a metrics/context bar. No async runtime,
    no frontend build — `smart-coder serve "<task>"` prints a `localhost` URL.
  - **`sc-tui`** — a full-screen [ratatui](https://ratatui.rs) terminal dashboard
    with the same panes — `smart-coder run "<task>"`.
  - **Proven on real Gemma 4 E4B** (local Ollama): both drive a failing test
    red→green in a handful of clean turns. The agent runs on a worker thread; the
    JSON/state folds are headless-tested.
  - **"Junior asks senior" live** (`--advisor <model>`): point a *larger* model at
    the same endpoint and the UI lights up a magenta nudge line whenever the coder
    stalls. Verified with `--model gemma4:e2b --advisor gemma4:e4b` — the tiny
    coder thrashed, the harness detected the stall, consulted e4b, and got a
    specific hint ("re-examine the modulo operator…") without the senior writing
    the fix (spec 02 tiered models).

- **M4 planning & recovery** (`sc-core`) — the agent survives multi-step tasks and
  its own mistakes (spec 03). A **planner** decomposes the task into a short,
  harness-owned step plan (`PlanState`), rendered as compact structured state. The
  harness detects **loops/stalls** (action-hashing + a no-progress counter) and a
  per-step retry budget, and decides when to intervene — the model never has to.
  `update_plan` / `ask_user` meta-tools let the model revise the plan or escalate.
  Escalation is **"junior asks senior"** (spec 02 tiered models): a stuck small
  coder consults a larger *advisor* model for a one-line *nudge* — advice, not the
  implementation — and keeps doing the work itself; with no advisor it stops
  cleanly with a structured `StopReason` (Finished / BudgetExhausted / Stalled /
  Escalated). Proven: recovers from a bad edit, breaks loops via an advisor nudge,
  and escalates cleanly when there's no senior to ask.

- **M3 editing & TDD verification** — the agent now changes code and *proves* it
  via tests (spec 04/11). Anchored `edit_file` (exact `old_str`→`new_str`, refused
  on 0 or >1 matches) + `create_file`; `run_command` / `run_verification` with
  **structured per-test results** (`sc-verify` parses cargo / pytest, generic
  exit-code fallback). An enforced **permission layer** (`PermissionPolicy`): edits
  auto within the workspace, shell denied-by-default, and approved **contract
  tests frozen** — a cheat-edit is denied at the tool layer. The loop runs the
  TDD whole-suite gate (`finish` is refused while the suite is red) and journals
  every mutation for a diff overview + rollback. End-to-end: a scripted run drives
  a failing test red→green without breaking the suite or weakening the test.
- **M2 context manager** (`sc-context`) — the window as a hard-budgeted resource
  (spec 05): a zoned prompt assembler that fits each turn under an *effective*
  fraction of the advertised window (evicting lowest-priority zones first, never
  the sacred task anchor / current step / latest observation), aggressive
  observation truncation (head+tail, error-prioritized, flagged), a rolling
  extractive history summary, and token accounting (backend tokenizer → estimator
  fallback). A multi-turn run on an 8k window provably stays under budget even
  with whole-file observations every turn.
- **M2 retrieval index** (`sc-index`) — the aider-style **PageRank repo map**: a
  tree-sitter (Rust + Python) symbol definition/reference graph scored by
  hand-rolled personalized PageRank, with boosts for symbols the task names and
  files in play, rendered as a token-budgeted map of the most-central symbols.
  Plus workspace symbol lookup surfaced to the agent as the `find_symbol` tool.
- **M1 tool registry** (`sc-tools`) — strict, strongly-typed tool schemas with
  structured validation (bad calls are rejected *before* execution, with a
  precise reason), the narrow v1 tool surface (`read_file`, `list_dir`,
  `search_code`, `write_file`, `finish`), and producers that emit a JSON-Schema
  or a **GBNF grammar** from the same specs so a constraint can never drift from
  validation.
- **M1 tool-call strategies** (`sc-core`) — the capability-driven matrix from
  spec 02: **GBNF-constrained** → **native function-calling** → **parse+repair**,
  selected from the backend's advertised capabilities. Every strategy yields a
  registry-validated call or a structured repair error; malformed output is fed
  back, never executed. A deterministic suite asserts the M1 **≥95% valid-call**
  target, and `sc-eval` reports the live rate against a real backend.
- **Model gateway** (`sc-model`) — the `ModelBackend` trait + `OpenAiBackend`
  (any OpenAI-compatible server: Ollama compat, vLLM, LM Studio — with native
  `tools`/`tool_choice`), `OpenAiBackend::llama_cpp` (GBNF `grammar`),
  and `MockBackend`. Requests carry an optional output constraint (tool schemas /
  grammar).
- **M0 CLI** (`sc-cli`, the `smart-coder` binary) — `doctor` and a chat REPL
  against a real model, with `--backend`/`--model`/`--tool-calling` selection.
- **M0 agent loop** (`sc-core`) — a bounded act→observe loop over the registry +
  strategy; plugs into the harness as `AgentSolver`, tested end-to-end red→green.
- **M1 eval harness** (`sc-eval`) — a TDD-enforcing, backend-agnostic scoreboard
  for red→green tasks (verify-red-first, frozen contract tests, green-after-solve),
  now also reporting the aggregate tool-call validity rate.

See the [roadmap](docs/specs/07-roadmap.md).

## Running the backends

The rig's model launchers and the verify sandbox image live in a separate
repo, **[smart-coder-ops](../smart-coder-ops)** — they're environment concerns, so
swapping models doesn't churn this source tree:

- **Backend launchers** (`scripts/`) — llama.cpp servers for the models the agent
  talks to (the daily-driver 30B split, the 8B swarm pool). Reached over HTTP; the
  endpoint/model this app uses is set in `%APPDATA%\smart-coder\config.json` (or
  `SC_BASE_URL`/`SC_MODEL`), never hard-coded here.
- **Verify sandbox** (`docker/pyenv/`) — the pinned Python image generated code is
  tested in, referenced by name (`smart-coder-pyenv`).

## Running the apps

- **Desktop GUI** — `cargo run -p sc-win --release` opens the vibe-coding window
  (no CLI args; a model backend is only needed once you drive a task). Open a
  project folder, type intent, and use **Breakdown**/**Build** on a spec to run the
  staged workflow.
- **CLI / TUI** — `smart-coder run "<task>"` (terminal dashboard) or
  `smart-coder serve "<task>"` (prints a `localhost` URL for the web dashboard);
  `smart-coder doctor` checks the backend.

## Key decisions

| Decision | Choice |
| --- | --- |
| Implementation language | **Rust** (portable core + thin per-platform shells) |
| Interface | **CLI** core; **Windows client** ([12](docs/specs/12-platform-clients.md)) |
| Inference backend | **Pluggable** via `ModelBackend` — Ollama / llama.cpp / vLLM / OpenAI-compatible |
| Model ceiling | ≤ 12B params; primary target **Gemma 4 E4B** |

## Spec index

- [00 — Overview, goals & non-goals](docs/specs/00-overview.md)
- [01 — Architecture](docs/specs/01-architecture.md)
- [02 — Model backends & abstraction](docs/specs/02-model-backends.md)
- [03 — The agent loop](docs/specs/03-agent-loop.md)
- [04 — Tools](docs/specs/04-tools.md)
- [05 — Context management](docs/specs/05-context-management.md)
- [06 — CLI & UX](docs/specs/06-cli-ux.md)
- [07 — Roadmap & milestones](docs/specs/07-roadmap.md)
- [08 — Orchestration & the worker swarm](docs/specs/08-orchestration-and-swarm.md)
- [09 — Workflow & human checkpoints](docs/specs/09-workflow-and-checkpoints.md)
- [11 — Testing & TDD](docs/specs/11-testing-and-tdd.md)
- [12 — Platform clients (Windows)](docs/specs/12-platform-clients.md)
- [10 — Prior art & references](docs/specs/10-prior-art.md)
