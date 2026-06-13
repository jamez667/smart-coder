# 03 — The agent loop

## Principle

A small model cannot be trusted to hold a long plan, reason many steps ahead, or
recover from its own mistakes. So the loop is built around **one decision per
model turn**, with the *harness* — not the model — owning planning structure,
progress tracking, recovery, and stopping.

## The cycle

```
        ┌──────────┐
        │   PLAN   │  decompose task into small, single-purpose steps
        └────┬─────┘
             │  (re-plan when reality diverges)
             ▼
        ┌──────────┐
   ┌──▶ │   ACT    │  build tight prompt → model picks ONE tool call
   │    └────┬─────┘
   │         ▼
   │    ┌──────────┐
   │    │ OBSERVE  │  execute tool, capture real result, record event
   │    └────┬─────┘
   │         ▼
   │    ┌──────────┐
   └────│  VERIFY  │  check progress / loop / budget; gate on build+test
        └────┬─────┘
             │  step done → next step; plan done → STOP
             ▼
        ┌──────────┐
        │   STOP   │  summarize diff, report, hand back to user
        └──────────┘
```

### PLAN
The orchestrator asks the model for a **short, ordered list of small steps**
(target: 3–8 steps, each a single concrete action like "find where X is defined"
or "edit function Y in file Z"). The plan is grounded in a retrieved repo
overview, not the model's imagination. The plan is data owned by the harness;
the model proposes, the harness stores and tracks it.

Re-planning is triggered by the harness when observations diverge from
expectations (a step fails repeatedly, a file isn't what was assumed, tests
reveal a wrong approach) — not left to the model to decide spontaneously.

### ACT
For the current step only, the Context Manager builds a minimal prompt
([05](05-context-management.md)). The model is asked for **exactly one tool
call** (see [04](04-tools.md)). Where the backend supports it, the output is
grammar/schema-constrained so the call is well-formed by construction; otherwise
a parse-and-repair loop runs.

Keeping it to one action per turn is deliberate: small models that try to plan
and act simultaneously tend to hallucinate multi-tool sequences that don't hold
together.

### OBSERVE
The harness executes the tool and captures the **real** result (file bytes,
command exit code + output, search hits). The result — truncated/summarized to
fit the budget — becomes the grounding for the next turn. The model never
proceeds on assumed output.

### VERIFY
Two layers:

1. **Cheap, every turn (harness-side, no model):**
   - *Loop / stall detection* — same tool + same args repeated, or N turns with
     no file/state change → break out, re-plan or escalate.
   - *Budget checks* — token / step / wall-clock / tool-call budgets.
   - *Sanity* — did the tool actually change what the step claimed?
2. **Gated, after edits:** run the project's build/test/lint (a verification
   tool, [04](04-tools.md)). Failures are fed back as observations and the step
   re-attempts; persistent failure triggers re-plan or a user prompt.

### STOP
The loop ends when: the plan completes and verification passes; a budget is
exhausted; an unrecoverable error occurs; or the user interrupts. On stop, the
harness summarizes what changed (diff overview) and reports honestly — including
partial or failed outcomes.

## State the harness owns (not the model)

- **Task** — the user's original request (always re-grounded, never paraphrased
  away).
- **Plan** — ordered steps with status (`pending` / `active` / `done` /
  `failed`).
- **Working set** — files/symbols currently relevant (drives retrieval).
- **Event log** — full ordered history of turns, calls, results, decisions.
- **Budgets & counters** — tokens used, steps taken, retries per step,
  wall-clock.

The model sees a *curated slice* of this each turn; it never holds the whole
state itself.

## Failure & recovery policy

| Situation | Harness response |
| --- | --- |
| Malformed tool call | Repair loop: re-prompt with the exact parse/schema error (bounded retries). |
| Tool error (e.g. file not found) | Feed error back as observation; let the model adjust once; then re-plan. |
| Same action repeated (loop) | Detect via action hash; force a re-plan or ask the user. |
| Build/test fails after edit | Feed failure output; re-attempt step; cap attempts, then escalate. |
| Step budget exceeded | Stop the step, mark `failed`, re-plan or surface to user. |
| Model output empty/garbage | Retry with lower temperature; if still bad, escalate to user. |

The unifying rule: **bad model behavior is expected and handled**, never acted
on blindly and never a crash.

## Determinism & replay

With a pinned seed and recorded sampling params, the event log is a replayable
transcript: same inputs → same decisions. This is the primary debugging tool for
"why did the agent do that?" and for regression-testing harness changes against
fixed model behavior.

## Human-in-the-loop (v1)

v1 is interactive. The user can: approve/deny risky tool calls
([04](04-tools.md)), interrupt the loop, edit or reject the plan, and answer
when the harness escalates. Unattended autonomy is explicitly future work
([07](07-roadmap.md)).
