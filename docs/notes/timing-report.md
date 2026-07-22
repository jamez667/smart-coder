# Where the time goes — speed report

Measured against the live qwen3-coder-30b backend (:11435, dual-GPU) + Docker pytest
sandbox, on a full `ab_scale` ladder run (3 rungs × 2 arms, with `SC_DIAGNOSE=1`):
**770s wall-clock, 477 model turns, 57 verifications, 11 diagnoses.**

Method: wrapped the backend to time every in-loop `generate()` call, and timed Docker verify
directly. (An earlier isolated probe was misleading — single warm calls returned ~0.1s; a
*real in-loop* call with a full prompt is ~1.36s. The in-loop numbers are the ones below.)

## The breakdown

| Component | Time | % of wall-clock |
|---|---|---|
| **Model inference** (477 calls × ~1.36s) | **~649s** | **~84%** |
| Docker verification (57 × ~1.1s) | ~63s | ~8% |
| Diagnostic calls (11 × ~2.8s extra) | ~31s | ~4% |
| Harness / IO / variance (residual) | ~27s | ~4% |
| **Accounted** | **743s** | **96%** |

**The model is ~84% of the wall-clock. Everything else — Docker, the harness, the
diagnostic, the run-log work — is noise by comparison.** This is why the diagnostic re-run
removal (a Docker subprocess) was unmeasurable: it touched the 8% slice, not the 84% one.

## What drives model time

A model call's cost is dominated by **how many turns happen**, not per-call size:
- Per-call latency is ~1.36s for a normal turn (a few hundred output tokens), up to ~4s for a
  file-write (≈1000 output tokens) or a diagnosis.
- Prompt size barely matters for latency at these sizes (prefill is cheap on this rig); the
  cost is **generation** + per-call overhead.

So **turns are the currency.** And most turns are not productive work:

| Tool | Calls | |
|---|---|---|
| read_file | 340 | navigational |
| list_dir | 44 | navigational |
| run_verification | 21 | navigational |
| write_file / create_file / edit_file | **59** | **productive** |
| search_code | 1 | |

**Of 465 tool calls, only 59 (~13%) write or edit code. ~385 (~83%) are reads/lists** — the
model re-reading files to orient. Plus **34 stalls** and **15 malformed-output repairs**, each
burning turns. At ~1.36s/turn, the ~385 navigational reads alone are **~520s** — the bulk of
the run is the model *looking around*, not *building*.

## The levers (in priority order)

Since model-time = turns × per-turn-latency, and ~84% of wall-clock is model:

1. **Cut navigational turns (biggest lever).** ~83% of tool calls are reads/lists. The
   harness already pins `focus_files` and a progress ledger in some paths — extending "show
   the model what it needs so it stops re-reading" to the whole-task path would remove turns
   directly. Every read eliminated is ~1.4s saved. This is where the 520s lives.
2. **Cut stalls + repairs (34 + 15 = 49 wasted turns ≈ 67s).** Each stall/repair is a turn
   that did nothing. The session's stall/nudge/batch fixes already chip at this; fewer stalls
   = fewer turns.
3. **Token-trim the diagnostic** (the `last_failure_detail()` follow-on): feeds ~10 lines
   instead of the raw dump. Cuts the diagnostic's *generation* a little, but the diagnostic is
   only ~4% of wall-clock — small win. (Worth doing for token cost, not speed.)
4. **Parallelism / a faster model** — out of scope (hardware/model decision). The rig is
   already dual-GPU MoE at ~110 tok/s.

## Bottom line

The earlier instinct ("optimize the diagnostic re-run / Docker") was aiming at the 8-12%
slices. **The real cost is model turns (84%), and the dominant waste inside that is the model
re-reading files (~83% of tool calls).** The highest-leverage speed work is **reducing turns**
— especially navigational reads — not shaving Docker or harness overhead.
