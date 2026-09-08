# The pi arm

An eval arm that runs the external [pi](https://github.com/badlogic/pi-mono)
coding agent on the same red->green tasks as the in-tree agent, graded by the
same unchanged harness (`run_task` in `crates/sc-eval/src/runner.rs`). Red-first,
frozen contract tests, green-after and the tamper check all apply identically;
only the thing that drove the edits differs. It is a calibration point: a rung
pi solves and our agent does not is a harness gap, not a model gap.

## How it is invoked

`PiSolver` (`crates/sc-eval/src/pi_arm.rs`) runs, per task, inside the task's
workspace:

```
PI_CODING_AGENT_DIR=<repo>/evals/pi/agent-dir \
pi --offline -p --mode json \
   --no-session --no-extensions --no-skills --no-prompt-templates --no-context-files \
   --provider local-tiel --model tiel-coder-35b --tools read,edit,write,bash \
   -- "<task description>

The tests are run with: `<verify_cmd>`. Make them pass. Do not modify the test files."
```

That prompt is the whole of what pi is told. The solver does not run the verify
command; the harness does, afterwards.

- `--offline` is required: without it pi checks for a catalogue update at
  startup, which on this machine stalled a two-second run past a 60 s timeout
  with nothing on either stream.
- `--mode json` is what makes the run countable: text mode prints only the
  final answer. `steps` in the report is the number of `tool_execution_start`
  events; the full event stream is written to `<out_dir>/<task>-pi-<n>.log`
  when an output directory is set.
- The wall clock is the only cap. pi has no step limit, so each task's
  `timeout_secs` (default 600) bounds the run and the whole process tree is
  killed on expiry.
- The binary is found via `PI_BIN`, else `where`/`which pi`, else the known
  install under `%LOCALAPPDATA%\pi-node\current`. On Windows the solver execs
  `node.exe` on pi's bundled `cli.js` directly, exactly as `pi.cmd` would,
  because a batch file cannot be handed a prompt containing newlines.

## `agent-dir/`: pi's config, kept out of `~/.pi`

`PI_CODING_AGENT_DIR` points pi at `agent-dir/` instead of the user's own
`~/.pi/agent`, so a run never reads the user's providers or keys and never
writes sessions or settings there. The directory holds:

- `models.json`: one provider, `local-tiel`, an OpenAI-completions endpoint at
  `http://localhost:11436/v1` (the llama.cpp server from `../smart-coder-ops`),
  with the single model `tiel-coder-35b` (32k context, 4k max output, no
  reasoning, zero cost).
- `settings.json`: empty; pi's defaults.

Sessions are not saved (`--no-session`), so nothing accumulates here between
runs.

pi itself writes two files here on first use, `auth.json` and
`models-store.json`, both just `{}`. That is the isolation working: those are
the files that would otherwise land in `~/.pi/agent`.
