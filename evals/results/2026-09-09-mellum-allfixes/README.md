# Mellum with every fix applied — still 2 of 17. It is the model.

Third attempt, with all of today's harness work in place: native tool calls
replayed in the model's own wire format, the invalid-arguments guard, and
honest no-op reporting.

## The same rung, three times

| run | steps | secs | tokens | truncated replies |
| --- | --- | --- | --- | --- |
| pre-fix | 23 | 368 | 350,493 | 15 |
| after the tool-call fix | 32 | 313 | 478,612 | 16 |
| **after every fix** | 27 | 378 | 461,076 | **17** |

`rust-symptomatic` is `STILL-RED` in all three. Nothing improved; the
truncation count went slightly UP each time. Tiel passes this rung in 2 steps
and 9 seconds.

## Verdict: a capability wall, not a harness problem

The forensic read of the transcripts (`scratchpad/mellum-postmortem.md`) is
unambiguous. Mellum has the right file and the exact failing assertion by step
3 — where Tiel simply solves it. It then deletes the wrong line and spends
every remaining turn undoing that damage. Across 15 `edit_file`/`write_file`
calls it **never once touches the buggy index expression**, and two of its
rewrites reproduce the original buggy file byte-for-byte.

Every harness bug found along the way was real and worth fixing on its own
merits — they affect any model, and Tiel is measurably unharmed (17/17, 81%
cache, flat cost). But none of them was why Mellum fails, and fixing all three
changed the outcome by zero rungs.

## What this cost, and what it bought

Three GPU runs to establish one negative result. The value is elsewhere: chasing
it surfaced three genuine harness defects — a wire-format bug affecting any
model whose template wraps tool calls, an HTTP 500 that aborted tasks outright,
and the harness reporting "ok (1 replacement)" for edits that changed nothing.
The last one had been lying to every model, including Tiel, for the whole
project.

**Mellum2-12B-A2.5B is not a Tiel replacement.** Its 2x token throughput and
single-card residency are real and irrelevant while it cannot diagnose. Two
instances of a model that cannot solve the task is not a faster harness.
