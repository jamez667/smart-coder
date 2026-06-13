# 09 — Workflow & human checkpoints

## Principle

`dumb-coder` does not jump from a one-line request straight to editing code. For
any non-trivial task it runs a **staged pipeline**, and between every stage it
**stops at a human checkpoint** for review and sign-off before continuing.

This is "human-in-the-loop" at the **macro** level — phase boundaries — and is
deliberately distinct from the **micro** per-tool confirmations in
[04 — Tools](04-tools.md):

| Granularity | What it gates | Doc |
| --- | --- | --- |
| **Macro — checkpoints** | Advancing from one workflow phase to the next | this doc |
| **Micro — permissions** | An individual risky tool call (shell, destructive) | [04](04-tools.md) |

Within a phase the agent works autonomously; at the boundary it yields control.
The human is "in the middle" only at the gates — not babysitting every action.

## Why this fits the small-model thesis

Each phase produces a **compact, approved artifact** that becomes the grounding
context for the next phase ([05](05-context-management.md)). A small model never
has to hold the whole problem at once — it reasons over one approved artifact to
produce the next. The checkpoints also catch a small model's mistakes *early*,
where they're cheap, instead of after a swarm has written code against a flawed
plan.

## The pipeline

```
  task
   │
   ▼
┌─────────────┐  ⛳ ┌───────────────┐  ⛳ ┌──────────┐  ⛳ ┌──────────────┐
│ 1. SPECS    │───▶│ 2. ARCHITECTURE│──▶│ 3. LAYOUT │──▶│ 4. STAGE      │
│ what & why  │    │ how, high-level│   │ structure │   │   BREAKDOWN   │
└─────────────┘    └───────────────┘    └──────────┘    │  (test-first) │
                                                          └──────┬───────┘
                                                                 │ ⛳
                                          ┌──────────────────────▼───────┐
                                          │ 5. IMPLEMENTATION PLAN        │
                                          │   how to make each stage pass │
                                          └──────────────────────┬───────┘
                                                                 │ ⛳
                                          ┌──────────────────────▼───────┐
                                          │ 6. WORK DECOMPOSITION         │
                                          │   slice into worker subtasks  │──▶ swarm
                                          └───────────────────────────────┘   ([08])

  ⛳ = human checkpoint (approve · revise · send back · abort)
```

### Phase 1 — Specs
**Produces:** spec documents (goals, non-goals, constraints) — exactly the kind
of docs in this `docs/specs/` tree. *Always the first step, for every task.*
**Checkpoint:** the human confirms "this is the right thing to build" before any
design happens.

### Phase 2 — Architecture
**Produces:** the high-level design — components, boundaries, data flow, key
technical choices — grounded in the approved specs.
**Checkpoint:** confirm the shape is sound before committing to a layout.

### Phase 3 — Layout
**Produces:** the concrete project structure — directories, modules/crates,
files, and their responsibilities — derived from the architecture.
**Checkpoint:** confirm where everything will live before planning the work.

### Phase 4 — Stage breakdown (test-first / TDD)
**Produces:** the work split into **incremental stages**, each stage defined
**by its unit tests written first** (full TDD, [11](11-testing-and-tdd.md)). A
stage's definition of done is "these tests go green." This is where TDD enters:
tests are specified before any implementation is planned, and the harness
verifies each new test actually **fails first** (no vacuous tests).
**Checkpoint:** confirm the staging order and that the tests capture the intent —
**approving the tests here freezes them as the contract** workers must satisfy
(and may not weaken) downstream ([08](08-orchestration-and-swarm.md), [11](11-testing-and-tdd.md)).

### Phase 5 — Implementation plan
**Produces:** for each stage, the concrete plan to make its tests pass — the
changes, in order, that turn red into green.
**Checkpoint:** confirm the approach before work is handed to models.

### Phase 6 — Work decomposition (→ the swarm)
**Produces:** the implementation plan sliced into **small, independent subtasks**
sized for the tiny worker models — i.e. the **task board / subtask DAG** that
the orchestrator and swarm consume directly ([08](08-orchestration-and-swarm.md)).
**Checkpoint:** confirm the decomposition and assignment before execution begins.

After Phase 6's gate, the swarm executes: workers work **red → green** against
the Phase-4 tests, with per-worker and integration verification
([08](08-orchestration-and-swarm.md), [03](03-agent-loop.md)).

## Checkpoint mechanics

At each ⛳ the agent halts and presents the phase artifact. The human chooses:

| Action | Effect |
| --- | --- |
| **Approve** | Artifact is accepted; proceed to the next phase. |
| **Revise** | Human edits the artifact directly (it's a file); the edited version is accepted. |
| **Send back** | Return to this phase (or an earlier one) with feedback notes; the agent regenerates. |
| **Abort** | Stop the workflow. Approved artifacts so far are kept. |

Rules:
- The gate is enforced by the **harness**, outside the model's control — the
  model cannot self-approve or skip a phase.
- **Send-back can target an earlier phase.** Discovering a layout problem during
  stage breakdown can bounce the workflow back to Phase 3; downstream artifacts
  are invalidated and regenerated. The pipeline is iterative, not strictly
  one-way.

## Artifacts are durable, versioned, and inspectable

- Every phase artifact is written to disk (e.g. under `docs/` and/or a
  `.dumb-coder/plan/` directory) and committed, so the plan is **reviewable as a
  diff** and survives across sessions (important in ephemeral environments).
- Because artifacts persist, the workflow is **resumable**: stop after the
  architecture gate today, resume at layout tomorrow — the approved artifacts are
  the state, not anything held in a model's context.
- The whole chain — spec → architecture → layout → stages/tests → plan →
  subtasks → code — is traceable end to end.

## Who drives the phases

The **orchestrator** model ([08](08-orchestration-and-swarm.md)) runs Phases 1–6
(the reasoning/planning work), producing each artifact via the single-agent loop
([03](03-agent-loop.md)). The **worker swarm** only engages after Phase 6's gate,
to execute. So the workflow is the connective tissue from "a request" to "an
orchestrated swarm building against approved, test-defined work."

This is the **tiered model assignment** ([02](02-model-backends.md)) in action:
the reasoning-heavy planning phases — including **authoring the tests** in Phase 4
— run on the biggest allowed model (T1, the architect), while the high-volume,
test-guarded implementation runs on the tiny, fast workers (T2). Hard to define,
cheap to satisfy.

## Scaling the ceremony to the task

Full six-phase ceremony is overkill for "fix this typo." The workflow is
**adaptive**:
- Trivial tasks may collapse phases (or run as a single-agent loop with one final
  checkpoint).
- The user can configure the **gate set** — e.g. auto-approve specs+architecture
  for small changes, or require every gate for large ones.
- Defaults: more gates for broader/destructive scope, fewer for narrow edits.

## CLI surface

Checkpoints are a first-class CLI interaction — present the artifact, accept an
approve/revise/send-back/abort decision, and show which phase the workflow is in.
See [06 — CLI & UX](06-cli-ux.md). In one-shot/non-interactive mode, the gate
policy determines whether the workflow auto-advances or stops at the first
un-approved gate and reports.

## Relationship to other specs

- Sits **above** the agent loop ([03](03-agent-loop.md)): each phase's artifact is
  produced *by* the loop; the workflow sequences the phases and gates them.
- Phase 6 is the **input contract** for the swarm ([08](08-orchestration-and-swarm.md)).
- Distinct from, and complementary to, per-tool permissions ([04](04-tools.md)).
- Phase artifacts are budgeted grounding context for later phases
  ([05](05-context-management.md)).
