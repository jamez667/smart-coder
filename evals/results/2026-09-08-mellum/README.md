# Mellum2-12B-A2.5B on the 17-rung ladder — ABANDONED after 6 runs

Faster at generating, far worse at the work. Stopped early because the result
was not close.

| task | Mellum | | | Tiel (same rungs) |
| --- | --- | --- | --- | --- |
| | outcome | steps | secs | |
| rust-stated | PASS | 2 | 4 | PASS, 2 steps |
| rust-located | PASS | 3 | 5 | PASS, 4 steps |
| rust-symptomatic | **STILL-RED** | 23 | 368 | PASS, 3 steps, 8s |
| rust-misdirected | **STILL-RED** | 28 | 337 | PASS, 4 steps, 18s |
| rust-two-stage | **STILL-RED** | 29 | **2847** | PASS, 4 steps, 9s |
| rust-invariant | PASS | 7 | 6 | PASS, 3 steps, 8s |

It solves the two easiest rungs (the fix is named in the task text) and fails
everything requiring diagnosis. Tiel scores 51/51 on this suite.

## ROOT CAUSE FOUND (2026-09-09): a harness bug, not a weak model

Mellum's chat template wraps calls in `<tool_call>...</tool_call>` and ends the
turn with `<|im_end|>`. The harness stores each assistant turn in the recent
window as **bare JSON** — `{"path":"lib.rs","tool":"read_file"}` — with no
wrapper (`agent/window.rs`, `Message::assistant(action)`, fed from the
normalised text form in `sc-model`).

So the model reads a dozen turns of history in a format its template never
produces, concludes that is the house style, imitates it — and never emits the
stop token, because the thing that ends its turn is the tag it was never shown.

Isolated and reproduced:

| prompt | completion tokens | result |
| --- | --- | --- |
| one tool, clean history | **24** | perfect call, clean stop |
| six tools, clean history | **24** | perfect call, clean stop |
| six tools, **harness's bare-JSON history** | 164 | stray `</tool_call>`, drifting |
| the same at real run length | 3,072 (cap) | runaway: `{"tool":"finish"}</tool_call>` × 60 |

The model is fine. Native tool calling is fine. The registry is fine. The
*history format we replay back to it* is what breaks it, and it breaks worse
the longer the run goes — which is exactly the observed shape (easy rungs pass
in 2-3 turns, anything needing 20+ turns collapses).

**This affects any model whose template uses tool-call tags**, not just Mellum.
Tiel tolerates it, which is why the bug stayed hidden.

## The tell: 15 truncated replies per failed run

Each failure burned ~350-400k prompt tokens with 14-15 `ReplyTruncated` faults —
the model rambling past the 3,072-token cap on every hard turn, never
converging on a call. That is the failure mode that wastes the most wall-clock,
and it is why `rust-two-stage` took 47 minutes before giving up.

## Throughput was never the problem

Measured before this run: Mellum generates at 151 tok/s (3080 Ti) and 216 tok/s
(3080), against Tiel's 92-110. Two instances run concurrently at ~222 tok/s
combined. All true, and all irrelevant — a model that cannot finish the task
does not benefit from finishing tokens faster.

This is the clearest evidence yet for the day's recurring lesson: **turn count
and task completion dominate, not token rate.** A model 2x faster per token
that needs 10x the turns is 5x slower in practice.

## Hardware note (CORRECTED 2026-09-09)

Re-measured with each card alone and properly pinned:

| card | tok/s |
| --- | --- |
| 3080 Ti, x16 Gen2 | **253** |
| 3080, x4 Gen1 | 215 |

The Ti is 1.18x faster, which is what its memory bandwidth predicts (912 vs
760 GB/s) — decode is memory-bound, so the ratio tracks bandwidth and the x4
Gen1 link is irrelevant once weights are resident.

**The first version of this file claimed the x4 card was faster.** That was two
errors pointing the same way: Compose's `device_ids` does not hide the other GPU
from the process, so llama.cpp split each instance 6.3/4.1 GB across both cards
and neither was ever on one card; and Tiel was still partly resident on the Ti,
so it contended while the 3080 ran clean. `CUDA_VISIBLE_DEVICES` is what
actually pins an instance to a card.

What survives: PCIe carries weights at load time and per-token traffic for a
*split* model, so part of why Tiel is slower is that `--tensor-split 11,9` pays
exactly that. A single-card model good enough for the work would win on that
alone. Mellum is not that model.

Both Mellum profiles remain in the ops compose (`--profile mellum`) with these
numbers recorded, so nobody re-runs this experiment blind.
