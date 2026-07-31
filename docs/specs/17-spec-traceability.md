# 17 — Spec traceability & drift

## Principle

**A spec that cannot be checked will drift, and the drift will be silent.**

This project has the evidence. The pipeline ran on five phases while every spec
described six; `ThinkPolicy` carried a dead array slot sized for the phase that no
longer existed; `sc-cli` printed "6 phase artifacts" while writing five. Spec 09
claimed artifacts were "versioned and committed" when saves overwrite in place, and
claimed a test-first stage breakdown that only happens on Python. None of this was
caught by a test, because none of it is *wrong code* — it is code that stopped
matching its description. Every instance was found by a human reading two documents
side by side and noticing.

The commitment this spec protects:

> **Drift is detected by a machine, not by remembering to look.**

That is a sharper constraint than "we have a reviewer agent." A model-driven audit
(the `spec-guardian` agent) is genuinely useful and should keep running — it reads
prose and judges meaning, which no static check can. But it is **advisory and
non-reproducible**: it must be invoked, it may notice different things on two runs
over the same diff, and its silence is not evidence of correctness. It is the
second layer here, never the first.

The first layer is boring and deterministic, and this spec is mostly about making
that layer possible — because a spec written as pure prose *cannot* be checked by
a machine. Traceability is the enabling mechanism; drift detection is the payoff.

## The two failure modes

They need different machinery, and conflating them is why "keep the docs updated"
never works:

| | What happened | Caught by |
| --- | --- | --- |
| **Stale claim** | The spec asserts something the code no longer does ("six phases", "and committed") | An anchored claim, checked |
| **Ungoverned code** | New behaviour exists that no spec describes | Coverage: code no spec claims |

The first is a *false* document. The second is an *incomplete* one. The first is
more dangerous, because a reader trusts it.

## Shape: anchors make prose checkable

A spec claim becomes checkable when it names the thing it is about. The mechanism
is a lightweight anchor inline in the Markdown, naming a code symbol or path:

```markdown
The pipeline runs five phases <!--@ sc_workflow::Phase::ALL len=5 -->
in order: specs → architecture → layout → stage breakdown → work decomposition.
```

An anchor is a comment, so specs stay readable prose for a human — the primary
audience — and the checker reads only the anchors. Three kinds, deliberately few:

| Anchor | Asserts | Checked by |
| --- | --- | --- |
| `@ path/to/file.rs` | This file exists and this spec governs it | Path resolution |
| `@ crate::Symbol` | This symbol exists | The symbol graph (`sc-index`) |
| `@ crate::Symbol len=N` | A collection has N members | The symbol graph |

The value is not in how expressive they are — it is that `len=5` would have failed
the moment the sixth phase was removed, in CI, without anyone remembering to check.
Anchors are added where a claim is *load-bearing and countable*, not everywhere; a
spec whose every sentence is anchored is a spec nobody will edit.

**Anchors are never generated from code.** A spec derived from the implementation
cannot contradict it, and a document that cannot contradict the code cannot catch
it drifting — it just becomes a second, wordier copy. The human writes the claim;
the anchor only says which code the claim is about.

## Shape: coverage finds the ungoverned

The reverse direction needs no anchors. Every crate and top-level module is
expected to be *claimed* by at least one spec — a spec that names it in prose or an
anchor. `sc-comply` already establishes the pattern
([13](13-compliance-evidence.md)): walk the workspace, match against declarations,
and report what matched, what didn't, and what could not be determined.

```
docs/specs/*.md ──► anchors ──┐
                              ├──► resolve ──► Claim{ spec, target, status }
workspace symbols ────────────┘                        │
                                                        ▼
                              ┌─────────────────────────────────────┐
                              │ BROKEN   anchor names what's gone   │
                              │ STALE    anchor's assertion is false│
                              │ UNGOVERNED  code no spec claims     │
                              │ OK       resolved and true          │
                              └─────────────────────────────────────┘
```

`UNGOVERNED` is reported at **crate and subsystem** granularity, never per
function. A new function in a governed module is not a documentation failure; a
new crate nobody described is. Pitched finer, the check produces noise, and a
noisy check is one that gets `--no-verify`'d and then deleted.

## Honest statuses

Borrowed from [13](13-compliance-evidence.md), because the reasoning is identical
and was hard-won: **a check that cannot determine something must say so, not guess.**

- `BROKEN` — an anchor names a symbol or path that no longer exists. Deterministic,
  unambiguous, and always an error: either the spec is stale or the anchor is wrong,
  and both need a human.
- `STALE` — an anchor resolved but its assertion is false (`len=5` against six
  members). The highest-value finding, and the one that would have caught the phase
  drift.
- `UNGOVERNED` — a crate no spec mentions.
- `UNKNOWN` — the anchor could not be resolved *because the checker is limited* —
  an unsupported language, a macro-generated symbol. Distinct from `BROKEN`, and
  never counted as passing. Collapsing `UNKNOWN` into `OK` is how a checker starts
  lying quietly.

There is no single headline score. A "94% traceable" number invites exactly the
misreading [13](13-compliance-evidence.md) refuses — the 6% is where the drift is.
Reporting *counts* per status is fine and useful, in the same way that spec's
coverage and determinacy figures are: what is banned is one blended number that
lets a reader stop reading.

## Surfaces

- **`smart-coder trace`** — report the claim table; human-readable, or `--json`
  ([06](06-cli-ux.md)). The name is free today. Note the CLI is a hand-rolled
  positional parser, not `clap`: a new subcommand touches the `Command` enum, the
  parser arm, the `main` dispatch, and the hand-written help text, and an unknown
  token errors rather than falling through.
- **`smart-coder trace --check`** — non-zero exit on `BROKEN` or `STALE`. This is
  the CI gate, and the only part that must be fast. `UNGOVERNED` warns rather than
  fails by default: adding a crate and its spec in one commit is good practice, but
  a hard failure would block a legitimate work-in-progress.
- **The check gate** — `scripts/check.sh`, next to fmt/clippy/check/test. The
  deterministic layer costs no model calls, so it can run every time.
- **The `spec-guardian` agent** stays exactly as it is: the semantic second layer,
  advisory, reading meaning that anchors cannot capture ("this spec's *principle*
  no longer describes what the code does"). This spec does not replace it — it
  removes the load-bearing cases from its shoulders so its judgment is spent where
  judgment is required.

## Cost and scope

- Deterministic, no model calls. Rust, Python and C# via the existing tree-sitter
  symbol index (`sc-index`); anything else resolves to `UNKNOWN` rather than being
  silently skipped.
- Path anchors work regardless of language, so a spec governing a config file or a
  TOML pack is still checkable.
- Anchors are optional. An un-anchored spec is not an error — it is simply
  unchecked prose, which is what every spec is today. Adoption is incremental: add
  anchors to the claims that have already drifted once.

## Relationship to other specs

- The mechanism is `sc-comply`'s ([13](13-compliance-evidence.md)) turned inward:
  same walk-declare-resolve-report shape, same first-class `UNKNOWN`, same refusal
  of a headline score. The target is this repository's own documentation rather
  than a regulatory framework.
- Uses the symbol graph from [05](05-context-management.md) (`sc-index`).
- Complements [16](16-post-integration-review.md): that reviews a *change* against
  the code around it; this checks the *documentation* against the code. Both are
  gates that fire after the suite is green, on questions the suite cannot ask.
- [11](11-testing-and-tdd.md) makes tests the oracle for behaviour. This makes the
  workspace the oracle for documentation — the same move, applied to prose.
