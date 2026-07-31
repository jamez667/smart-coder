# 07 — Roadmap & milestones

The roadmap is sequenced so that the **hardest small-model risks are de-risked
first**: getting valid tool calls out of a tiny model, and keeping its context
under control. Everything else builds on a working, reliable single-step loop.

## M0 — Skeleton & gateway (walking skeleton)
**Goal:** prove the plumbing end-to-end with the dumbest possible loop.
- Cargo workspace + crate boundaries ([01](01-architecture.md)).
- `ModelBackend` trait + **OpenAI-compatible adapter** and **Ollama adapter**
  ([02](02-model-backends.md)).
- `smart-coder doctor`: backend reachable, model present, context budget printed.
- Trivial loop: prompt → model text → print. No tools yet.
- **Exit criteria:** can chat with Gemma 4 E4B via both backends from the CLI.

## M1 — Reliable tool calls (the core risk)
**Goal:** a small model issues *well-formed* tool calls, reliably.
- Tool Registry + strict schemas ([04](04-tools.md)).
- Read-only tools first: `read_file`, `list_dir`, `search_code`.
- Capability-driven tool-call strategy: native function calling / JSON-schema /
  **GBNF grammar** (add llama.cpp adapter here) / prompt+parse+repair
  ([02](02-model-backends.md)).
- Single-turn ACT→OBSERVE with the repair loop.
- **Exit criteria:** ≥95% valid tool calls on a small fixed task suite; malformed
  calls always recovered or escalated, never acted on.

## M2 — Context discipline ✅
**Goal:** keep a tiny window useful across many turns.
- ✅ Context Manager (`sc-context`) with hard budget + prioritized zones, eviction
  lowest-first, sacred zones never dropped ([05](05-context-management.md)).
- ✅ Retrieval index (`sc-index`): tree-sitter (Rust + Python) symbol graph +
  **PageRank repo map** with task/in-play boosts; `find_symbol` tool.
- ✅ Observation truncation (head+tail, error-prioritized, flagged) + rolling
  extractive history summary.
- ✅ Token accounting via the gateway tokenizer (`count_tokens`) with a heuristic
  estimator fallback.
- ✅ Wired into the agent loop, replacing the clone-everything prompt.
- **Exit criteria:** ✅ a multi-turn run with whole-file observations every turn
  provably stays under an 8k budget (`sc-core` integration test); the assembled
  prompt is inspectable via `BuiltContext`.
- *Deferred to a follow-up:* per-step (vs. per-run) repo-map refresh; embedding
  retrieval; lexical chunk search beyond `search_code`; more tree-sitter
  grammars.

## M3 — Editing & TDD verification (closing the loop) ✅
**Goal:** the agent actually changes code and *proves* it via tests.
- ✅ Mutating tools: anchored `edit_file` (exact `old_str`→`new_str`, refused on
  0/>1 matches), `create_file`; an edit journal records before/after for diff +
  rollback (the single apply-and-record path).
- ✅ `run_command` + `run_verification` with **structured per-test results**
  (`sc-verify`: cargo + pytest parsers, generic exit-code fallback), behind the
  permission layer ([04](04-tools.md)).
- ✅ **TDD loop:** the whole-suite gate refuses `finish` while the suite is red,
  feeding the failing cases back ([11](11-testing-and-tdd.md), [03](03-agent-loop.md)).
- ✅ Frozen contract-test protection (`PermissionPolicy` denies edits to approved
  test paths at the tool layer) + shell denied-by-default + workspace sandboxing
  ([04](04-tools.md)).
- **Exit criteria:** ✅ a scripted run drives a failing `sh` test red→green on a
  sample repo without breaking the suite or weakening the frozen test
  (`sc-core` `tdd_loop` integration test); cheat-edits and red-suite finishes are
  both rejected.
- *Deferred:* interactive `[y/n]` confirmation prompt for `Confirm`-gated calls
  (CLI/M5); verify-red-*first* as an explicit harness-run pre-check (the loop lets
  the model run it); more test-framework parsers (jest, go test, …).

## M4 — Planning & recovery ✅
**Goal:** survive multi-step tasks and the model's own mistakes.
- ✅ Planner: decompose into a short ordered step list; harness-owned `PlanState`
  (status per step, retry counter), rendered as compact structured state.
- ✅ Loop/stall detection (action-hash repeats + no-progress counter) + per-step
  retry budget + global step budget; structured `StopReason`.
- ✅ `update_plan` + `ask_user` meta-tools (+ existing `finish`).
- ✅ **Escalation = "junior asks senior"** (spec 02): on a stall or `ask_user`,
  consult a larger *advisor* backend for a one-line nudge (advice, not the fix);
  the junior keeps doing the work. No advisor → clean `Escalated`/`Stalled` stop.
- **Exit criteria:** ✅ recovers from induced failures (bad edit, repeated action)
  without human rescue, breaks loops via an advisor nudge, and escalates cleanly
  with no advisor (`sc-core` `recovery_loop` integration test).
- *Deferred:* automatic step-completion detection (the harness renders the plan
  and runs the retry budget, but advances steps only via the model's
  `update_plan` / on retry-exhaustion, not by inferring which call satisfied a
  step); per-step token/wall-clock budgets; re-running the planner mid-task.

## M5 — UX, replay & polish ✅
**Goal:** pleasant, inspectable, scriptable.
- ✅ **Event-stream architecture** ([01](01-architecture.md)): typed `AgentEvent`s
  emitted through an `EventSink` at every phase — the hub all observers consume.
- ✅ Live event rendering + plan panel + honest stop lines — delivered as **two**
  renderers over the one stream: a **full-screen TUI** (`sc-tui`, ratatui,
  `smart-coder run`) and a **local web dashboard** (`sc-web`, `tiny_http` + browser,
  `smart-coder serve`). Both are ahead of the spec's line-oriented plan / the
  daemon-mode-out-of-scope note ([06](06-cli-ux.md)) — pragmatic given the event
  stream made a second renderer cheap. Both verified driving real Gemma 4 E4B.
- ✅ One-shot `run` mode (`run <task> [--verify CMD] [--plan]`).
- ✅ **`--json` line output** — a `JsonLinesSink` over the event stream; headless
  `run --json` emits NDJSON on stdout (human notes to stderr), scriptable.
- ✅ **Session logging + `replay`** — every `run` tees its event stream to
  `.smart-coder/sessions/<id>.jsonl` (override with `--log`); `replay <id>`
  re-renders a past run from the log (`AgentEvent` round-trips Serialize↔Deserialize).
- ✅ **`--dry-run`** — preview only: read-only tools run for real, every
  side-effecting tool (edit/create/run_command/run_verification) is short-circuited
  to a `[dry-run]` note; the workspace is never touched.
- ✅ **`--yolo` / `--allow <prefix>`** — wired into `AgentConfig.permission`
  (`PermissionPolicy.allow_shell` / `shell_allowlist`).
- ✅ **`--verbose` / `-v`** — emits the fully-assembled, budgeted prompt each turn
  as `AgentEvent::PromptAssembled` (gated; large payload off by default), so a
  renderer/log/replay shows *exactly* what the model saw (spec 05/06). The TUI
  shows a compact marker live; `--json`/`replay` carry the verbatim text.
- **Exit criteria:** ✅ a newcomer can install, `doctor`, and run a task guided only
  by CLI output; live runs are logged and replayable. Proven live against the
  containers: `run --json` emits valid NDJSON ending in `Stopped`, `replay`
  reconstructs the run (incl. advisor exchanges and, with `--verbose`, the exact
  per-turn prompt), and `--dry-run` leaves the workspace byte-for-byte unchanged.

## M6 — Staged workflow & human checkpoints
**Goal:** drive tasks through the gated pipeline ([09](09-workflow-and-checkpoints.md))
so mistakes are caught before code is written. Runs with the single-agent core;
its final phase becomes the swarm's input (M7).
- ✅ Phase engine: specs → architecture → layout → test-first stage breakdown →
  work decomposition. **Five phases, not six** — the separate implementation-plan
  phase is folded into the stage breakdown, which carries the per-stage steps
  ([09](09-workflow-and-checkpoints.md) records why, and when it's worth revisiting).
  - *Limitation:* the breakdown is **test-first only on Python**; other stacks get
    the ordered steps but no frozen-test contract (spec 09).
- ✅ Durable phase artifacts on disk, resumable across sessions: one Markdown file
  per phase plus `state.json`, in `specs/<slug>/` (OpenSpec names) or the numbered
  `.smart-coder/plan/`. Both front-ends resolve the directory through one engine
  helper, so a Build resumes the design a prior Breakdown approved.
  - *Deferred:* artifact **versioning** — saves overwrite in place, so a send-back
    discards the prior draft, and the workflow never commits. Ordinary version
    control covers the reviewable-diff intent day to day, which is why this hasn't
    bitten ([09](09-workflow-and-checkpoints.md) records it as not built).
- ✅ Checkpoint gates: approve / revise / send-back (incl. to earlier phases) /
  abort ([06](06-cli-ux.md)); harness-enforced — the runner, not the model, applies
  each decision, and send-back invalidates downstream. Both front-ends implement
  one `Gate` trait over the shared loop: the CLI reads stdin; the GUI resolves a
  send-back from PR-style line comments, whose *placement* picks the target phase.
  The GUI deliberately drops **revise** — commenting supersedes hand-editing the
  artifact — while the CLI keeps it, where an editor is the natural surface.
- **Adaptive ceremony + configurable gate set.** ✅ The gate set is configurable
  (`--ceremony minimal|standard|full`, `--gates …`), applied by a `CeremonyGate`
  the runner can't tell from any other. ⬚ Nothing *adaptive* is built: no scope
  heuristic picks a tier (it defaults to full), and no phase ever collapses — a
  tier changes which phases **gate**, never how many run.
- **Exit criteria:** ✅ a real task goes from a one-line request through all five
  gated phases to an approved, test-defined work decomposition, with send-back
  correctly invalidating and regenerating downstream artifacts (`sc-workflow`
  runner tests). ⬚ The adaptive half of the ceremony bullet is unbuilt, so this
  milestone is **not** closed.

## M7 — Orchestration & the worker swarm (core landed)
**Goal:** scale out — many tiny workers on one codebase under a larger
orchestrator ([08](08-orchestration-and-swarm.md)). Each worker *is* the M0–M4
agent loop, unchanged; the swarm is a coordinator above it.
- ✅ Orchestrator + worker backends via the gateway (separate profiles/endpoints).
- ✅ **Task board** (subtask DAG, status, deps) + **model-driven decomposition**
  into independent subtasks (JSON, parse/repair, fallback to one subtask).
- ✅ **Bounded-concurrency scheduler** running independent subtasks in parallel
  *waves*, dependency-ordered.
- ✅ **Isolation = scratch-copy-per-worker** (the chosen first cut): each worker
  runs in an isolated copy and returns a *proposed* diff; never touches mainline.
- ✅ **Serialized integration** — proposals applied one at a time, each gated by
  **integration verification**; a change that breaks the suite is reverted and the
  subtask marked failed (parallel intelligence, serialized writes, spec 08).
- ✅ **Failure containment** — a derailed worker damages only its scratch copy; its
  proposal is rejected, never corrupting the result.
- ✅ Swarm event stream (`Decomposed`/`WorkerStarted`/`WorkerFinished`/`Integrated`/
  `SwarmDone`) for inspection + a future multi-worker dashboard.
- **Exit criteria:** ✅ a task decomposing into multiple subtasks (incl. a
  dependency) is completed by parallel workers and integrated green; a
  suite-breaking proposal is reverted (`sc-swarm` `swarm_run` + orchestrator tests).
- ✅ **CLI/dashboard surfacing of swarm state** — the `SwarmEvent` stream renders
  two ways over one source: the `sc-web` swarm dashboard *and* a line-oriented CLI
  view (`swarm --cli`: task board · which worker on which subtask · integration
  accept/reject), plus `swarm --json` NDJSON parity (`SwarmEvent` round-trips
  Serialize↔Deserialize) — mirroring M5's `print_event` ([06](06-cli-ux.md)).
- ✅ **Driven live against the real multi-model swarm** (orchestrator + two E4B
  workers): parallel workers complete and integrate green; the run is reported
  done **only after a final whole-suite integration verification** passes (spec 08
  step 5) — closing a live-found gap where a partial fix could be reported done
  over a red suite (honest stop, [06](06-cli-ux.md)).
- ✅ **Subtask retry on partial/rejected integration** (spec 08 "Subtask retry") —
  a subtask is `Done` only when its **own** scoped tests pass, not merely when the
  cumulative "didn't make it worse" gate accepted it. On an incomplete (or rejected)
  proposal the orchestrator re-dispatches the *same* subtask with a feedback-augmented
  prompt (still-failing test names + assertion messages + the merged file) under
  `max_subtask_retries` (default 2; `0` = the prior no-retry behaviour), each attempt
  scratch-isolated and gated exactly like the first; on exhaustion the subtask is
  `Failed` (not `Done`) with the residual failures as the reason and dependents block
  via quiescence. Visible as `SwarmEvent::SubtaskRetry { attempt, max, failing_tests }`
  ("↻ retry 1/2 — N tests still red"). The swarm now *recovers* a partial fix instead
  of stopping honestly-but-red; M4's per-step retry is the single-agent precedent.
- ✅ **CLI reaches the precise scoped check** — a free-text `swarm <task>` run now
  freezes the test oracle too: `--frozen a.py,b.py` sets the contract-test paths
  explicitly, and when omitted the CLI **auto-detects** them (`test_*.py`, `*_test.py`,
  anything under `tests/`). This drives the per-subtask scoped completion check (vs. the
  coarse whole-suite-delta fallback) and stops a worker from rewriting a test to make it
  "pass" — previously only the staged-workflow path (which knows its Phase-4 tests) got
  this; the ad-hoc path silently fell back. Three swarm prompts were also fixed (found
  live): the over-verbose decomposer that returned empty, missing `/no_think` on
  propose/merge (Qwen3 wrote its reasoning into the file), and the decomposer creating
  test-editing subtasks.
- ✅ **Advisor escalation before the final retry** (spec 08 / spec 02 "junior asks
  senior") — when a subtask's last retry is about to run, the orchestrator consults
  the configured advisor via `sc_core::consult` for a one-line nudge (advice, not the
  fix) and folds it into that attempt's worker prompt. Once per subtask, only on the
  final attempt (a subtask that recovers earlier never pays the senior call), and
  strictly optional (no advisor → clean no-op, the retry still runs). Visible as
  `SwarmEvent::AdvisorConsulted { subtask, advice }` ("⚑ asked senior — …").
- *Deferred (vs. spec 08):* git-worktree isolation + branch merges (we propose
  diffs instead); conflict *arbitration* by the orchestrator (we reject+reassign);
  the serialized shared-workspace lease fallback; specialized worker roles.

## M8 — Windows client (flexible)
**Goal:** the capable desktop client — same Rust core, full tools, flexible
backends ([12](12-platform-clients.md)).
- ✅ Desktop shell (CLI per [06](06-cli-ux.md); `sc-win` GUI) for
  `x86_64-pc-windows-msvc` — an iced app with chat, file tree, code view, git diff,
  line comments, terminal, and the plan panel.
- ✅ Flexible backends (Ollama / llama.cpp / OpenAI-compat) incl. up to the 12B
  ceiling, so this client can act as the **T1 orchestrator** ([02](02-model-backends.md)):
  plain / native-tool-calling / GBNF-constrained modes, with a separate orchestrator
  connection wired into the staged workflow.
- ✅ Full filesystem + shell with the permission layer ([04](04-tools.md)) — the
  GUI surfaces the confirmation itself (Allow / Deny / remember-prefix) and
  fails **closed**, denying rather than hanging if the UI is gone.
- **Exit criteria:** completes a real multi-file TDD task on Windows.

---

## M9 — Compliance evidence ✅
**Goal:** turn the tree-sitter index and verification machinery into a second
product surface — auditing a repository against regulatory frameworks
([13](13-compliance-evidence.md)).

The thesis is a reversal of the rest of this roadmap: **no model runs during an
audit.** Roughly 70% of code-relevant compliance controls are mechanically
decidable, and for those an LLM makes the output strictly worse — non-reproducible
and unciteable, which is disqualifying when the deliverable *is* an audit trail.

- **`sc-comply`** — one engine, frameworks as **TOML packs**. Ten shipped (SOC 2,
  ISO 27001 Annex A, NIST SSDF, SLSA+SBOM, CIS v8, PCI DSS v4, NIST 800-53,
  HIPAA, GDPR, EU NIS2/DORA/AI Act): 110 controls, 193 checks. Adding a framework
  is authoring, not engineering.
- **The honesty properties**, which are the design rather than a feature list:
  `Unknown` is a first-class status; there is no headline compliance percentage;
  weighted scoring divides by *observable* weight so a codebase is not penalised
  for the tool's blind spots; pack-driven shell commands are off by default.
- **`sc-comply-author`** ([14](14-pack-authoring.md)) — 16 deterministic lints
  over pack authoring. It found five real defects in the shipped SOC 2 pack on
  its first run, and one false positive in itself. A Gemini drafting path
  proposes checks for new frameworks; every draft is validated, linted and
  marked `# DRAFT` before a human sees it.
- **The drafting eval** ([15](15-compliance-eval.md)) — twelve real controls
  weighted toward the traps, graded deterministically by the lints plus
  hand-written labels. It measures *honesty under temptation*: does a model
  refuse to invent evidence for a control no repository can settle? Dishonesty
  scores zero, not a partial deduction.
- **Surfaces:** a live dashboard with a framework selector (`sc-web`), a lint
  gate (`comply-lint`, non-zero on a blocking finding), and a **redacted** static
  export for GitHub Pages (`comply-export`) — redaction is structural, and the
  renderer panics rather than publish citations.
- **Exit criteria:** ✅ audits this repo against all ten frameworks with zero
  collector errors; every shipped pack lints clean; the self-critique test
  enrolls new packs automatically.

**Deliberately not built:** the judgment collector — an LLM for controls regex
cannot express ("is authorization enforced on *every* admin route?"). Its
authority is already settled: **`Gap` or `Unknown`, never `Pass`**, enforced in
the collector rather than the pack so an author cannot route around it.

---

## M10 — Remote task intake & the background runner
**Goal:** file a task from a phone, come back to a drafted spec — without the
code or the model ever leaving the developer's machine
([18](18-task-intake.md), [19](19-queue-and-runner.md), [20](20-remote-review.md)).

This is the concrete form of the **bounded autonomous mode** idea that was carried
in the future-ideas list until this milestone, and it resolves that idea's tension
with
[00](00-overview.md)'s human-in-the-loop non-goal — since amended to "no unattended
**approval**" — by reading it precisely: it protects the *gate*, not the *uptime*.
The runner
executes phases unattended and **parks at every gate it reaches**. No
self-approval, under any ceremony, behind any flag.

- **`sc-daemon`** — a durable on-disk queue and a serial runner, the first thing
  in the workspace that outlives its launching process. Everything today is
  thread-scoped: `Session::spawn` dies with the GUI, `sc-web serve` exits when the
  run drains, and the remote mirror attaches to a session it does not own.
- **A third front-end over `sc-workflow`**, not a second pipeline. It resolves
  artifact directories through `sc_workflow::artifact_dirs`, so a phone-filed task
  and a desktop session land in the same `specs/<slug>/` — which is what makes a
  run startable on a phone and finishable on the desktop with no handoff step.
- **The trust boundary is the design** ([18](18-task-intake.md)): the web surface
  holds no workspace, runs no model, and has no `sc-workflow` dependency at all.
  It enqueues and it renders. Auth reuses `sc-web`'s proven token posture —
  per-launch bearer token, `?k=` on reads, `Authorization` on writes, loopback
  bind with `tailscale serve` terminating TLS.
- **Agent choice is a named profile**, never a URL/model/key form
  ([02](02-model-backends.md)). A credential field on a network-reachable page is
  a stolen credential, and a free model field is the frontier-model escape hatch
  [00](00-overview.md) refuses, wearing a web form.
- **Exit criteria:** ⬚ a task filed from a phone with the desktop closed produces
  an approved spec artifact; a parked run survives a daemon restart; a run started
  on the phone is finishable on the desktop; budget exhaustion fails a run without
  a human present.

**Deliberately not built:** concurrent runs (one local model server is the
bottleneck, so concurrency buys contention), cross-run scheduling or priorities,
and inline artifact editing on mobile — send-back with a note is the phone-shaped
corrective ([20](20-remote-review.md)).

**Depends on M6**, which is not closed: the adaptive half of the ceremony work is
still unbuilt. The runner needs the gate set, not the adaptive tier selection, so
this is not blocked — but it builds on an open milestone rather than a finished one.

---

## M11 — Post-integration review (engine + CLI landed)
**Goal:** a second gate over the integrated diff, asking what the suite cannot —
*should this code stay?* rather than *does it work?* ([16](16-post-integration-review.md)).

Green is a floor, not a finish line. A small worker can go green by duplicating a
helper it never found, swallowing an error to make an assertion pass, or making
tangential changes nobody asked for. Every one of those is green, and every one is
a defect a reviewer would have caught. The bet is sharpened by M7's own shape: a
swarm worker gets its subtask and the text of its own files and nothing else, so
"I couldn't find the existing helper" is not a lapse but the *expected* behaviour
of a correctly-working worker.

- ✅ **`sc-review`** — engine only (no CLI, no UI), mirroring `sc-verify`/`sc-comply`.
  Four **lenses**, each a separate call with one question, run in parallel:
  duplication, error handling, abstraction fit, unrelated changes.
- ✅ **Grounding is retrieved, not hoped for** — the part most easily skipped and
  the reason the gate works at all. Every lens gets the PageRank repo map the
  worker never had (`sc-swarm` had no `sc-index` dependency); duplication
  additionally gets pre-retrieved lookalike symbols, so the model is asked the
  part only it can do — *is this the same thing?* — rather than *does something
  like this exist?*, which the index answers better.
- ✅ **The authority constraints**, which are the design rather than a feature
  list: review never rewrites code (a finding is evidence handed to a decision,
  never an edit), and **only a corroborated finding may block or feed a retry**.
  Reviewer agreement ranks a finding; it never promotes an opinion to a fact.
- ✅ **Anchoring to hunk + symbol**, with the line as a render hint only —
  findings are never matched or identified by line number, and a named symbol the
  index cannot resolve drops in rank as a cheap hallucination check.
- ✅ **Retry carries evidence, not verdicts** — "`format_date` already exists at
  `src/utils/date.rs:41`, import it", never the model's prose summary. It shares
  the *existing* `max_subtask_retries` budget; two independent budgets multiply
  into a run that never terminates.
- ✅ **Green tests + a failed review never fail a subtask.** The work is verified
  correct; discarding it over an unfixed finding is the worse outcome. The subtask
  is `Done` with findings attached, and the run stops at a checkpoint if any meet
  the gating severity. Headless, it completes and reports them loudly.
- ✅ **Events + CLI** — `ReviewStarted`/`ReviewFinding`/`ReviewFinished` on the
  swarm stream (round-tripping like the rest, so `--json` and replay hold), and
  `--review` / `--review-action` / `--review-gate`. Off by default, and skipped
  below a diff-size threshold.
- **Exit criteria:** ✅ an uncorroborated finding can never block; a corroborated
  one produces a retry prompt naming the symbol *and* its location; two models
  flagging different problems in one hunk stay two findings; an unreachable
  reviewer is skipped, not fatal. All host-tested against scripted backends — no
  test needs a live model.

**Not built yet, and named as such:** the **multi-model panel** (the types carry
`raised_by`/`considered_by` from the start so it drops in without reshaping them,
but it needs *named connections* first — the connection model is still a fixed
`Local`/`Gemini` pair with one provider per stage), and the **desktop surface**
that renders findings as line comments on the subtask's diff
([12](12-platform-clients.md)). `sc-win` therefore leaves review off rather than
paying for calls it cannot yet show properly.

---

## M12 — Spec traceability ✅
**Goal:** make the drift that has bitten this project repeatedly fail a build
instead of waiting for someone to read two documents side by side
([17](17-spec-traceability.md)).

The evidence was already in the repo: the pipeline ran on five phases while every
spec described six; `ThinkPolicy` carried a dead array slot sized for the phase
that no longer existed; `sc-cli` printed "6 phase artifacts" while writing five.
None of it was *wrong code*, so no test caught it. The commitment is **drift is
detected by a machine, not by remembering to look** — and the machine is boring,
deterministic and model-free, which is what lets it run every time.

- ✅ **`sc-trace`** — engine only, mirroring `sc-verify`/`sc-comply`. Anchors are
  HTML comments in the prose (`<!--@ sc_workflow::Phase::ALL len=5 -->`), so specs
  stay readable for their primary audience and the checker reads only the anchors.
- ✅ **Honest statuses**, borrowed wholesale from [13](13-compliance-evidence.md):
  `BROKEN` (the anchor names what is gone), `STALE` (it resolved and the assertion
  is false), `UNGOVERNED` (a crate no spec claims), `UNKNOWN` (the *checker* could
  not look). `UNKNOWN` is never coerced into a pass, and there is no headline
  score — the missing few percent is where the drift is.
- ✅ **`len=N` checks two things**, which is the amendment implementation forced:
  element count *and* declared array length. Checking only the former would have
  reproduced the dead-slot bug rather than caught it, since a spec agreeing with
  the wrong declared length reads as clean. Counting is a targeted parse, kept out
  of `sc-index`'s shared query so the repo map and `find_symbol` are not degraded
  to serve one consumer.
- ✅ **Resolution refuses to cry wolf.** A false `BROKEN` is how a gate gets
  deleted, so only the crate segment may reject (it maps to a manifest member,
  verifiably); ambiguous names, unindexable crates and re-exported module paths
  all yield `UNKNOWN`. A symbol absent from its named crate but present elsewhere
  is `BROKEN` *and the message says where it went*.
- ✅ **Coverage at crate granularity** — narrowed from the spec's "crate and
  top-level module" on measurement: crates give 2 findings here, modules give
  dozens, and the spec's own "a noisy check gets `--no-verify`'d" warning decides
  it. Whole-token matching only, so "run tests, iterate" does not govern a crate
  by accident.
- ✅ **`smart-coder trace [--check] [--json]`**, gated in **both** `scripts/check.sh`
  and `scripts/check.ps1`. `--check` fails on `BROKEN`/`STALE` only.
- **Exit criteria:** ✅ every anchor in `docs/specs/` resolves (0 broken, 0 stale,
  0 unknown); breaking one fails the gate with a message naming the file, line and
  reason; a dead array slot is caught whatever the spec claims. The tool checks
  itself — a test asserts this repo has no broken anchors, and writing spec 17's
  own prose about malformed anchors briefly produced one, which it caught.

**Deliberately not built:** module-granularity coverage (noise, per above), and
any caching — sources are re-read per anchor, which is ~2s over this workspace and
noise beside the `cargo test` gate it sits next to. The `spec-guardian` agent is
unchanged and stays the semantic layer above this: it reads meaning anchors cannot
capture, and this removes the load-bearing cases from its shoulders.

---

## Post-v1 / future ideas
- **User-defined tools** via config.
- **Heterogeneous swarms** — specialized worker roles (searcher/editor/tester/
  integrator) mapped to different small models, beyond the M7 baseline
  ([08](08-orchestration-and-swarm.md)).
- **Embedding-based retrieval** with a small local embedder (optional).
- **TUI** (v2 interface).
- **LoRA/adapter experiments** — light task-specific tuning of the small model.
## Cross-cutting throughout
- **We dogfood TDD.** Every milestone lands with unit tests for its components,
  written test-first ([11](11-testing-and-tdd.md)) — the harness that drives
  tiny models red→green is itself built red→green. Unit tests are part of each
  milestone's definition of done.
- A **fixed task suite** (sample repos + graded tasks) as the regression
  benchmark; tracked from M1 so harness changes are measured against real
  small-model behavior.
  - **SWE-bench is the post-M3 feasibility check, not a current target.** Our
    `sc-eval` red→green machinery already mirrors SWE-bench's
    `FAIL_TO_PASS`/`PASS_TO_PASS` split, but three preconditions must land first
    or a run measures missing infrastructure, not the model: **(1)** per-task
    environment isolation (Docker images with pinned deps); **(2)** the retrieval
    index + context budgeter (M2 / `sc-index`) so a 4B model can navigate a large
    unfamiliar repo; **(3)** structured `run_verification` with pytest parsing
    (M3, [04](04-tools.md)). Sequence: a `sc-eval` SWE-bench *adapter* + a tiny
    pure-Python Docker subset once M2/M3 are in, then **SWE-bench Lite/Verified**
    as the real benchmark. Expect low absolute scores — purpose-built 7B coders
    sit ~18–23% ([10](10-prior-art.md)); the value is the *relative* signal across
    harness changes, not the headline number.
- Determinism/replay maintained at every milestone for debuggability
  ([03](03-agent-loop.md)).
