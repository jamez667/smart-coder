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
the [overview](docs/specs/00-overview.md)). Landed so far (`crates/`, 83 tests):

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
