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
| **Abort** | Stop the workflow; approved artifacts are kept. | As **Discard** — this surface drops the *request*, it does not stop a workflow |
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
  model's own output. A summary of an artifact is a second artifact nobody
  verified, and approving it means approving something the developer did not read.

  ✅ **Held**, and it survives the two-step below: the confirmation restates the
  artifact's opening and closing lines **verbatim**, naming how many it omitted —
  it never paraphrases. The decision now completes on a second page, so "approval
  below it" is true of the first step; the artifact is still the only thing the
  reviewer is shown.
- **Approve is a deliberate two-step action, taken below the full artifact.**

  ⚠️ **This bullet previously read "Approve is not reachable without scrolling to
  the end … reaching the bottom is the minimum evidence that the page was seen."
  That was false, and it is corrected here rather than implemented.** Reaching the
  bottom is evidence that *a scroll gesture terminated at the bottom* — one flick,
  under 300ms, nothing read.

  This is not a limitation of building without JavaScript. An
  `IntersectionObserver` on a sentinel element proves exactly the same thing and
  no more. The gap between "the bottom was reached" and "the page was seen" is a
  category gap, not a capability gap, and **no client-side mechanism closes it.**

  What is built instead <!--@ crates/sc-server/src/page.rs -->:

  - The decision controls sit after the *close* of the artifact block, so on a
    phone they are physically below it. Asserted as an invariant, anchored on the
    closing tag — "after the opening tag" would pass with the buttons mid-document.
  - Approving requires confirming against a page that **restates** what is being
    approved — the artifact's opening and closing lines verbatim, never a summary,
    because a summary is a second artifact nobody verified.
  - The confirmation **binds the approval to the exact bytes displayed**, by
    carrying a digest that `Store::approve`
    <!--@ crates/sc-server/src/store.rs --> re-checks. This is the part that earns the second step: it is not ceremony but
    a real guarantee, and it closes a race described below.
  - A **visible** "skip to the decision" link. Counter-intuitive and deliberate:
    hiding the bypass does not remove it — flicking is the bypass, always
    available — it only lets the system believe nobody took one.

  **What the system may report is "a human confirmed this specific text", never
  "a human read it."** That distinction is the whole point. A gate that overclaims
  is the same failure as a gate that is skipped, and harder to notice.
- **No bulk approve.** No "approve all remaining gates," no "approve and continue
  to done." Each gate is one decision. A control that clears four gates at once is
  a control that exists to skip reading four artifacts.
- **Deferring is free and obvious.** The developer must be able to close the page
  and leave the run parked indefinitely, with no penalty and no nagging. Parked is
  a valid resting state ([19](19-queue-and-runner.md)); when the honest answer is
  "not on a phone, I'll read this properly later," the surface should make that the
  path of least resistance rather than making approval the easy way out.

### What this does not achieve

Stated plainly, because each one is a claim that would otherwise be made falsely:

1. **Evidence of deliberate action, not evidence of reading.** See above. This is
   the ceiling for any surface, not a shortfall of this one.
2. **Approve remains cheaper than send-back.** Send-back requires typing a
   non-empty note, enforced server-side; approve requires two taps. The asymmetry
   runs the *wrong way* relative to this spec's aim. It is accepted because
   send-back's friction is **productive** — the note grounds the redraft — whereas
   manufactured approve-friction would be pure tax, and a tax people learn to pay
   without thinking is exactly the reflex this spec exists to prevent.
3. **A long artifact's elided middle is never re-shown.** The confirmation restates
   both ends and names how many lines it omitted. The defence is this spec's own
   premise — "short by construction" — which is an *assumption about model output,
   not an enforced bound*. If drafted specs start running long, that premise should
   be revisited rather than quietly relied upon.
4. **Nothing prevents approving without loading the page at all.** The committing
   route is reachable directly with a credential; the two-step makes that two
   requests instead of one. **The gate is against thoughtlessness, never against
   intent** — a developer determined to rubber-stamp their own queue can, and no
   design here should pretend otherwise.

### The race this closed

Before the digest binding, `approve` settled whatever text was on disk when the
request landed. A reviewer reading a spec on a train, while the daemon pushed a
redraft, would approve the *new* text on the strength of having read the *old*.
Consent was attached to an id rather than to bytes.

This was a live defect in the shipped server, found while implementing this spec
and fixed by the same mechanism — which is the argument for the second step being
worth its cost.

The check lives in the store rather than the route, so every caller of *this
server's* store gets it. **The CLI and desktop gates do not inherit it**: they
approve through `sc-workflow`'s `state.approve(Phase)`
<!--@ crates/sc-workflow/src/state.rs -->, a different code path with no digest
binding at all. The same race is therefore still open on those surfaces, and
closing it there is separate work — worth doing, because a desktop reviewer
reading while `queue serve` redrafts is in exactly the same position.

**The binding covers approve alone.** Send-back and discard carry no digest and
act on whatever is current. That asymmetry is deliberate rather than an oversight:
neither one signs off text, and a note aimed at a superseded draft still grounds
the redraft usefully. But it means the race is closed for *consent*, not for every
decision on this surface.

### On JavaScript and the CSP

The surface is server-rendered HTML with **no script at all**, and the
Content-Security-Policy is `default-src 'none'` <!--@ crates/sc-server/src/routes.rs -->.

The reason to record is not "the trade was unfavourable" but **"the capability
JavaScript would buy does not exist"**. Scroll detection proves nothing more than
document order proves. There is no security cost worth paying for a capability
that is not real, and stating it this way is what stops the question being
reopened every time someone rediscovers `IntersectionObserver`.

Two further reasons the line holds:

- **Script and escaping are multiplicative defences today.** The artifact is
  rendered as escaped text in a `<pre>`, and no script runs. If script ever ran, a
  bug in the escaper would upgrade from "some angle brackets render wrong" to
  "model-authored text influences a live script context" ([18](18-task-intake.md)).
- **A CSS scroll-reveal is worse than either.** `animation-timeline: scroll()` is
  Chromium-only and unsupported on every iOS Safari, where the un-animated state
  *is* the initial state — so a control hidden until the technique fires becomes
  **permanently unreachable**, bricking the gate on the platform this feature was
  built for. A test asserts no control in the review path depends on it, or on
  `opacity: 0`, `display: none`, `pointer-events: none`, or `:target`.

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

✅ **The queue and the artifact are built** <!--@ crates/sc-server/src/page.rs -->,
along with the header and CSP hardening ([18](18-task-intake.md)).

**Provenance is half built, and the half that is missing is the one this section
argues for.**

- ✅ *When* — the request's filing and drafting times, shown as relative ages
  ("filed 2 hr ago · drafted 2 min ago"). Relative rather than wall-clock because
  the server has no idea what timezone the phone is in, and the reviewer's actual
  question is "is this fresh, or did it sit overnight?". The **server** stamps the
  draft on receipt; the wire carries no timestamp, and a clock the daemon controls
  is one this server cannot check.
- ⬚ *Which agent profile* — **not built, and not approximable.** The daemon holds a
  `&dyn ModelBackend` whose `name()` returns the constant `"openai-compat"` for a
  local 4B and a Gemini planner alike, so it cannot make the very distinction this
  section asks for; the real model string is private to the backend with no
  accessor. Named profiles do not exist in any config or record —
  [18](18-task-intake.md) already scopes that as separate work. Showing `name()`
  here would be worse than showing nothing, because it would *look* like
  provenance while telling the reviewer nothing.

⬚ **The event log is not built, and there is nothing to expose.** There is no
per-drafting-run log anywhere: `sc-workflow`'s event sinks sit on the *build*
paths the daemon is structurally forbidden to reach ([19](19-queue-and-runner.md)),
and the only place a real model name is persisted is `sc-model`'s transcript —
one file per *process*, carrying no request id, written outside the repository.
[19](19-queue-and-runner.md)'s promise that the drafting stream is teed to
`.smart-coder/sessions/<id>.jsonl` is **aspirational for the daemon and not
honoured today**. Delivering this bullet is new plumbing, not exposure of existing
data, and it should not be scoped as if it were the latter.

*Also not built:* the send-back targeting lifted out of the desktop crate, and the
lease-aware arbitration.

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
- Inherits its entire security posture from [18](18-task-intake.md). Approval is a
  `POST`, never a link someone can be induced to follow — and now **two** POSTs,
  the second bound to a content digest.

  *Correction:* the "bearer-on-write" rule this line assumed is not what shipped.
  A page with no script cannot set a header, and requiring one would mean
  requiring script — the thing that makes rendering model-authored text
  dangerous. The CSRF defence is `SameSite=Strict` plus `form-action 'self'`
  instead, recorded as a deviation in [18](18-task-intake.md) rather than glossed.
- Deliberately narrower than [12](12-platform-clients.md)'s desktop review: that
  surface has the screen for line-anchored comments on any artifact and full inline
  editing. This one optimises for the decision a phone can honestly support, and
  defers the rest rather than approximating it badly.
- Shares [16](16-post-integration-review.md)'s stance that a gate which cannot be
  taken seriously is worse than an absent one — there, "review never blocks on
  taste"; here, approval never becomes a reflex.
