# Are the four never-passing rungs ALWAYS failing, or just unlucky at n=3?

Commit `1a3e3e3`, mellum2-12b @ localhost:11441 (GPU1). Six attempts each, seeds
1-6 (`--seed 1 --repeat 6`, which offsets per round).

Prompted by a rung that scored 2/3 in one pass and 1/10 in another: three
attempts is not enough to call anything "always failing" on this ladder.

## Result

| rung | this batch | pooled Mellum, all runs |
|---|---|---|
| `rust-symptomatic` | 0/6 | **1/27** |
| `engine-grid-scan` | **1/6** | **1/14** |
| `engine-ecs-query` | 0/6 | **0/14** |
| `engine-diagonal-wired` | 0/6 | **0/14** |

**Two rungs are genuinely 0-for-14**: `engine-ecs-query` and
`engine-diagonal-wired`. The other two are not "always failing" -- each has one
recorded pass, and `engine-grid-scan`'s came in this very batch, which is
precisely the case n=3 would have missed.

## These rungs are not broken -- they are above a 12B model's ceiling

Splitting every recorded attempt by MODEL is what makes the picture legible, and
is the step that should have come first:

| rung | tiel-coder-35b | mellum2-12b |
|---|---|---|
| `rust-symptomatic` | 38/38 | 1/27 |
| `engine-ecs-query` | 35/37 | 0/14 |
| `engine-grid-scan` | 35/37 | 1/14 |
| `engine-diagonal-wired` | 32/37 | 0/14 |

Tiel solves all four essentially every time. So the rungs work as instruments;
Mellum simply cannot do them. Any harness fix aimed at these rungs should be
justified by the turns it saves TIEL, not by an expected Mellum pass.

## Guard rejections track failure, on the one rung with a pass

`engine-grid-scan`, per seed:

| seed | turns | guard rejections | outcome |
|---|---|---|---|
| 1 | 17 | 2 | **PASS** |
| 2 | 40 | 22 | red |
| 3 | 21 | 14 | red |
| 4 | 40 | 14 | red |
| 5 | 29 | 20 | red |
| 6 | 15 | 11 | red |

The passing run tripped the pre-write guards twice; every failing run tripped
them 11-22 times. **This is a correlation and the direction of causation is not
established** -- a confused model sends bad edits, so heavy guard traffic may be
a symptom rather than a cause. It is not evidence that the guards are blocking
correct work: a separate audit of `engine-ecs-query` checked the rejections
individually and found them correct, including three that would each have
deleted `iter2` and broken every caller, and one that would have replaced a
276-line ECS with a stub.

## Corrections to earlier claims in this session

1. **The `rust-symptomatic` "regression" was a phantom.** Its baseline PASS at
   commit `adea9d5` was one draw; dedicated runs at that same commit scored 1
   pass / 3 red. Nothing landed today caused it.
2. **The 39 historical passes I first cited were Tiel's, not Mellum's.** Pooling
   two models into one tally nearly produced a second phantom.
3. **A stop-sequence misfire does not destroy a correct answer.** It fired 31
   times across three ladder runs; in all 14 traced instances the NEXT turn
   emitted a valid tool call. It costs a turn, not an answer.

## Method note

Three attempts cannot distinguish "always fails" from "usually fails" on this
ladder, and single-run baselines have now produced two phantom regressions in
one session. Report n and the pooled figure; split by model before pooling
anything.
