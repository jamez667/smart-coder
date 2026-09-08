# The gateway experiment

Three measurements, in increasing order of cost and of what they can tell you.

## 1. Routing — `evals/gateway/suite.toml`

Model-free, deterministic, runs in `scripts/check.sh`. Does a plain-English need
reach the right capability?

    cargo test -p sc-gateway --test bench -- --nocapture

Graded three ways, kept separate because they fail independently: **routing**
(right capability, or a declared refusal), **reduction** (bytes in vs. out), and
**sufficiency** (`must_contain` survived).

Cases marked `expect = "refuse"` are needs the gateway is *supposed* to decline.
They pass by being refused. A suite of only-winnable needs measures nothing and
quietly becomes a suite the capability table was tuned against.

## 2. Reduction — `evals/gateway/output.toml`

Same command. Every sample under `output/` is **captured from a real cargo run**
in this workspace, never hand-written: a hand-authored "messy output" fixture
measures the author's idea of mess, which is exactly the fixture an extractor
gets tuned against.

`max_retained_percent` is a ceiling, not a target.

## 3. Captured needs — `evals/gateway/captured.toml`

Same command. Every need in this file is a phrasing a live model typed at `ask`
and the classifier REFUSED, captured verbatim from A/B runs.

This exists because measuring the refusal rate used to cost a 40-minute run
against a live 35B — and the last one produced **no data at all**: back-to-back
runs degraded throughput to 11 tok/s, requests timed out, and autoheal restarted
the container mid-suite. Replaying the captured needs answers the same question
in milliseconds.

It measures whether a model can get an answer at all. It does NOT measure
whether routing changes the solve rate — only the live A/B does that.

Needs marked `expect = "refuse"` are genuinely ambiguous and should stay
refused; they keep the file from becoming a list of only-winnable cases.

## 4. The A/B — `cargo run -p sc-eval --bin ladder-ab`

The expensive one, and the only one that answers the actual question: does a
small model *solve more tasks* with one classified `ask` than with the measured
six tools?

    cargo run --release -p sc-eval --bin ladder-ab -- \
        --url http://localhost:11436/v1 --model tiel-coder-35b --repeat 3

Both arms run through the same `run_task`, so the TDD invariants are enforced
identically and neither arm can pass by editing a test. Same config, same step
cap, same strategy, same model. The arms differ only in how the model LOOKS:

| | control | gateway |
|---|---|---|
| look | `read_file` + `run_command` | `ask` |
| change | `edit_file`, `write_file` | same |
| check | `run_verification` | same |

### Reading the result honestly

* **One pass is a signal, not a result.** A 1-2 task difference on a ten-task
  ladder against a nondeterministic model is noise. Use `--repeat 3`.
* **Watch the refusal rate.** A good solve rate with a high `ask` refusal rate
  means the model worked *around* the gateway, not through it. The report says
  so itself when the answer rate drops below 60%.
* **The `stated` rung cannot discriminate.** Its task text names the fix, so a
  competent model edits without looking and `ask` is never called. The rungs
  that need diagnosis — `symptomatic`, `misdirected`, `two-stage` — are where a
  difference in how the model looks can show up at all.
* **The gateway arm has no shell.** Deliberate, and stated in the report every
  run: on the control arm `run_command` is the investigation tool, and on the
  gateway arm that job is what `ask` exists to do.
* **It DOES keep `run_verification`,** and the model reaches for it rather than
  asking. That is the arm as it would ship — nobody would hide the test command
  behind a classifier when a direct tool is available. The cost is that this A/B
  compares *routing*, not context savings; the reduction is measured properly by
  `output.toml` where it does not depend on a model choosing to use it.
* **A low `ask` call count is therefore expected,** not a bug. What would be a
  bug is a high REFUSAL rate — that means the model asked and the gateway could
  not answer. The first run of this experiment refused 14 of 50 calls because
  the verification seam was left unwired; that run measured nothing.

## What the A/B has actually produced

Four live runs against Tiel-Coder-35B, roughly two hours of GPU time, and **no
usable solve-rate signal from any of them.** Recorded here so nobody spends the
same two hours discovering it again:

| run | result | why it says nothing |
|---|---|---|
| ladder x1 | control 8/10, gateway 7/10 | the verify seam was unwired, so `ask` refused every "what is failing" while control had a shell |
| ladder x3 | control 23/30, gateway 20/30 | **9 of 10 tasks scored identically on both arms every round** — the whole gap was one task control itself only solves half the time |
| engine x2 | control 3/8, gateway 0/8 | 37% of `ask` calls refused; the classifier, not the design, was being measured |
| engine x2 | no data | Tiel degraded to 11 tok/s, requests timed out, autoheal restarted it mid-suite |

**The instrument is the problem, not the measurement.** Tiel-35B solves 7 of the
10 ladder tasks every time on both arms and never solves 2 of them on either, so
there is almost nothing left for a difference to move. A ladder that cannot
discriminate cannot answer "does one door beat a menu", however many times it is
run.

Solve rate is therefore **deliberately unmeasured**. Re-running the live A/B as
it stands will produce another version of the table above. What would change
that, roughly in order of value:

1. **A ladder with harder tasks** — ones the model solves sometimes, not always
   or never. Everything else is a coin flip or a constant.
2. **A smaller model.** Small models are the actual target of this design, and
   they have room to be helped or hurt. (Only Tiel is run on this box today.)
3. **Dropping `run_verification` from the gateway arm**, so the reduction fires
   on every verification instead of the ~2% of `ask` calls it currently reaches.

Until one of those exists, the honest measurements are the three above:
**routing** (100% on 27 cases), **reduction** (30% retained on real cargo
output), and **captured needs** (18/18 handled). All three are model-free.

## Status

The gateway is not wired into the live agent loop. `sc-core` gained a generic
`ExternalTool` seam that defaults to `None` and has no dependency on
`sc-gateway`; the eval supplies the implementation. Nothing ships until the
numbers say it earns its place.
