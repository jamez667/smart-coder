# Implementation prompt — spec 16 (post-integration review)

Paste the block below into a new session.

---

Implement **spec 16 — post-integration review** (`docs/specs/16-post-integration-review.md`).
Read that spec in full first; it is the source of truth and it is detailed. Read
spec 08 (orchestration/swarm) alongside it, since this hangs off the swarm's
integration path.

## What this is

A second gate over the integrated diff, after verification and before a run is
reported done. Tests answer *does it work?*; this asks *should this code stay?* —
did the worker duplicate a helper it couldn't find, swallow an error, or make
tangential changes nobody asked for. None of those are visible to a test suite.

Two rules from the spec constrain everything and must not be traded away for
convenience:

1. **Review never rewrites code.** A finding is evidence handed to a decision,
   never an edit.
2. **Only a *corroborated* finding may block or feed a retry.** An uncorroborated
   model opinion is reported and ranked, and can never stop a run. Reviewer
   agreement ranks; it never promotes an opinion to a fact.

## Scope for this first pass

Build the engine and the CLI path. **Do not** build the multi-model panel or the
desktop surface yet — the spec describes both, but they are follow-ups. Design the
types so the panel drops in later without reshaping them: `Finding` carries
`raised_by` / `considered_by` from the start, even when there is exactly one
reviewer and those lists always have one entry.

Deliver:

- A new `crates/sc-review` crate (engine only — no CLI, no UI, mirroring how
  `sc-verify` and `sc-comply` are structured).
- Lenses: **duplication**, **error handling**, **abstraction fit**, **unrelated
  changes**. Each is a separate model call with one question. Run them in parallel.
- Grounding, which is the part most likely to be skipped and most important:
  a review call gets the diff **plus** retrieved context. Per the spec, swarm
  workers today receive *no* repo map at all (`sc-swarm` has no `sc-index`
  dependency — check `crates/sc-swarm/Cargo.toml`), so the reviewer having one is
  the entire point. Duplication additionally gets pre-retrieved similar symbols;
  the deterministic lookup runs *before* the model call and its result goes into
  the prompt.
- Corroboration, ranking, and anchoring exactly as the spec describes. Findings
  anchor to **hunk + symbol**, with the line as a render hint only — do not match
  or identify findings by line number.
- Wire into the swarm at `integrate_with_retry` in
  `crates/sc-swarm/src/orchestrator.rs`, which already owns the
  integrate → check → retry loop that review extends.
- New `SwarmEvent` variants (`ReviewStarted` / `ReviewFinding` / `ReviewFinished`)
  in `crates/sc-swarm/src/event.rs`, following the existing variants' doc-comment
  style. They must round-trip Serialize↔Deserialize like the rest, and
  `--json`/replay parity must hold.
- Off by default. A flag turns it on. Skipped for diffs under a size threshold.

## Known obstacles — the spec calls these out, don't rediscover them

- **`find_symbol` returns human-readable prose**, not structured hits
  (`crates/sc-index/src/workspace.rs`). Corroboration needs the symbol *and its
  location* to build an actionable retry prompt, so add a structured variant. The
  index already has the data; the API doesn't expose it. Don't parse the prose.
- **`sc-swarm` doesn't depend on `sc-index`.** Adding the dependency is fine and
  expected. Note `sc-comply` is deliberately dependency-constrained; `sc-swarm`
  is not.
- **The "unrelated changes" lens is weak.** `Subtask.files` is a non-enforced
  decomposer hint and is often empty, and integration already draws merge targets
  from it. With an empty list the lens reports `Unknown` — it must not silently
  pass. Build it last; drop it if it produces noise.

## Two things the spec is explicit about that are easy to get wrong

**Retry prompts carry evidence, not verdicts.** Look at `feedback_text` in
`orchestrator.rs` — it names failing tests and quotes assertion messages, because
"some tests failed" is useless to a 4B worker. Review feedback must clear the same
bar: inject the deterministic evidence ("`format_date` already exists at
`src/utils/date.rs:41`, import it"), never the model's prose summary. This is why
`Finding` has both `evidence` and `summary`.

**Green tests + failed review on the last retry gates, it does not fail.** The work
is verified correct; discarding it over an unfixed finding is the worse outcome.
The subtask is `Done` with findings attached, and the run stops at a human
checkpoint if any meet the gating severity. Headless (`AutoApprove`) completes and
reports the findings loudly — never drops them.

## How to work

- **TDD, per spec 11 and this repo's own practice.** Tests first. The lens
  prompts, corroboration, ranking, anchor matching and vote merging are all pure
  logic over fixtures and should be host-testable with a scripted backend — see
  the `Scripted` backend pattern in `crates/sc-core/tests/tdd_loop.rs` and the
  workflow runner tests. No test should need a live model.
- Cover at minimum: an uncorroborated finding can never block; a corroborated one
  produces an actionable retry prompt containing the symbol *and* its location; two
  models flagging *different* problems in one hunk stay two findings (over-merging
  is the failure mode); an unreachable reviewer is skipped, not fatal; a finding
  whose named symbol doesn't resolve drops in rank.
- Keep files under 500 lines; split into a module directory if a file grows past
  it (this repo has just done that sweep — match the existing shape).
- Run `bash scripts/check.sh` before committing — fmt, clippy `-D warnings`,
  check, test. It must be green.
- Run the `spec-guardian` agent before committing. If the implementation diverges
  from spec 16, the spec may well be the thing that's wrong — it has never been
  executed. Report the drift and propose the spec edit rather than quietly
  bending the code to match prose written in advance.
- Commit and push to `main` (this repo's convention), and use the Bash tool's
  heredoc for the commit message — not a PowerShell here-string.

## If something in the spec doesn't survive contact

Say so rather than implementing something you believe is wrong. The spec was
written without running any of it; the "unrelated changes" lens and the exact
ranking order are the parts most likely to need revision. A reasoned objection
with evidence is more useful than faithful implementation of a bad idea.
