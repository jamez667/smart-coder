# 28 — The sync bar: git over a network, in a GUI

## Principle

Craft mode ([21](21-craft-mode.md)) promises an editor that is a real editor
with no model in it. Its Source Control panel keeps that promise by shelling out
to `git` — which is the right dependency, and which imports one problem the rest
of the panel does not have.

Every other git operation the panel performs is *local and bounded*: a status, a
diff, a stage, a commit. They cost milliseconds and either succeed or fail.
Push, pull and fetch are none of those things. They cross a network, negotiate
TLS, may consult a credential helper, and have no upper bound on how long they
take.

> **The three network ops are a different kind of operation and get different
> rules.** Everything local may run to completion where it stands. Nothing that
> touches a network may, because the time it takes is not ours to predict.

Three rules follow, and the rest of this spec is those three rules and what they
cost.

## Rule 1 — Never on the UI thread

`iced` cannot repaint while `update()` is running. An operation that blocks
inside it therefore freezes the window for its whole duration *and* cannot draw
a spinner from in there, because the frame it would draw in never arrives.

For a local git call this is a stutter. For a pull it is the application hanging
— seconds at best, indefinitely if git is waiting on a credential prompt.

The network ops run on a blocking task and report back as a message.

<!--@ crates/sc-win/src/app/logic_b.rs -->

This is the same shape the compile and profiler runs already use, and the same
argument [25](25-plugins.md) makes against putting a synchronous round trip on
the render path. The sync bar was simply the last place that had not had it
applied.

**The refresh afterwards is part of the rule.** A pull that succeeded has
rewritten the working tree, which is precisely when re-walking it is most
expensive — a tree walk plus several `git` subprocesses. Refreshing
*synchronously* after an asynchronous pull hands back the freeze that moving the
pull off-thread just removed. The completion path therefore goes through the
same off-thread snapshot the periodic heartbeat uses.

## Rule 2 — A button must not lie about what pressing it does

An operation with no visible state is indistinguishable from a broken button.
The user presses Pull, nothing changes, and the two available conclusions — "it
is working" and "it is broken" — are both consistent with what is on screen.

So the button that started an op **becomes** the control that stops it:

| State | The Pull button reads | Pressing it |
| --- | --- | --- |
| Idle | `↓ Pull 3`, or `↓ Pull` when the behind-count is zero | pulls |
| Running | `✕ Cancel pull`, in red | cancels |
| Another op running | `↓ Pull 3`, greyed | nothing |

<!--@ crates/sc-win/src/app/view_core.rs -->

The profiler's Record button already does this — it becomes `■ Stop` while a
recording is in flight — but [24](24-profiler.md) never wrote the rule down, so
this spec is where it becomes a house rule rather than a coincidence between two
panels.

<!--@ crates/sc-win/src/app/view_flame.rs -->

A branch with no upstream reads `↑ Publish` and pushes with `-u origin <branch>`,
because `git push` against no upstream fails with advice the panel cannot act
on. Fetch is the unlabelled `⟳` chip; it still becomes `✕ Cancel fetch` while
running.

**A cancel affordance beside the button was tried and removed.** A separate `✕`
chip is two controls for one decision, and no spacing made it read otherwise: at
the two chips' natural padding it floated in a gap, with the padding removed it
collided with the label, and at every value between it still showed the seam of
two adjacent bordered buttons. The single mutating button is smaller code and
one less thing on screen.

**The other two buttons grey out and stop responding while one op runs.** One
network op at a time, enforced by the in-flight state itself rather than by a
separate guard, so a second click cannot stack a second pull.

**A worker that panics still reports through the same completion message.** If
it did not, the in-flight state would stay set and all three buttons would
remain dead for the rest of the session — the guard that enforces one-op-at-a-time
is also the thing that would trap the bar.

<!--@ crates/sc-win/src/app/types.rs -->

## Rule 3 — Cancel must actually reach the process

A cancel the user cannot trust is worse than no cancel, and this is the rule
with the most ways to get it wrong.

**A flag alone is not enough here.** The op still sets an `AtomicBool`, but it
cannot be polled between reads the way the compile and profiler loops are — so
the worker never blocks in `wait()`. It polls `try_wait()` on a 50ms sleep and
checks the flag each turn, and the flag's only job is to trigger a kill. A flag
without the kill would be observed only once git had already finished, which is
the thing being escaped.

**The kill is a process *tree* kill.** Measured: `git pull` over ssh is three
processes deep — `git` spawns a second `git`, which spawns `ssh`. Killing only
the process we hold leaves `ssh` alive holding the connection. This is the same
reason the profiler kills a tree rather than a child.

<!--@ crates/sc-craft-ui/src/proc.rs -->

**Child stdin is closed.** An inherited stdin is how a credential prompt wedges
an op forever: git waits on a terminal a GUI does not have, invisibly. Closed,
git fails immediately with a message that can be shown.

**Cancel returns without waiting for the output readers.** The pipe-draining
threads block until every handle closes, including those held by grandchildren
the kill is still unwinding — measured at 21 seconds, during which the button
still said the op was running. The outcome is already known the moment the user
cancels, so it is reported then. Cleanup completes on its own.

That last point is the one worth a regression test rather than a comment: the
bug it prevents looks exactly like the bug this spec exists to fix.

<!--@ crates/sc-win/src/app/logic_b.rs -->

## Where the outcome goes

Into the **terminal**, as a note. The chat panel left with the agent
([25](25-plugins.md)), and a push that silently failed is the one git outcome a
user must not miss, so it needs somewhere to land that survives Craft mode.

A cancelled op reports as **cancelled, not failed**. The user asked for it;
reporting their own choice back to them as a failure is a small lie that makes
the report untrustworthy.

## What this deliberately is not

**Not a merge UI.** Pull is `--ff-only`, so it never auto-merges and never
leaves the working tree in a conflicted state the panel has no way to show. A
pull that cannot fast-forward fails with git's own message. Resolving that is a
different piece of work.

**Not a credential manager.** The client closes stdin and surfaces what git
says. Configuring an askpass or a credential helper is git's own configuration
and not something the editor wraps.

**Not a progress bar.** The button says an op is running and offers to stop it.
Parsing git's progress output to drive a percentage is a lot of fragile screen
scraping for an op that is usually over in a second.

**Not applicable to the local git operations.** Stage, unstage, discard, commit
and diff stay as they are. They are bounded, and the machinery here would be
ceremony around a call that costs less than the message dispatch.

## Open questions

- **Whether the behind-count should ever refresh itself.** `↓ Pull 3` is only as
  fresh as the last fetch, because reading it is a local operation against
  cached remote-tracking refs. A periodic background fetch would keep it honest
  and would also be unsolicited network traffic on someone's machine.
- **Whether a failed pull should offer its own next action.** "Cannot
  fast-forward" has an obvious follow-up the panel currently cannot offer, and
  adding one starts the merge UI this spec declines to build.
