# dumb-coder

An agentic coding tool built to run entirely on **small** language models — 12B
parameters at the absolute ceiling, and ideally something tiny like
**Gemma 4 E4B** (~4B-class, small enough to run on a phone).

The bet behind `dumb-coder`: most agentic coding tools assume a large frontier
model and lean on its raw intelligence. `dumb-coder` assumes the opposite. The
model is "dumb" — limited reasoning depth, small context window, shaky at
free-form tool calls — and the *harness* does the heavy lifting. The interesting
engineering is in the scaffolding that makes a small, cheap, local model behave
like a competent coding agent.

The second bet: **scale out, not up.** Instead of one big model, run a *swarm*
of very small workers (Gemma 4 E4B class) on the same codebase, coordinated by a
single larger orchestrator (up to the 12B ceiling) that plans, assigns, and
integrates their work. See [08 — Orchestration & the worker swarm](docs/specs/08-orchestration-and-swarm.md).

The third bet: **structured, gated workflow.** Every non-trivial task moves
through staged phases — specs → architecture → layout → test-first stage
breakdown → implementation plan → work decomposition — with a **human checkpoint
between each**. The agent works autonomously within a phase and stops for
sign-off at the boundary. See [09 — Workflow & human checkpoints](docs/specs/09-workflow-and-checkpoints.md).

The fourth bet: **tests are the control system.** Full TDD, mandatory at the unit
level — every unit of work is defined by a failing test *before* it's
implemented, and "done" means the test goes green. A test is the unambiguous,
machine-checkable oracle a dumb model lacks: it turns "trust the model" into
"trust the test runner." See [11 — Testing & TDD](docs/specs/11-testing-and-tdd.md).

## Why small models?

- **Local & private** — runs on a laptop, a homelab box, or even an Android
  phone. No code leaves the machine, no API bill.
- **Fast & cheap** — small models are fast to load and fast per token; tight
  loops feel interactive.
- **Forces good design** — constraints that make a small model usable
  (decomposition, narrow tools, disciplined context) also make the agent more
  predictable and auditable.

## Status

🚧 **Early implementation.** Specs are in [`docs/specs/`](docs/specs/) (start with
the [overview](docs/specs/00-overview.md)). Landed so far (`crates/`, 182 tests):

- **M4 planning & recovery** (`dc-core`) — the agent survives multi-step tasks and
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
  **structured per-test results** (`dc-verify` parses cargo / pytest, generic
  exit-code fallback). An enforced **permission layer** (`PermissionPolicy`): edits
  auto within the workspace, shell denied-by-default, and approved **contract
  tests frozen** — a cheat-edit is denied at the tool layer. The loop runs the
  TDD whole-suite gate (`finish` is refused while the suite is red) and journals
  every mutation for a diff overview + rollback. End-to-end: a scripted run drives
  a failing test red→green without breaking the suite or weakening the test.
- **M2 context manager** (`dc-context`) — the window as a hard-budgeted resource
  (spec 05): a zoned prompt assembler that fits each turn under an *effective*
  fraction of the advertised window (evicting lowest-priority zones first, never
  the sacred task anchor / current step / latest observation), aggressive
  observation truncation (head+tail, error-prioritized, flagged), a rolling
  extractive history summary, and token accounting (backend tokenizer → estimator
  fallback). A multi-turn run on an 8k window provably stays under budget even
  with whole-file observations every turn.
- **M2 retrieval index** (`dc-index`) — the aider-style **PageRank repo map**: a
  tree-sitter (Rust + Python) symbol definition/reference graph scored by
  hand-rolled personalized PageRank, with boosts for symbols the task names and
  files in play, rendered as a token-budgeted map of the most-central symbols.
  Plus workspace symbol lookup surfaced to the agent as the `find_symbol` tool.
- **M1 tool registry** (`dc-tools`) — strict, strongly-typed tool schemas with
  structured validation (bad calls are rejected *before* execution, with a
  precise reason), the narrow v1 tool surface (`read_file`, `list_dir`,
  `search_code`, `write_file`, `finish`), and producers that emit a JSON-Schema
  or a **GBNF grammar** from the same specs so a constraint can never drift from
  validation.
- **M1 tool-call strategies** (`dc-core`) — the capability-driven matrix from
  spec 02: **GBNF-constrained** → **native function-calling** → **parse+repair**,
  selected from the backend's advertised capabilities. Every strategy yields a
  registry-validated call or a structured repair error; malformed output is fed
  back, never executed. A deterministic suite asserts the M1 **≥95% valid-call**
  target, and `dc-eval` reports the live rate against a real backend.
- **Model gateway** (`dc-model`) — the `ModelBackend` trait + `OpenAiBackend`
  (any OpenAI-compatible server: Ollama compat, vLLM, LM Studio — with native
  `tools`/`tool_choice`), `OpenAiBackend::llama_cpp` (GBNF `grammar`),
  `MockBackend`, and `CallbackBackend` (the Android/AICore seam). Requests carry
  an optional output constraint (tool schemas / grammar).
- **M0 CLI** (`dc-cli`, the `dumb-coder` binary) — `doctor` and a chat REPL
  against a real model, with `--backend`/`--model`/`--tool-calling` selection.
- **M0 agent loop** (`dc-core`) — a bounded act→observe loop over the registry +
  strategy; plugs into the harness as `AgentSolver`, tested end-to-end red→green.
- **M1 eval harness** (`dc-eval`) — a TDD-enforcing, backend-agnostic scoreboard
  for red→green tasks (verify-red-first, frozen contract tests, green-after-solve),
  now also reporting the aggregate tool-call validity rate.

See the [roadmap](docs/specs/07-roadmap.md).

## Key decisions

| Decision | Choice |
| --- | --- |
| Implementation language | **Rust** (portable core + thin per-platform shells) |
| Interface | **CLI** core; **Android app** first client, **Windows client** later ([12](docs/specs/12-platform-clients.md)) |
| Inference backend | **Pluggable** via `ModelBackend` — **AICore** on Android; Ollama / llama.cpp / vLLM / OpenAI-compatible elsewhere |
| Model ceiling | ≤ 12B params; primary target **Gemma 4 E4B** (AICore runs it as Gemini Nano 4) |

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
- [12 — Platform clients (Android app, Windows) & AICore](docs/specs/12-platform-clients.md)
- [10 — Prior art & references](docs/specs/10-prior-art.md)
