# Ladder results

One directory per measurement, `rows.jsonl` written by `ladder-ab` (one JSON row
per task, arm, model, commit and repeat), plus the run's report and progress log.

| dir | commit | what changed | arms |
| --- | --- | --- | --- |
| `2026-09-07-baseline` | `2a4c557` | before the harness work | control, raw, pi |
| `2026-09-08-phase1` | `d7d5044` | append-only prompt, observation last | control |
| `2026-09-08-phase123` | `e04abe5` | + guard consolidation, six tools, numbered reads | control |
| `2026-09-08-cache-probe` | — | direct llama.cpp KV-cache probe (2 requests) | — |
| `2026-09-08-full14` | `fa1a82f` | 14 rungs incl. 4 cross-file, + cache metrics | control, raw, pi |

## What the baseline said

Control solved 30/30, pi 29/30, raw (native tool calling, no harness) 25/30.
Control spent 15.8k prompt tokens per task against raw's 27.7k, at pi's
wall-clock. Six of the ten rungs scored identically on every arm every round,
so **this ladder cannot rank the arms on solve rate** — it can only compare
cost, and only on the four rungs that move.

## What the phase runs said, honestly

Solve rate stayed 30/30 throughout. Summed over medians (three repeats per
task, to blunt the model's own variance) the totals are **flat, not a win**:
170k baseline, 178k phase1, 150k phase123 prompt tokens.

Two things this measurement cannot see:

- **Prefix reuse is invisible here.** `total_prompt_tokens` counts what the
  harness *sends*, and the append-only prefix changes what the server has to
  *re-prefill*. The effect belongs in wall-clock, and wall-clock on a shared
  desktop GPU is too noisy at n=3 to carry it (the medians move both ways).
  Measuring it properly needs llama.cpp's own `prompt_n` / cache-hit counters
  off `/slots`, not this column.
- **One run dominates any total.** `engine-ecs-query` repeat 2 on phase123 took
  33 steps and 396k tokens where its two siblings took 6 and 8 — and still
  passed, with no harness faults and two wasted turns. It is model variance on
  the hardest rung, not a regression; totals-of-sums double because of it,
  which is why the medians above are the honest read.

Both of those were then built, and `2026-09-08-full14` is the answer:

- **The cache counters settled it.** 87% hit rate over 126 runs: the harness
  sends 1.57M prompt tokens and the server re-prefills 233k. The append-only
  work does what it was supposed to; this column could never have shown it.
- **The harness halves the work.** Against a bare model with native tool
  calling and no harness: 199% of the tokens, 200% of the wall-clock, 2x the
  median steps for the same tasks.
- **The harder rungs did NOT discriminate.** All four are 3/3 for every arm, so
  11 of 14 tasks are now flat. Cross-file spread was the wrong axis — see that
  run's README for what to try instead (a budget ceiling is the leading
  candidate, since cost is where the arms already separate 2:1).
