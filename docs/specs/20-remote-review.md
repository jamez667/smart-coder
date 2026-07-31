# 20 — Remote review & approval

## Principle

**A gate reached from a phone is the same gate.**

The queue's whole value is that work happens while nobody is watching
([19](19-queue-and-runner.md)) — and the moment it parks, that value is only
realised if the developer can actually clear the gate from wherever they are. A
run that parks at 9am and waits until someone opens the desktop app has moved the
bottleneck, not removed it.

So this spec is the other half of the loop. But it carries the risk that makes the
whole design worth scrutinising:

> **A review surface optimised for a small screen must not become a surface that
> encourages approving without reading.**

Approval is the one human judgement in the entire pipeline
([09](09-workflow-and-checkpoints.md)). A phone UI whose easiest action is a large
green button, on an artifact too long to read on that screen, produces rubber-stamp
approval — and a rubber-stamped gate is worse than no gate, because the system
still reports that a human signed off. The gate would remain in the code and be
gone in practice.


## The four decisions, unchanged

The decisions are [09](09-workflow-and-checkpoints.md)'s, verbatim — this spec adds
no fifth and removes none:

| Action | Effect | On this surface |
| --- | --- | --- |
| **Approve** | Artifact accepted; the run returns to `Queued` for the next phase. | Yes |
| **Send back** | Return to this or an earlier phase with feedback; the agent regenerates. | Yes — the primary corrective |
| **Abort** | Stop the workflow; approved artifacts are kept. | Yes |
| **Revise** | The human edits the artifact text directly. | **No** — see below |

`Revise` stays in the engine and is not offered here, which is not a mobile
compromise: the desktop GUI dropped its revise button too, on the grounds that
editing by comment supersedes it ([12](12-platform-clients.md)). This surface
reaches the same conclusion from a different direction, and both leave the
decision in the enum for the CLI.

The state transitions belong to the workflow engine. This surface submits a
decision; it does not implement one — which is what makes a web approval and a
desktop approval the same operation rather than two that resemble each other.

**Send-back targeting must be preserved, and it is not free.** Notes placed against
an earlier phase return the run to the earliest commented phase, carrying every
phase's notes along ([09](09-workflow-and-checkpoints.md)) — because invalidating
from there drops the later artifacts anyway. A reviewer who spots a layout problem
while reading the stage breakdown needs that reach, and simplifying it to "send
back one phase" would remove the pipeline's only path for a late discovery.

*Not built, and awkwardly placed:* the resolution logic lives in the desktop crate
<!--@ crates/sc-win/src/comments.rs -->, not the engine, so this surface cannot
inherit it by depending on `sc-workflow` — it would have to be lifted into the
engine first. The `Gate` trait is also narrower than the feature: it receives the
current phase and one artifact, never the whole state, so a gate implementation
cannot by itself show a reviewer the earlier artifacts to comment against. Either
the trait widens or the surface reads the artifacts alongside it. This is real work
that neither this spec nor [19](19-queue-and-runner.md) can wave through.

## Designing against the rubber stamp

Four concrete commitments, each aimed at the failure above:

- **The artifact is the page.** Not a summary, not a model-written précis of the
  model's own output, with approval below it. A summary of an artifact is a second
  artifact nobody verified, and approving it means approving something the
  developer did not read.
- **Approve is not reachable without scrolling to the end.** Not a dark pattern —
  the artifact is short by construction (each phase produces one compact document)
  and reaching the bottom is the minimum evidence that the page was seen.
- **No bulk approve.** No "approve all remaining gates," no "approve and continue
  to done." Each gate is one decision. A control that clears four gates at once is
  a control that exists to skip reading four artifacts.
- **Deferring is free and obvious.** The developer must be able to close the page
  and leave the run parked indefinitely, with no penalty and no nagging. Parked is
  a valid resting state ([19](19-queue-and-runner.md)); when the honest answer is
  "not on a phone, I'll read this properly later," the surface should make that the
  path of least resistance rather than making approval the easy way out.

> **On revise from a phone.** Editing a Markdown artifact on a touch keyboard is
> miserable, and pretending otherwise produces worse artifacts. The mobile surface
> supports **send-back with a note** as the primary corrective — a sentence of
> feedback is comfortable to type, and regeneration is what the pipeline is for.
> The desktop's advantage is not inline editing (it dropped that too) but
> **line-anchored comments** on a large screen ([12](12-platform-clients.md)),
> which a phone cannot match. This is a deliberate asymmetry, not a gap to close
> later.

## Continuity across surfaces

A run parked by the daemon can be cleared from the website, the desktop GUI, or
the CLI — **one at a time**. The shared artifact directory gives every front-end a
common location and a resumable format: the state is `specs/<slug>/state.json`,
and they all already read it.

The practical shape this should take:

- Start a task on the phone, approve the specs gate on the phone, then open the
  desktop and find the run exactly where it was — mid-workflow, artifacts on disk.
- Approve the architecture gate on the desktop with its line-anchored review, and
  the website shows the run advanced next time it is loaded.
- No "transfer session" step, because there is no session to transfer.

**But this is not free, and the spec should not imply it is.** The shared directory
buys a common location and format, not safe concurrent access:
[19](19-queue-and-runner.md) shows that two writers silently clobber each other,
and requires an ownership lease before any of the above is true. Handoff between
surfaces is what the lease *enables*; it is not a property of using the same file.

**A decision is applied once.** Two surfaces can view the same parked run, so two
approvals can race. The first decision wins, and the second is rejected against the
run's current phase rather than replayed against whatever phase it has since
advanced to — approving the specs gate twice must never approve architecture by
accident. Arbitration belongs to whichever process holds the lease, and a decision
submitted without it is refused, not merged.

## What the surface shows

- **The queue** — every task and its state, oldest first, with parked runs first
  because they are the ones needing a human.
- **The artifact** — the current phase's document, rendered.
- **Provenance** — which agent profile ([18](18-task-intake.md)) produced it, and
  when. A spec drafted by the local 4B and one drafted by the planner profile
  warrant different reading, and the reviewer needs that before deciding, not
  after.
- **The event log** — available, not foregrounded. The developer's first question
  about a surprising artifact is "what did it read," and
  [19](19-queue-and-runner.md) guarantees the log exists to answer it.

*Not built:* none of this exists. Rendering an artifact is the easy half; the
work this spec actually implies is the send-back targeting lifted out of the
desktop crate, the lease-aware arbitration, and the header and CSP hardening that
rendering model-authored Markdown demands ([18](18-task-intake.md)).

## Anti-goals

- **No approval notifications that invite one-tap action.** A push notification
  with an "Approve" button in it is the rubber stamp with a shorter path. Notifying
  that a run parked is useful; the decision belongs on the page with the artifact.
- **No model-generated approval recommendations.** "This spec looks good" from the
  same class of model that wrote it is not a second opinion, and it would anchor
  the one judgement the pipeline depends on. [16](16-post-integration-review.md)
  uses a model to *review* precisely because it constrains that model's authority
  to advisory — a gate is not advisory.
- **No approval history as a metric.** No streaks, no throughput, no
  time-to-approve. Anything that makes approving feel like progress rewards the
  behaviour this spec exists to prevent.
- **No partial approval.** A phase artifact is accepted or it is not. "Approve
  sections 1–3" invents a state the workflow engine has no representation for.

## Relationship to other specs

- The decisions, their semantics, and send-back targeting are
  [09](09-workflow-and-checkpoints.md)'s, unchanged. This spec adds **one more
  `Gate` implementation** — joining `AutoApprove`, `CeremonyGate`, the CLI's
  `StdinGate` and the GUI's `ChannelGate` <!--@ crates/sc-workflow/src/gate.rs -->
  — not a new gating model.
- Completes the loop opened by [18](18-task-intake.md) (intake) and
  [19](19-queue-and-runner.md) (execution): file, run, park, review, resume.
- Inherits its entire security posture from [18](18-task-intake.md) — same token,
  same bearer-on-write rule. Approval is a `POST`, so it is a bearer route, never
  a link someone can be induced to follow.
- Deliberately narrower than [12](12-platform-clients.md)'s desktop review: that
  surface has the screen for line-anchored comments on any artifact and full inline
  editing. This one optimises for the decision a phone can honestly support, and
  defers the rest rather than approximating it badly.
- Shares [16](16-post-integration-review.md)'s stance that a gate which cannot be
  taken seriously is worse than an absent one — there, "review never blocks on
  taste"; here, approval never becomes a reflex.
