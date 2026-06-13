# 07 — Roadmap & milestones

The roadmap is sequenced so that the **hardest small-model risks are de-risked
first**: getting valid tool calls out of a tiny model, and keeping its context
under control. Everything else builds on a working, reliable single-step loop.

## M0 — Skeleton & gateway (walking skeleton)
**Goal:** prove the plumbing end-to-end with the dumbest possible loop.
- Cargo workspace + crate boundaries ([01](01-architecture.md)).
- `ModelBackend` trait + **OpenAI-compatible adapter** and **Ollama adapter**
  ([02](02-model-backends.md)).
- `dumb-coder doctor`: backend reachable, model present, context budget printed.
- Trivial loop: prompt → model text → print. No tools yet.
- **Exit criteria:** can chat with Gemma 3n E4B via both backends from the CLI.

## M1 — Reliable tool calls (the core risk)
**Goal:** a small model issues *well-formed* tool calls, reliably.
- Tool Registry + strict schemas ([04](04-tools.md)).
- Read-only tools first: `read_file`, `list_dir`, `search_code`.
- Capability-driven tool-call strategy: native function calling / JSON-schema /
  **GBNF grammar** (add llama.cpp adapter here) / prompt+parse+repair
  ([02](02-model-backends.md)).
- Single-turn ACT→OBSERVE with the repair loop.
- **Exit criteria:** ≥95% valid tool calls on a small fixed task suite; malformed
  calls always recovered or escalated, never acted on.

## M2 — Context discipline
**Goal:** keep a tiny window useful across many turns.
- Context Manager with hard budget + zones ([05](05-context-management.md)).
- Retrieval index (`dc-index`): lexical search + symbol lookup; `find_symbol`.
- Observation truncation + rolling history summary.
- Accurate token accounting via the gateway tokenizer.
- **Exit criteria:** multi-turn tasks stay coherent on an 8k window without
  blowing the budget; logged prompts show only relevant context.

## M3 — Editing & verification (closing the loop)
**Goal:** the agent actually changes code and proves it.
- Mutating tools: anchored `edit_file`, `create_file`, atomic apply+record.
- `run_command` + `run_verification` behind the permission layer
  ([04](04-tools.md)).
- VERIFY gate: build/test/lint feedback re-enters the loop ([03](03-agent-loop.md)).
- Permission prompts + workspace sandboxing in the CLI ([06](06-cli-ux.md)).
- **Exit criteria:** completes a real "edit a few files + make tests pass" task
  on a sample repo, with verification gating success.

## M4 — Planning & recovery
**Goal:** survive multi-step tasks and the model's own mistakes.
- Planner: decompose into small steps; harness-owned plan state.
- Re-planning triggers; loop/stall detection; per-step + global budgets.
- `update_plan`, `ask_user`, `finish` meta-tools.
- **Exit criteria:** recovers from induced failures (bad edit, failing test,
  repeated action) without human rescue, or escalates cleanly.

## M5 — UX, replay & polish
**Goal:** pleasant, inspectable, scriptable.
- Live event rendering, plan panel, honest stop lines ([06](06-cli-ux.md)).
- `--verbose` prompt inspection, session logging, `replay`, `--json`, `--dry-run`.
- One-shot `run` mode + permission policies (`--yolo`, allowlist).
- **Exit criteria:** a newcomer can install, `doctor`, and run a task guided only
  by CLI output.

## M6 — On-device / Android backend
**Goal:** fully offline on a phone, the showcase of the "small model" thesis.
- On-device adapter (in-process or local runtime) ([02](02-model-backends.md)).
- Tighter default budgets/timeouts for mobile constraints.
- **Exit criteria:** a scoped task runs offline on-device with Gemma 3n E4B.

---

## Post-v1 / future ideas
- **MCP client** — consume external Model Context Protocol tool servers.
- **User-defined tools** via config.
- **Multi-model routing** — a tiny fast planner + a slightly larger coder.
- **Embedding-based retrieval** with a small local embedder (optional).
- **TUI** (v2 interface).
- **Bounded autonomous mode** — unattended runs with strong budgets/guardrails.
- **LoRA/adapter experiments** — light task-specific tuning of the small model.

## Cross-cutting throughout
- A **fixed task suite** (sample repos + graded tasks) as the regression
  benchmark; tracked from M1 so harness changes are measured against real
  small-model behavior.
- Determinism/replay maintained at every milestone for debuggability
  ([03](03-agent-loop.md)).
