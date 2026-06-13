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

📋 **Specification phase.** No implementation yet. See [`docs/specs/`](docs/specs/)
for the design. Start with the [overview](docs/specs/00-overview.md).

## Key decisions (locked for v1)

| Decision | Choice |
| --- | --- |
| Implementation language | **Rust** (single static binary, low overhead) |
| Interface | **CLI / terminal** agent loop |
| Inference backend | **Pluggable** — Ollama, llama.cpp, vLLM, on-device Android, any OpenAI-compatible server |
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
- [10 — Prior art & references](docs/specs/10-prior-art.md)
