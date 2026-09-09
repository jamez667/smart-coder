# Mellum after the native-tool-call fix — the fix is real, the model still fails

Killed after 3 of 17 rungs (the watchdog stops on the first failure).

| rung | before fix | after fix |
| --- | --- | --- |
| rust-stated | PASS 2 steps 4s | PASS 2 steps 4s |
| rust-located | PASS 3 steps 5s | PASS 8 steps 12s |
| rust-symptomatic | **STILL-RED** 23 steps 368s | **STILL-RED** 32 steps 313s |

## CORRECTION: the runaway is NOT gone

An earlier version of this file claimed "near-cap replies went from 14 of 114
to 0 of 56". **That was a measurement error and it was wrong.** I compared reply
lengths in CHARACTERS against a cap measured in TOKENS: 9,023 chars of dense
JSON is ~3,072 tokens, i.e. exactly the cap. `rows.jsonl` had the truth the
whole time — `peak_reply_tokens: 3072` and `faults: [["reply truncated", 16]]`,
one MORE than the 15 before the fix.

Reading the actual replies confirms it. The longest is 9,513 chars repeating
one identical `edit_file` (`new_str` and `old_str` both `let mut out =
Vec::with_capacity(self.len);`) until it is cut off. Another degenerates into
unrelated prose about string-edit distance. 15 of the 32 replies repeat a
single call 30-98 times; those turns cost ~230s of the 313s run.

What the fix DID achieve is unmeasured here: it is a genuine wire-format
correctness fix (verified end to end against the live server — the reply
carries the structured call and the assistant turn goes back out with
`tool_calls` plus a paired `role:"tool"` result), but it did not stop this
model looping.

## What it did NOT do

**Mellum still cannot solve `rust-symptomatic`.** It took 32 steps (up from 23)
and 478k tokens (up from 350k) before the step cap. So the history corruption
was a real bug that made things worse, but it was not the reason this model
fails a diagnosis rung. The capability gap is genuine.

A residue remains: replies still trail a `</tool_call>` tag and run long (p90 =
9,023 chars), just under the 90% cap threshold so they no longer register as
truncated. A `stop: ["</tool_call>"]` makes no difference on a simple call
(24 vs 23 tokens) — the long replies are large `edit_file` payloads that
legitimately need the tokens.

## The honest read

Two separate things were conflated in yesterday's write-up:

1. A harness bug that corrupts the conversation for **any** model whose
   template uses tool-call tags. Real, now fixed, and it was inflating Mellum's
   failure — 14 runaway replies is a lot of wasted budget.
2. Mellum being weaker than Tiel at diagnosis rungs. Also real, and unchanged
   by the fix.

The fix stands on its own merits (it is a correctness bug regardless of which
model is in use). Mellum remains unsuitable as a Tiel replacement on this
evidence.

**Still untested:** whether the fix changes anything for Tiel, whose numbers are
the baseline for every other measurement in this directory.
