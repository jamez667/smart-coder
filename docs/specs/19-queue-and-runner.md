# 19 — The queue & the background runner

## Principle

**Autonomy is in the scheduling, never in the approval.**

A task filed from a phone should be a drafted spec by the time the developer sits
down. That requires something to run while nobody is watching — which sits
directly against the instinct behind [00](00-overview.md)'s human-in-the-loop
non-goal, which before this spec read "no autonomous, unattended operation. v1 is
human-in-the-loop. Long-running background autonomy is future work."

The resolution is that the non-goal is about *judgement*, not about *uptime*. The
gate is the human-in-the-loop mechanism, and the gate is untouched:

> **The runner runs phases. It never passes a gate.** It works up to the first
> checkpoint the ceremony demands, parks, and waits. A parked run is the system
> working correctly, not a run that failed to finish.

[09](09-workflow-and-checkpoints.md) already grants exactly this: "within a phase
the agent works autonomously; at the boundary it yields control." A background
runner moves *where* the yielding happens — from a blocking terminal prompt to a
durable parked state — without moving *who* decides. Unattended execution between
gates was always the design. This spec makes it survive the process exiting.

## The gap this fills

No *workflow* execution runs detached today. Every path that drives the agent loop
is thread-scoped and dies with its process: `Session::spawn` is one
`std::thread::spawn` per run, owned by the GUI
<!--@ crates/sc-win/src/session/mod.rs -->; `sc-web`'s `serve` exits its HTTP loop
the moment the run finishes and the browser drains the stream. The remote mirror
is explicitly the opposite of detached — it attaches to a session another process
already owns. (The workspace does spawn detached *child processes* — terminal
launches — and already tracks live sessions by pid across processes; neither is a
durable run.)

So the queue and the daemon are genuinely new machinery. What is *not* new is
everything they drive: the five phases, the artifacts, the resume path, and the
gate trait all exist and are reused unchanged.

## Shape

```
  intake ([18])
     │
     ▼
 ┌─────────────────────┐
 │  queue (on disk)    │   Queued → Running → Parked → Done
 └──────────┬──────────┘              │        ▲
            │ claim                   │        │ approve ([20])
            ▼                         ▼        │
 ┌──────────────────────────────────────────────────┐
 │  runner: sc-workflow phases 1‥5                   │
 │  specs → architecture → layout → stages → decomp  │
 └───────────────────────┬──────────────────────────┘
                         │
                         ▼
              specs/<slug>/  +  state.json      ← the durable state
              .smart-coder/sessions/<id>.jsonl  ← the replay log
```

The queue is **on disk, not in memory**. A daemon that loses its queue to a power
cut is one nobody trusts with a task filed from a train.

✅ **The local half is built** <!--@ crates/sc-daemon/src/lib.rs -->: the durable
queue, the state machine, the parking gate, the git preflight, the spec-only
runner, the four intake kinds and the feedback store. It is driveable today
through `smart-coder queue`, and has been run end to end against a live model.

⬚ **The remote half is not**: the hosted server ([18](18-task-intake.md)) and the
daemon's long-poll loop that reaches it. Until those exist, a request is filed at
a terminal rather than from a phone — which is the same queue, entered by a
different door.

What was never new is everything the runner drives: the five phases, the gate
trait, the artifact directories and the resume path, reused unchanged — plus the
single-writer machinery below, built ahead of the daemon because the GUI and CLI
could already clobber each other without it. The sections below are requirements,
not descriptions, except where marked ✅.

## The state machine

| State | Meaning | Leaves via |
| --- | --- | --- |
| `Queued` | Accepted, not started | The runner claims it |
| `Running` | A phase is executing | Phase completes, or fails |
| `Parked` | At a gate, awaiting a human | Approve / send-back ([20](20-remote-review.md)) |
| `Done` | Final gate passed | — |
| `Aborted` | Stopped by a human; approved artifacts kept | — |
| `Failed` | Budget exhausted, or the run could not continue | Requeue, or discard |

`Aborted` is distinct from `Done` for the same reason `Parked` is distinct from
`Failed`: a run someone stopped and a run that finished are not the same outcome,
and a queue that renders them identically is one that cannot answer "what happened
to my task." `Running` can move directly to `Failed` on budget exhaustion.

**Requeue and discard are queue operations, not gate decisions.** They act on a
task that is not at a gate, so they are deliberately outside
[20](20-remote-review.md)'s four — that table governs what a reviewer does *with an
artifact*. A `Queued` task can also be discarded before the runner ever claims it;
a developer who files a bad task from a train needs that, and no gate exists yet to
express it.

`Parked` is the state that carries the design. It is **not** an error, not a
timeout, and not a partial failure — it is a run that has done all the work it is
permitted to do without a person. A task filed at 8am against `--ceremony full`
parks four times before it is finished, and that is the ceremony working as
specified.

`Failed` is kept distinct from `Parked` for the same reason
[13](13-compliance-evidence.md) keeps `Unknown` distinct from a verdict: collapsing
"I need you" into "I broke" trains the developer to ignore both.

## Serial by default

**One run at a time.** The queue is a queue, not a scheduler.

The reasoning is the local-first premise. Concurrency's benefit is throughput on
spare capacity, and there is no spare capacity — a single local model server is
the bottleneck for every run, so two concurrent runs mostly take turns inside the
backend while doubling the memory pressure. It also compounds: two runs against
the same workspace touch the same files, which is a whole isolation problem
([08](08-orchestration-and-swarm.md) solves that for *workers within a run*, and
that machinery does not extend to unrelated runs).

**A parked run does not hold the slot.** Otherwise one unread gate blocks the
whole queue, and [20](20-remote-review.md)'s promise that deferring is free would
starve every other task. So the slot is held by `Running`, not by `Parked` — which
means a parked task A and a running task B can coexist, and if they target the same
project they contend for the same working tree. The queue must therefore serialise
on the **project**, not merely on the runner: at most one non-terminal run per
workspace, with a second task against a busy project staying `Queued`.

Serial execution makes the failure modes boring, which is the right ambition for a
component the developer is not watching.

## The single-writer problem

This is the largest unsolved design question in the spec, and it is worth stating
plainly rather than discovering during implementation.

The daemon is not the only writer of `specs/<slug>/state.json`. The desktop GUI and
the CLI write it too, from their own in-memory copies.

**This section described an unsolved problem; three of its four requirements are
now built.** `save_to` writes via temp-file → fsync → rename and compare-and-swaps
on a `generation` counter <!--@ crates/sc-workflow/src/state.rs -->, and a lease
arbitrates ownership of an artifact directory
<!--@ crates/sc-workflow/src/lease.rs -->. The read-modify-write window still spans
an entire workflow including every unbounded human gate — that is *why* the lease
heartbeats rather than locking, since no lock held across an unbounded human wait
survives a crash gracefully.

The concrete failure: the daemon parks a run at the specs gate; the developer gets
home, opens the same project in the GUI, and starts a plan run on the same task.
Both hold divergent copies. The phone-side approval is written, then silently
clobbered by the GUI's next phase save. Neither process notices; nothing logs it.

So the appealing sentence — *approve from anywhere, no handoff protocol* — is
**false as stated**, and this spec should not claim it. Single-writer arbitration
*is* a handoff protocol. What the shared directory actually buys is a common
*location* and a resumable *format*, not concurrent access. Making it safe needs:

- ✅ an **ownership lease** — owner pid, heartbeat, expiry — so a second writer
  can detect a live first one instead of racing it;
- ✅ **atomic writes** (temp file plus rename) so a crash mid-save cannot truncate
  the state;
- ✅ a **generation counter** for compare-and-swap, so a stale writer fails loudly
  rather than overwriting;
- ⬚ and a rule that the daemon **refuses to claim a project the GUI holds open**.

The honest position remains that a project is either the daemon's or the GUI's at
any moment, and the lease is what makes that enforceable rather than a convention.

### What the lease actually protects, and what it does not

The lease is scoped to an **artifact directory**, not to a project. Two runs on
the same task contend; two runs on *different* tasks in one workspace do not. The
fourth requirement above is therefore only half-met: the workspace-level rule
belongs with "Serial by default" (*at most one non-terminal run per workspace*),
which the daemon must enforce when it exists. `holder()` is exported for exactly
that check.

Three properties earned by building it:

- **A lease identifies a run, not a program.** Keying on the pid alone was tried
  and is wrong: the GUI spawns one thread per run inside a single process
  <!--@ crates/sc-win/src/session/mod.rs -->, so two runs on one directory shared
  a pid, both acquired, and the first to finish deleted the lease out from under
  the second. Each run carries a token alongside the pid, and a guard may only
  ever refresh or release the lease it was actually granted.
- **Contention refuses loudly and names the holder.** Proceeding read-only was
  considered and rejected: a run that silently cannot persist loses five phases of
  work at the end rather than at the start.
- **A lease with no heartbeat for ninety seconds is dead and is reclaimed
  automatically** — no `--force` flag and no pid-liveness probe. Pids are
  recycled, and an override the user has to discover is not recovery.

The lease lives in a sibling `lease.json`, deliberately **not** inside
`state.json`: the state file is an artifact a human reads and diffs, and a
heartbeat every fifteen seconds must not rewrite it. Being runtime state rather
than a planning artifact, it is gitignored — `specs/<slug>/` is committed, and a
committed lease would make a long-dead process look like a live holder to whoever
cloned it.

One limit worth stating rather than discovering: the compare-and-swap is per-save,
so the check-then-write is not itself atomic. Two writers racing one directory
could both pass the comparison and interleave. That is safe only because the lease
excludes them first — the two mechanisms are a pair, not alternatives.

## What resume actually recovers

[09](09-workflow-and-checkpoints.md)'s "the approved artifacts are the state" is
true, and load-bearing here — but it is narrower than it sounds, and the gap lands
squarely on the daemon's most common scenario.

**All three gaps in this section are now closed** — they were bugs in the existing
runner, not daemon work, so they were fixed ahead of it.

- ✅ **The pending draft is restored, not regenerated.** Resume adopted *approved*
  artifacts only, so a phase that generated cleanly and was saved — but whose gate
  was never answered because the machine rebooted overnight — was discarded and
  re-generated. The developer approving in the morning was shown a *different*
  artifact than the one the phone showed them last night, which is corrosive for
  something pitched as "come back to a drafted spec." The draft is restored and
  re-**gated**: it was never signed off, and only a human may do that.

  This one had a second half that the obvious implementation missed.
  `next_phase()` returned the first phase with *no artifact*, so a restored draft
  — which has one — was stepped straight past: displayed, never gated, left
  `Draft` forever, and `is_complete()` never became true, poisoning the signal
  the CLI and GUI use to decide a run finished. **A phase is advanced by its
  gate, not by a file existing**, so `next_phase()` now keys on approval. The two
  definitions coincide on a fresh run and diverge exactly at the case that
  matters.
- ✅ **The frozen contract tests survive.** `test_files` lived only in memory, so a
  resumed run started with an empty list and nothing downstream knew which tests
  were frozen — a worker could rewrite the very tests
  [09](09-workflow-and-checkpoints.md) calls the approved contract. They are now
  part of the persisted state, because they *are* part of the contract rather than
  a by-product of the run that produced them. A daemon restart silently unfreezing
  them is a correctness bug, not a papercut.
- ✅ **A corrupt `state.json` is an error, not a fresh start.** It was swallowed and
  the run silently restarted from the top, discarding an approved design and
  re-running work a human had already signed off — a failure that reads as "the
  tool redid everything", with nothing to point at. The run now refuses and says
  to move the file aside deliberately.
- ✅ **Send-back feedback notes survive a resume.** Persisted all along, they were
  simply never copied onto the resuming state — so a phase sent back with "make it
  event-driven" came back regenerated having never seen the note. Narrower than the
  three above (guidance, not an approved decision) but the same class of silent
  loss, and a two-line fix once the others were in place.
- ✅ **A task killed mid-draft stops blocking its repository.** Found while
  building `queue serve`. A daemon that dies while drafting leaves its task
  `Drafting`, and that state *holds the repo*
  <!--@ crates/sc-daemon/src/task.rs --> — so on the next start every request for
  that repository was skipped in silence. Not an error, not a log line: work
  simply stopped arriving, forever, with nothing to point at.

  Latent before, routine now: `queue serve` is a long-running foreground process
  that people Ctrl-C, where `queue run` drained and exited.

  The fix is `requeue_abandoned()` <!--@ crates/sc-daemon/src/queue.rs -->,
  called at startup **before the first claim** — the one moment when nothing can
  legitimately be in flight, because this process has claimed nothing yet, so
  anything `Drafting` is provably a corpse. **Requeued, not failed:** nothing
  about the *request* went wrong, and reporting a failure would send the
  developer investigating their own interrupt. The note says what happened,
  because a task that silently reappears at the back of the queue is its own
  small mystery.

  Only `Drafting` is touched. It is the one state meaning "a process is working
  on this right now" — reclaiming `AwaitingReview` would throw away a spec a
  human has not read yet.

## Task identity

A run needs an **id assigned at intake**, distinct from the artifact slug.

The slug is derived from the task text — truncated at the first sentence and capped
at 40 characters <!--@ sc_workflow::artifact_dirs --> — so "Fix the bug. In auth"
and "Fix the bug. In the parser" produce the same directory. For an interactive user
this is rare and visible. For a queue accepting free-form text typed on a phone,
short generic first sentences are the *normal* case, and the collision was silent:
the second run adopted the first's approved artifacts as its own and then
overwrote them.

✅ **The collision is now detected rather than the slug made unique.** `state.json`
always recorded the task text; it was simply never read back. A run whose task
disagrees with the one already in the directory refuses, naming both. Assigning an
id at intake — still the right long-term answer, and what a queue needs — makes
the *directory* unambiguous; this makes the *silence* impossible either way, and
it protects the GUI and CLI today rather than only the daemon later.

One thing worth recording, because it was got wrong first: the comparison must be
the **whole first line**, not the first sentence. Comparing sentences seems more
principled — the slug is cut there — but two tasks agreeing on the sentence and
differing after is *exactly* the collision, so that version of the check never
fired at all.

Its scope is worth stating too: the check fires only where a `state.json` already
exists. Two colliding tasks starting at once still race for the directory, and the
lease — not this check — is what separates them. The two are complements, one for
sequential collisions and one for simultaneous.

> **This is not the swarm.** [08](08-orchestration-and-swarm.md) parallelises
> *workers within one task*, after the final gate, with an orchestrator and a
> blackboard. This queue sequences *whole tasks*, before any code is written. A
> reader who conflates them will look for an orchestrator here and find none — the
> queue's scheduling logic is `pop the oldest Queued task` whose project is free.

## Preflight: the repository state

Nothing on the workflow path inspects git today — the CLI checks backend liveness,
the GUI warns about the sandbox, and neither looks at the tree. That is survivable
for an interactive user who can see the repository they are sitting in front of.
It is not survivable for a task claimed at 3am against a tree left mid-rebase.

Phase artifacts land in `specs/<slug>/`, **inside the repository**, so an
unattended run adds tracked files to whatever state the tree is in. Before
claiming a task the runner must check for an interrupted operation — a rebase,
merge, cherry-pick or bisect in progress — and refuse rather than write into it,
leaving the task `Queued` with the reason recorded. A merely *dirty* tree is fine
and must not block: phases 1–5 write only under `specs/<slug>/`, and refusing on
uncommitted work would make the daemon useless on any real working repository.

### "Only under `specs/<slug>/`" was not true, and is now

✅ **Fixed** <!--@ crates/sc-workflow/src/artifact_dir.rs -->. That claim was load
bearing for this whole design and it did not hold.

`spec_artifact_dir` scans the task text for a token starting `specs/` and ending
`/spec.md` — so a **Build** can resume a design a prior run wrote — and returned
`workspace.join(dir_rel)` with no containment check. Task text of the form
`specs/../../../../etc/cron.d/x/spec.md` therefore resolved *outside* the
repository, and the workflow went on to `create_dir_all` it and write
model-authored content there.

The reach is whatever the running user can write: shell profiles, autostart
entries, CI configuration. It was latent rather than harmless — the CLI and the
GUI both pass user-typed text straight through — and it becomes remote the moment
task text arrives from anywhere but the developer's own keyboard, which is
precisely what [18](18-task-intake.md) exists to enable.

Containment is checked **lexically, on the relative form, before the join**.
Deliberately not `canonicalize`: that requires the path to exist, so it would pass
a hostile path simply because the target directory had not been created yet. The
escape has to be impossible to *construct*, not merely impossible to resolve. A
climbing path falls back to the derived slug rather than erroring, so a hostile
string cannot deny service to a legitimate run.

## Reusing the workflow engine

The runner is a **third front-end over `sc-workflow`**, not a second pipeline. It
must resolve artifact directories through the same engine helper the CLI and GUI
use <!--@ sc_workflow::artifact_dirs --> — that shared resolution is precisely
what lets a phone-filed task and a desktop session land in the same
`specs/<slug>/`, and what lets the desktop pick up a run the daemon parked.

The resume path needs no new work. [09](09-workflow-and-checkpoints.md): "the
approved artifacts are the state, not anything held in a model's context." A
parked run is a `state.json` on disk and nothing else, which means:

- The daemon can restart and resume mid-workflow.
- A crashed run resumes at its last approved artifact rather than from the top.
- The website, the GUI and the CLI all read and write the *same* state, so a run
  is legible to whichever surface picks it up — **provided only one holds it at a
  time**, which is the lease problem below, not a free property.

The runner gets the location and the format by not inventing its own storage. It
does not get concurrency, and two sections below say so rather than assuming it.

## Budgets and stopping

An unattended run needs a ceiling that is not the developer noticing. Each run
carries hard, harness-enforced bounds — wall-clock, token, and step — and
exceeding one moves the run to `Failed` with the partial artifacts kept.

Of the three, only the step budget exists today. **Wall-clock is the one that
cannot be added cheaply**: the loop checks its cancel flag between turns and
cannot interrupt an in-flight model call, so a local model server that hangs
hangs the daemon past any deadline. A real wall-clock bound needs a timeout at the
backend request, not just at the loop.

Budgets are measured over `Running` time only. A run may sit `Parked` indefinitely
without consuming budget — otherwise every deferred gate eventually fails the run,
which would destroy the resting state [20](20-remote-review.md) depends on.

The bounds are **per run and enforced outside the model**, in the same spirit as
the gate: a model cannot extend its own budget any more than it can approve its
own artifact. This is the concrete form of the **bounded autonomous mode** idea
that [07](07-roadmap.md) carried as a future idea until M10 promoted it.

## Everything is replayable

A background run logs exactly as an interactive one does — the full event stream
teed to `.smart-coder/sessions/<id>.jsonl`, replayable after the fact
([03](03-agent-loop.md)).

This is not optional polish. [00](00-overview.md) principle 5 is "everything is
inspectable," and a run nobody watched is the one that most needs to be
reconstructable afterwards. The developer's first question about a parked run will
be "why did it decide that," and the answer has to be a log, not a shrug.

## Tool permissions: deny, do not park

A background run cannot answer a confirmation prompt, and the resolution must not
be to auto-approve — silently granting shell access to a run nobody is watching
would undo the permission layer entirely. An unattended run therefore takes a
**restrictive default policy** ([04](04-tools.md)), and a call requiring
confirmation is **denied**, with the denial becoming an ordinary model
observation the run can react to.

> **A confirmation cannot park, and this is structural.** It is tempting to say a
> permission prompt parks the run "just like a gate does." It cannot. A gate is
> called at a phase boundary where the entire recoverable state is a serializable
> `WorkflowState` on disk — which is exactly why resume works. A confirmation
> blocks *mid-loop*, and the live state is a thread's stack: the model
> conversation, the turn history, the plan, the stall counters and four trait
> objects, none of which serialize. `Confirmation` has no *defer* variant and
> returns by value, so there is no way to unwind and resume it later. Making
> confirmations parkable means turning the agent loop into a resumable state
> machine — a rewrite of `sc-core`, not a feature of a daemon.

The scope rule below makes this mostly moot: phases 1–5 produce Markdown and JSON
via model calls and do not run shell commands, so an unattended run should rarely
reach a confirmation at all. Denying is the correct behaviour for the residual
case, not a compromise.

**A remembered-approval grant must never come from the network.** The existing
approve route accepts an arbitrary `prefix` and pushes it onto the session
allowlist; an empty prefix matches every command, converting a run to
unrestricted for its remaining lifetime. Whatever the daemon exposes, it must not
accept an unbounded allowlist grant over HTTP — which is the concrete meaning of
"the strictest permission context in the system."

## Anti-goals

- **No self-approval.** Not behind a flag, not for `minimal` ceremony, not for
  "trivial" tasks. Ceremony chooses which phases gate ([09](09-workflow-and-checkpoints.md));
  it never chooses to skip a human at a gate that exists.
- **No writing code.** The runner drives phases 1–5 — planning artifacts only. The
  swarm executes, and it executes after the final gate, with a human on the other
  side of it. This must be **structural, not a policy line**: a staged-build path
  already exists that writes across the whole tree with no snapshot and no revert
  bookkeeping, and the daemon must be unable to reach it — the same argument
  [18](18-task-intake.md) makes for keeping `sc-workflow` out of the web edge,
  applied where the blast radius is larger.
- **No cross-run scheduling.** No priorities, no fairness, no preemption. Oldest
  first. A feature request here is a signal that the queue is being asked to be a
  CI system, which it is not.
- **No silent retries.** A failed run stays failed and visible. Automatic retry
  hides a reproducible problem behind eventual success.

## Relationship to other specs

- Sits **above** [09](09-workflow-and-checkpoints.md) in the same relation 09 has
  to [03](03-agent-loop.md): 09 sequences and gates the phases, this schedules and
  persists whole workflows. It adds no phase and changes no gate.
- Distinct from [08](08-orchestration-and-swarm.md): that parallelises workers
  inside one task after the last gate; this sequences tasks before the first.
- Fed by [18](18-task-intake.md); its parked runs are surfaced by
  [20](20-remote-review.md).
- Honours [04](04-tools.md) by *tightening* it — an unattended run is the strictest
  permission context in the system, not the loosest.
- Realises the roadmap's bounded-autonomous-mode idea as M10
  ([07](07-roadmap.md)).
- [00](00-overview.md)'s non-goal now reads "no unattended **approval**", stating
  the gate/uptime distinction on its own side rather than leaving this spec to
  assert it. The amendment was made *with* this spec; if that narrowing is ever
  rejected, this spec falls with it rather than surviving as a contradiction.
