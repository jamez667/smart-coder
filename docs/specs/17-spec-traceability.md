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
| `@ crate::Symbol len=N` | A collection has N members | A targeted parse (see below) |

The value is not in how expressive they are — it is that `len=5` would have failed
the moment the sixth phase was removed, in CI, without anyone remembering to check.
Anchors are added where a claim is *load-bearing and countable*, not everywhere; a
spec whose every sentence is anchored is a spec nobody will edit.

Two behaviours of the reader, both chosen so the check cannot lie by omission:

- **An unreadable anchor is retained, not dropped.** An anchor whose comment is
  never closed, one carrying an assertion other than `len=`, one naming a token
  that is neither a path nor a symbol — each becomes a claim resolved `UNKNOWN`,
  with the reason. An anchor that vanished on a typo would leave its spec reading
  as governed while nothing verified it, which is the worst available outcome: a
  checker reporting clean over a blind spot.

  (Writing that sentence with a literal opening delimiter in it produced exactly
  such a claim on the first run — the checker read the prose as a real anchor and
  reported it `UNKNOWN`. That is the rule working, and the reason this paragraph
  now describes the syntax rather than spelling it.)
- **Anchors inside fenced code blocks are parsed like any other.** This is why
  the `len=5` example above resolves against real code rather than being inert.
  The cost is that an illustrative anchor naming a deliberately fake symbol would
  report `BROKEN`; the escape hatch, if that ever bites, is a distinct marker
  rather than a fence rule.

### `len=N` checks two things, not one

Written as "a collection has N members", which is spec-versus-code. Implementing
it showed that to be too weak, and weak in exactly the case this spec was written
for. Consider the drift cited above — `ThinkPolicy`'s dead array slot:

```rust
pub const ALL: [Phase; 6] = [ /* five entries */ ];
//                     ^ declared        ^^^^^ actual
```

The declared length and the element count are **independently checkable and not
the same claim**. A spec-versus-code check alone reports `OK` here whenever the
spec also says 6 — reproducing the original bug rather than catching it. So both
are measured, and either disagreeing with `N` is `STALE`. So is the two
disagreeing with *each other* — and that case **pre-empts** the comparison with
`N` entirely, reported as `STALE` against the code rather than the spec. A dead
slot is a defect whatever the spec claimed, so `[T; 6]` holding five entries is
flagged for both `len=5` and `len=6`. It costs one comparison.

Two consequences worth stating plainly:

- **`len=` is Rust-only.** The measurement pass parses Rust and nothing else, so
  a `len=` on a Python or C# symbol yields `UNKNOWN` — the symbol itself still
  resolves normally. This is a limitation stated up front rather than discovered
  later, and it is not an accident of the index: Python has no declared array
  length to compare against in the first place, so the two-truths check that
  makes `len=` worth having has no analogue there.
- **The repeat form `[value; N]` is not element-counted.** It has one element
  node but N values; counting nodes there would report 1 and fire a false
  `STALE`. The count is withheld and the declared length answers alone.

The counting itself is a **separate, targeted parse** rather than an extension of
`sc-index`'s query. That query builds the def/ref graph behind PageRank and
`find_symbol`; widening it to carry consts and array arities would degrade the
repo map and every `find_symbol` result workspace-wide to serve one consumer, and
a count has nowhere to live in `SymbolDef` without widening it for every caller.

**Anchors are never generated from code.** A spec derived from the implementation
cannot contradict it, and a document that cannot contradict the code cannot catch
it drifting — it just becomes a second, wordier copy. The human writes the claim;
the anchor only says which code the claim is about.

### Resolving a symbol: the crate segment is the only reliable part

`sc_win::config::types::UiConfig` looks like a path into a directory tree, and
treating it as one produces false `BROKEN`s — which matters more than it sounds,
because **a false `BROKEN` is how this gate gets deleted**. A checker that cries
wolf on correct documentation teaches people to bypass it.

The asymmetry that makes resolution honest:

- **The crate segment is reliable.** It maps to a workspace member, verifiably,
  from the manifest <!--@ crates/sc-trace/src/manifest.rs -->. A segment naming
  no member is unambiguously wrong, and is the *only* place a symbol anchor may
  be rejected outright.

  What "from the manifest" buys is worth spelling out, because the rest of this
  section rests on it. Members come from the `[workspace] members` list rather
  than a directory walk, so a leftover directory never becomes a phantom
  `UNGOVERNED` finding and a commented-out member never becomes a phantom crate.
  A `[lib] name` override is read rather than assumed — no crate here overrides
  the `-`→`_` convention today, but deriving it costs the same as assuming it and
  cannot silently rot. And a member whose own manifest is unreadable still yields
  a crate: the directory is ground truth for *existence*, and dropping it would
  quietly un-govern real code.
- **Module segments are not.** Re-exports mean a symbol's use-path routinely
  differs from its file path: `sc_workflow::artifact_dirs`
  <!--@ sc_workflow::artifact_dirs --> is defined in `artifact_dir.rs`. Module
  segments therefore never reject a candidate; they only break ties.
- **An owning type is required when named.** `Phase::ALL` must resolve to an
  associated item of `Phase` — five distinct `ALL` consts exist in this workspace
  and without an owner they are one name.

Everything else prefers `UNKNOWN`: an ambiguous name with several definitions in
one crate, a crate with no indexable Rust, a symbol kind the parse does not cover.
A `BROKEN` claims the code is wrong; `UNKNOWN` claims only that the checker could
not tell, and the second is the honest answer far more often than it is the
convenient one.

A symbol that is absent from its named crate but present in another is still
`BROKEN` — but the message says where it went. That search enriches the report
and never changes the verdict.

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

`UNGOVERNED` is reported at **crate** granularity, never per module or function.
A new function in a governed module is not a documentation failure; a new crate
nobody described is. Pitched finer, the check produces noise, and a noisy check is
one that gets `--no-verify`'d and then deleted.

Written as "crate and top-level module", and narrowed on measurement: at crate
granularity this repo yields two findings; at module granularity it yields dozens
(`sc-comply` alone has thirteen). The finer pitch is precisely the noise the
paragraph above warns against, and module granularity drops in trivially once
crate coverage is clean.

A crate counts as governed when a spec names it in prose **or** anchors into it,
and the report keeps the receipt. Two matching rules earned from real data:

- **Whole-token match only.** `docs/specs/00-overview.md` contains the phrase
  "run tests, iterate" — the English verb. A substring match would silently mark
  the crate of that name governed by an accident of prose, and the check would
  report clean while a crate went undescribed.

  (Naming that crate here would itself have governed it, since a whole-token
  match cannot tell an example from a description. That is a real limit of
  coverage-by-mention, and the honest response is to write examples carefully
  rather than to add a heuristic that guesses at intent.)
- **A longer crate name never governs a shorter one.** A mention of
  `sc-comply-author` says nothing about `sc-comply`.

Mentions inside fenced code blocks *do* count: a crate drawn in an architecture
diagram is genuinely described, and excluding fences would flip it ungoverned on
a technicality.

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

### A claim about another repository

✅ **Built** <!--@ crates/sc-trace/src/resolve/path.rs -->, and currently unused.
An anchor may name a sibling repo — a `<repo>:<path>` target — and resolves
`UNKNOWN`. (Written in prose rather than shown verbatim, because a real example
is itself an anchor and this document would then contain a permanent `UNKNOWN`
about a repository that does not exist.)

Built when the hosted intake surface briefly shipped from its own repository, and
kept after that split was reverted ([18](18-task-intake.md)) because the
reasoning outlives the occasion. When a spec governs code this checker cannot
read, the alternatives are both worse than admitting it: **deleting the anchor**
loses the claim entirely — the drift this spec exists to catch, arrived at by
tidying — and **leaving it pointing at an absent path** reports a `BROKEN` that
is not one. Twenty-one false alarms is how a check gets switched off, which is
the failure [13](13-compliance-evidence.md) warns about in its own domain.

`UNKNOWN` is the honest answer because it is *literally* true: this checker reads
one working tree. It does not gate, it is never counted as passing, and the
report says so in the same line — "the checker could not look … not a pass".

A cross-repo anchor stays `UNKNOWN` **even when a path of that name happens to
exist here**, so the answer cannot depend on a coincidence of local layout.

**The monorepo is what makes this unnecessary**, and that is the point worth
keeping: one repository means every anchor is verifiable in one pass, and the 21
claims that were `UNKNOWN` under the split resolve against the working tree
again — so `sc-trace` *gates* on them. That is the whole value of restoring them,
and it only holds if the check is actually run over them.

A capability for answering honestly when you cannot check is worth having;
needing it routinely is a smell.

There is no single headline score. A "94% traceable" number invites exactly the
misreading [13](13-compliance-evidence.md) refuses — the 6% is where the drift is.
Reporting *counts* per status is fine and useful, in the same way that spec's
coverage and determinacy figures are: what is banned is one blended number that
lets a reader stop reading.

## Surfaces

The engine is `sc-trace` <!--@ crates/sc-trace/src/engine.rs -->, engine-only in
the shape of `sc-verify` and `sc-comply` — no CLI and no UI of its own.

- **`smart-coder trace`** — report the claim table; human-readable, or `--json`
  ([06](06-cli-ux.md)). Note the CLI is a hand-rolled positional parser, not
  `clap`: a new subcommand touches the `Command` enum, the parser arm, the `main`
  dispatch, and the hand-written help text, and an unknown token errors rather
  than falling through.
- **`smart-coder trace --check`** — non-zero exit on `BROKEN` or `STALE`. This is
  the CI gate, and the only part that must be fast. `UNGOVERNED` warns rather than
  fails by default: adding a crate and its spec in one commit is good practice, but
  a hard failure would block a legitimate work-in-progress. `UNKNOWN` likewise
  never gates — failing a build over the *checker's* own limits is what teaches
  people to bypass it.

  `--check` is accepted in any position (`--check trace` parses as
  `trace --check`) and is an **error** on any other subcommand rather than being
  ignored: a gate flag that silently does nothing is a gate that is not running,
  and the user would believe otherwise.
- **The check gate** — `scripts/check.sh` <!--@ scripts/check.sh --> next to
  fmt/clippy/check/test, and its Windows twin `scripts/check.ps1`
  <!--@ scripts/check.ps1 -->, which must carry the same gates. The deterministic
  layer costs no model calls, so it runs every time.
- **The `spec-guardian` agent** stays exactly as it is: the semantic second layer,
  advisory, reading meaning that anchors cannot capture ("this spec's *principle*
  no longer describes what the code does"). This spec does not replace it — it
  removes the load-bearing cases from its shoulders so its judgment is spent where
  judgment is required.

## Cost and scope

- Deterministic, no model calls. Rust, Python and C# via the existing tree-sitter
  symbol index (`sc-index`); anything else resolves to `UNKNOWN` rather than being
  silently skipped.
- **Fast enough, not optimised.** A full run over this workspace is ~2s in a debug
  build, which is noise next to the `cargo test` gate it sits beside. Sources are
  re-read per anchor rather than indexed once, and a `BROKEN` symbol triggers a
  workspace-wide search to say where it went. Both are deliberate: the second only
  fires on a finding, and caching would trade the checker's simplicity for a
  saving nobody is waiting on. If the spec count grows enough to notice, index
  once and thread it through — not before.
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
