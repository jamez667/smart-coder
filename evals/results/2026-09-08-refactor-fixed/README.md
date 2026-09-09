# The same refactor, after the completion fix — 598s → 117s

Extract five methods from an 8,190-line `app.rs`. Same model, same repo, same
prompt as `2026-09-08-refactor-anatomy`.

| | before | after | pi |
| --- | --- | --- | --- |
| wall clock | 598s | **117s** | 236s |
| model calls | 43 | 15 | — |
| capped replies | 9 (295s) | **0** | — |
| result | correct | correct | correct |

We are now ~2x faster than pi on this task. The whole gain came from removing
turns, not from any change to sampling: same GPU, same ~150 tok/s, 5x faster
because it took 15 turns instead of 43.

## Where the remaining 117s goes

| | turns | time |
| --- | --- | --- |
| small tool calls (<400 chars of reply) | 12 | **61s** |
| real generation (the two writes + one long think) | 3 | 51s |

**Twelve turns emit ~20 tokens each and still cost ~5s apiece.** That is prefill
plus round-trip, not thinking. One turn spent 13.3s to read 8 lines. Turn cost,
not token rate, is what this workload is made of.

## Why prefill is still costing us

Cache: 85k reused / 61k re-prefilled (58%, against 81% on the ladder). The
prompt-size trace shows why — it grows monotonically to call 12 and then
**shrinks three times**:

```
12     48307     +584
13     37435  -10872   <- window eviction dropped older turns
14     36341   -1094
15     27573   -8768
```

Every shrink invalidates the cached prefix and forces a full re-prefill of what
remains. Eviction is doing exactly what it was designed to do; the cost is that
it happens one turn at a time, so the prefix is repeatedly broken near the end
of a long run. Batch eviction (drop to ~80% of budget once, rather than a pair
per turn) was already listed as a Phase-4 follow-up and is now measured.

## Ranked next levers

1. **Fewer turns.** At ~5s of fixed cost per turn, every avoided turn beats any
   sampling change. Five of fifteen turns here were `read_file` on the same file
   at shifting offsets (27s total) — a model re-establishing where it is.
2. **Batch eviction**, to stop breaking the prefix three times at the end of a
   run. Worth ~10-20s here on the cache numbers above.
3. **A persistent status file** (the user's "manager" idea): the harness already
   pins `PLAN-*.md` on the way IN, and the comment there describes this exact
   stall — "the model reads it every few turns to remember what it's building".
   Nothing writes progress back OUT. Note `PlanState::complete_active` exists
   and is never called: the plan can only fail, never advance.
