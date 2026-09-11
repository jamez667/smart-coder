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

---

## CORRECTION (same day, after commit 7ccda47)

Commit `7ccda47` added seed/temperature plumbing and its message claims this
makes a repeat reproducible. **That claim is not supported and should not be
relied on.**

Tested immediately after, on the freshly built binary: `rust-two-stage` twice
with `--seed 1`, everything else identical.

    run A: [STILL-RED]  40 steps
    run B: [PASS]        2 turns

Same seed, same commit, same server, opposite outcomes. The two runs differed
from step 1 onward: run A reported `cached=0` on its first turn, run B
`cached=855`.

The plumbing itself is real and tested -- the seed does reach every request,
sabotage-verified. What is NOT established is that pinning it makes an agent run
reproducible. Something outside the sampler differs between runs.

**The KV cache is NOT the cause.** A powered probe settles it: one identical
request, fixed seed, 6 draws with `cache_prompt: true` and 6 with it `false` --
all twelve byte-identical (sha1 `862685562fd4`). The server is deterministic
under a fixed seed regardless of cache state. An earlier probe that appeared to
show cache-driven divergence was comparing requests whose own payloads differed;
it was measuring itself, not the cache.

So a single request is reproducible and a whole RUN is not. That puts the
divergence in what the harness SENDS between turns, not in the sampler and not
in the server. It has not yet been isolated.

**Standing conclusion: the ladder is still not a reproducible instrument, and
numbers taken off it still carry the error bars described above.**


---

## RESOLVED: it is not the harness, the seed, the context, or the cache

The decisive test, run with `--verbose` so the fully-assembled prompt is logged
and can be diffed byte for byte.

**Same seed, both servers restarted so the KV slot was genuinely empty** (runs F
and G, `cached=0 / prefill=1228` at turn 1 in both):

| turn | prompt | reply |
|---|---|---|
| 1-5 | byte-identical | byte-identical |
| 6 | byte-identical | **diverges mid-sentence** |

Turns 1 to 5 match exactly, prompt AND reply. Turn 6 receives a byte-identical
prompt in both runs and returns different text. F then finished in 10 turns, G
took 23. Both happened to pass, which is coincidence and not reproducibility.

So every candidate is eliminated in turn:

* **Harness/context bleed** -- refuted. Prompts are byte-identical up to the
  divergence; the assembly is clean.
* **Sampling** -- refuted. Same seed, same temperature; the control (seed 99,
  cold) behaves differently, so the seed is plumbed and does matter.
* **KV cache inheritance** -- refuted. Both runs began cold after a restart and
  still diverged.
* **Delta-memory leak** -- already fixed, and unrelated: the baselines matched.

**What remains is the server.** Turns 1-5 are short structured tool calls; turn 6
is the first long free-text reply, and the two versions share a long identical
prefix before splitting mid-sentence. That is the signature of nondeterministic
floating-point reduction in batched/MoE kernels: llama.cpp does not guarantee
bitwise-identical logits across runs even at temperature 0 with a fixed seed, and
a difference far below sampling threshold only changes an emitted token once a
reply is long enough for it to cross a boundary.

### What this means for the ladder

Run-level reproducibility is not achievable from our side on this server. The
seed plumbing is still worth having -- it removes one real source of variance and
makes the draw replayable in principle -- but it cannot make an agent run
deterministic while the backend is not.

The instrument must therefore be treated as statistical. Report n and the spread,
never a single figure; do not compare single runs across commits; and size any
claimed improvement against the observed run-to-run variance, which on
`rust-two-stage` alone spanned 1/10 to 2/3.
