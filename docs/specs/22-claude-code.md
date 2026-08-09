# 22 — Claude Code as a run kind

## Principle

`smart-coder` exists to prove that **a well-engineered harness extracts reliable
coding behaviour from a small model** ([00](00-overview.md)). This spec adds a
run kind that uses somebody else's harness and somebody else's frontier model.

That is a real tension, and it is worth naming before the design: the first
non-goal in [00](00-overview.md) is *no large/frontier-model support path* —
"the constraints are the product… we will not add 'just use GPT-4 for the hard
parts'." This spec **amends** that non-goal rather than quietly stepping around
it. The amendment is narrow, and the boundary is what makes it safe:

> Claude Code is a **peer surface**, not a tier. It never becomes a fallback the
> small-model path can escalate into, and no phase of the staged workflow may
> delegate to it. The harness thesis is tested by the harness running its own
> models; a user choosing to run Claude Code in the same window does not
> weaken that test, because the two never appear in one run.

The distinction that keeps this honest: **an escalation path would let a weak
result be rescued by a strong model, which would make the small-model evaluation
meaningless.** A separate run kind cannot do that. If the two ever need to mix,
that is a new decision requiring a new amendment, not an extension of this one.

## Relationship to the stated non-goals

An amendment stated in one direction is not an amendment — it is a spec
disagreeing with another spec. [00](00-overview.md) must carry this one, in the
same shape its other two amended non-goals already use.

**The non-goal should be narrowed** from *"No large/frontier-model support path"*
to keep its sentence about tuning and testing, and gain a clause:

> A user may run **Claude Code** as a separate run kind ([22](22-claude-code.md)),
> which is a peer surface rather than a tier — nothing escalates into it and no
> phase delegates to it, so the harness thesis is still tested by the harness
> running its own models.

What the non-goal still forbids, unchanged: a frontier model behind
`ModelBackend`, a tier that the swarm or the staged workflow can route work to,
and any path by which a failing small-model run becomes a succeeding large-model
one. Those are the things that would make the evaluation meaningless. Choosing to
run a different agent, in a window that also hosts this one, is not.

## Why a `RunKind`, not a `ModelBackend`

The obvious integration is wrong, and it is worth stating why so nobody tries it
later.

`ModelBackend` ([02](02-model-backends.md)) is a **single-turn completion seam**:
messages in, one assistant turn out, and the *harness* owns the loop — it decodes
tool calls, runs the tools, manages context, and decides when to stop.

Claude Code is not a model. It is a **complete agent** that owns its own loop,
its own tools, and its own file edits. Wiring it behind `ModelBackend` would put
two agent loops in charge of one workspace: smart-coder would parse tool calls
out of Claude Code's prose and run them a second time, having already been run.
The result is not a worse integration — it is an incoherent one.

So the seam is `RunKind` <!--@ crates/sc-win/src/session/mod.rs -->, which
already names seven strategies that each own their loop (`Agent`, `Swarm`,
`Tdd`, `SequentialBuild`, `Iterate`, `Plan`, `StagedBuild`). Claude Code is an
eighth. `Session` already spawns a run on a worker thread and streams `UiEvent`s
back to the UI; everything downstream of that channel — the activity feed, the
chat panel, cancellation, the phone mirror ([20](20-remote-review.md)) — works
unchanged, because it was never coupled to *how* the run produces events.

## The transport

Claude Code ships as a CLI that speaks newline-delimited JSON:

```
claude -p "<task>" --output-format stream-json --verbose
```

One JSON object per line on stdout: assistant turns, tool calls, tool results,
and a final result object. That is a stream of events, which is exactly the shape
`Session` already consumes.

Requirements:

- **Spawn through `proc::command`** <!--@ crates/sc-win/src/proc.rs -->, never
  `std::process::Command` directly. That helper sets `CREATE_NO_WINDOW` on
  Windows; the rule exists because subprocess spawns previously flashed hundreds
  of console windows during git polling, and a new spawn site that bypasses it
  reintroduces exactly that.
- **The workspace is the working directory.** Claude Code resolves paths and
  finds `CLAUDE.md` relative to cwd, so the run must start where the project is.
- **Read stdout line by line on the worker thread**, mapping as it goes. Buffering
  the whole run and parsing at the end would lose the live view, which is the
  point of having a UI at all.
- **A line that fails to parse is skipped, not fatal.** The output format is
  another project's and may gain fields or emit diagnostics; a run must not die
  because one line was unexpected. Skipped lines are counted and reported at the
  end rather than swallowed silently.

**Cancellation is a new obligation, not the existing path.** `Session::spawn`
<!--@ crates/sc-win/src/session/mod.rs --> passes its shared cancel flag to
`RunKind::Iterate` alone, and the flag is *cooperative*: it is checked at a turn
boundary. A subprocess has no turn boundary. This run kind must therefore take
the flag and translate it into **killing the child process** — and a cancel that
leaves an orphaned `claude` still editing files would be worse than no cancel
button, because the user would believe they had stopped it.

## Mapping to `UiEvent`

The existing vocabulary <!--@ crates/sc-core/src/event.rs --> covers this almost
exactly, which is the strongest evidence the seam is right:

| Claude Code stream | `UiEvent` / `AgentEvent` |
| --- | --- |
| assistant text | `Agent(ModelTurn { raw, .. })` |
| streamed text delta | `Agent(ContentDelta { cumulative, .. })` |
| tool use | `Agent(ToolCall { tool, arg })` |
| tool result | `Agent(ToolResult { summary, full, is_error })` |
| final result | `Done { ok, summary }` |
| spawn failure, non-zero exit | `Failed(String)` |

Two fields have no honest source and **must not be faked**: `prompt_tokens` (on
`ModelTurn`) and `prompt_budget` (carried once on `RunStarted`) describe
smart-coder's own context management ([05](05-context-management.md)), and Claude
Code manages its own. Both are reported as `0`, meaning *not applicable*, rather
than a guess — a plausible-looking token count that nothing measured is worse
than an obvious zero.

`Planned` is not emitted. Claude Code plans internally and does not surface a
step list in the same form; inventing one from prose would be a fabrication.

## Permissions and approvals

This is the design's one genuinely hard choice, because **both systems have an
approval model and they must not both be live.**

The rule: **smart-coder's gate surface owns approvals, or Claude Code does —
never both.** A run where the user answers a prompt in one system while the other
also believes it is gating is a system that cannot say what was approved.

Two supported postures, chosen per run:

- **Delegated (v1 default).** Claude Code runs with its own permission handling.
  smart-coder's approve/deny UI is **explicitly dark** for the run — not merely
  unused, but visibly not offered, so nobody reads its absence as "nothing needed
  approving".
- **Routed (later).** Claude Code's permission requests are surfaced through the
  existing `Pending::Confirm` <!--@ crates/sc-win/src/bridge.rs -->, which
  already carries a command plus a one-shot reply channel — the same path the
  agent's shell confirmations use. This is the better experience and the more
  work; it is deliberately not v1, because getting the delegated case honest
  matters more than getting the routed case first.

The **`Decision::Approve` gate rule from [09](09-workflow-and-checkpoints.md) is
untouched.** Claude Code runs are not staged-workflow runs; they have no phases
and reach no checkpoints. The non-goal "no unattended *approval*"
([00](00-overview.md)) is not weakened, because there is no smart-coder gate here
for a model to pass — the human is choosing to run an agent that asks its own
questions.

## Craft mode

**Craft mode refuses this run kind**, on exactly the reasoning that refuses the
remote mirror ([21](21-craft-mode.md)): Craft mode's promise is that *no language
model is contacted*, and Claude Code is unambiguously a model surface. Spawning
it would be a model arriving through a side door.

The refusal goes through `cfg.craft()`, the single predicate every other model
surface already consults, so it fires for the `craft-only` build automatically.
A test asserts it, alongside the existing Craft-mode refusal legs — health probe,
chat send, run start, remote mirror, and line-comment triage.

Note there is **no existing per-run-kind predicate to reuse.** `needs_model()` is
a `PanelKind` method covering only `PanelKind::Chat`, and no `RunKind` is
filtered by anything today — every one of the seven needs a model, so the
question has never been asked. This run kind introduces the first `RunKind` that
must be gated, so the gate is new code rather than a call to something that
already exists.

Where the refusal must bite:

- The run kind is **not offered in the UI** — not shown-and-disabled, since a
  disabled control implies a setting that could be enabled.
- The spawn path **refuses even if reached**, because a queued `Task` or a stale
  message can arrive after a mode switch.

## Availability

Claude Code is a separate install and **may not be present**. The integration
must be honest about that rather than failing at spawn time:

- **Detect once, at startup and on workspace change:** is `claude` resolvable on
  `PATH`? Cache the answer; do not probe per keystroke.
- **Absent ⇒ the run kind is not offered**, with a one-line explanation of what
  to install if the user goes looking. A menu item that always fails is worse
  than no menu item.
- **Present but failing to spawn** ⇒ `Failed` with the OS error, not a silent
  no-op.

This is the same shape as the Unity detection in [21](21-craft-mode.md): find the
tool, offer the button only when it is real, and say why when it is not.

## What this is not

Named so they are decisions rather than oversights:

- **Not a tier, and not a fallback.** See the Principle. No phase delegates, and
  nothing escalates.
- **Not a replacement for the agent loop.** The single-agent, swarm and staged
  paths are untouched. This is a peer.
- **Not a Claude Code plugin.** smart-coder drives the CLI; it does not extend
  Claude Code, ship hooks into it, or depend on its internals beyond the
  documented stream format.
- **Not session resumption.** v1 spawns a fresh run per task. `--resume` and
  multi-turn continuation are real features and deliberately deferred.
- **Not routed permissions in v1.** See above.
- **No cost accounting.** Claude Code reports usage; surfacing spend is a
  separate feature with its own design questions.

## Delivery

1. **Detection + refusal.** `claude` on PATH, cached; Craft mode refuses; the run
   kind appears only when both allow it. Ships first because it is what makes
   every later step safe to expose.
2. **Spawn + stream.** `proc::command`, line-by-line stdout, the event mapping
   above, and cancellation by killing the child — which is new work, not a reuse
   of the cooperative flag (see "The transport").
3. **Delegated approvals.** The gate surface goes visibly dark for these runs.
4. *(Later)* **Routed approvals** through `Pending::Confirm`.

## Testing

Per [11](11-testing-and-tdd.md), the assertion is the specification:

- **The mapping is pure and host-testable.** A function from a stream-json line to
  `Option<UiEvent>` needs no subprocess: feed it recorded fixture lines and assert
  the events. This is where most of the logic lives and where most of the tests
  go.
- **A malformed line does not kill the run** — asserted directly, because the
  format belongs to another project and will change.
- **Craft mode never spawns it**, asserted on the refusal predicate rather than on
  a proxy, matching the zero-construction contract in [21](21-craft-mode.md).
- **Absent `claude` is not an error state** — the run kind is simply not offered,
  and detection returning `false` must not surface as a failure.
- **Not tested:** that Claude Code itself does the right thing. That is its
  project's contract, not this one's. The boundary here is the stream format and
  the spawn — assert those and stop.

## Relationship to other specs

- Amends the first non-goal of [00](00-overview.md), narrowly — see the Principle.
- The seam it deliberately does *not* use is `ModelBackend` ([02](02-model-backends.md)).
- The event vocabulary is [03](03-agent-loop.md)'s, reused unchanged.
- Craft mode's refusal rule and the detect-then-offer pattern are [21](21-craft-mode.md).
- Approvals route through the confirmation path of [09](09-workflow-and-checkpoints.md).
- The run appears on the phone through the mirror ([20](20-remote-review.md)) with
  no extra work, because it rides the same event channel.
