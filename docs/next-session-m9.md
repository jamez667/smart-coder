# Next session — M9: a native Windows vibe-coding desktop app

**Paste the block below as the opening prompt for the next session.** It's written
to be self-contained; it points at the authoritative specs and the current code so
the next session re-derives facts rather than trusting this summary.

---

Context: dumb-coder (Rust agentic coding tool for small models), at
`c:\Users\mail\working\Personal\dumb-coder`, on branch `main` (the repo's default;
the old `claude/agentic-coding-tool-specs-fp1oid` branch is kept and points at the
same commit). M0–M8 core is done; the swarm (M7) is complete including the
subtask-retry loop and advisor-escalation-before-final-retry. Everything is pushed.
Rust edition 2021, rustc 1.95.

Task: build **M9 — the Windows client**, as a **full, native, modern desktop
application**. The user was explicit: **a real native desktop app, NOT the existing
web dashboard and NOT a web-view wrapper.** It is a **vibe-coding** app:

> You type *intent* and watch the agent (and the swarm) do the work — **no code
> editor**, no manual file editing. You drive by describing what you want; the app
> surfaces the agent's progress, the plan / task-board, diffs, test results, and the
> human approval gates. Stay in intent-space; the harness does the editing. The look
> should be genuinely modern — this is a showcase of the "small model" thesis, so
> the app should feel polished, not a debug panel.

The Rust core, the `dc-cli` shell, flexible backends
(`dc_model::OpenAiBackend` for Ollama/llama.cpp/OpenAI-compat), Windows shell
selection (`cmd /C` in `crates/dc-verify/src/run.rs`), and the permission layer
already exist and have been built and live-tested on this Windows box throughout —
so M9 is **almost entirely new GUI work over a proven core**, not core work.

Read first (authoritative — follow them, don't trust this file where they differ):
- `docs/specs/12-platform-clients.md` § "Windows client (later)" — same Rust core,
  flexible backends, full tools; the GUI is the desktop shell (it says "GUI optional"
  — the user is electing the GUI as M9's centerpiece).
- `docs/specs/06-cli-ux.md` — the UX contract the app must honor: the honest stop
  line, the event stream, confirm-gated `run_command`, `--dry-run`, `--yolo/--allow`.
- `docs/specs/09-workflow-and-checkpoints.md` — the staged-workflow human gates
  (approve / revise / send-back / abort) the app must present as first-class UI.
- `docs/specs/01-architecture.md` — portable core + THIN shells; the GUI is a shell,
  logic stays in the core.
- `docs/specs/07-roadmap.md` M9 bullet.

Study these to bind the UI to existing data, NOT to copy the web UI's looks:
- `crates/dc-core` events — `AgentEvent` is the live data model (it's
  `Serialize`/`Deserialize`); `dc_swarm::SwarmEvent` is the swarm's. The app renders
  these. See how `dc-cli`'s `print_event` / `dc-swarm`'s `print_swarm_event`
  interpret every variant — that's the vocabulary of what to show.
- `crates/dc-cli/src/lib.rs` — the `Cli` struct IS the config surface (backends,
  models, advisor/orchestrator, verify command, `--max-workers`/`--max-retries`/
  `--frozen`, `--dry-run`/`--yolo`/`--allow`). The GUI is just another front-end
  producing the same config; mirror these fields as UI controls.
- `crates/dc-cli/src/main.rs` — how a run and a swarm are actually wired
  (`cli.backend()`/`cli.orchestrator()`, `swarm_config`, the sinks). The app calls
  the same `dc_core` / `dc_swarm` entry points on a worker thread and pumps events
  to the UI.
- `crates/dc-web` and `crates/dc-tui` — ONLY as references for *what* is worth
  surfacing (Hub/sink pattern, event→view mapping). Do not wrap or embed them; the
  user wants native.

**First decision to settle with the user before building (recommend, then ask):
the native GUI stack.** Options for a Rust-native Windows desktop app:
- **egui (via eframe)** — RECOMMEND for v0. Pure-Rust immediate-mode GUI, trivial to
  bind a live event stream to (poll the channel each frame), single `cargo build`
  produces a native `.exe`, no JS/Node/WebView dependency, runs great on Windows.
  Fastest path to a polished, *native* app; theming/modern look is very achievable.
- **iced** — pure-Rust, retained/Elm-style; cleaner for complex state, prettier
  defaults, steeper to wire a streaming backend into. Good if the app grows large.
- **Slint** — declarative `.slint` markup + Rust; most "designed" look out of the
  box; adds a markup language/build step.
- (Explicitly NOT Tauri/WebView — the user rejected web-based.)

Lead with **egui/eframe** and a concrete v0, get the user's pick, then plan.

Suggested v0 (confirm/trim with the user in plan mode):
- A new THIN shell crate `crates/dc-win` (cdylib? no — a `bin` producing the native
  `.exe`). Add to workspace `members`. Keep logic in the core (spec 01).
- One window, modern layout: an **intent input** (type what you want, submit), a
  **live activity view** that renders the `AgentEvent`/`SwarmEvent` stream as it
  arrives (steps, tool calls, retries, integration, the honest stop line), a
  **plan / task-board panel**, and a **diff + test-results panel** (read-only —
  there's no editor; show `dc_verify::TestReport` failure-first per spec 05/11).
- **Approval gates as first-class UI**: confirm-gated `run_command` and the staged
  workflow checkpoints (spec 09) become approve/revise/abort buttons, not a CLI
  prompt. This is the core interaction that makes it "drive a coding agent", not
  "watch a log".
- A **settings/connection panel** mirroring the `Cli` config: backend URL+model,
  optional advisor/orchestrator for swarm, worker/retry/frozen knobs, dry-run/yolo.
- Run the agent/swarm on a worker thread; communicate via a channel (an
  `mpsc`-backed `SwarmSink`/event sink) the UI drains each frame. The core already
  exposes sinks (`HubSink`, `FnSwarmSink`) — add a channel-backed one if needed.

Architecture must stay: portable core + thin shell. If something the GUI needs
isn't exposed by the core yet (e.g. pausing at a gate for a UI decision rather than
a CLI prompt), add it to the core as a clean seam (a callback/decision trait),
host-test THAT, and keep the GUI glue thin.

Working rules (unchanged):
- Plan mode for this — new milestone, real design fork (GUI stack + the gate-driving
  seam). Enter plan mode, settle the stack with the user, write a detailed
  spec/plan before coding.
- TDD the host-testable logic (the driver/session, config wiring, any new core
  seam for interactive gates); the GUI rendering glue is the thin, less-testable
  part — keep it thin so most logic stays tested.
- Gate every change: `cargo test` (touched crates) + `cargo clippy --all-targets --
  -D warnings` + `cargo fmt --all`. Python-based verify commands in tests, never
  `sh` (not on this Windows box). Two pre-existing `dc-core/tests/tdd_loop.rs`
  shell-based failures are NOT regressions (confirm with `git stash` if unsure).
- Live backends are Docker containers: advisor-e4b :11434, coder-0 :11435,
  coder-1 :11436 (`GET /v1/models` to check). Build and run the native app, plus the
  fresh `target/debug/dumb-coder.exe` for any CLI cross-check — not the stale
  `~/.cargo/bin` copy. Rebuild before a live run.
- A GUI app can't be proven by a unit test alone — plan to actually launch it on
  this Windows box and drive a real task through a live backend as the exit check
  (spec 06 honest-stop must hold in the UI). Screenshot/observe it working.
- Commit/push only when asked. Leave `grouped-query-attention.md` (untracked, stray)
  alone unless the user mentions it.

Confirm the GUI stack (egui vs iced vs Slint) and the v0 scope with the user before
building.

---

## Quick state notes (for the human, not the prompt)

- The user wants a **native modern desktop app**, explicitly not the web dashboard
  and not a WebView wrapper. egui/eframe is the recommended starting stack.
- Stray untracked file `grouped-query-attention.md` still sits in the repo root —
  unrelated to any milestone; decide commit/delete/ignore sometime.
- There is **no CI** (`.github/workflows/` absent). Separate easy add if ever wanted.
- The two `tdd_loop` failures are shell-portability (`sh`-assuming) tests; unrelated
  to M9.
