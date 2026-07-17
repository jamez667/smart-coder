# demo: is_even (red → green)

A tiny red fixture for trying `smart-coder run` against a real local model.

`impl.sh` is broken (always reports "odd"); `test.sh` is the contract test.
The task: make the test pass **without editing test.sh**.

```powershell
cd examples\demo-is-even
smart-coder run "Fix is_even in impl.sh so it reports even numbers correctly" --verify "sh test.sh"
```

Press `q` when the run finishes. To reset after a successful run:
`git checkout examples/demo-is-even/impl.sh`
