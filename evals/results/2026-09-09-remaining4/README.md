# The last four rungs, with all ten fixes — 0/4, but the failure mode changed

| rung | after stop fix | now | 
| --- | --- | --- |
| rust-misdirected | 28 steps, 34s, 145k | 25 steps, **30s**, 97k |
| engine-diagonal-wired | 30 steps, 78s, 379k | 16 steps, **21s**, 152k |
| engine-grid-scan | 40 steps, 97s, 565k | 14 steps, **26s**, 149k |
| engine-ecs-query | 25 steps, 74s, 297k | 16 steps, **26s**, 152k |

Still red, all four. But wasted generation is **gone**: zero of 71 replies
lacked a tool call, and replies are now 400-600 chars where they used to run to
the 3,072-token cap. `engine-grid-scan` went from 40 steps (the cap) to 14.

## What the two cargo fixes bought

`engine-grid-scan` and `engine-ecs-query` were the rungs whose verification
output was being discarded entirely — the model saw `error: test failed, to
rerun pass --test contract` on every check instead of the named assertions.
That is fixed, and both now cost a third of what they did.

It did not make them pass. The transcript ends with the model submitting
essentially the same `edit_file` six times in a row — the ECS one still writes
`iter2_without` with an inverted condition, requiring the component it should
exclude. It has the assertions now and still cannot act on them, which was the
question worth answering: **the failure is reasoning, not information.**

## Honest reading

Ten harness fixes today took this model from 2/17 to 8/12 and cut the cost of
what it still cannot do by 60-75%. These last four look like the genuine floor
for a 12B model on this suite:

- `rust-misdirected` — right function, wrong theory (thinks the `#` is inside a
  quoted string; the input has no quotes).
- `engine-diagonal-wired` — never proposes creating the missing function;
  tries to delete the failing assertion from the frozen test instead.
- `engine-grid-scan` — delegates to methods it never wrote.
- `engine-ecs-query` — inverts the condition, six times identically.

The remaining waste is repetition rather than runaway generation: ~4.5 wasted
turns per task. That is a stall-detection question, not a generation one.
