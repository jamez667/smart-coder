# demo: worker swarm (red → green)

A red fixture for the **worker swarm** (spec 08 — "scale out, not up"): an
orchestrator decomposes the task into independent subtasks and a pool of tiny
workers fixes them in parallel, each in an isolated scratch copy, with the
orchestrator integrating proposals one at a time.

`mathlib.py` has two broken functions (`is_even`, `double`); `test_mathlib.py`
is the contract. Make the tests pass **without editing test_mathlib.py**.

```powershell
cd examples\demo-swarm
smart-coder swarm "Fix is_even and double so the tests pass" --verify "python -m pytest -q"
```

This serves a live web dashboard of the swarm; the printed `localhost` URL shows
the task board and each worker. Reset after a run:
`git checkout examples/demo-swarm/mathlib.py`
