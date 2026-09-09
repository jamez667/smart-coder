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

Every harness verification run is bounded by a timeout (300s in `sc-eval`); the
code being verified was written by a model, so non-termination is a routine
outcome rather than an exotic one. A timed-out verification is scored red, and
the whole process tree is killed — not just the shell the harness spawned, whose
children would otherwise survive it and block the harness on their inherited
handles.

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
  layer, [04](04-tools.md).) The freeze is enforced twice, independently: the
  permission layer denies the edit tools up front, and the harness re-hashes
  every contract test after the solve and scores any change as tampering
  regardless of whether the suite went green. The second check is load-bearing
  because shell access lets a model reach a frozen file without going through a
  denied tool.
- **No special-casing the test** — heuristics flag implementations that hardcode
  the exact expected value / detect the test environment; suspicious diffs
  escalate.
- **Whole-suite gate** — making one test green while breaking another is a
  failure, not a pass.
- **Green must be EARNED** — the harness records a **baseline** verification once,
  before the first turn, and reports it as `started_green`. Auto-finish on green
  exists because red→green is the agent's doing; on a task whose suite is *already
  green* (a refactor, an extraction, a rename) green proves nothing, so the harness
  will not end the run on it — the model must call `finish` itself, and `finish` is
  still honoured. Measured: on a real extraction task the model created one
  unreferenced new file, `cargo check` passed (an unreferenced file changes
  nothing), and the loop reported `finished: true, verified: true` with a third of
  the job done.
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

### The harness is measured, not assumed

Every ladder number is "model plus harness", so the harness itself is measured
by A/B: `ladder-ab --arms control,raw,pi,gateway` runs `evals/ladder` once per
arm against the same model, step cap and `run_task` grader, so red-first, frozen
contract tests and the tamper check apply identically and no arm can pass by
editing a test. `raw` is the model with native tool calling and no harness at
all (no repo map, plan, nudges, stall detection or repair) — the control that
says whether a gain or loss belongs to the harness or the model. `pi` is an
external coding agent (`evals/pi/`) on the same model, a calibration point: a
rung pi solves that the in-tree loop cannot is a harness gap, not a model one.
A single pass over ~14 tasks is a signal, not a result; `--repeat N` shows the
spread, and a difference that does not survive repetition is not a difference.
Rungs are graded on two axes, because the first alone stopped ranking:
diagnostic distance (how far the fix sits from the symptom, `rung:stated`
through `rung:invariant`) and *spread* — how many files one correct change must
touch. Six of the ten distance rungs scored identically for every arm every
round, so the `rung:cross-file-*` family makes a change span three or four files
(a new enum variant handled at three sites, a parameter whose right value
differs at four call sites, an off-by-one in a helper the failing test does not
name, a trait method that cannot be copy-pasted), each built so the shortcut — a
`_ =>` catch-all, patching the file the test points at — provably fails.

Each row also records `cached_prompt_tokens`, `prefilled_prompt_tokens` and
`cache_hit_percent` — what the server reused versus re-prefilled
([02](02-model-backends.md)) — which `ab_report` prints as a "prefix cache"
table. An arm whose backend reports no split prints "not reported", never a 0%
it did not observe.

Every (task, arm, model, commit, repeat) is appended as one JSON line to
`rows.jsonl` under `--out` (`sc_eval::ResultRow`), so a run outlives its
scrollback and rows from different commits are never mistaken for the same
experiment. Dated baselines are committed under `evals/results/`. Beside
outcome, steps, wall time, total/peak prompt tokens and harness-fault counts, a
`MetricsSink` on the event stream records how the task was solved, not only
whether: `turns_to_first_edit` (the step of the first mutating call — twenty
turns of reading first is deliberation a solve rate cannot see), `re_reads` (a
read-only call repeated verbatim: harness amnesia or model thrash, told apart by
the prompt size beside it), and `wasted_turns` (stalls plus repair prompts).

## Relationship to other specs

- Defines the VERIFY gate of the loop ([03](03-agent-loop.md)).
- Phase 4 of the workflow produces the tests; its checkpoint approves them
  ([09](09-workflow-and-checkpoints.md)).
- A worker's done-condition and integration verification ([08](08-orchestration-and-swarm.md)).
- Executed via `run_verification` with structured results ([04](04-tools.md)).
- Failing-test feedback is budgeted context ([05](05-context-management.md)).
