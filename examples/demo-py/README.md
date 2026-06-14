# demo: is_even in Python (red → green)

A red fixture that exercises the *real* pipeline: the symbol index parses
`calc.py` (so `find_symbol`/`search_code`/the repo map work), and verification
runs **pytest** (so `run_verification` returns structured per-test results).

`calc.is_even` is broken (always returns False); `test_calc.py` is the contract.
Make the tests pass **without editing test_calc.py**.

```powershell
cd examples\demo-py
dumb-coder run "Fix is_even in calc.py so even numbers return True" --verify "python -m pytest -q"
```

Reset after a run: `git checkout examples/demo-py/calc.py`
