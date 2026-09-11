# `rust-symptomatic` x10, fully traced

Commit `235f503`, mellum2-12b, seeds 1-6+ (`--repeat 10 --verbose`). **10/10 STILL-RED.**

`traces/run-01.txt` .. `run-10.txt` carry, per turn: the **prompt in** (every
message, from `PromptAssembled`), the **model's raw response out**, and **what the
harness did** (parsed call, verbatim observation, advice, faults, verification).
The prompt is shown in full on turn 1 and as an append-delta afterwards, because
it is append-only and repeating 10k tokens per turn makes the trace unreadable.
Observations and advice are never elided.

## Is the model being given too much?

Not at the start; increasingly so by the end.

| | |
|---|---|
| first-turn prompt | **990 tokens** |
| median peak prompt | **15,095 tokens** (budget ~19,000) |
| largest single observation | 2,394 chars |
| median observation | 369 chars |
| advice injections across 10 runs | 37 |

The opening prompt is lean. The window then fills to ~80% of budget over 15-40
turns, entirely from accumulated observations.

## What the turns are spent on

| outcome | count |
|---|---|
| `ok (` | 114 |
| **anchor not found** | **53** |
| shell output | 48 |
| **byte-identical (rejected)** | **31** |
| **no-op** | **20** |

Tools: `edit_file` 191, `run_command` 48, `read_file` 37, `write_file` 28,
`run_verification` 10, `finish` 2.

Stops: 5 BudgetExhausted, 5 Stalled.

**104 of the edit attempts achieve nothing** (anchor miss, byte-identical, no-op)
against 114 that land. The model is not short of information; it is failing to
address the file.

## The one concrete redundancy

An anchor-miss observation that also carries the whole-file dump prints the file
**twice**: once as `closest match:` and again under `--- lib.rs (N lines), in
full ---`. Every such message repeats its closest-match block verbatim.

## Correction: the file destruction is NOT this rung's failure mode

An earlier run showed `lib.rs` going 50 -> 8 lines, and I twice guessed wrong
about the cause. The trace settles it: at **step 10 the model ran**

    run_command  echo '     pub fn values...' > lib.rs.new && mv lib.rs.new lib.rs

A shell redirect overwrote a 50-line file with an 8-line fragment. `exited 0`,
correctly reported. No edit tool and no harness message was involved; the
whole-file dump six turns later was the harness accurately showing the wreckage.

**It happened in 0 of these 10 runs** -- all hold at 50 lines throughout -- so it
is one unlucky path, not the reason this rung fails.
