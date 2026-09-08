# Did raising the reply cap and steering a capped turn help? — commit eb2bcea

Same 14 rungs, control arm, 3 repeats, against `2026-09-08-full14`'s control arm
(commit fa1a82f, reserve 2048).

| | pre-fix (2048) | post-fix (3072 + steer) |
| --- | --- | --- |
| solved | 41/42 | 41/42 |
| runs hitting the cap | 5 | **2** |
| truncation faults | 13 | **3** |
| wasted turns | 24 | **13** |
| summed per-task median wall-clock | 359s | 343s (96%) |

**The failure mode is mostly gone: truncation faults down 77%, wasted turns
roughly halved, at the same solve rate.**

Two honest caveats:

- **Wall-clock barely moved** (96%). The saved turns were real but the run is
  dominated by the tasks' normal work, and desktop-GPU timing at n=3 is noisy.
  The win here is reliability, not speed.
- **It is not eliminated.** Two runs still reached the new 3072 ceiling, so the
  distribution has a tail past the shoulder. The steer is the backstop for
  those, and the remaining 13 wasted turns are where to look next.

The single failure (`rust-multi-site`, repeat 3) is a different task from the
pre-fix failure (`engine-ecs-query`, repeat 2) — both look like model variance
on hard rungs rather than a harness defect.
