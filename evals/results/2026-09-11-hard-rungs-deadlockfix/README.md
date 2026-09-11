# Does the stall-ladder deadlock fix help Mellum on the four hard rungs?

Commit `4413e2d` against baseline `1a3e3e3`, mellum2-12b @ localhost:11441 (GPU1).
Six runs per rung, seeds 1-6, identical to the baseline batch.

**The expectation was recorded in `RUN.txt` BEFORE the run: fewer wasted turns,
NOT more passes.** Tiel already solves all four; Mellum's failures are a
capability ceiling, not a harness block.

## The narrow claim the fix made, and it held

| | before | after |
|---|---|---|
| deadlock sequences (advice names `write_file`, then `write_file` refused within 5 events) | **6** | **0** |
| `too large to safely overwrite` rejections | **8** | **0** |
| advice naming `write_file` at all | 80 | 64 |

The specific pathology is gone: across 24 runs the harness never again recommended
a wholesale rewrite of a file `write_file` would refuse.

## Pass rate: unchanged, as predicted

| rung | before | after |
|---|---|---|
| `rust-symptomatic` | 0/6 | 2/6 |
| `engine-grid-scan` | 1/6 | 0/6 |
| `engine-ecs-query` | 0/6 | 0/6 |
| `engine-diagonal-wired` | 0/6 | 0/6 |
| **total** | **1/24** | **2/24** |

One net pass. That is inside the noise this ladder was already measured to have --
`rust-two-stage` scored 2/3 in one batch and 1/10 in another on an unchanged
commit. **Do not read the symptomatic 0/6 -> 2/6 as a fix.** It is the same rung
that has one pass in its entire recorded Mellum history before today.

## Turns: mixed, and only two rungs could possibly be affected

| rung | before (median) | after (median) | can the fix fire? |
|---|---|---|---|
| `rust-symptomatic` | 27.5 | 26.5 | no -- 50-line fixture |
| `engine-grid-scan` | 29.0 | **22.5** | yes -- `tilegrid.rs` 225 lines |
| `engine-ecs-query` | 25.5 | **23.0** | yes -- `world.rs` 247 lines |
| `engine-diagonal-wired` | 28.5 | **35.0** | no -- largest file 144 lines |

Budget-exhausted runs fell from 5 to 2.

The two rungs the fix *can* reach both got shorter; the two it cannot reach moved
in opposite directions, one of them notably worse. `engine-diagonal-wired` at
28.5 -> 35 is **variance, not a regression**: its largest file is under the
150-line threshold, the `only_edit_tool` branch fired zero times in both batches,
and its real blocker is unchanged -- 27 frozen-file refusals as the model keeps
trying to edit the contract test.

## Honest summary

The fix removes a measurable harness pathology and shortens the runs where it
applies. It does not convert failures, and nothing here should be cited as
evidence that it does. The value is turns saved -- which matters most for Tiel,
which passes these rungs and pays the same deadlock cost in wasted turns.
