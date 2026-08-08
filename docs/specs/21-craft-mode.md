# 21 — Craft mode: the editor without the model

## Principle

`smart-coder` assumes you want an agent. Craft mode is the setting where you
don't — and where that answer is **honoured structurally, not cosmetically**.

The user this spec serves is not undecided. They have an opinion about language
models and it is *no*. Serving them means two things, and the second is the one
that is easy to get wrong:

1. **The model is genuinely gone.** Not hidden behind a flag while the health
   probe keeps dialling out. In Craft mode no `ModelBackend` is ever constructed,
   no HTTP request leaves the process, and that claim is enforced by a test — not
   by a promise in a settings panel.
2. **What remains is a real editor.** A mode that disables the agent and leaves
   behind a read-only file viewer is not a product, it is a broken product. Craft
   mode is only worth shipping if `sc-win` can *edit and save files* — which,
   today, it cannot.

That second point is the bulk of this spec. The toggle is a day's work. The
editor is the feature.

The framing that makes this coherent: **Craft mode is not "the app with the AI
switched off" — it is the app's other half, which happens to have been built
second.** Assistant mode is Craft mode plus an agent. Everything Craft mode has,
Assistant mode also has; a user who never touches the toggle still gains a real
editor, working panels, and find-in-file. Nothing here is a degraded path.

## Relationship to the stated non-goals

This spec knowingly contradicts [00](00-overview.md), and says so rather than
quietly reinterpreting it.

- **"No editor/IDE extension"** ([00](00-overview.md), non-goals) rules out
  shipping an *IDE plugin* — a smart-coder extension living inside someone
  else's editor. That non-goal stands and this spec does not touch it. But
  `sc-win` growing a text editor of its own is a different thing, and the
  non-goal as written does read against it. **The non-goal should be narrowed**
  to "no plugin for third-party IDEs", which is what it was defending.
- **Target users** ([00](00-overview.md)) are described entirely as people who
  want an assistant. Craft mode adds one who does not. That is a real expansion
  of the product's audience and should be an explicit decision, not a side
  effect of this spec landing.

Neither is a blocker. Both are edits [00](00-overview.md) needs if this ships,
and the spec-traceability gate ([17](17-spec-traceability.md)) should be the
thing that notices if they are forgotten.

## The two modes

```
┌──────────────────────────────────────────────────────────────┐
│                        sc-win                                 │
│                                                               │
│   ┌───────────────────────────────────────────────────────┐  │
│   │  CRAFT — always present                                │  │
│   │  editor · tabs · find/replace · undo · save            │  │
│   │  file tree · git · terminal · panel layout · settings  │  │
│   └───────────────────────────────────────────────────────┘  │
│                            +                                  │
│   ┌───────────────────────────────────────────────────────┐  │
│   │  ASSISTANT — additive, absent in Craft mode            │  │
│   │  chat · gates · proposals · line-comment fixes         │  │
│   │  swarm · plan flow · routing settings · health probe   │  │
│   │  remote mirror · compliance model summary              │  │
│   └───────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────┘
```

`Mode::Craft` and `Mode::Assistant`. One enum, persisted, consulted in exactly
two kinds of place: the view layer (what is rendered) and the model seam (what
may be constructed). Nowhere else.

## Part 1 — The first-run choice

### Behaviour

On a launch where no mode has ever been chosen, `sc-win` presents a modal before
the main window is usable. It is not dismissable by clicking away; there is no
default selection and no pre-checked radio. The user picks.

```
        How do you want to work?

  ┌─────────────────────┐   ┌─────────────────────┐
  │     Just code       │   │    Code with AI     │
  │                     │   │                     │
  │  Editor, files,     │   │  Everything in      │
  │  git, terminal.     │   │  Just code, plus    │
  │  No model is ever   │   │  the agent, chat    │
  │  contacted.         │   │  and review gates.  │
  └─────────────────────┘   └─────────────────────┘

        You can change this any time in Settings.
```

### Rules

- **No persuasion.** Neither option is recommended, defaulted, pre-selected,
  visually favoured, or listed with the other greyed. The two cards are the same
  size and weight. A user who came here because they distrust AI products will
  read a nudge as confirmation, and they will be right.
- **No dark pattern on the way back.** Switching Craft → Assistant is exactly as
  easy as the reverse: one control, same place, no confirmation dialogue, no
  "are you sure you want to lose these features".
- **Escape / window-close on the first-run modal quits** rather than picking for
  them. Choosing is cheap; being chosen for is the thing being avoided.
- The modal is **per install, not per project.** The answer is a statement about
  the user, not about a repository.

### Persistence

Mode lives in `config.json` (`%APPDATA%\smart-coder\config.json`) as a new
`ConfigFields` entry, so it flows through the existing hand-written serde in
[`config/file.rs`](../../crates/sc-win/src/config/file.rs) and the
env > file > default precedence in
[`config/load.rs`](../../crates/sc-win/src/config/load.rs).

The tri-state matters:

| Stored value | Meaning | Startup |
| --- | --- | --- |
| absent | never chosen | show the first-run modal (an ordinary build only — a `craft-only` build has one honest answer, so it never asks) |
| `"craft"` | chosen | Craft mode, no modal |
| `"assistant"` | chosen | Assistant mode, no modal |
| garbage | corrupt | treat as absent — ask again, never guess |

**`save_config` was extended.** It previously wrote only connection/routing
fields ([`config/load.rs`](../../crates/sc-win/src/config/load.rs)), which is why
`yolo`, `dry_run` and `verify_command` silently reset on restart. Mode joining
that set of unpersisted flags would have been a bug the user experiences as the
app ignoring them — the worst possible first impression for this feature. All
four now persist, along with the Unity editor path.

Two rules govern the flags, because `yolo` disables permission prompts:

- **Absent means the compiled default, never `true`.** A config file that lost
  the key must not come back with permissions loosened, and a non-boolean value
  (a hand-edited `"yolo": "yes"`) falls back rather than being read generously.
- **An unset flag is not written at all**, so "unset" never silently becomes
  "explicitly off" in a file people hand-edit.

The commit point is `commit_settings`, which runs on settings-*close* as well as
before a run. That distinction is load-bearing: the pre-run path never fires in
Craft mode, so anything committed only there would never persist for exactly the
users who have no model — including the Unity path, which is a Craft-mode
setting.

Unlike `mode` and the connection fields, these four are read from the **file
only**. None has ever had an env override and this change does not invent one, so
their precedence is file > compiled default. (`SC_UNITY_EDITOR` is a separate
`sc-iterate` fallback consulted when no path is configured there — not an
override of the path saved here.)

`SC_MODE=craft|assistant` overrides for testing and locked-down deployments,
following the existing env-var precedence — **except in a `craft-only` build**,
where `mode` is forced to `Craft` and neither the env var nor a `config.json`
carried over from an ordinary build is consulted. An org that wants Craft mode
mandatory sets the env var; the setting then displays as enforced rather than
silently refusing to change. An org that wants it *unavailable* ships the
craft-only build instead.

**A `craft-only` build never writes `mode`.** The filter sits in `save_config`
rather than only at the two mode-writing messages, because that function also
runs on ordinary connection edits — so saving a Gemini key would otherwise stamp
a mode into `config.json` that a later ordinary build would silently honour,
pinning a user into Craft with no record of their ever having chosen it.

### The craft-only build

A `craft-only` cargo feature on `sc-win` ships the editor with **no mode at
all** — for someone who doesn't want the choice, only the tool.

It is a flag on the ordinary binary, **not a separate crate graph**. The agent
crates still compile in, because Craft mode was always a *runtime* kill enforced
at the `Option`-returning builders (Part 2); a compile-time split would create a
second enforcement path that could drift from the runtime one, and the whole
argument for trusting this feature is that there is exactly one.

The feature pins three predicates, and that is the entire mechanism:

| Predicate | craft-only | Meaning |
| --- | --- | --- |
| `craft()` | always `true` | every guard that already consults it fires at once |
| `mode_chosen()` | always `true` | nothing to ask, so no first-run modal |
| `mode_switchable()` | always `false` | no toggle, and `mode` is never written |

Settings shows a **statement** — "This is a Craft-only build" — in place of the
toggle, and the detail copy drops the word "On". A disabled checkbox would be
worse than none: it implies a setting that exists and is merely unavailable, when
in this build there is nothing to switch to.

Both gates run in `scripts/check.ps1` and `check.sh`. A cargo feature is only
compiled when something asks for it, so without them the flag rots silently —
nothing in the default build would notice a `cfg(feature = ...)` block that
stopped compiling, or a test whose assumptions the pinned mode invalidates.

## Part 2 — Craft mode is a hard kill

This is the part that earns the feature's trust, and it is a *structural*
requirement rather than a UI one.

### What must be true

In Craft mode, for the entire lifetime of the process:

- `UiConfig::backend()`, `backend_cancellable()`, `orchestrator()`, `advisor()`
  and `swarm_advisor()` are **never called**.
- No health probe is scheduled. The `HealthTick` subscription is not merely
  ignored — it is not subscribed.
- No `ChatSession` or `Session` worker thread is spawned.
- The remote mirror (`SC_REMOTE`) **refuses to start**, and says why. A phone
  attaching to a Craft-mode desktop is a model surface arriving through a side
  door.
- The compliance engine runs with `ComplyModel::None`, and the picker offers no
  other value. The deterministic control scan is unaffected — it never used a
  model ([13](13-compliance-evidence.md)).
- **Net effect: the process makes no outbound network request attributable to a
  model backend.**

### How it is enforced

Not by discipline at ~15 call sites. By making the wrong thing hard to write:

**The backend builders return `Option`.** `UiConfig::backend()` and its siblings
consult mode and return `None` in Craft mode. Every existing caller already sits
on a worker thread that can fail, and every one of them already has an error
path for an unreachable endpoint — so they degrade into that path rather than
needing new handling. A future contributor who adds another call site gets the
right behaviour without knowing this spec exists. That is the property worth
paying for. (`advisor()` already returns `Option`, so this is a change for four
of the five, not all of them.)

**The `Option` return does not cover the health probe**, and that gap is the one
this spec's Principle names by name. `tick_health_probe`
([`app/logic_c.rs`](../../crates/sc-win/src/app/logic_c.rs)) calls
`sc_model::OpenAiBackend::new` **directly** on a spawned thread — it never goes
through a builder. So the single caller that dials out on a timer is precisely
the one the builder seam would miss. Mode must therefore also be consulted where
the subscription is wired: in Craft mode the health tick is *not registered*, not
merely ignored. Any other direct `OpenAiBackend::new` outside
[`config/build.rs`](../../crates/sc-win/src/config/build.rs) is the same hole.

**The test is the real contract**, and it must assert on *construction*, not on
builder calls — otherwise it reproduces the same blind spot. A test asserts that
with `Mode::Craft` set, driving the app through chat-send, run-start, health-tick
and remote-attach produces zero `OpenAiBackend` constructions. Structure it the
way [11](11-testing-and-tdd.md) asks: the assertion is the specification, and it
fails loudly if someone reintroduces a path.

This mirrors principle 9 in [00](00-overview.md) — *don't use a model where
determinism will do*, and where it must not be used, **remove it from the path
rather than constrain it, and structure the code so it cannot creep back**. That
is exactly the move [13](13-compliance-evidence.md) makes for evidence packs.
Craft mode applies it to the whole application.

### What is *not* claimed

Be precise, because overclaiming here is worse than underclaiming:

- Craft mode is **not** a general network kill switch. `git push`, `git fetch`
  and anything the user runs in the integrated terminal still reach the network,
  obviously and by design.
- Craft mode makes **no claim about telemetry** beyond model calls, because
  `sc-win` has none to disable.
- The claim is exactly: *no language model is contacted*. The UI should say that
  sentence and not a grander one.

## Part 3 — The editor

**Before this spec, `sc-win` had no editor.** The CODE pane was
`container(text(...))` rows with a mouse-drag line selection used for
*commenting* ([`view_code.rs`](../../crates/sc-win/src/app/view_code.rs)): no
caret, no keyboard path into the buffer, no dirty state, no save.
[`lib.rs`](../../crates/sc-win/src/lib.rs) said "no code editor" outright. The
only `text_editor` in the crate rendered chat bubbles and deliberately discarded
edits.

So this was new construction, and it was the majority of the work.

### The pane splits in two

The existing read-only pane is not a failed editor — it is a **review surface**,
and a good one. It interleaves widgets *between* code lines: red removed-line
rows, revert-block bars, stored inline comments, the open comment box, and
per-line background washes for diff state and the pulsing agent-working range.
An `iced::text_editor` is a single leaf widget and can do none of that.

Trying to make one widget serve both purposes is where this design would fail.
Instead:

| | **Review view** (exists) | **Edit view** (new) |
| --- | --- | --- |
| Widget | per-line `container(text)` rows | `iced::widget::text_editor` |
| Purpose | reading a diff, line comments, gates | typing |
| Caret | none | yes |
| Inline widgets | yes — comments, revert bars | no |
| Per-line wash | yes — diff/working colour | no (see below) |
| Available in Craft mode | yes (git diffs still matter) | yes |

A tab is in one view or the other, switchable, and the choice is remembered per
tab. Defaults that match intent: opening a file from the **file tree** opens the
edit view; opening one from the **git panel** or via `follow_agent` opens the
review view. In Craft mode the review view is still reachable — reading a diff
has nothing to do with models — but the default everywhere is edit.

This preserves every existing Assistant-mode behaviour untouched, which is the
main risk this spec has to manage. Line comments, send-back harvesting, the
minimap viewport box, `scroll_code_to_line`, gate buttons on the tab strip: all
continue to work in the review view exactly as they do now.

### Tabs become real

`open_tabs: Vec<String>` — bare paths, with all editor state global to the pane —
cannot survive an editor. Switching tabs currently resets scroll to top; with
unsaved buffers it would lose work.

Promote to a per-tab struct owning:

- `path`
- `content: text_editor::Content` — the buffer
- `dirty: bool`
- `view: TabView` — edit or review
- cursor position and scroll offset
- `disk_mtime` and the bytes-on-open hash, for conflict detection (below)
- undo/redo — whatever `text_editor::Content` provides natively, not a bespoke
  stack

Tab strip shows a dirty marker. Closing a dirty tab prompts; closing the window
with dirty tabs prompts once, listing them.

**The window-close prompt needs two halves and is useless with one.**
`exit_on_close_request(false)` makes iced hand over the request instead of
obeying it, and a `CloseRequested` subscription arm turns it into a message; with
the flag alone the window becomes unclosable, and with the arm alone the close
still happens. The title-bar ● is a *warning*, not a guard — it was for a while
mistaken for one, on the belief that an OS close could not be intercepted.

The prompt **names the files**, in full relative paths rather than basenames. "You
have unsaved changes" alone makes the user guess what they are about to lose, and
with several panes open the dirty buffer is frequently not the one on screen —
which is also when two same-named files from different directories need telling
apart. Save-all writes every dirty buffer in every pane, and **a refused save
cancels the quit**: a save-conflict must not be steamrolled by the quit it was
blocking.

This is also why `save_tab` addresses the pane that *holds* the path rather than
the focused one. Saving a background pane's buffer was previously a silent no-op,
which the save-all path made reachable.

**A tab is also the handle you drag it by, so neither its label nor its ✕ may be
a `button`.** A button calls `shell.capture_event()` on press, and `mouse_area`
skips its own handler once a child has captured — so a button anywhere inside a
tab swallows the drag before it begins. The label is a plain container; the ✕ is
an inner `mouse_area`, which fires without capturing and so lets the outer one
see the press too.

Selection therefore happens on **a release that never moved**, not on press.
Selecting on press would switch you to the tab you were about to drag away, which
reads as a flicker at the start of every drag. Movement is measured against a 4px
threshold: a tab has two jobs where a panel header has one, and without a
threshold every click is a one-pixel drag.

The threshold is measured against the app's window-space cursor position, **never
against a position reported by the tab's own `mouse_area`**. A `mouse_area` inside
a `scrollable` receives *content-space* coordinates — iced translates the cursor
by the scroll offset before handing it down — so the two are different frames of
reference. For the same reason, dropping a tab on another pane's strip **appends**
it; choosing a position within the strip is deferred, since resolving an index
needs that same untracked offset.

### Data-loss hazards that must be fixed first

`CodeView` was written for a viewer, and three of its properties become
**silent data corruption** the moment a save path exists. These are not polish;
they are correctness blockers and the editor must not ship without them
resolved.

1. **`MAX_LINES = 4000` truncation.** Files past 4000 lines are *not fully in
   memory*. Saving such a buffer destroys everything after line 4000. The edit
   view must load the whole file, or refuse to open the file for editing at all
   and say so. Truncation is acceptable in the review view (it only reads);
   it is never acceptable in the edit view.
2. **Lossy UTF-8.** `String::from_utf8_lossy` replaces invalid sequences with
   U+FFFD. Saving round-trips that corruption onto disk. The edit view must
   detect non-UTF-8 and open read-only with an explanation rather than
   pretending.
3. **Line endings normalised away.** `.lines()` discards `\r`; the existing
   splice rejoins with `\n`. A CRLF file silently becomes LF on save — on
   Windows, in a git repo, this presents as every line changed. **Detect the
   file's dominant ending on load, store it per tab, and restore it on save.**
   This is a Windows-first client operating in a git repo; shipping an editor
   that silently rewrites every line ending is not defensible.

A fourth, from the same family: **there is no file watching anywhere in the
workspace** (`notify` is not a dependency). Today that is safe because nothing
can be dirty. With an editor, the agent writing a file you have open and
modified is a data-loss race with no guard.

The policy should follow the precedent already set by `locate_range` in
[`linecomment.rs`](../../crates/sc-win/src/linecomment.rs), which re-anchors by
content and **refuses when ambiguous rather than guessing**:

- On save, compare `disk_mtime` against the value recorded at open.
- Unchanged → write.
- Changed and the buffer is clean → reload silently.
- Changed and the buffer is dirty → **refuse, and surface the conflict.** Never
  silently clobber, in either direction.
- `follow_agent` must not steal a tab with unsaved changes.

Whether this uses polling or a `notify` watcher is an implementation choice; the
mtime check on save is the non-negotiable floor.

### Editing capability

The bar is the user's phrase — "super fast easy to use IDE" — with **fast** read
as the load-bearing word. This is an editor that opens instantly and never
stutters, not one that accumulates features.

Core, required for the mode to be worth shipping:

- Type, with caret, selection, and the standard keyboard conventions.
- Save (`Ctrl+S`); dirty state visible. (Save-all and `Ctrl+G` go-to-line were
  in this list and are deferred — see "What shipped, and what did not".)
- Multi-tab editing with per-tab cursor and scroll retained across switches.
- Undo/redo per buffer.
- Find and replace within the file (`Ctrl+F` / `Ctrl+H`), with match count.
- Go to line (`Ctrl+G`).
- New file, and save-as, from the file tree.

Deliberately deferred, and worth naming so they are decisions rather than
oversights: multi-file search-and-replace, LSP/completion/diagnostics, format-on-save,
multi-cursor, code folding, bracket matching. Each is a real feature; none is
required for "just code" to be true, and every one of them threatens *fast*.

**Syntax highlighting** came from the editor widget, not from a feature flag.
The plan was to enable `iced 0.14`'s syntect-backed highlighter; in the event
`iced-code-editor` (pinned `=0.3.11`) was adopted for the edit view and brings
highlighting, folding, undo and search with it, so no iced feature was taken.
Note that highlighting produces font and colour only — it cannot paint the
per-line background washes the review view depends on, which is a further reason
the two views stay separate.

**What shipped, and what did not.** Typing, `Ctrl+S`, per-tab cursor and scroll,
dirty markers, the close/quit prompts and the conflict refusal are all in, as are
undo/redo and find/replace — the latter two from the widget rather than from
`sc-win`. Save-all exists only as the quit prompt's *Save all and quit*; there is
no `Ctrl+Shift+S`. **Deferred rather than required:** a save-all binding,
go-to-line, and new-file/save-as from the tree. Files reach the editor through the file tree, the
git panel, or a click on a compile diagnostic.

## Part 4 — Modular panels

The requested behaviour: **panels arrange like VS Code — one column, two, three,
the user's choice.**

### Why the current layout can't express that

`view()` hardcodes three children in a row — explorer, centre, code — with the
explorer pinned at 20% and `chat_frac` splitting the remaining 800 portions
([`app/view_core.rs`](../../crates/sc-win/src/app/view_core.rs)). Drag handling
in [`app/update.rs`](../../crates/sc-win/src/app/update.rs) carries literal
geometry (`0.20 * window_w`) and a hand-summed stack of chrome constants to
guess the explorer's height. Panel identity is *positional*: each
`view_*` method has a different signature and applies its own sizing internally.

So "hide the chat column and give the space to the editor" is a special case in
a layout that has only special cases. The user asked for the general thing
instead, which is the right instinct — it makes Craft mode fall out as data
rather than as a branch.

### The shape

A recursive layout tree, persisted:

```
Layout := Leaf(PanelKind)
        | Split { id: String, axis: Horizontal | Vertical, a: Layout, b: Layout }

PanelKind := Files | Git | Editor(EditorId) | Bottom | Chat
```

`Editor` is the one kind that may appear **more than once** in a tree, and its
`EditorId` keys the pane holding that strip's tabs. Every other panel renders one
piece of app state, so a second copy would be the same view twice. `EditorId::FIRST`
serialises as the bare `editor` the app already persisted, so existing layouts —
and the split ids in `splits.json` that are built from slugs — load unchanged.
See "Several editor panes" below.

Two details settled during implementation, both load-bearing:

**The fraction is not in the tree.** A node carries a stable `id`, and the
fraction lives in the existing `SplitStore` (`id → f32`, with its NaN/range
rejection). That keeps `Layout` a pure `Eq` structure — whole trees can be
compared in tests — and it is what makes the migration free: reusing the ids the
app already persisted means an upgrading user's divider positions load straight
into the new tree with no migration code.

**Terminal is not a panel.** It is a tab inside `Bottom`, alongside Problems, so
the bottom strip is one leaf. Making every bottom tab its own panel would have
put four near-empty panels in the tree to no benefit.

Requirements this imposes:

- **Uniform panel signature.** `fn view_panel(&self, kind: PanelKind) -> Element`.
  Sizing is applied by the tree walker, never by the panel itself.
- **Per-node drag state.** Replace the two bespoke fields and two bespoke
  messages with `dragging: Option<(NodeId, Axis, f32)>` for divider drags.
  Picking a panel *up* is a second, separate state — `drag: Option<DragSubject>`,
  one field covering both a panel carried by its header and a tab carried out of
  its strip, so the shared drop machinery can never be handed two things in
  flight at once.
- **Real bounds, not guessed ones.** The hardcoded `0.20 * window_w` and the
  chrome-constant arithmetic must go; each split node needs its actual rect.
  This is the one genuinely fiddly part, since iced does not hand bounds back
  from a `mouse_area` — it needs a bounds-reporting mechanism per pane.
- **`splits.rs` needs essentially no change.** It is already an arbitrary
  `id → fraction` map with NaN/out-of-range rejection, which is exactly a tree's
  persistence layer. Keyed by node id, it just works.

### Several editor panes

One editor was a limit of the model, not a decision. The gesture users expect is
VS Code's: **drag a tab out to open a second editor, drag it back to close that
one.** That requires editor panes to have identity, which the rest of the tree
does not need.

**Panes are keyed by `EditorId`, and ids are never reused.** A monotonic
allocator means a stale id — one left in a queued `Task`, or in a `PanelSlot`
persisted before a pane closed — resolves to nothing, rather than silently
addressing a *different* pane that inherited the number. `EditorPane` owns what
is genuinely per-pane: its tabs, selected file, scroll position, viewport, diff
and comment state. `App` keeps what is per-*file* — the save-conflict slot, the
git status map — because those are path-keyed, which the next rule is what makes
sound.

**A file may be open in exactly one pane.** Asking for one already open elsewhere
focuses that pane. This is a data-loss rule, not a preference: a `Tab` owns its
live buffer, so two tabs on one path means two buffers, two disk stamps, and one
path-keyed conflict slot between them. Edit and save in A, then edit and save in
B, and B reports a conflict caused by you ten seconds earlier — and its Overwrite
destroys A's work. The proper fix is one document with N views, but the editor
widget owns its own cursor, scroll and undo stack; that is a larger project and
must not be smuggled inside this one. Because a tab drag is a **move**, the
natural gesture for "put this over there" still does the right thing.

**A tab drag resolves to a pane, never to a tree edit of its own.**

| Released on | Result |
|---|---|
| Another pane's tab strip | A pure move. No layout change. |
| A pane edge, or a window dock band | A **new pane** opens there (`Layout::with_at_edge` for a full-span edge, `insert_at` beside a panel) and the tab moves into it. |
| Its own pane, or nothing | Nothing. |

**A `Tab` is relocated whole, never reopened.** It carries its live buffer, dirty
flag and disk stamp, so re-opening it at the destination would discard unsaved
edits and re-read from disk — silent data loss dressed up as a layout gesture.

**Emptied panes close; the last pane never does.** A pane that empties without
closing is a dead rectangle you cannot get rid of: there is no tab in it to drag,
and its only remaining affordance is the View menu. Pruning lives in one function
called after anything that removes a tab, because a *drag* empties a pane without
any close path running. The last pane survives however empty — the same rule as
"never hide the last editor", since an IDE with nowhere to open a file is a broken
window rather than a layout choice. Focus is retargeted **only** when the pane it
pointed at is actually gone.

Because these invariants are the kind a "remember to call X afterwards"
convention lets rot, a debug-only assertion checks them at the end of every
update: every editor leaf has a pane, focus points at a live pane, no pane is
empty unless it is the only one, and no path is open twice.

**Splitting a single-tab pane makes an empty second pane** rather than moving the
tab. Moving a pane's only tab would empty it, and pruning would then close it —
collapsing straight back to one pane.

### How mode uses it

Craft mode ships a **default layout without an Assistant panel in it** — explorer
plus editor, terminal below. Assistant mode's default is today's three-column
arrangement. Both are just trees; neither is privileged.

Switching mode does not destroy the other mode's arrangement. Persist a layout
*per mode*, so a user who toggles back and forth finds each as they left it.

If a Craft-mode layout somehow contains an Assistant panel — a config edited by
hand, a layout carried across a mode switch — the tree walker **omits it and
rebalances**, rather than rendering an empty frame or refusing to start. Same
rule as a corrupt split fraction: fall back, never wedge.

## What Craft mode hides

Precise, because "hide the AI bits" is not implementable as written. The
surfaces below are absent in Craft mode, all of them Assistant-only today:

| Surface | Where |
| --- | --- |
| Chat panel, turns, streaming bubble, composer | `app/view_menus.rs` |
| Proposed-file Apply / Breakdown / Build cards | `app/view_menus.rs` |
| Proposed-command Run card | `app/view_menus.rs` |
| Gate controls (Approve / Send back / Abort), both placements | `app/view_menus.rs`, `app/view_code.rs` |
| Shell-confirm approval gatebar | `app/view_menus.rs`, `bridge.rs` (crate root) |
| Backend health badge | `app/view_panels.rs` |
| Settings → Connections and Routing tabs | `app/view_menus.rs` |
| Step-flow phase strip | `app/view_panels.rs` |
| Swarm topology canvas and coder I/O | `app/view_menus.rs`, `canvas.rs` (crate root) |
| Build and Verification bottom tabs | `app/view_panels.rs` |
| Line-comment → triage → auto-fix | `app/view_panels.rs`, `app/logic_a.rs` |
| Compliance model picker (forced to `None`) | `app/view_comply.rs` |
| Remote mirror | `app/mod.rs` |

Everything else — file tree, git panel and diff engine, terminal and its sandbox,
code review view, minimap, tabs, menu bar, recents, settings shell, splits — is
already model-independent and stays.

Two subtleties worth stating, since both are places where a naive
hide-the-widget implementation would leave something broken:

- **Line comments are not purely an AI feature.** They are also how *Send back*
  harvests revision notes. Removing the auto-fix path in Craft mode must not
  break the review view's comment display in Assistant mode.
- **The Settings modal shell is generic; only its contents are model config.**
  Craft mode keeps Settings and shows the mode toggle, editor preferences and
  layout — not an empty dialogue.

## Switching modes

Craft → Assistant and Assistant → Craft, both live, no restart:

- Craft → Assistant: subscribe the health probe, restore the Assistant layout,
  render the chat column. No model call happens until the user makes one.
- Assistant → Craft: cancel any in-flight session first, then drop the
  Assistant surfaces. A run must never survive the switch into a mode that
  claims no model is contacted.
- Unsaved editor buffers survive a mode switch in both directions. The mode
  changes what surrounds the editor, never its contents.

## Delivery

Three parts, and the ordering is not arbitrary — **the editor gates the mode**.
Shipping the toggle first produces a mode whose entire promise is "you can just
code" over an app that cannot edit a file. That is worse than not shipping it.

1. **Editor.** Per-tab state; the three data-loss fixes (truncation, encoding,
   line endings), the save-conflict guard and the close/quit prompts;
   edit/review split; typing, save, find/replace, undo. Lands in Assistant mode,
   useful immediately, independent of any mode work. (Go-to-line was in this
   list and is **deferred** — see "What shipped, and what did not" above, which
   is the later decision.)
2. **Panels.** The layout tree, uniform panel signatures, per-node drag, real
   bounds. Also useful on its own; also mode-independent.
3. **Mode.** The enum, persistence with the tri-state, the first-run modal, the
   `Option`-returning backend builders, the zero-backend-construction test, the
   per-mode default layouts.
4. **Compile & check.** Project-type detection, the Problems panel, the compile
   button, parsed clickable diagnostics; Unity first, then the toolchains that
   fall out of the same seam. Last only because it needs the editor to click
   *into* — but it is what makes Craft mode a place you can actually work, and
   it improves Assistant mode too by replacing the Verification tab's wall of
   text with a navigable list.

Each part stands alone and improves the product without the other two. Part 3 is
small once 1 and 2 exist, which is the strongest evidence the decomposition is
right.

## Part 5 — Compile and check: the Unity case

An editor that can't tell you whether the code compiles is a text editor. In
Assistant mode the agent runs verification and reports back; **in Craft mode
there is nobody to ask**, so the loop the model was closing has to close some
other way — a button, and a list of errors you can click.

This is the part of Craft mode most at risk of being under-built, because in
Assistant mode it looks redundant.

### Detection, not configuration

The client already detects a verify command by looking at the workspace
(`detect_verify_command` picks pytest / vitest / cargo test from what it finds).
Project *type* follows the same rule: recognised from the tree, never asked for.

A **Unity** project is one with an `Assets/` directory and a
`ProjectSettings/ProjectVersion.txt` — the latter also states the exact editor
version, which is what makes a headless build reproducible rather than a guess.

Unity is the named case here because it is the one that motivates this section,
but it must not be special-cased into a corner: the seam is *project type →
compile command → parsed diagnostics*, and Unity is the first implementation of
it. Cargo, dotnet and npm fall out of the same shape.

### What the button does

- **Compile** runs the toolchain's headless build. For Unity that is the editor
  binary with `-batchmode -quit -projectPath . -logFile -`, which compiles the
  assemblies and exits non-zero on failure. It does **not** enter play mode and
  does not need the GUI editor open.
- **Diagnostics are parsed, not dumped.** A wall of log output is what the
  terminal already does. The value here is a list: file, line, column, message,
  each one clickable straight into the editor at that position. Unity's compiler
  errors come through in the standard C# form
  (`Assets/Foo.cs(12,7): error CS0103: ...`), which is a stable parse.
- **Errors and warnings are counted and separated**, so "did it build?" is
  answerable at a glance rather than by reading.
- Running is **cancellable** and never blocks the UI thread — a cold Unity build
  is minutes, not seconds.

### Where it lives

A **Problems** panel, beside Terminal in the bottom strip, present in both modes.
In Assistant mode it is also where the agent's verification output belongs, which
means this is not Craft-only work — it replaces the Verification tab's wall of
text with something navigable, and both modes gain from it.

The compile button sits in the panel and is enabled when a project type is
recognised. When none is, the panel says which toolchains it looked for rather
than showing a dead button — the same rule as everywhere else here: state the
reason, never fail silently.

### Constraints worth stating

- **Unity must be located, not assumed.** The editor version comes from
  `ProjectVersion.txt`; the install path is machine-local (Hub installs sit under
  a well-known root, but not reliably). Look it up, allow an override in
  Settings, and if it can't be found say so with the version that was wanted.
  The override **persists** (see Part 1 → Persistence) — a path the user retypes
  every launch is not really an override. A compile reads the path currently
  *typed*, so a correction takes effect without closing the panel; committing is
  what makes it survive a restart.
- **The build is a real subprocess** and must go through `proc::command` so it
  doesn't flash a console window on Windows — the existing rule for every spawn
  in this client.
- **Unity holds a project lock.** A headless build while the GUI editor has the
  project open will fail. Detect that case and say so, rather than surfacing
  Unity's own less obvious error.
- This is **not** a Unity integration. No play mode, no asset pipeline, no editor
  extension. Compile and report — nothing that requires understanding Unity
  beyond invoking it.

## Corrections found during implementation

Two claims made earlier in this spec turned out to be wrong once the editor was
built. Both are recorded because the reasoning that produced them was sound and
would otherwise be repeated:

1. **Line endings are not a data-loss hazard in the way described above.**
   `core::text::editor::Line` carries a per-line `LineEnding` and `Content::text()`
   writes each line's own ending back, so CRLF round-trips without help. Detecting
   the dominant ending is still worth doing — a buffer that goes *mixed* mid-edit
   needs a decision — but it is an assertion, not machinery.
2. **"Use whatever undo the widget provides natively" was not possible.** iced
   0.14 provides no undo at all. This became moot when the editor widget was
   adopted (it brings its own), but the instruction as written was unfollowable.

A third hazard appeared that this spec did not anticipate, and it is the one that
actually bit: **the editor widget's `content()` rejoins its lines and drops a
trailing newline.** Nearly every source file has one, so every save would have
rewritten the last line of every file touched. Whether the file ended with a
newline is now recorded on open and restored on save. The general lesson is the
one this spec already argues — *round-trip fidelity is the property to test*,
and the test is what caught it.

Two more of the same family, both handled in
[`editbuf.rs`](../../crates/sc-win/src/editbuf.rs): a **UTF-8 BOM** is stripped
on load and restored on save (Windows tooling writes them; dropping one is a
spurious whole-file diff), and the edit ceiling is **two limits, not one** —
20 000 lines *and* 2 MiB, because a file can be short and enormous.

**The `Option`-returning builders were nearly not built.** The first
implementation used `if craft { return; }` guards at the call sites instead —
precisely the "discipline at ~15 call sites" this spec rejects. It passed its
tests, because those tests asserted on proxy state (`session.is_none()`) rather
than on construction. A spec audit caught it: five callers in `session/` and
`chat_session.rs` reached the builders unguarded, protected only by whatever
happened to sit upstream. The builders now return `Option`, the compiler found
every one of those callers, and the test asserts on construction as this spec
said it must. Recorded because the failure mode is instructive: **a guard that
works today, tested through a proxy, looks exactly like a seam that works
forever.**

Two constraints from the codebase that will bite during implementation:
[`app/logic_c.rs`](../../crates/sc-win/src/app/logic_c.rs) (806 lines) and
[`app/types.rs`](../../crates/sc-win/src/app/types.rs) (752) are the two largest
files in the `app/` module, so per-tab state and mode logic need new modules
rather than growth. And `App` is a ~90-field flat struct with no screen
enum; mode should be one field consulted by the view layer and the model seam,
not a new axis threaded through everything.

## Relationship to other specs

- The model seam this switches off is `ModelBackend` ([02](02-model-backends.md)),
  reached through `UiConfig`'s builders. Craft mode is a new consumer of that
  seam — one that declines to use it.
- The client is [12](12-platform-clients.md), which describes `sc-win` as a thin
  shell over a portable core. Craft mode is that claim tested: if the shell can
  run with the core's model path entirely unused, the seam was real.
- Removing the model from the path rather than constraining it is principle 9 of
  [00](00-overview.md) and the design of [13](13-compliance-evidence.md), applied
  at application scope instead of feature scope.
- The gates and review surfaces Craft mode hides are
  [09](09-workflow-and-checkpoints.md); the review *view* survives because
  reading a diff was never a model feature.
- The remote surface ([20](20-remote-review.md)) is refused in Craft mode: it
  exists to bring model output to a phone. The trust boundary that makes it a
  side door is [18](18-task-intake.md)'s — a surface running *away from* the
  workspace — and Craft mode declines it.
- [00](00-overview.md) needs two edits if this ships — narrowing the
  editor/IDE non-goal to third-party plugins, and admitting a target user who
  does not want an assistant. [17](17-spec-traceability.md) should catch it if
  they are missed.
