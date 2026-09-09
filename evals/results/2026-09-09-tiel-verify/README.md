# Tiel after the native-tool-call work — 17/17, no regression

The check I owed. Commits d788d02 (replay native tool calls) and 1667f08 (never
replay invalid arguments) change how EVERY turn is sent, and every baseline
number in this directory predates them. Tiel tolerated the old wire format,
which is the only reason the bug stayed hidden — "should be unaffected" is not
a measurement.

| | before (medians, 3 repeats) | after (1 run) |
| --- | --- | --- |
| solved | 17/17 | **17/17** |
| prompt tokens | 305,027 | 304,120 |
| wall clock | 302s | 312s |
| prefix cache | 81% | **81%** |
| truncation faults | — | 2 |

Flat on every axis, which is the desired result: the fix targets models whose
template wraps calls in markup, and Tiel is not one of them. Per-task movement
(rust-symptomatic 6,329 → 2,446 tokens; engine-diagonal-wired 34,512 → 62,733)
is ordinary run-to-run variance on a nondeterministic model at n=1 against a
median of 3, not signal.

## The regression this run caught first time round

The first attempt at this verification died at `engine-diagonal-path` with
HTTP 500:

```
Failed to parse tool call arguments as JSON: parse error at column 1261:
invalid string: missing closing quote
```

Tiel emits a large `write_file` whose arguments JSON is cut off at the token
cap. The harness salvages usable work from that; the SERVER will not accept it
back, and rejects the entire request — aborting the task at zero steps. Before
d788d02 the broken text sat harmlessly in `content`.

Fixed in 1667f08: a call whose arguments do not parse is never replayed, and
the turn reverts to the old plain-content shape with its observation as a plain
user message rather than a `role:"tool"` with a dangling id.

**Worth keeping in mind:** 2,701 unit tests passed on the change that aborted a
real task at step zero. The suite and the live run disagreed, and the live run
was right — the third time today that has happened.
