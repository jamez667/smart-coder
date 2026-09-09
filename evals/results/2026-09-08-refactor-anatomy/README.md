# Why the harness took 598s where pi took 236s

Same task (extract five methods from an 8,190-line `app.rs` into a module),
same model (`tiel-coder-35b`), same server, same clean repo. Both produced a
correct refactor. Transcript: 43 model calls, 568s of the 598s inside the model.

## The refactor finished at call 35. Calls 36-43 were pure waste.

| | calls | wall |
| --- | --- | --- |
| doing the work | 1-35 | 321s |
| insisting it was done | 36-43 | **246s** |

Every one of those eight replies opens with a correct summary — "The build
passes. The extraction is complete: created `app/net.rs`… added `mod net;`…
deleted the moved bodies" — then buries `cargo check -p miner 2>&1 | tail -3`
in the middle and keeps writing to the 3,072-token cap. The harness runs the
check, returns green, and the model does it again. Six of the commands are
byte-identical.

Take those eight turns away and the run is ~330s against pi's 236s.

## Three harness failures compound

**1. Nothing tells the model it may stop.** Phase-4 removed auto-finish on a
green-at-start run (correctly — that bug let an orphan file count as success),
but nothing replaced it with a positive signal. The model has finished, knows
it has finished, says so eight times, and keeps checking because no observation
ever says "yes, you are done, call finish".

**2. The stall ladder resets itself.** `handle_stall` returns `Recovered` after
delivering advice, which calls `stall.reset()`. So: repeat → advice → reset →
repeat. It only escapes via the bounded `SELF_RECOVERY_LIMIT`, and each round
costs 30s+. The run hit the 45-step cap before the ladder ran out. Worse, the
advice is generic ("take a concrete next action") when the actual problem is
that the model wants permission to stop.

**3. Each wasted reply makes the next one slower.** The replies are appended to
the prompt, which grows 27k → 97k chars across the tail. Call 36 takes 30s;
call 42 takes 37s. The waste is self-amplifying, and it is what collapsed the
prefix cache to 17% (80k cached vs 399k prefilled) against 81% on the ladder.

## What the ladder could never have shown

Every ladder rung is red-first and small, so `finish` is unambiguous: the tests
go green and the run stops. A refactor has no such signal — the build was green
before the agent started — and no fixture in the suite is shaped that way.
