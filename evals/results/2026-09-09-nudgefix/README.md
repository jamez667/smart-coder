# The four rungs after the nudge fixes — rust-misdirected converts

| rung | before the nudge fixes | after |
| --- | --- | --- |
| rust-misdirected | STILL-RED 25 steps, 30s | **PASS, 8 steps, 8s** |
| engine-diagonal-wired | STILL-RED 16 steps, 21s | STILL-RED 28 steps, 68s |
| engine-grid-scan | STILL-RED 14 steps, 26s | STILL-RED 37 steps, 108s |
| engine-ecs-query | STILL-RED 16 steps, 26s | STILL-RED 31 steps, 57s |

## `rust-misdirected` was never a reasoning failure

I examined this rung forensically, concluded the model had the wrong theory
(that the `#` was inside a quoted string), and reported plainly that no harness
change would reach it. **It passes in 8 seconds once the harness stops
contradicting itself.** The forensic read was of what the model EMITTED; nobody
read what the harness SAID to it until the user insisted.

The two fixes:
- The stall directive recommended and forbade the same tool in one sentence —
  `Emit \`write_file\` or \`edit_file\` … Do NOT emit \`edit_file\` again.` It
  fired 8 times in one run; on 3 of those the model answered with a read or a
  verification, which are exactly the do-nothing turns the nudge exists to
  prevent.
- A call naming `write_file` while carrying `old_str`/`new_str` was thrown away
  with "tool write_file has no parameter new_str". The harness had ORDERED
  `write_file` in the rewrite escalation; the model obeyed the name while
  meaning an edit. It now runs as the edit it is, and the model is told.

Verified in the live prompts afterwards: **0 contradictions across 101 calls.**

## The three engine rungs got slower, and that is worth watching

They also ran longer than the previous attempt (16→28, 14→37, 16→31 steps).
The previous run's low step counts were partly the model giving up sooner, so
this is not straightforwardly a regression — but it is not an improvement
either, and one run cannot separate the two. What the transcripts show now:

- 24 no-op edits, several with `old_str` and `new_str` byte-identical, sent
  immediately after the harness said the last one changed nothing.
- The model editing doc comments (`/// A* on any TileSource…`) while the bug is
  in the code below them.

That is the remaining failure mode and it is not obviously a harness defect —
but neither was `rust-misdirected` until someone read the prompts.
