# 11 — Testing & TDD

## Principle

Testing is not a phase in `smart-coder` — it is the **control system** for the
whole agent. A small model cannot be trusted to judge whether its own code is
correct; a **test can**. So every unit of work is defined by a test *before* it
is implemented, and "done" means "the test goes green (and nothing else went
red)." **Full TDD, mandatory at the unit level.**

This is arguably the most important single technique for making dumb models
usable, because a test gives us the one thing a small model lacks: an
**unambiguous, machine-checkable oracle** that is independent of the model's
opinion of its own work.

## Why TDD is uniquely powerful *for small models*

| Small-model weakness | How a test compensates |
| --- | --- |
| Can't reliably judge correctness | The test decides — pass/fail is objective, not the model's say-so |
| Drifts from the goal mid-task | A failing assertion is a fixed, concrete target the model can't wander from |
| Poor self-verification | Red→green is verification the *harness* runs, not the model |
| Vague task understanding | Writing the test *first* forces the intent to be made concrete and small |
| Bad at large scope | A single unit test scopes the work to one tiny, checkable behavior |
| Failure output is the best prompt | A failing test's message is precise, grounded feedback for the next turn |

In short: tests turn "trust the model" into "trust the test runner." That swap is
what makes a 4B-class model viable for real edits.

**Who writes the tests matters.** Authoring a good test is *harder* than passing
it — it pins down intent and edge cases, which is exactly the reasoning a tiny
model is worst at. So under the **tiered model assignment** ([02](02-model-backends.md)),
test authoring (Phase 4) is **T1 "architect" work** (the biggest allowed model),
while making the tests pass is **T2 "coder" work** for the tiny fast workers. The
expensive judgment is spent defining correctness once; the cheap models race to
satisfy it.

## The cycle: red → green → refactor (harness-driven)

```
   ┌─────────────────────────────────────────────────────────────┐
   │ 1. RED    write/confirm a failing unit test for the behavior  │
   │           → harness RUNS it and verifies it actually FAILS    │  ← proves the
   │                                                               │    test bites
   │ 2. GREEN  implement the minimum to make it pass               │
   │           → harness runs the test until green                 │
   │           → harness runs the WHOLE suite (no regressions)     │
   │                                                               │
   │ 3. REFACTOR (optional) clean up with the suite as a safety net│
   └─────────────────────────────────────────────────────────────┘
```

The crucial, non-obvious step is **"verify red first."** The harness runs the new
test *before* any implementation and confirms it fails. A test that passes before
the code is written tests nothing — this catches tautological/vacuous tests a
small model is prone to writing, and proves the test is actually wired to the
behavior. Only a genuinely-red test may proceed to GREEN.

## Where TDD lives in the system

TDD is woven through the specs, not bolted on:

- **Workflow ([09](09-workflow-and-checkpoints.md)) — Phase 4 is test-first.**
  The stage breakdown defines each stage *by the unit tests written first*. The
  human reviews and signs off on the tests at that checkpoint — so the tests
  (the contract) are **human-approved before any implementation**.
- **Agent loop ([03](03-agent-loop.md)) — tests are the VERIFY gate.** The
  primary verification signal each step is the test run; failures re-enter the
  loop as grounded observations.
- **Tools ([04](04-tools.md)) — `run_verification` runs the tests** and returns
  structured pass/fail per test, not a raw blob.
- **Swarm ([08](08-orchestration-and-swarm.md)) — a worker's definition of done
  is "my tests are green."** The orchestrator hands a worker a subtask *and its
  tests*; the worker works red→green. Integration verification re-runs the full
  suite after each merge to catch semantic conflicts.

## The test contract (orchestrator ↔ worker)

Tests are the **interface** between the planning layer and the execution layer:

1. The orchestrator (via Phase 4) produces, for each subtask, the unit tests that
   define success — reviewed and frozen at the checkpoint.
2. A worker receives **subtask + its frozen tests**, and must make them green
   without breaking any other test.
3. The worker **may not weaken or delete the contract tests.** It may *add* tests
   (encouraged), but the approved tests are immutable to the worker.
4. A worker may only declare `finish` when its tests are green and the full suite
   still passes.

This makes worker success **objectively checkable by the harness** — no model
needs to vouch for another model's work.

## Anti-gaming guards

Small models (and, honestly, large ones) will "pass the test" the lazy way unless
prevented. The harness defends the integrity of the signal:

- **Verify-red-first** — a test must fail before implementation (above), so it
  can't be vacuous.
- **Frozen contract tests** — edits to approved test files are blocked for
  workers; an attempt is flagged to the orchestrator/human, never silently
  allowed. (`edit_file` on a contract-test path is denied by the permission
  layer, [04](04-tools.md).)
- **No special-casing the test** — heuristics flag implementations that hardcode
  the exact expected value / detect the test environment; suspicious diffs
  escalate.
- **Whole-suite gate** — making one test green while breaking another is a
  failure, not a pass.
- **Coverage as a guard, not a goal** — a configurable coverage floor for changed
  code catches "implemented but untested" paths; it is a backstop, not the
  target (coverage is gamed easily; behavior tests are the point).

## Scope: "at least unit level"

- **Unit tests: mandatory.** Every implementation subtask has them, first.
- **Integration / end-to-end: where they add value** — e.g. the swarm's
  integration-verification step, or cross-module behavior the unit tests can't
  reach. Encouraged, not required at the same strictness as unit.
- **Test framework is per-project**, discovered/configured like the build command
  ([06](06-cli-ux.md)'s project file): the harness needs to know how to *run*
  tests and *parse* their results, nothing more.

## Test result parsing

`run_verification` ([04](04-tools.md)) must return **structured** results — which
tests passed/failed, and the failure messages — not a 5k-line log. The Context
Manager ([05](05-context-management.md)) feeds the *failing* cases (prioritized,
truncated) back to the model: a small window should be spent on what's broken, not
on a wall of green.

## We dogfood this

`smart-coder` itself is built test-first at the unit level. Each roadmap milestone
([07](07-roadmap.md)) lands with unit tests for its components (the model gateway,
tool schemas/validation, context budgeter, planner, integration logic). The
agent's own test suite is part of every milestone's definition of done — if we
expect tiny models to work red→green, the harness that drives them must too.

## Relationship to other specs

- Defines the VERIFY gate of the loop ([03](03-agent-loop.md)).
- Phase 4 of the workflow produces the tests; its checkpoint approves them
  ([09](09-workflow-and-checkpoints.md)).
- A worker's done-condition and integration verification ([08](08-orchestration-and-swarm.md)).
- Executed via `run_verification` with structured results ([04](04-tools.md)).
- Failing-test feedback is budgeted context ([05](05-context-management.md)).
