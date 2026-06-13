# 08 — Orchestration & the worker swarm

## Principle

A single tiny model is a weak agent. But **many tiny models, each given a
narrow, well-scoped subtask, coordinated by one larger model**, can cover a lot
of ground — cheaply and in parallel. This is the second core bet of
`dumb-coder` (the first being "the harness is smart, the model is dumb",
[00](00-overview.md)):

> One **orchestrator** model (the biggest we allow — up to the 12B ceiling)
> plans, decomposes, assigns, and integrates. A **swarm of worker** models
> (E4B-class, the smallest) each execute one small subtask in isolation.

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

## Each worker IS a `dumb-coder` agent loop

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

So `dumb-coder` defaults to the **conservative** posture and treats aggressive
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
   ([04](04-tools.md)) before it's offered for integration. Failing work never
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
