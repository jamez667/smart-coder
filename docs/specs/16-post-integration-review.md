# 16 — Post-integration review

## Principle

**Green is a floor, not a finish line.**

A worker's proposal is accepted today when the suite passes ([08](08-orchestration-and-swarm.md)).
That gate answers *does it work?* and answers it well — the test is the oracle
([11](11-testing-and-tdd.md)). It is silent on *should this code stay?* A small
model can go green by duplicating a helper it never found, swallowing an error to
make an assertion pass, widening a signature nothing asked it to widen, or
reimplementing an abstraction three files away. Every one of those is green, and
every one of those is a defect a reviewer would have caught.

This spec adds a second gate over the *integrated diff*, after verification and
before the run is reported done. It exists because the tiny-model thesis makes
this failure mode more likely, not less: a 4B worker sees a keyhole of the
repository ([05](05-context-management.md)), so "I couldn't find the existing
helper, so I wrote one" is its most natural mistake, and no test will ever notice.

The constraint that makes this honest, and the one it would be tempting to break:

> **Review never rewrites code, and never blocks on taste.**

A review finding is *evidence handed to a decision*, not an edit. The moment a
reviewer is allowed to fix what it flags, a green-but-ugly result becomes a
silently-modified result, and the swarm's careful "propose → verify → integrate"
discipline is bypassed by a critic with write access. And because a reviewer is a
model, its findings are not reproducible in the way `sc-verify`'s are — so they
inform a stop, they do not *become* one, unless a deterministic check agrees.

## What it is not

- **Not a linter.** `cargo clippy` and friends already run in verification, are
  deterministic, and are cheaper. If a check can be expressed as a lint rule, it
  belongs in the verify command, not here.
- **Not a second verification.** It never runs the suite. It reads a diff.
- **Not compliance.** [13](13-compliance-evidence.md) audits a repository against
  an external framework with no model in the loop. This audits *one change* with a
  model in the loop, because "did this duplicate an existing abstraction?" has no
  regex.

## Shape

The review runs over the **integrated diff** — what actually landed in mainline
after a proposal was accepted — not the worker's raw proposal. A proposal that was
partially applied, or applied alongside another worker's, produces a different
diff than the worker wrote, and the reviewed artifact must be the one that ships.

```
worker proposal ─► integration verify (spec 08) ─► integrated diff
                                                        │
                                                        ▼
                                          ┌──────────────────────────┐
                                          │  review lenses (parallel)│
                                          │  duplication · error-    │
                                          │  handling · scope ·      │
                                          │  abstraction-fit         │
                                          └────────────┬─────────────┘
                                                       │ findings
                                                       ▼
                                            deterministic corroboration
                                                       │
                                                       ▼
                                        Finding{ severity, file:line,
                                                 corroborated: bool }
                                                       │
                                                       ▼
                                       report · gate · or feed a retry
```

### Lenses, not one reviewer

A single "review this diff" prompt returns whatever the model noticed first. Each
lens is a separate call with one question, because a reviewer asked one question
answers it far better than a reviewer asked four:

| Lens | The question | Why a test can't ask it |
| --- | --- | --- |
| **Duplication** | Does this reimplement something the repo already has? | The duplicate passes its own tests |
| **Error handling** | Is a failure swallowed, or an error path untested? | A swallowed error *is* a passing test |
| **Scope** | Does the diff do things the subtask didn't ask for? | Extra correct code is still green |

Scope is listed last deliberately: it is the least corroborable of the four (see
below), so it is the first lens to drop if review proves too expensive.
| **Abstraction fit** | Does this match how the surrounding code solves this? | Style is invisible to the suite |

Lenses are independent, run in parallel, and each returns structured findings with
a `file:line` anchor. A lens that finds nothing is a normal outcome and must be
cheap to express — a reviewer that always finds something is a reviewer nobody
reads.

### Corroboration is what gives a finding teeth

Model findings are ranked, not trusted. Where a deterministic check *can* speak to
a finding, it is run, and its answer outranks the model's:

- **Duplication** — `sc-index`'s symbol lookup ([05](05-context-management.md))
  answers "does a symbol by this name already exist?" A claimed duplicate is
  corroborated by finding it; uncorroborated, it stays a suspicion. (`find_symbol`
  returns human-readable prose today, so this needs a structured variant — the
  index has the data, the API doesn't expose it.)
- **Error handling** — a swallowed error is often syntactically visible.
- **Scope** — the weakest of the three, and worth stating plainly. A subtask
  carries a `files` list, but it is a decomposer *hint*, explicitly not enforced,
  and frequently empty on the free-text `swarm <task>` path
  ([08](08-orchestration-and-swarm.md)). Integration already draws its merge
  targets from that same list, so a diff largely cannot exceed it. With an empty
  list the lens has nothing to check and must report `Unknown`, not silently pass.

A corroborated finding may gate. An uncorroborated one is reported and ranked, and
never blocks — that asymmetry is the whole design. It is the same commitment
[13](13-compliance-evidence.md) makes by keeping `Unknown` first-class: the tool
says how confident it is, rather than flattening confidence into a verdict.

## What happens to a finding

Three outcomes, chosen by configuration, in increasing order of intervention:

1. **Report** (default) — findings ride along with the swarm report and the event
   stream. The run still succeeds. This is the honest default because an
   uncorroborated finding is a suggestion, and a suggestion that halts a run is a
   tool that gets switched off.
2. **Gate** — a corroborated finding at or above a configured severity stops the
   run for a human checkpoint, reusing the existing `Gate` seam
   ([09](09-workflow-and-checkpoints.md)) rather than inventing a second one.
3. **Feed a retry** — a corroborated finding becomes feedback on a re-dispatch of
   the same subtask, exactly as still-failing tests do today
   ([08](08-orchestration-and-swarm.md), "Subtask retry"). This is the highest-value
   outcome and the reason the whole spec is worth building: the swarm already knows
   how to retry with feedback; this widens what counts as a reason to.

Retry-on-review-finding is bounded by the *existing* `max_subtask_retries` budget.
It never gets its own, because two independent retry budgets multiply into a run
that never terminates.

## Cost, and when it doesn't run

Review is model calls over a diff, on a machine that is already running an
orchestrator and workers. It must be possible to not pay for it:

- Off by default for `run`; on by default for `swarm` only where a T1 backend is
  configured — reviewing with a 4B model produces 4B-quality review.
- Skipped entirely for a diff below a size threshold. A three-line change does not
  need four lenses.
- The reviewer runs on the **advisor/T1 backend** ([02](02-model-backends.md)), not
  a worker. A worker reviewing a worker's output is two keyholes, not one review.
  As with escalation, that tier is a configured backend rather than a type in the
  code — the reviewer takes a `ModelBackend` like `consult` does.

## Events and surfaces

Review emits into the existing swarm event stream ([01](01-architecture.md)), so
every renderer gets it for free — the same "one stream, many renderers" property
that let M5 add a second UI cheaply:

- `ReviewStarted { subtask, lenses }`
- `ReviewFinding { subtask, lens, severity, file, line, corroborated, summary }`
- `ReviewFinished { subtask, findings, blocking }`

The desktop client renders findings as **line comments** on the diff
([12](12-platform-clients.md)), which is most of the way there already: the code
view draws live HEAD diffs (added/removed rows, PR-style) and renders anchored
comments inline beneath their lines in the same pass. A finding and a human's own
review comment become the same object, so a reviewer acts on the machine's finding
without retyping it — and the existing comment triage (question / small / big)
gives a finding somewhere to go.

The gap is *which* diff: today's is working-tree-versus-HEAD for an open file. A
finding is anchored to a subtask's integrated diff, so the view needs to accept a
supplied diff rather than always computing its own. That is a narrower change than
building a review surface.

## Relationship to other specs

- Runs **after** integration verification ([08](08-orchestration-and-swarm.md));
  never replaces it. A red suite is still a rejection, decided before review runs.
- Complements [11](11-testing-and-tdd.md): the suite says *correct*, review says
  *worth keeping*. Neither can answer the other's question.
- Reuses the `Gate` seam ([09](09-workflow-and-checkpoints.md)) for its blocking
  mode, and the retry-with-feedback path already built for failing tests.
- Deliberately unlike [13](13-compliance-evidence.md): that engine bans the model
  because its output must be reproducible and citable. Here the model is the only
  thing that can answer the question, so the design constrains its *authority*
  instead — findings rank, deterministic checks decide.
