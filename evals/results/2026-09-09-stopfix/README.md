# Stopping at `</tool_call>` — 86% faster, two rungs converted

The six rungs Mellum2-12B failed on the 2026-09-09 full run, re-run with the
strategy declaring `stop: ["</tool_call>"]` for native tool calling.

| rung | before | after |
| --- | --- | --- |
| rust-misdirected | STILL-RED 316s, 11 truncated | STILL-RED **34s, 0** |
| rust-two-stage | STILL-RED 546s, 20 truncated | **PASS 11s, 0** |
| engine-diagonal-wired | STILL-RED 140s, 6 truncated | STILL-RED **78s, 0** |
| engine-grid-scan | STILL-RED 352s, 8 truncated | STILL-RED **97s, 0** |
| engine-ecs-query | STILL-RED 168s, 4 truncated | STILL-RED **74s, 0** |
| rust-multi-site | STILL-RED 661s, 27 truncated | **PASS 12s, 0** |
| **total** | **2,183s, 76 truncated** | **305s, 0 truncated** |

**86% of the wall-clock gone, every truncated reply eliminated, and two rungs
that looked like reasoning failures were actually drowning in wasted
generation.** `rust-multi-site` went from 661s to 12s.

## The mechanism

The model emitted one complete valid call, then `</tool_call>`, then began a
SECOND call in a different format and ran to the 3072-token cap — ~19s a turn,
discarded. The harness parsed the leading call correctly, so no turn was ever
lost; the cost was pure wall-clock, plus the junk poisoned the prefix cache.

## The 21 "misfires" are mostly my detector, not the stop

`FaultKind::StopSequenceMisfire` fires when we set a stop, the server reports
`finish_reason: "stop"`, and the reply yields no parseable call. It reported 21.
Checking the transcripts: only 5 of 146 replies contain no tool object at all,
and 3 of those are a `write_file` payload ending `...}}\n` — a COMPLETE nested
`{"name":…,"arguments":{…}}` object. The stop fired cleanly after a well-formed
call; the detector's "parseable" check only recognised the flat `{"tool":…}`
shape.

So the fault is over-reporting on a healthy stop. That is the exact noise the
fault count exists to avoid, and it should be narrowed to match both call
shapes before anyone reads the number. The underlying stop behaviour is sound —
zero truncations and no lost work across 146 calls.

## Scope

Mellum only, one repeat. The stop list is declared per strategy and empty for
`ParseRepair`/`Grammar`, so Tiel's grammar path is untouched — but Tiel on
native tool calling would now also carry it, and that is unmeasured.
