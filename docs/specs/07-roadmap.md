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
- **Exit criteria:** can chat with Gemma 4 E4B via both backends from the CLI.

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

## M2 — Context discipline ✅
**Goal:** keep a tiny window useful across many turns.
- ✅ Context Manager (`dc-context`) with hard budget + prioritized zones, eviction
  lowest-first, sacred zones never dropped ([05](05-context-management.md)).
- ✅ Retrieval index (`dc-index`): tree-sitter (Rust + Python) symbol graph +
  **PageRank repo map** with task/in-play boosts; `find_symbol` tool.
- ✅ Observation truncation (head+tail, error-prioritized, flagged) + rolling
  extractive history summary.
- ✅ Token accounting via the gateway tokenizer (`count_tokens`) with a heuristic
  estimator fallback.
- ✅ Wired into the agent loop, replacing the clone-everything prompt.
- **Exit criteria:** ✅ a multi-turn run with whole-file observations every turn
  provably stays under an 8k budget (`dc-core` integration test); the assembled
  prompt is inspectable via `BuiltContext`.
- *Deferred to a follow-up:* per-step (vs. per-run) repo-map refresh; embedding
  retrieval; lexical chunk search beyond `search_code`; more tree-sitter
  grammars.

## M3 — Editing & TDD verification (closing the loop) ✅
**Goal:** the agent actually changes code and *proves* it via tests.
- ✅ Mutating tools: anchored `edit_file` (exact `old_str`→`new_str`, refused on
  0/>1 matches), `create_file`; an edit journal records before/after for diff +
  rollback (the single apply-and-record path).
- ✅ `run_command` + `run_verification` with **structured per-test results**
  (`dc-verify`: cargo + pytest parsers, generic exit-code fallback), behind the
  permission layer ([04](04-tools.md)).
- ✅ **TDD loop:** the whole-suite gate refuses `finish` while the suite is red,
  feeding the failing cases back ([11](11-testing-and-tdd.md), [03](03-agent-loop.md)).
- ✅ Frozen contract-test protection (`PermissionPolicy` denies edits to approved
  test paths at the tool layer) + shell denied-by-default + workspace sandboxing
  ([04](04-tools.md)).
- **Exit criteria:** ✅ a scripted run drives a failing `sh` test red→green on a
  sample repo without breaking the suite or weakening the frozen test
  (`dc-core` `tdd_loop` integration test); cheat-edits and red-suite finishes are
  both rejected.
- *Deferred:* interactive `[y/n]` confirmation prompt for `Confirm`-gated calls
  (CLI/M5); verify-red-*first* as an explicit harness-run pre-check (the loop lets
  the model run it); more test-framework parsers (jest, go test, …).

## M4 — Planning & recovery ✅
**Goal:** survive multi-step tasks and the model's own mistakes.
- ✅ Planner: decompose into a short ordered step list; harness-owned `PlanState`
  (status per step, retry counter), rendered as compact structured state.
- ✅ Loop/stall detection (action-hash repeats + no-progress counter) + per-step
  retry budget + global step budget; structured `StopReason`.
- ✅ `update_plan` + `ask_user` meta-tools (+ existing `finish`).
- ✅ **Escalation = "junior asks senior"** (spec 02): on a stall or `ask_user`,
  consult a larger *advisor* backend for a one-line nudge (advice, not the fix);
  the junior keeps doing the work. No advisor → clean `Escalated`/`Stalled` stop.
- **Exit criteria:** ✅ recovers from induced failures (bad edit, repeated action)
  without human rescue, breaks loops via an advisor nudge, and escalates cleanly
  with no advisor (`dc-core` `recovery_loop` integration test).
- *Deferred:* automatic step-completion detection (the harness renders the plan
  and runs the retry budget, but advances steps only via the model's
  `update_plan` / on retry-exhaustion, not by inferring which call satisfied a
  step); per-step token/wall-clock budgets; re-running the planner mid-task.

## M5 — UX, replay & polish
**Goal:** pleasant, inspectable, scriptable.
- Live event rendering, plan panel, honest stop lines ([06](06-cli-ux.md)).
- `--verbose` prompt inspection, session logging, `replay`, `--json`, `--dry-run`.
- One-shot `run` mode + permission policies (`--yolo`, allowlist).
- **Exit criteria:** a newcomer can install, `doctor`, and run a task guided only
  by CLI output.

## M6 — Staged workflow & human checkpoints
**Goal:** drive tasks through the gated pipeline ([09](09-workflow-and-checkpoints.md))
so mistakes are caught before code is written. Runs with the single-agent core;
its final phase becomes the swarm's input (M7).
- Phase engine: specs → architecture → layout → test-first stage breakdown →
  implementation plan → work decomposition.
- Durable, versioned phase artifacts on disk (resumable across sessions).
- Checkpoint gates in the CLI: approve / revise / send-back (incl. to earlier
  phases) / abort ([06](06-cli-ux.md)); harness-enforced, model can't self-approve.
- Adaptive ceremony + configurable gate set (collapse phases for trivial tasks).
- **Exit criteria:** a real task is taken from a one-line request through all six
  gated phases to an approved, test-defined work decomposition — with send-back
  correctly invalidating and regenerating downstream artifacts.

## M7 — Orchestration & the worker swarm
**Goal:** scale out — many tiny workers on one codebase under a larger
orchestrator ([08](08-orchestration-and-swarm.md)). Deliberately sequenced
**after** the single-agent loop is solid (M0–M5), since each worker *is* that
loop.
- Orchestrator profile + worker profiles via the gateway ([02](02-model-backends.md)).
- Task board (subtask DAG, status, deps) + decomposition into independent subtasks.
- Worktree-per-worker isolation; bounded-concurrency scheduler.
- Integration: ordered branch merges, conflict arbitration by the orchestrator,
  mandatory integration verification.
- Serialized shared-workspace fallback for non-parallelizable work.
- CLI surfacing of swarm state (active workers, board, integration)
  ([06](06-cli-ux.md)).
- **Exit criteria:** a task that decomposes into ≥3 independent subtasks is
  completed by parallel workers and integrated green, faster than the
  single-agent baseline — with failure containment (a derailed worker is
  discarded/reassigned, never corrupts the result).

## M8 — Android app + AICore (first platform client)
**Goal:** the showcase of the "small model" thesis — fully on-device on a phone,
as a **native Android app** using **AICore** (Gemma 4 / Gemini Nano 4) via ML Kit
GenAI ([12](12-platform-clients.md), [10](10-prior-art.md)).
- `dc-android` cdylib (Rust core via cargo-ndk) + Kotlin app shell.
- AICore inference wired through `dc_model::CallbackBackend` over the JNI bridge
  ([02](02-model-backends.md)).
- Android effects/tools: app-scoped working directory (no arbitrary shell);
  platform abstraction for filesystem ([04](04-tools.md)).
- Tighter default budgets/timeouts for mobile.
- **Exit criteria:** a scoped task runs fully offline on a device via AICore.
- *(Stretch: self-hosted LiteRT-LM fallback for non-AICore devices.)*

## M9 — Windows client (flexible)
**Goal:** the capable desktop client — same Rust core, full tools, flexible
backends ([12](12-platform-clients.md)).
- Desktop shell (CLI per [06](06-cli-ux.md); GUI optional) for `x86_64-pc-windows-msvc`.
- Flexible backends (Ollama / llama.cpp / OpenAI-compat) incl. up to the 12B
  ceiling, so this client can act as the **T1 orchestrator** ([02](02-model-backends.md)).
- Full filesystem + shell with the permission layer ([04](04-tools.md)).
- **Exit criteria:** completes a real multi-file TDD task on Windows; optionally
  orchestrates an Android device as an on-device worker ([08](08-orchestration-and-swarm.md)).

---

## Post-v1 / future ideas
- **MCP client** — consume external Model Context Protocol tool servers.
- **User-defined tools** via config.
- **Heterogeneous swarms** — specialized worker roles (searcher/editor/tester/
  integrator) mapped to different small models, beyond the M7 baseline
  ([08](08-orchestration-and-swarm.md)).
- **Embedding-based retrieval** with a small local embedder (optional).
- **TUI** (v2 interface).
- **Bounded autonomous mode** — unattended runs with strong budgets/guardrails.
- **LoRA/adapter experiments** — light task-specific tuning of the small model.

## Cross-cutting throughout
- **We dogfood TDD.** Every milestone lands with unit tests for its components,
  written test-first ([11](11-testing-and-tdd.md)) — the harness that drives
  tiny models red→green is itself built red→green. Unit tests are part of each
  milestone's definition of done.
- A **fixed task suite** (sample repos + graded tasks) as the regression
  benchmark; tracked from M1 so harness changes are measured against real
  small-model behavior.
  - **SWE-bench is the post-M3 feasibility check, not a current target.** Our
    `dc-eval` red→green machinery already mirrors SWE-bench's
    `FAIL_TO_PASS`/`PASS_TO_PASS` split, but three preconditions must land first
    or a run measures missing infrastructure, not the model: **(1)** per-task
    environment isolation (Docker images with pinned deps); **(2)** the retrieval
    index + context budgeter (M2 / `dc-index`) so a 4B model can navigate a large
    unfamiliar repo; **(3)** structured `run_verification` with pytest parsing
    (M3, [04](04-tools.md)). Sequence: a `dc-eval` SWE-bench *adapter* + a tiny
    pure-Python Docker subset once M2/M3 are in, then **SWE-bench Lite/Verified**
    as the real benchmark. Expect low absolute scores — purpose-built 7B coders
    sit ~18–23% ([10](10-prior-art.md)); the value is the *relative* signal across
    harness changes, not the headline number.
- Determinism/replay maintained at every milestone for debuggability
  ([03](03-agent-loop.md)).
