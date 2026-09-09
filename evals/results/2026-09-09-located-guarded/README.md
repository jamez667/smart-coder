# `rust-located` after the destructive-edit guard — 3/3

The rung a bad `edit_file` wrecked, run three times with the guard in place.
Binary stamp `65813a3`, verified current (see the build-stamp work in 1b83e7f).

| repeat | outcome | steps | secs | tokens | faults |
| --- | --- | --- | --- | --- | --- |
| 1 | PASS | 4 | 6 | 5,933 | none |
| 2 | PASS | 8 | 12 | 17,932 | none |
| 3 | PASS | 5 | 5 | 8,022 | none |

Against the run that prompted the fix: **STILL-RED, 23 steps, 565s, 335k tokens,
10 truncated replies.**

## What the guard changed

The failing run's turn 2 sent `{"old_str":"        here = here.max(v);",
"new_str":":"}` and `edit_file` wrote it, leaving line 10 as a bare colon. Every
later correct edit then failed with "anchor not found" against a line that no
longer existed, and the model thrashed for twenty turns.

`edit_file` was the only writer with no balance check. It now runs
`delimiter_regression` like the others, plus a `destructive_replacement` guard —
needed because the tripwire alone would not have caught this: replacing a
statement with `:` leaves every bracket balanced.

## Scope of the claim

This says the guard fixes the regression on this rung. It does **not** say
Mellum is now viable: the model still fails `rust-symptomatic`, where it spends
its turns in the wrong function entirely, and that has held across three runs.
The guard is worth having for Tiel too — the same destructive edit would have
cost it the same way.
