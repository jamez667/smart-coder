# 17 rungs, 3 arms, 3 repeats (153 runs) — commit 35c438b

Adds the three `recovery-*` rungs, built specifically to make a plausible first
fix wrong so the stall ladder, the failed-edit breaker and the
unchanged-failure detector would finally be exercised.

## The recovery rungs are a NEGATIVE RESULT

| | control | raw | pi |
| --- | --- | --- | --- |
| recovery rungs solved | 9/9 | 9/9 | 9/9 |
| wasted turns on them | **0** | **0** | **0** |
| interventions | **0** | **0** | **0** |
| median steps | 3.0 | 6.5 | 4.5 |

**Not one arm fell into a single trap.** Every trap was verified to bite when
applied by hand — the decoy edit in `recovery-unchanged-failure` compiles clean
and leaves the byte-identical failure signature (`9fb078d1ea46` before and
after) — but no model ever made the tempting edit. The likely reason is that
the model greps for the buggy construct rather than reproducing code from
memory, so it never reaches the decoy or the ambiguous anchor.

So the recovery machinery is STILL unexercised, and these three rungs join the
other fourteen as tasks every arm passes. Building harder tasks has now failed
twice (cross-file spread, then wrong-first-move) to produce a discriminating
suite. **Stop building tasks.** The open question — do the stall ladder, the
failed-edit breaker and the unchanged-failure detector earn their place? —
should be answered by instrumenting which guards ever fire in a real run, and
deleting the ones that never do.

## What the run does say

Control solved **51/51** for the first time (raw 50/51, pi 50/51), and the cost
gap is the widest measured:

| | control | raw (no harness) | pi |
| --- | --- | --- | --- |
| prompt tokens, summed medians | 305k | 611k (**200%**) | not instrumented |
| wall-clock, summed medians | 302s | 624s (207%) | 367s |
| average steps | 5.1 | 9.6 | 7.2 |
| prefix cache hit | **81%** | not reported | not reported |

The raw arm's single failure is a 32,793-token request against a 32,768-token
window — the exact failure a context manager exists to prevent, and the
clearest single illustration of what the harness is for.

Truncation faults are down to 4 across 51 runs (13 across 42 before the cap
fix), and 2 wasted turns total. The loop is close to waste-free on this suite.
