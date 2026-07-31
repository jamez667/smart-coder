//! The `help` text. One `&'static str` — kept in its own file so the surface it
//! documents stays readable as a whole, and so edits to it never collide with
//! parser changes.

/// Usage text (spec 06 — invocation modes, trimmed to the M0 surface).
pub fn usage() -> &'static str {
    "\
smart-coder — an agentic coding tool for small models (M0)

USAGE:
    smart-coder [COMMAND] [OPTIONS]

COMMANDS:
    chat            Interactive chat with the model (default)
    run <task>      Run a coding task in the current dir with a live TUI
    serve <task>    Run a task and watch it in your browser (web dashboard)
    swarm <task>    Decompose + run with parallel workers (swarm dashboard)
    plan <task>     Staged planning workflow → specs/<slug>/ (spec 09)
    staged <task>   Plan + BUILD via the staged decomposition engine (JSON stream)
    replay <id>     Re-render a recorded session from its log (spec 06)
    comply          Audit this dir against a compliance pack; serve the evidence
                    pack as a dashboard (spec 13). See --pack.
    comply-lint     Critique a compliance pack's own authoring (spec 14): unsafe
                    on_no_files, unreachable patterns, over-claiming controls.
                    Deterministic — no model involved. See --pack.
    comply-export   Audit every framework and write a static REDACTED HTML site
                    for publishing (GitHub Pages). File paths, line numbers and
                    excerpts are withheld. See --out.
    comply-eval     Grade models on compliance drafting honesty (spec 15).
                    Repeat --author-model to compare, e.g.
                    --author-model gemini-pro-latest@https://…/v1beta/openai
                    --author-model qwen3-coder-30b@http://localhost:11435/v1
    queue ACTION    The task queue (spec 19): file a request against any configured
                    repository, draft its spec, approve or send it back. Actions:
                      file TEXT --repo NAME [--kind K]
                                              file a request. K is bug, feature
                                              (default), improvement or feedback.
                                              The first three draft a spec; feedback
                                              is just kept — no model, no repo write.
                      list                    show the queue
                      run                     draft queued tasks (Ctrl-C is safe)
                      show ID                 print a drafted spec
                      approve ID              settle the spec; starts nothing
                      send-back ID NOTES      redraft it, with a reason
                      discard ID              drop a task
                      feedback [--repo N] [--all]
                                              show kept feedback
                      ack ID --repo NAME      mark feedback read (kept, not deleted)
                      repos                   what this daemon serves
                      add-repo NAME PATH      serve another repository
                      forget-repo NAME        stop serving one
    trace           Check the specs against the code (spec 17): anchors that no
                    longer resolve, assertions that are false, crates no spec
                    claims. Deterministic — no model runs. See --check.
    doctor          Check the backend is reachable; print effective config
    help            Show this message

OPTIONS:
    --base-url URL        OpenAI-compatible endpoint  [default: http://localhost:11434/v1]
    --model NAME          Model to use                [default: gemma4:e4b]
    --tool-calling MODE   none | native | gbnf — how the backend enforces tool
                          calls (spec 02)             [default: none]
    --verify CMD          Test command for `run` (enables the TDD whole-suite gate)
    --advisor MODEL       A larger model consulted when the coder stalls
                          (\"junior asks senior\", spec 02).
    --advisor-url URL     Endpoint for the advisor when it runs on a different
                          server than the coder (a swarm). [default: --base-url]
    --key TOKEN           Bearer token for the coder endpoint (e.g. a Gemini API key
                          for a hosted provider). Also read from GEMINI_API_KEY.
    --no-think            Append /no_think to the prompt (needed for Qwen3 models;
                          auto-applied when the model name contains 'qwen3').
    --pack NAME|PATH      Compliance framework for `comply`/`comply-lint`: a
                          shipped pack name (soc2, iso27001, ssdf, slsa, cis,
                          pci, nist-800-53, hipaa, gdpr, eu-regulatory) or a
                          path to your own. Defaults to soc2.
    --list-packs          List the shipped compliance packs and exit.
    --check               `trace` only: exit non-zero on a broken or stale claim
                          — the CI gate. `unknown` never gates (the checker could
                          not look), and an ungoverned crate warns rather than
                          fails. Pair with --json for machine-readable output.
    --no-token            Serve `comply` without a URL token — a plain
                          http://127.0.0.1:PORT/ link. Still loopback-only.
                          Do not combine with `tailscale serve`.
    --plan                Decompose the task into a plan before running (`run`)
  run output, logging & safety (spec 06):
    --json                Emit the event stream as JSON lines on stdout (no TUI).
                          With `trace`, emits the claim report as JSON instead.
    --log PATH            Write the session log here  [default:
                          .smart-coder/sessions/<id>.jsonl]
    --dry-run             Preview only: run read-only tools but never apply an edit
                          or run a command; the workspace is left untouched
    --verbose, -v         Show the full assembled prompt each turn (what the model
                          actually saw); full text in --json / the session log
    --yolo                Pre-approve all run_command shell calls
    --allow PREFIX        Auto-approve shell commands starting with PREFIX
                          (repeatable, e.g. --allow \"cargo test\")
  swarm / plan (workers use --base-url/--model):
    --cli                 Render the swarm to the terminal (task board · workers ·
                          integration) instead of serving the web dashboard. `--json`
                          implies this and emits one NDJSON SwarmEvent per line.
    --orchestrator MODEL  The model that decomposes/plans (the breakdown). For Gemini,
                          prefer the cheap/fast gemini-2.5-flash-lite. [default: --model]
    --orchestrator-url U  Endpoint for the orchestrator. For Gemini:
                          https://generativelanguage.googleapis.com/v1beta/openai
                                                                  [default: --base-url]
    --orchestrator-key T  Bearer token for the orchestrator/planner endpoint (the
                          Gemini API key). [default: --key / GEMINI_API_KEY]
    --max-workers N       Max parallel workers                    [default: 2]
    --review              Review each integrated diff before calling the subtask done
                          (spec 16): asks *should this code stay?* — duplication,
                          swallowed errors, abstraction fit, unrelated changes — after
                          the tests answered *does it work?*. Costs model calls on the
                          advisor backend, so it is off unless asked for.
    --review-action A     What happens to a finding: report (default, findings ride
                          along and the run succeeds) | gate (stop for a human) |
                          retry (re-dispatch the subtask with the evidence). Only a
                          finding a deterministic check agreed with can gate or retry;
                          an unconfirmed one is always report-only. Implies --review.
    --review-gate SEV     Severity at which a confirmed finding stops the run:
                          low | medium | high        [default: high] Implies --review.
    --interactive, --gate Halt at each `plan` phase boundary for a human checkpoint:
                          approve / revise / send-back / abort (spec 09). Default is
                          autonomous (auto-approve every gate).
    --ceremony TIER       Scale the ceremony to the task (spec 09): which phases stop
                          at a gate. minimal (final sign-off only) | standard (specs,
                          tests, decomposition) | full (every phase). Implies
                          --interactive.
    --gates PHASES        Precise gate set: a comma-separated list of phase slugs to
                          gate (e.g. specs,stage-breakdown). Overrides --ceremony;
                          implies --interactive.
  plan only — per-phase thinking (spec 09; default: think on the JSON phases,
  /no_think on the prose phases):
    --think-all           Think on every phase
    --no-think-all        /no_think on every phase
    --think PHASE         Force thinking on one phase (slug, e.g. layout)
    --nothink PHASE       Force /no_think on one phase

EXAMPLES:
    smart-coder doctor
    smart-coder run \"make the failing test in is_even pass\" --verify \"sh test.sh\"
    smart-coder run \"fix parse_config\" --json --verify \"cargo test\" > run.jsonl
    smart-coder run \"refactor the parser\" --dry-run
    smart-coder replay 1718000000000
    smart-coder trace --check
    smart-coder serve \"fix the bug in parse_config\" --verify \"cargo test\"
    smart-coder swarm \"add validation and a test\" --cli --verify \"python -m pytest -q\" \\
        --base-url http://localhost:11435/v1 --model coder-0 --max-workers 2 \\
        --orchestrator-url http://localhost:11434/v1 --orchestrator advisor-e4b
    smart-coder --model gemma4:e4b --tool-calling native"
}
