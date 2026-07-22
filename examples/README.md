# Examples

Small **red → green** fixtures for driving `smart-coder` against a real local
model. Each one ships with a deliberately broken implementation and a passing-once-
fixed contract test; the task is always to make the test pass **without editing
the test**. After a successful run, reset with the `git checkout` shown in each
demo's README.

You'll need a model backend configured (see
[Running the backends](../README.md#running-the-backends)) — these exercise the
live pipeline, not the deterministic unit tests.

| Demo | What it exercises | Verify |
| --- | --- | --- |
| [`demo-is-even`](demo-is-even/) | Smallest possible fixture, shell impl + test | `sh test.sh` |
| [`demo-py`](demo-py/) | Real pipeline: symbol index over Python + pytest results | `python -m pytest -q` |
| [`demo-multi`](demo-multi/) | Multi-file change (two modules, four functions) | `python -m pytest -q` |
| [`demo-swarm`](demo-swarm/) | The worker **swarm** decomposing and fixing in parallel | `python -m pytest -q` |

See also [`tasks/`](tasks/) and [`suite.toml`](suite.toml) — the eval suite
consumed by `sc-eval`.
