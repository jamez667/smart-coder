# Does an append-only prefix actually hit llama.cpp's KV cache?

Direct probe against llama.cpp b10015, tiel-coder-35b, `cache_prompt: true`.
Two requests: the second is the first with an assistant reply and one new user
message APPENDED, so the byte prefix is identical.

| turn | prompt_tokens sent | cache_n (reused) | prompt_n (prefilled) | prompt_ms |
| --- | --- | --- | --- | --- |
| 1, cold | 698 | 0 | 698 | 931 |
| 2, appended | 721 | 694 | 27 | 132 |

**96% of turn 2 was served from cache; it prefilled 27 tokens instead of 721 and
took 7x less time.**

## Why this matters for the phase measurements

`evals/results/README.md` records the phase runs as flat on
`total_prompt_tokens`. This probe shows why that column could never have moved:
it counts what the harness **sends** (721 here, up from 698), while the work the
append-only prefix saves is what the server **re-prefills** (698 -> 27). The two
numbers move in opposite directions.

`ladder-ab` now records `cached_prompt_tokens` / `prefilled_prompt_tokens` per
run from each reply's `timings`, so future rows measure the right thing.
