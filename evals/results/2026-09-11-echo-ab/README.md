# A/B: does the after-edit echo cause the turn regression?

Mellum only (no Tiel, per instruction). Identical binary, identical seeds 1-6,
5 rungs x 6 runs per arm = 60 runs. The ONLY difference is `SC_NO_EDIT_ECHO=1`.

Control arm verified before spending the GPU: OFF produced **0 echoes across 40
turns**, ON produced 122. The arms genuinely differ.

## Result: the hypothesis is refuted

| metric | ON (echo) | OFF (no echo) | prediction |
|---|---|---|---|
| median turns, failed runs | **30.5** | 34.0 | wrong direction |
| budget-exhausted | **5** | 7 | wrong direction |
| median PEAK prompt | 16,958 | **14,476** | held (+17%) |
| pass rate | 6/30 | 9/30 | p = 0.55, chance |

I predicted, in writing before the run, that if the echo caused the
09-10 -> 09-11 regression (turns 23->34, budget-exhausted 4->9) then the OFF arm
would show fewer turns and fewer budget exhaustions.

**It showed more of both.** The echo does inflate the prompt -- that part of the
suspicion was right, +17% peak -- but it does not lengthen runs. If anything it
shortens them slightly, which is the direction the feature was built for.

So the turn regression between 09-10 and 09-11 was variance or one of the other
two commits, not this. Two 51-run passes differing in three commits cannot
attribute anything; that is what this controlled arm was for.

## Per rung

| rung | ON pass / median turns | OFF pass / median turns |
|---|---|---|
| engine-grid-scan | 0/6, 30 | 0/6, 31 |
| engine-ecs-query | 0/6, 30 | 0/6, 24 |
| rust-symptomatic | 0/6, 30 | 0/6, 34 |
| rust-hidden-invariant | 2/6, 31 | 4/6, 38 |
| rust-trait-impl | 4/6, 24 | 5/6, 40 |

No rung dominates the arm medians, and the two rungs where OFF passed more often
are also the two where OFF took *longer* per failed run -- the opposite of a
coherent "echo hurts" story.

## Verdict

**Keep the echo.** It costs 17% peak prompt and buys slightly shorter runs; the
pass-rate difference is chance at p=0.55. The honest summary is that it is not
harmful, not that it is proven helpful -- n=6 per rung cannot show a real effect
on pass rate, and I said so before running rather than after.

`SC_NO_EDIT_ECHO` stays as a documented off switch so this can be re-measured
against Tiel, where saved turns actually show up.
