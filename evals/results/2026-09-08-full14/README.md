# 14 rungs, 3 arms, 3 repeats (126 runs) — commit fa1a82f

The first run with both instruments the earlier phases were missing: the four
cross-file rungs, and the server's own prefix-cache split.

## The one unambiguous result: the prefix cache works

| arm | tokens sent | re-prefilled | served from cache | hit rate |
| --- | --- | --- | --- | --- |
| control | 1,565,722 | 233,045 | 1,498,504 | **87%** |
| raw / pi | — | — | — | not reported |

The harness sends 1.57M prompt tokens and the server only has to prefill 233k
of them. This is what the append-only prompt work bought, and it is invisible
in `total_prompt_tokens` — which is exactly why the phase runs looked flat.

## The harness halves the work against a bare model

Summed over per-task medians (three repeats, to blunt model variance):

| | control | raw (no harness) | pi |
| --- | --- | --- | --- |
| prompt tokens, all rungs | 394k | 782k (**199%**) | not instrumented |
| wall-clock, all rungs | 359s | 717s (**200%**) | 338s |
| median steps | 5.0 | 10.5 | 7.0 |
| median steps, cross-file rungs | 9.5 | 16.0 | 9.0 |

The bare model with native tool calling needs twice the tokens, twice the time
and twice the steps for the same work. That gap is the harness's value, and it
is the clearest it has been measured.

## The uncomfortable result: the new rungs did not discriminate either

Solve rate: control 41/42, raw 42/42, pi 40/42. **Eleven of fourteen tasks
scored identically on every arm every round** — worse than the ten-rung suite's
six of ten, because the four new rungs are all 3/3 for everyone.

They are genuinely harder work (4x the tokens, 1.6x the steps of the old rungs)
and they separate the arms on *cost*. They do not separate them on *pass/fail*.
Cross-file spread was the wrong axis: this model can do multi-file edits, it
just does them expensively.

The three failures are all noise rather than signal: control lost
`engine-ecs-query` once (the rung that also produced a 33-step outlier last
run), pi lost `engine-diagonal-wired` and `rust-trait-impl` once each.

## What would actually discriminate

Not more files. Candidates, in order of my confidence:

1. **A budget ceiling.** Score at a fixed step or token cap. Everything above
   passes eventually; the question is what passes *cheaply*. Cost is already
   where the arms separate 2:1, so make it the grade.
2. **Tasks with a wrong-but-plausible first move**, where recovery is the skill
   being measured — that is what the stall detector and the failure-signature
   guard exist for, and nothing here exercises them (`wasted` averaged 0.6).
3. **Long-horizon tasks** past the context window, where compaction and the
   recent-window floor decide the outcome.
