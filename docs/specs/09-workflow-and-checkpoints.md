# 09 — Workflow & human checkpoints

## Principle

`smart-coder` does not jump from a one-line request straight to editing code. For
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
└─────────────┘    └───────────────┘    └──────────┘    │  (test-first, │
                                                          │   + per-stage │
                                                          │     steps)    │
                                                          └──────┬───────┘
                                                                 │ ⛳
                                          ┌──────────────────────▼───────┐
                                          │ 5. WORK DECOMPOSITION         │
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
**by its unit tests written first** (full TDD, [11](11-testing-and-tdd.md)), *and*
the concrete per-stage steps that turn those tests red → green. A stage's
definition of done is "these tests go green." This is where TDD enters: tests are
specified before any implementation is planned, and the harness verifies each new
test actually **fails first** (no vacuous tests).
**Checkpoint:** confirm the staging order, the steps, and that the tests capture
the intent — **approving the tests here freezes them as the contract** workers
must satisfy (and may not weaken) downstream
([08](08-orchestration-and-swarm.md), [11](11-testing-and-tdd.md)).

> **Test-first is currently Python-only.** The phase emits the JSON coverage plan
> that drives worker-written frozen tests only for a Python project; every other
> stack gets an ordered Markdown breakdown with per-stage steps and an explicit
> "not tests, not code" directive. So on a Rust/JS project the frozen-test contract
> described above does not yet exist — the per-stage steps do. Widening it is a
> matter of more test-framework support ([11](11-testing-and-tdd.md)).

> **On the folded implementation-plan phase.** This phase carries the per-stage
> steps that a separate Phase 5 ("implementation plan") used to add. In practice
> that phase re-chewed the breakdown rather than adding to it: with the stages and
> their tests already settled, a reviewer read it, found nothing new, and clicked
> through. Deriving the sequence and the steps in one grounded call is the cheaper
> equivalent, so the pipeline is **five phases**, not six.
>
> The merge is worth revisiting for **headless runs driving a weak worker**, where
> the artifact is executed rather than read: a separate phase could spell out the
> steps *after* the sequence is approved, grounding one call on the other instead
> of asking a single call to do both. That would make phase granularity scale to
> the worker's capability, the same way ceremony scales gating to the task (see
> "Scaling the ceremony to the task"). Deferred until full-auto output actually
> proves too coarse — a real signal, not a hypothesis.

### Phase 5 — Work decomposition (→ the swarm)
**Produces:** the stage breakdown sliced into **small, independent subtasks**
sized for the tiny worker models — i.e. the **task board / subtask DAG** that
the orchestrator and swarm consume directly ([08](08-orchestration-and-swarm.md)).
**Checkpoint:** confirm the decomposition and assignment before execution begins.

After the final phase's gate, the swarm executes: workers work **red → green**
against the Phase-4 tests, with per-worker and integration verification
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
- Where a front-end anchors feedback to artifacts, the *placement* of the notes
  chooses the target: the send-back returns to the **earliest phase carrying a
  note**, because invalidating from there drops every later artifact anyway, so a
  downstream phase that was also commented regenerates from the correction. Notes
  from all commented phases ride along on that one send-back, so the reviewer's
  downstream observations are not lost.

## Artifacts are durable and inspectable

- Every phase artifact is written to disk — by default `specs/<slug>/` beside the
  feature it describes (the OpenSpec layout: `spec.md`, `architecture.md`,
  `layout.md`, …), falling back to a numbered `.smart-coder/plan/NN-phase.md` only
  when the task text yields no usable slug — so the plan is **reviewable as a
  diff** and survives across sessions (important in ephemeral environments).
  *Not built:* the workflow never **versions or commits** artifacts itself — a
  save overwrites in place, so a send-back discards the prior draft. Ordinary
  version control covers the reviewable-diff intent day to day.
- The directory is derived from the task text itself: a task naming an existing
  `specs/<slug>/spec.md` uses that feature directory verbatim, otherwise the task
  is slugified into `specs/<slug>/`. Every front-end resolves it through the same
  engine helper (`sc_workflow::artifact_dirs`), so the CLI and the desktop GUI land
  the same task in the same place — which is what lets a later run resume from the
  `state.json` a prior approved run left there.
- Anchored review notes are consumed by the send-back that delivers them: once
  handed to the workflow they are cleared, because the line ranges they point at
  describe text the regeneration is about to replace.
- Because artifacts persist, the workflow is **resumable**: stop after the
  architecture gate today, resume at layout tomorrow — the approved artifacts are
  the state, not anything held in a model's context.
- The whole chain — spec → architecture → layout → stages/tests → subtasks →
  code — is traceable end to end.

## Who drives the phases

The **orchestrator** model ([08](08-orchestration-and-swarm.md)) runs Phases 1–5
(the reasoning/planning work), producing each artifact via the single-agent loop
([03](03-agent-loop.md)). The **worker swarm** only engages after the final gate,
to execute. So the workflow is the connective tissue from "a request" to "an
orchestrated swarm building against approved, test-defined work."

This is the **tiered model assignment** ([02](02-model-backends.md)) in action:
the reasoning-heavy planning phases — including **authoring the tests** in Phase 4
— run on the biggest allowed model (T1, the architect), while the high-volume,
test-guarded implementation runs on the tiny, fast workers (T2). Hard to define,
cheap to satisfy.

## Scaling the ceremony to the task

Full five-phase ceremony is overkill for "fix this typo." The workflow is
**adaptive**:
- The user can configure the **gate set** — e.g. auto-approve specs+architecture
  for small changes, or require every gate for large ones. *Built:* named tiers
  (`--ceremony minimal|standard|full`) and an explicit `--gates` list, applied by
  a gate that consults the human only for the phases in the set and auto-approves
  the rest. All tiers still run all five phases — only which phases **gate**
  changes.
- Trivial tasks may collapse phases (or run as a single-agent loop with one final
  checkpoint). *Not built:* no phase ever collapses. The single-agent path is a
  separate command (`run`), not a tier of this pipeline.
- Defaults: more gates for broader/destructive scope, fewer for narrow edits.
  *Not built:* nothing inspects the task to pick a tier — it comes from an
  explicit flag, defaulting to full ceremony.

A front-end whose review surface makes an extra stop cheap may reasonably gate
every phase regardless of tier: the desktop GUI shows each artifact for inline
review, so clicking through an uninteresting phase costs far less than a terminal
prompt does. Ceremony tiers earn their keep where a stop is expensive.

## CLI surface

Checkpoints are a first-class CLI interaction — present the artifact, accept an
approve/revise/send-back/abort decision, and show which phase the workflow is in.
See [06 — CLI & UX](06-cli-ux.md). In one-shot/non-interactive mode, the gate
policy determines whether the workflow auto-advances or stops at the first
un-approved gate and reports.

## Relationship to other specs

- Sits **above** the agent loop ([03](03-agent-loop.md)): each phase's artifact is
  produced *by* the loop; the workflow sequences the phases and gates them.
- The final phase (work decomposition) is the **input contract** for the swarm
  ([08](08-orchestration-and-swarm.md)).
- Distinct from, and complementary to, per-tool permissions ([04](04-tools.md)).
- Phase artifacts are budgeted grounding context for later phases
  ([05](05-context-management.md)).
