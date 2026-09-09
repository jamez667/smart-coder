# Next harness work

Written 2026-09-08, after a day of measuring the harness against a real
repository (`mini-miner-2`, an 8,190-line `app.rs`) rather than eval fixtures.
Every item below has a measurement behind it; the numbers are in
`evals/results/2026-09-08-refactor-fixed/README.md` and
`evals/results/2026-09-08-refactor-anatomy/README.md`.

The headline that frames all of it: on that refactor we went 598s → 117s
against pi's 236s, and **the entire gain was turn count** (43 calls → 15). Same
GPU, same ~150 tok/s. Twelve of the remaining fifteen turns emit a ~20-token
tool call and still cost ~5s each — that is prefill and round-trip, not
thinking. **Optimise turns and prefill, not sampling.**

---

## 1. Batch window eviction — IN PROGRESS

The prompt grows monotonically and then shrinks once per turn as eviction drops
the minimum needed to fit, breaking the KV prefix every turn near the end of a
run. Cache hit 58% here vs 81% on the ladder. Fix is hysteresis: once eviction
triggers, evict down to ~80% of budget so the next several turns append
cleanly.

---

## 2. A persistent status file (the "manager" idea)

**The user's framing, which is the right one:** a small model cannot hold much
at once, so keep its high-level state OUTSIDE the prompt and let it focus on
the current step.

**Why this is well-founded, from this codebase:**

- `crates/sc-core/src/agent/stable.rs` already pins a `PLAN-<slug>.md` doc into
  the prompt, hash-keyed. Its own comment describes the exact stall this would
  fix: *"the model reads it every few turns to remember what it's building,
  then re-reads it once the read scrolls out of the window."* The harness
  learned this lesson for information going IN and never applied it to state
  coming OUT.
- `PlanState::complete_active` **exists and is never called**. Grep the loop:
  the only plan mutations are `record_attempt` and `fail_active`. The plan can
  fail but never progress.
- `plan_first` defaults to **false**, so most runs carry no structure at all.
- Measured: on the 598s run the model wrote an 11,800-character "I am done"
  summary eight times because it had nowhere else to record that fact. On the
  117s run, five of fifteen turns were `read_file` on the same file at shifting
  offsets (27s) — a model re-establishing where it is.

**Shape to build (not a second model on the hot path):** a file the harness
owns, pinned hash-keyed like the plan doc, with ONE narrow `update_status`
tool. The harness writes the objective half itself — files changed this run,
last verification result, whether the run started green — because it already
knows those and the model should not spend tokens restating them. The model
writes only the subjective half: what it is doing and what is left.

**Why not a manager model deliberating each turn:** it adds latency to the path
that is already the bottleneck, and the existing advisor seam
(`crates/sc-core/src/advisor.rs`) shows the failure mode — it fired **zero
times across 51 control runs**, and twice on the real refactor without helping,
because it sees only `summarize_history` (a tool-name bullet list) and an empty
plan render. Fix the advisor's *inputs* before adding a tier above it.

**How to judge it:** it must reduce turns on the refactor and on a genuinely
multi-step task. A status file nobody reads is a cost, not a feature.

---

## 3. Let the model act on what it just read

The trace shows a read-then-act pattern costing two turns where one would do.
Returning a numbered view of the changed region after an edit — so the model
can chain its next edit without a fresh `read_file` — would cut turns directly.
Speculative until 1 and 2 land; listed so it is not lost.

---

## Method note

Two bugs today (paged reads losing their middle; green-at-start counting as
success) were invisible to 153 eval runs and obvious within one real task,
because every ladder fixture is small and red-first. **Keep testing against
real repositories.** Also: single runs are noisy — repeat a task 3x before
believing a delta.
