# `rust-two-stage` x10 — what a single flaky rung actually does

Commit `fbbac5e`, mellum2-12b @ localhost:11441 (GPU1, the 3080). GPU0 sat at
~52% with the user's desktop/game load throughout; it was never touched.

**Result: 1 pass, 9 red.** The same rung scored **2 of 3** inside the full
17-rung ladder earlier the same evening, on the same binary and the same card.

## The divergence is one turn

All ten runs open identically:

1. `read_file lib.rs`
2. `read_file test.rs`
3. `run_command rustc --test ... && ./t_two_stage.exe`
4. `edit_file` — the doc-comment/`compare` edit

**Turn 5 is the split.** The run that passed ran the verifier, then re-read
`lib.rs` from line 19 before its second edit, and landed it in 8 turns. The nine
that failed skipped that read and immediately re-sent a near-identical edit,
then another, then a whole-file `write_file` — 10 to 40 turns, ending
`Stalled` or `BudgetExhausted`.

Sampling is `temperature: 0.2, seed: None` (`GenerateRequest::default`). There
is **no way to pin a seed from the eval path**: `AgentConfig` carries no
sampling fields and nothing in `sc-core` or `sc-eval` calls `with_seed`. So the
turn-5 choice is an unpinned draw, and the ladder cannot presently resolve a
one- or two-rung improvement. That is a property of the instrument, not of the
harness under test.

| repeat | verdict | turns | baseline shape |
|---|---|---|---|
| 1 | red | 23 | CLEAN |
| 2 | red | 19 | inherited `newly failing` |
| 3-7 | red | 10-40 | inherited `same 1 failure` |
| 8, 9 | red | 40, 25 | inherited `newly failing` |
| 10 | **PASS** | 8 | inherited `same 1 failure` |

## The bug this uncovered (fixed)

Repeat 1's baseline — the red-first check, before the model acts — reads
honestly:

    run_verification: 1 failed, 5 passed:

Every later repeat inherits state. Repeats 8 and 9, on a freshly copied fixture
the model has never touched:

    newly failing: a_missing_component_counts_as_zero;
    now passing: padding_still_respects_a_later_component

Nothing can be newly failing, or newly passing, at baseline. `run_task`
materializes a fresh `TempWorkspace` and re-copies the fixture every run, but
`delta::LAST_RUN` is a process-lifetime thread-local keyed on the **command
string**, and nothing in the tree ever cleared it. The workspace resets; the
memory of what failed does not.

`--repeat N` runs every iteration in one process on one thread, so each repeat
after the first starts poisoned. Two ladder rungs also share the spelling
`cargo test --offline -q` — `engine-grid-scan` and `engine-ecs-query`, both
0/3 — so they contaminate **each other** inside a single pass.

Fixed by `sc_verify::forget_runs()`, called from `run_task` where the fresh
workspace is made.

## What this does NOT explain

The contamination is a real defect and is **not** the cause of the flakiness.
The correlation is flat:

| baseline | red | pass |
|---|---|---|
| CLEAN | 1 | 0 |
| inherited `same` | 5 | 1 |
| inherited `newly` | 3 | 0 |

The one clean baseline failed; the one pass was poisoned. The 1/10-vs-2/3 gap
remains unexplained — position in the suite is the leading suspect and has not
been tested.

## Consequence for earlier numbers

Every multi-repeat figure measured before this fix — including the same
evening's 6/12/10 across three full-ladder passes — was taken through the
contaminated channel. They are not directly comparable to anything measured
after it.

## Method note

Three of today's harness bugs share one shape: **the harness told the model
something false about its own progress.** A false loop-stall, a broken build
reported as tests newly passing, and now a fresh fixture reported as a delta
against a workspace that no longer exists. All three were invisible in the
model's replies and obvious in the prompt.
