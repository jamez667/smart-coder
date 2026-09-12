# Written for this rung, not vendored

Unlike the four `engine-*` tasks, nothing here is copied from void-claim. The
crate is modelled on the shape of that engine's telemetry subsystem — a shared
ring buffer, one module per sensor channel, each channel owning the meaning of
its own readings — but every line was written for this fixture.

**Why written rather than vendored:** this rung is sized. It exists to drive the
prompt past its budget so the loop's history compaction runs against a real
model, and that requires a known number of files of a known token cost. A
vendored subsystem gives you whatever sizes it happens to have: the real
telemetry modules are 40 to 600 lines and mostly small, and ten of them come to
about 11k tokens — under half of what is needed. Twelve uniform channels are an
eval instrument, not a plausible codebase, and pretending otherwise in this file
would be the dishonest part.

## The sizing, measured

Every number here was measured against `llama.cpp`'s `/tokenize` on
`tiel-coder-35b`, not estimated:

| quantity                                    | value  |
|---------------------------------------------|--------|
| prompt budget (32768 × 0.9 − 3072)          | 26,419 |
| six-tool native schema (`fixed_overhead`)    | 453    |
| **fitting ceiling** (`budget − overhead`)    | 25,966 |
| one channel module, delivered as a read      | ~2,250 |
| `lib.rs` + `series.rs`, delivered            | ~1,780 |
| whole crate read once                        | 28,929 |

Eleven channels is the first count that crosses the ceiling (102%). Twelve is
what ships, because a model does not read in a tidy order — it interleaves
`run_verification`, re-reads what it lost, and abandons files half-read. Reading
the crate front to back crosses the ceiling on read 13 of 14.

A run that never crosses it is still a valid run: the rung is red until every
call site is fixed, and the cross-channel test in `tests/contract.rs` is what
forces the breadth. Compaction is the *consequence* of doing the work, not a
precondition for scoring.

## The defect

All twelve channels hand-rolled the same windowed walk, and all twelve carry the
same boundary error: the doc comment promises `[from, to]` **inclusive at both
ends**, the code tests `s.t_ms > from`. A reading landing exactly on the boundary
two adjacent dashboard panes share is reported in neither.

All twelve are wrong on purpose. A single wrong site (the shape
`rust-shifting-anchor` uses) would let a model find it early and stop reading,
which defeats the point of the rung.

## Regenerating

The twelve channel modules and `lib.rs` are generated, so their shape stays
uniform: `scripts/gen_channels.py` in this directory writes them from one
template plus a per-channel table of units, capacities and spike rules. Edit the
template or the table and re-run it; do not hand-edit a single channel, or the
twelve stop being comparable and a reviewer can no longer tell constants from
control flow.

`series.rs`, `tests/contract.rs` and everything under `solution/` are written by
hand.

No external dependencies: the crate is std-only, so a cold `cargo build
--offline` is about half a second and the model can verify as often as it likes.
The lock file is vendored anyway so `--offline` never has to resolve.
