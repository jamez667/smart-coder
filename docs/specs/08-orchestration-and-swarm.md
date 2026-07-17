# 08 — Orchestration & the worker swarm

## Principle

A single tiny model is a weak agent. But **many tiny models, each given a
narrow, well-scoped subtask, coordinated by one larger model**, can cover a lot
of ground — cheaply and in parallel. This is the second core bet of
`smart-coder` (the first being "the harness is smart, the model is dumb",
[00](00-overview.md)):

> One **orchestrator** model (the biggest we allow — up to the 12B ceiling)
> plans, decomposes, assigns, **authors the tests**, and integrates. A **swarm of
> worker** models (E4B-class, the smallest) each execute one small subtask in
> isolation, making those tests pass.

This is the **tiered model assignment** of [02](02-model-backends.md): the
orchestrator is the T1 "architect" tier; workers are the T2 "coder" tier. Match
the model to the difficulty of the work — define intent with the big model, fill
it in with the cheap ones.

This plays directly to small-model strengths: a worker only ever sees a tight,
single-purpose task with minimal context (exactly the regime small models handle
well, [05](05-context-management.md)), while the hard, big-picture reasoning —
decomposition, conflict arbitration, acceptance — is concentrated in the one
slightly-larger orchestrator.

> **Model ceiling note.** The orchestrator is "bigger" *relative to the
> workers*; it still respects the project's ≤12B ceiling by default (e.g. a 12B
> orchestrator over E4B workers). Whether the orchestrator may exceed 12B is a
> single config choice ([02](02-model-backends.md)); the harness is designed and
> benchmarked for the all-small case.

## Topology: hub-and-spoke + blackboard

Workers do **not** talk to each other. They talk to the orchestrator, and they
share exactly one thing: the codebase (plus a shared task board). This avoids the
combinatorial chaos of peer-to-peer small-model chatter.

```
                         ┌───────────────────────────┐
                         │        ORCHESTRATOR        │  (≤12B, "the big one")
                         │  plan · decompose · assign │
                         │  monitor · arbitrate ·     │
                         │  integrate · accept        │
                         └───────────┬───────────────┘
                                     │  task board (subtasks, status, deps)
              ┌──────────────┬───────┴───────┬──────────────┐
              ▼              ▼               ▼              ▼
        ┌──────────┐   ┌──────────┐   ┌──────────┐   ┌──────────┐
        │ worker A │   │ worker B │   │ worker C │   │ worker D │   (E4B-class)
        │ subtask  │   │ subtask  │   │ subtask  │   │ subtask  │
        └────┬─────┘   └────┬─────┘   └────┬─────┘   └────┬─────┘
             │ worktree A   │ worktree B   │ worktree C   │ worktree D
             ▼              ▼               ▼              ▼
        ╔════════════════════════════════════════════════════════╗
        ║                shared git repository                    ║
        ║      (each worker isolated in its own worktree/branch)  ║
        ╚════════════════════════════════════════════════════════╝
```

- **Blackboard = the repo + the task board.** The orchestrator writes subtasks
  to the board; workers claim one, do it, report results back. State is durable
  and inspectable, not held in any model's head.
- **Hub-and-spoke = all coordination flows through the orchestrator.** No worker
  ever depends on another worker's running state.

## Each worker IS a `smart-coder` agent loop

A worker is not a new concept — it's the single-agent loop from
[03](03-agent-loop.md), scoped to one subtask, with one tiny model, its own
context budget, and its own isolated workspace. The swarm layer is a coordinator
*above* that loop. This keeps the design composable: improve the agent loop and
every worker improves.

## Concurrency posture: parallel intelligence, serialized writes

The hard lesson from production multi-agent systems ([10](10-prior-art.md)):
naive parallel writers are **fragile** because agents lack shared global context —
each makes locally-sensible but globally-incoherent choices, and independently-
correct diffs can still break in combination (semantic conflicts). Cognition's
distilled rule is **"writes stay single-threaded; multiple agents contribute
intelligence."**

So `smart-coder` defaults to the **conservative** posture and treats aggressive
parallel-write as an opt-in we validate empirically against the eval suite
([07](07-roadmap.md)):

- **Parallelize exploration & proposals** — many workers read, search, and
  draft scoped changes concurrently in isolation (cheap, the swarm's real win).
- **Serialize integration** — changes land into the shared mainline **one at a
  time, through the orchestrator**, each gated by integration verification, so
  the mainline always has a single coherent state.

This keeps the throughput benefit (the slow part — reasoning over code — happens
in parallel) while removing the riskiest failure mode (uncoordinated concurrent
writes).

## Isolation: how N models edit one codebase safely

Under that posture, isolation has two modes:

### Default — worktree-per-worker (parallel work, serialized landing)
Each worker gets its **own git worktree on its own branch** off a shared base
commit, and works in parallel without stepping on others — the filesystem
enforces isolation. Their branches are then **integrated one at a time** by the
orchestrator (see Integration below), not merged in an uncontrolled free-for-all.
This captures the cheap-parallelism payoff of small models *without* concurrent
writes to a shared mainline.

### Fallback — serialized shared workspace (for tightly-coupled work)
When a task doesn't decompose into independent pieces, the swarm degrades
gracefully to **one shared workspace with a file-lease queue**: a worker must
hold a lease on the files it edits; the orchestrator serializes conflicting
leases. Slower, but safe for changes that can't be cleanly partitioned.

### Opt-in — full parallel-write (experimental)
For workloads proven (via the eval suite) to decompose cleanly, the orchestrator
may relax to fully concurrent integration. Off by default; enabled per-task via
config once the gains are demonstrated to outweigh the conflict risk.

The orchestrator chooses the mode per task based on how independent the subtasks
are (and the user can force one via config).

## Orchestrator responsibilities

1. **Decompose** the task into subtasks that are as **independent** as possible
   (minimize shared files → minimize merge conflicts). Record dependencies on
   the task board (a DAG; independent subtasks run concurrently, dependents wait).
2. **Scope each subtask tightly** — a precise goal + the specific files/symbols a
   worker should touch, so the worker's context stays tiny.
3. **Assign & schedule** subtasks to workers respecting the dependency DAG and a
   concurrency limit (how many workers run at once, bounded by hardware).
4. **Monitor** worker progress via their event streams; detect stuck/looping
   workers ([03](03-agent-loop.md)) and reassign, re-split, or escalate.
5. **Arbitrate conflicts** during integration (the bigger model's reasoning is
   spent here, not on line-by-line editing).
6. **Integrate & accept** — merge worker branches, run integration verification,
   and decide done/redo.

## Integration & conflict resolution

Integration is where multi-writer risk is actually paid down:

1. Worker finishes → its branch passes the worker's **local** verification
   ([04](04-tools.md)) before it's offered for integration: its **contract tests
   are green** and the suite still passes ([11](11-testing-and-tdd.md)). The
   orchestrator hands each worker a subtask *and its frozen tests*; "done" is
   objectively the test result, not the worker's claim. Failing work never
   reaches the merge.
2. Orchestrator merges branches in dependency order. Clean merges proceed
   automatically.
3. On a **textual conflict**, the orchestrator (the larger model) resolves it
   with full context of both sides — or spawns a dedicated **integrator worker**
   for a scoped resolution.
4. After each merge, run **integration verification** (full build/test) — because
   independently-correct changes can still break in combination (semantic
   conflicts). A failure here feeds back: re-split, reassign, or fix.
5. Only after integration verification passes does the orchestrator `finish`.

## Subtask retry on partial or rejected integration

Step 4 above says a failure "feeds back: re-split, reassign, or fix" — this
section makes that loop concrete. It exists because of a gap proven live: a tiny
worker often lands a **partial** fix — its proposal is *closer* but doesn't make
its scoped tests fully pass.

### The gap

Integration uses a **cumulative "didn't make it worse" gate**: a proposal is
accepted when the suite's failing-test count after the merge is `≤` the count
before. This is deliberate — it lets a multi-file task land its pieces one wave at
a time, each piece leaving the *whole* suite red while other files are still
unfixed, without reverting good partial progress (a worker that fixes only its own
file shouldn't be punished for the files it wasn't asked to touch).

But the same leniency accepts a *non-improving or partially-improving* change to a
subtask's **own** files. Concretely, observed live (2026-06-14): a `clamp` worker
proposed `max(lo, x)` — fixing the lower-bound test, leaving the upper-bound test
red. Failing count went `2 → 1`, so the gate **accepted** it and the board marked
the subtask `Done`. Every subtask "integrated", the board read all-done — yet the
subtask's contract was not met. Today the only backstop is the **final
integration verification** (step 5), which correctly reports the run *not done*
(honest stop, [06](06-cli-ux.md)) — but the swarm never gives the subtask another
attempt. It stops with a red suite and a `Done` subtask that isn't.

### The retry loop

A subtask is **objectively done only when its own scoped tests are green**, not
when it merely didn't worsen the suite. When an accepted-but-incomplete (or
outright rejected) proposal leaves a subtask's tests failing, the orchestrator
**re-dispatches that subtask to a worker, with feedback**, up to a bounded retry
budget — mirroring the single-agent per-step retry budget of M4
([07](07-roadmap.md), [03](03-agent-loop.md)).

Per subtask, after integration:

1. **Decide the subtask's true status** from a *scoped* verification, not the
   whole-suite delta. The orchestrator knows the subtask's frozen test files
   (`SwarmConfig.frozen_paths`, handed down from the staged workflow's Phase 4,
   [09](09-workflow-and-checkpoints.md)); it runs the verification command
   filtered to those tests (e.g. `pytest <the subtask's test files>`). Green →
   the subtask is genuinely `Done`. Still red → it's **incomplete**, regardless
   of whether the cumulative gate accepted the bytes.
   - *When the subtask's tests aren't individually known* — e.g. a free-text
     `swarm <task>` run that decomposes on the fly, where `frozen_paths` is empty
     and the decomposer's per-subtask `files` name source, not tests — the scoped
     check degrades to the **whole-suite delta vs. this subtask's own baseline**:
     incomplete iff the suite is still red *and* this subtask's merge didn't clear
     it. Coarser (it can't attribute a residual failure to a specific subtask), so
     the staged-workflow path with frozen per-subtask tests is the precise one;
     this fallback at least stops the swarm from declaring a red run done.
2. **On incomplete (or a hard reject), retry** if the subtask's retry counter is
   below `max_subtask_retries` (default **2**; `0` restores today's
   no-retry behaviour). Re-dispatch the *same* subtask to a worker with a
   **feedback-augmented prompt**: the still-failing test names and their assertion
   messages (`sc_verify::TestReport::failed()` already carries `name` +
   `message`), plus the current (already-merged) file contents. This is the
   swarm-level analogue of the agent loop feeding a failing case back to the model
   ([11](11-testing-and-tdd.md)) — "here's what's still wrong, try again", not a
   blind re-run.
3. **Each retry is gated and serialized exactly like the first attempt** — propose
   in a scratch copy, merge one-at-a-time, integration-verify. A retry that
   *regresses* the suite is reverted (the cumulative gate still applies on top of
   the scoped check); the prior best state is never lost. A retry that improves
   but still isn't green consumes one attempt and feeds back again.
4. **On retry-budget exhaustion**, mark the subtask `Failed` (not `Done`) with the
   residual failing tests as the reason, and surface it. A dependent subtask whose
   dependency `Failed` stays blocked and the board goes quiescent (the task
   board's existing quiescence rule) — the run stops honestly rather than
   building on a broken base. The orchestrator *may* escalate to the advisor for
   a one-line nudge before the final attempt ("junior asks senior",
   [02](02-model-backends.md), M4) — advice, not the fix.

### What stays the same

- The **cumulative whole-suite gate** is unchanged and still runs — it's the
  cross-file safety net (semantic-conflict containment). The scoped check is an
  *additional, per-subtask* completion criterion layered on top, not a
  replacement: a proposal must both (a) not worsen the whole suite **and** (b)
  make its own subtask's tests pass to be `Done`.
- The **final integration verification** (step 5) remains the last word on the
  run's honesty. With the retry loop, reaching it green is the *expected* outcome
  rather than a lucky one; it stays as the backstop for residual semantic
  conflicts the per-subtask checks can't see.
- **Failure containment** ([below](#why-this-is-worth-the-complexity)) is
  preserved: retries run in fresh scratch copies, so a worse retry never corrupts
  the accepted state.

### Events & inspection

The retry loop is visible in the swarm event stream
([determinism & inspection](#determinism--inspection), [06](06-cli-ux.md)): a new
`SwarmEvent::SubtaskRetry { subtask, attempt, max, failing_tests }` is emitted
before each re-dispatch, so the CLI/dashboard renders "↻ retry 1/2 — N tests still
red" and `replay` reconstructs exactly how many attempts each subtask took. A
subtask that exhausts its budget ends in the existing
`Integrated { accepted: false, .. }` with the residual failures as the reason.

### Bounds (so a tiny model can't spin forever)

- `max_subtask_retries` — per-subtask attempt cap (default 2). Total worker
  invocations for a subtask is `1 + retries`.
- Retries reuse the existing per-worker step/token budgets ([05](05-context-management.md));
  a retry is a fresh, equally-bounded worker run.
- The whole-run global step budget still bounds the swarm overall — retries draw
  from it, so a pathological board can't multiply work without limit.

## Specialized worker roles (optional)

Workers can be homogeneous (all the same model, generic) or **specialized** by
prompt/role — small models do better when narrowly pointed:

- `searcher` — read-only exploration, returns where things live.
- `editor` — applies a scoped change to named files.
- `tester` — writes/updates tests for a unit.
- `integrator` — resolves a specific merge conflict.

Roles are just different system prompts + tool subsets over the same worker loop;
different roles can even map to different small models via profiles
([02](02-model-backends.md)).

## Why this is worth the complexity

- **Throughput.** Small models are fast and cheap; running many in parallel turns
  a long serial task into a short concurrent one.
- **Bounded context per worker.** Each worker's window stays small and relevant —
  the thing small models need most ([05](05-context-management.md)).
- **Failure containment.** A worker that goes off the rails damages only its
  worktree; the orchestrator discards and reassigns. No single tiny model can
  corrupt the whole task.
- **Concentrated reasoning.** The one capability-limited "expensive" model is
  spent on planning and arbitration, where reasoning matters most.

## Risks & mitigations

| Risk | Mitigation |
| --- | --- |
| Merge conflict storms from overlapping edits | Decompose for independence; minimize shared files; orchestrator arbitrates |
| Semantic conflicts (each change correct, combo broken) | Mandatory **integration verification** after merges |
| Orchestrator (still smallish) mis-decomposes | Start coarse, allow re-split; human-in-the-loop can edit the task board |
| Coordination overhead > benefit on small tasks | Orchestrator may run single-worker (degenerates to plain [03](03-agent-loop.md)) |
| Resource exhaustion (N models at once) | Hard concurrency cap; schedule against available memory/compute |
| Wasted parallel work on dependent subtasks | Dependency DAG gates scheduling; only independent subtasks run concurrently |

## Determinism & inspection

The task board, every assignment, each worker's event log, and all
merge/arbitration decisions are recorded in the session log
([01](01-architecture.md), [06](06-cli-ux.md)). A swarm run is replayable and
auditable: you can see which worker did what, in which worktree, and how
conflicts were resolved.

## Relationship to other specs

- Workers reuse the agent loop ([03](03-agent-loop.md)) and tools
  ([04](04-tools.md)) verbatim.
- Orchestrator vs. worker models are just **model profiles**
  ([02](02-model-backends.md)) — already anticipated as "multi-model routing".
- The CLI must surface swarm state (active workers, board, integration);
  see [06](06-cli-ux.md).
- This is a **post-core** capability: the single-agent loop (M0–M5) must be
  solid first. See the roadmap ([07](07-roadmap.md)).
- The swarm's **input** is the task board produced by Phase 6 of the staged
  workflow, *after* its human checkpoint ([09](09-workflow-and-checkpoints.md));
  the orchestrator drives those planning phases, then the swarm executes.
