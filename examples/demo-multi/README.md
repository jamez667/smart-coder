# demo: multi-file fix (red → green)

A red fixture that spans **two modules**, so the agent has to change more than
one file to go green — exercising the repo map, `find_symbol`/`search_code`
across files, and pytest verification.

Both implementations are stubbed wrong: `mathutil.py` (`square`, `is_positive`)
and `stringutil.py` (`reverse`, `shout`). `test_utils.py` is the contract. Make
the tests pass **without editing test_utils.py**.

```powershell
cd examples\demo-multi
smart-coder run "Fix square, is_positive, reverse and shout so the tests pass" --verify "python -m pytest -q"
```

Reset after a run: `git checkout examples/demo-multi/mathutil.py examples/demo-multi/stringutil.py`
