# 01 — Architecture

## High-level shape

`smart-coder` is a single Rust binary that runs an **agent loop** in the
terminal. The loop drives a small LLM through one decision at a time, executes
the model's chosen tool, feeds the result back, and repeats until the task is
done or a budget is hit.

```
              ┌────────────────────────────────────────────────────────┐
              │                      smart-coder (CLI)                    │
              │                                                          │
  user  ───▶  │  ┌──────────┐   ┌───────────┐   ┌──────────────────┐    │
  prompt      │  │   REPL   │──▶│  Orchestr- │──▶│   Agent Loop     │    │
              │  │  / TUI   │   │   ator     │   │ (plan→act→observe│    │
              │  └──────────┘   └───────────┘   │   →verify)       │    │
              │        ▲                          └────────┬─────────┘    │
              │        │                                   │              │
              │        │        ┌──────────────────────────┼───────────┐ │
              │        │        ▼                          ▼            │ │
              │        │  ┌───────────┐  ┌──────────┐  ┌──────────┐    │ │
              │        │  │  Context  │  │   Tool   │  │  Model    │    │ │
              │        │  │  Manager  │  │ Registry │  │  Gateway  │    │ │
              │        │  └─────┬─────┘  └────┬─────┘  └────┬──────┘    │ │
              │        │        │             │             │           │ │
              │   render        │             │             │           │ │
              │   results       ▼             ▼             ▼           │ │
              │           ┌──────────┐  ┌──────────┐  ┌──────────┐     │ │
              │           │ Retrieval│  │ fs/shell │  │ Backend  │     │ │
              │           │  Index   │  │  /git    │  │ adapters │     │ │
              │           └──────────┘  └──────────┘  └────┬─────┘     │ │
              └─────────────────────────────────────────────┼──────────┘ │
                                                             ▼
                                            Ollama │ llama.cpp │ vLLM │
                                            any OpenAI-compatible server
```

## Crate / module layout (proposed)

A Cargo workspace keeps boundaries clean and lets the model gateway and tools be
reused/tested in isolation.

```
smart-coder/
├── Cargo.toml              # workspace
├── crates/
│   ├── sc-cli/             # binary: arg parsing, REPL, rendering, config load
│   ├── sc-core/            # orchestrator, agent loop, planner, budgets
│   ├── sc-model/           # Model Gateway: trait + backend adapters
│   ├── sc-tools/           # Tool Registry + built-in tools (fs, shell, git, search)
│   ├── sc-context/         # Context Manager: window builder, summarizer, history
│   ├── sc-index/           # Retrieval index over the repo
│   └── sc-proto/           # shared types: messages, tool schemas, events, errors
└── docs/specs/
```

> Module names are provisional; the boundaries are the point. `sc-core` never
> talks to a concrete backend or a concrete shell — only to the `sc-model` and
> `sc-tools` traits.

## Core components

### Orchestrator (`sc-core`)
Owns a session. Holds the task, the plan, the budgets, and the event log. Decides
when to call the planner vs. continue executing, when to compact context, and
when to stop. Pure logic — no I/O of its own beyond the traits it's handed.

### Agent loop (`sc-core`)
The plan → act → observe → verify cycle. One model turn = one decision. Detailed
in [03 — The agent loop](03-agent-loop.md).

### Model Gateway (`sc-model`)
A single `ModelBackend` trait that all inference backends implement. Normalizes
chat/completion, streaming, and (where supported) constrained decoding /
grammars. Detailed in [02 — Model backends](02-model-backends.md).

### Tool Registry (`sc-tools`)
Declares the tools the model may use, each with a strict schema. Validates and
executes calls; returns structured results. Detailed in [04 — Tools](04-tools.md).

### Context Manager (`sc-context`)
Builds every prompt under a hard token budget: system prompt + task + relevant
retrieved snippets + recent history (possibly summarized). The single most
important component for small models. Detailed in [05 — Context management](05-context-management.md).

### Retrieval Index (`sc-index`)
Lightweight index over the working repo (symbols, files, chunks) so the Context
Manager can pull in only what's relevant rather than dumping whole files.

## Cross-cutting concerns

- **Events & logging.** Every step emits a structured event (`PlanCreated`,
  `ModelTurn`, `ToolCall`, `ToolResult`, `ContextCompacted`, `BudgetHit`,
  `Stopped`). The CLI renders these live; they're also written to a session log
  for replay/debugging. This **event-stream architecture** — all agent↔env
  interaction as typed events through one hub — is borrowed from OpenHands
  ([10](10-prior-art.md)).
- **Budgets.** Token, wall-clock, step-count, and tool-call budgets are
  first-class and enforced by the orchestrator, not left to the model.
- **Determinism knobs.** Temperature, seed, and sampling are pinned per session
  and recorded, so a session log can be replayed.
- **Errors.** All fallible boundaries return typed errors (`sc-proto`). Model
  misbehavior (malformed output, loops) is a *normal*, handled condition — not a
  panic.
- **Safety.** Shell and write tools run behind a permission layer; destructive
  actions require confirmation unless explicitly pre-approved (see
  [04](04-tools.md) and [06](06-cli-ux.md)).

## Data flow for one task (happy path)

1. User enters a task in the REPL.
2. Orchestrator asks the planner (model) for a short step list, grounded in a
   retrieved repo overview.
3. For each step: Context Manager builds a tight prompt → Model Gateway gets a
   single tool-call decision → Tool Registry executes it → result is observed.
4. After edits, a verification step (build/test/lint) runs; failures feed back
   into the loop.
5. When the plan is complete and verification passes, the orchestrator stops and
   summarizes the diff for the user.

## Single agent vs. the swarm

The components above describe **one** agent. The second core capability —
**many tiny workers on one codebase under a larger orchestrator** — is layered
*above* this: each worker runs its own instance of the agent loop + tools +
context manager (in its own isolated worktree), and a swarm-coordinator drives
decomposition, scheduling, and integration. That layer reuses everything here
unchanged; see [08 — Orchestration & the worker swarm](08-orchestration-and-swarm.md).
It is sequenced after the single-agent core is solid ([07](07-roadmap.md)).
