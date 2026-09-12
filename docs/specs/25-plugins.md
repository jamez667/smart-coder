# 25 — Plugins: the agent becomes something the IDE loads

## Principle

[21](21-craft-mode.md) split the editor from the agent and made the separation
structural: `sc-craft-ui` is an editor whose dependency tree contains nothing
that can reach a model, and `scripts/check.*` asserts it with `cargo tree`.

That spec described two products. This one collapses them back into **one**:

> **Smart Coder Crafter is the IDE.** The agent and the Claude Code panel are
> plugins it loads. "No agent" is not a different build — it is no plugin
> installed.

The IDE then sits alongside `sc-cli`, `sc-tui` and `sc-android` as one more
front-end over the same core ([12](12-platform-clients.md)). The agent plugin is
a *fourth shell*, which is why this is not a rewrite: the CLI and TUI already
prove the core does not care who drives it.

The commitment that shapes everything below, and the one most likely to come
under pressure later:

> **The agent gets no privileged channel.** It uses the same protocol as any
> other plugin, and waits for the same capabilities. An API whose most demanding
> consumer is exempt from it stops being exercised and quietly rots — which is
> how most plugin systems die.

## Why not a second application

The alternative was two desktop binaries: the editor, and the editor-plus-agent.
It was rejected on a measurement. The current `App` is **59 editor fields and 59
agent**, and `Message` is **84 editor variants and 72 agent** — an even split,
with the editor half spread through files that interleave (`view_code.rs` needs
~45 lines cut from the middle of one function; `view_core.rs` four lines from
`__view_inner`).

Two binaries therefore means duplicating ~6,000 lines of view and logic that
immediately begin to drift, with nothing to keep them in step. One app with a
plugin boundary has one copy of the editor and one place to fix it.

## Loading, and the restart

**Plugins are discovered and loaded at startup. Enabling or disabling one
requires a restart.** There is no hot-swap.

This is not a simplification to be apologised for — it is what VS Code does.
Installing or disabling an extension prompts "Reload Window"; the hot path
exists only for extension *development*.

It is also the decision that makes the rest tractable. `App` is one struct and
`Message` one enum; hot-loading means both change shape while iced is rendering
— panels appearing mid-frame, in-flight work to cancel, plugin state to detach
cleanly. With startup loading the plugin set is known before `App::default()`
runs, so the app is built once for that configuration and never mutates.

## Transport: a subprocess, not a dynamic library

A plugin is **a child process** speaking line-delimited JSON over stdin/stdout.
Not a `.dll`/`.so`.

The comparison people reach for is VS Code, and it is worth being exact about
what VS Code actually does: its extensions are *a separate process over a stable
wire protocol*. The isolation everyone likes about them comes from the process
boundary, not from dynamic linking.

Native Rust plugins were rejected on four counts, and none of them shrink with
effort:

- **Rust has no stable ABI.** A plugin would have to be rebuilt for every
  compiler version the host is built with, and a mismatch is undefined
  behaviour rather than a clean error.
- **Panics cannot cross the boundary safely.** A plugin bug takes the editor
  with it, including unsaved buffers.
- **`iced` types cannot cross it at all**, which rules out the panel API.
- **A third-party plugin becomes in-process arbitrary code** with no seam at
  which to ever add a sandbox.

The usual argument for a dylib is IPC cost on a hot path. There is no such path
here: an agent run is seconds of model latency and subprocess spawns, against
which JSON framing is noise. Where a genuinely hot path appears later — language
features — the answer is LSP, not a tighter binding (see below).

### The pattern already exists here

Claude Code is already driven exactly this way, and got the important decisions
right the first time. The plugin host generalizes it rather than inventing
anything:

<!--@ crates/sc-win/src/claudecode.rs -->

Two rules are lifted from it verbatim.

**Translation is pure and lives apart from spawning.** `parse_line()` is a
function from one line of JSON to events; it needs no child process, so the
whole format contract is proven on the host against recorded fixtures. The
plugin protocol parser has the same shape and the same testability.

**An unparseable line is ignored, never fatal.** The existing comment states the
reason better than a restatement would: *the format belongs to another project
and will gain fields; a run must not die because one line was unexpected.* That
is precisely the forward-compatibility rule a plugin protocol needs, and it was
already written down. The host counts what it ignored so the silence is
reportable rather than invisible — see the Plugins panel below.

The one thing Claude Code does not do is listen. It streams out; nothing goes
in. Plugins need requests *into* the host, and that is the genuinely new
machinery in this spec.

<!--@ crates/sc-win/src/session/claude.rs -->

### One protocol definition, shared

The wire types live in their own crate, `sc-plugin-proto`, for the reason
`sc-proto` already documents about itself: both ends share one definition, and
neither has to depend on the other to obtain it. Restating a protocol on the far
side is exactly the drift [17](17-spec-traceability.md) exists to catch, so it
is prevented by construction instead of detected later.

<!--@ crates/sc-proto/src/lib.rs -->

## What a plugin may do

v1 is **six capabilities**: panels, commands, buffer read, buffer edit, events,
diagnostics. Roughly 22 message types.

The size is deliberate and is the main defence against the API rotting. A
smaller stable surface beats a larger unstable one, and the way to discover
whether a deferred capability was the right one to defer is to make a real
plugin want it.

### Panels are a view model, not widgets

The most consequential decision in the spec. A plugin pushes **a small
declarative view model** with exactly four content kinds:

| kind | renders as | covers |
|---|---|---|
| `list` | rows with optional detail, clickable to a command | diagnostics, file lists, search results, run feeds |
| `text` | markdown, through the existing renderer | chat turns, summaries, blame output |
| `form` | labelled inputs and buttons | settings, composers |
| `stack` | vertical composition of the above | everything else |

No pixels, no colours, no layout.

The alternative — exposing a widget tree — is an API over `iced`, and `iced` is
pre-1.0 and moves. Taking it would bound the plugin protocol's stability to
`iced`'s, which is not a promise anyone can make. Four kinds is a promise that
*can* be kept, and it renders in the app's existing visual language for free —
which matters, because a plugin panel that looks foreign is worse than no plugin
panel.

This will not be enough for someone. When that happens the answer is to version
the content model deliberately, **not** to leak renderer types. Stating that
here is cheaper than defending it under pressure later.

Content is **pushed** when the plugin's state changes; the host caches the last
content and repaints from cache. A pull-per-frame model would put a synchronous
round trip on the render path, which is the mistake this codebase has already
paid for twice — the file tree is cached because re-walking per frame *"made
filtering laggy"*, and the workspace sync was slowed from 500ms to 2s because a
steady drip of git spawns *"is felt as typing lag"*.

### Buffer edits, and why they survived the spike

`buffer.edit` carries the buffer version the plugin last saw. A mismatch is
rejected with `version_conflict` and nothing is applied — without that, a slow
plugin's edit computed against text the user has since changed corrupts the file
silently. This is the same hazard `save_conflict` already defends against on the
disk side, and the same answer: refuse, do not guess.

Multiple edits in one request apply **all or nothing**, with positions
interpreted against the original text. Partial application is unreconstructable.

**Plugin edits join the undo stack as a single entry, attributed to the plugin.**
A plugin edit the user cannot `Ctrl+Z` is a plugin edit the user will not
forgive. This was the one capability at risk of being cut, because it depended on
an editor behaviour nobody had checked. It was checked:
`iced-code-editor 0.3.11` exposes a command-pattern history with
`CommandHistory::push`, and `begin_group(description)` / `end_group()` to fold
several commands into one composite entry — with a description, so the entry can
be labelled. `TextBuffer::replace_range` is the primitive the protocol's range
edits map onto. The machinery exists and is designed for exactly this.

### Events carry versions, not content

`buffer.changed` carries the version and is debounced to ~200ms. It never
carries the text. A per-keystroke notification with content means a 1000-line
file is re-serialised to JSON on every keypress, for every subscribed plugin. A
plugin that wants the text after a change asks for it.

### Diagnostics reuse what exists

`diagnostics.publish` names a `source`; the host merges by source and renders
them in the existing Problems panel. This is low-risk precisely because the data
model is not new — `Diagnostic` already carries file, 1-based line and column,
severity, optional code and message, and `OpenDiagnostic` already jumps to them,
both proven against real toolchain output.

Diagnostics **replace** by source and path rather than appending. A plugin
publishing on every change while appending produces a growing pile of stale
problems.

## What is deferred, and why

**Decorations** (line highlights, gutter icons, inline hints) are deferred as the
API most coupled to the renderer. `CodeView`, the minimap, line comments and the
git changed-line markers all already draw into the same gutter and margin; a
plugin decoration API has to define priority and z-order against every one of
them, and getting it wrong lets a plugin hide the user's own review comments. It
was also confirmed during the spike that the editor canvas exposes no general
decoration surface today, so this is new work in two places rather than one.
Design it against a concrete plugin that wants it — the agent will be that
plugin.

**Language features** are deferred, and the spike changed *how*. The instinct was
to defer them because this is LSP, and rebuilding LSP badly is a well-trodden way
to waste a year: completion alone is a request/cancel/resolve dance with
incremental filtering and a hard latency budget, and a plugin round trip per
keystroke is a performance problem before it is a feature.

But `iced-code-editor` **already ships an LSP process client** — `LspProcessClient`,
`LspEvent`, a server config type, and hover/completion overlay rendering. So the
eventual answer is not "build a language API"; it is
`language.server.register` with a command line, a tiny protocol surface that
gets the whole of LSP for free. That is now a known-reachable v2, not an
aspiration.

This also keeps faith with [00](00-overview.md)'s non-goal as narrowed by
[21](21-craft-mode.md): reuse the existing standard rather than invent a
parallel one.

**Settings pages** and **themes** are deferred. A plugin can render a `form`
panel today, and `styles.rs` is 588 lines of app-wide constants whose exposure
is a visual-consistency problem rather than a protocol one.

Every deferral is cheap to reverse: capabilities are negotiated at handshake, so
adding one later needs no protocol version bump.

## Dynamic panels

`PanelKind` gains a `Plugin` variant. Three constraints govern how.

**It must stay `Copy`.** `panels()`, `contains`, `dedup`, `prune`, `slot_of` and
`menu_panels` all rely on it, and a `String` payload would kill it. So the
variant carries an interned `u32` handle into a registry built once at startup
from the handshake responses, before any layout is read.

**Its slug must be stable and collision-free.** `slug()` is not merely a
persisted spelling — it is the seed for the split ids in `splits.json`, which is
why the existing test asserting a single-pane layout *"generates the split ids
it always did"* exists at all. Plugin slugs are `plugin:<plugin>:<panel>`; the
prefix guarantees no collision with the four existing constants. They are ugly
and they must not be tidied by hashing or shortening, for the reason that test
already documents: changing an id string silently resets every user's dividers.

**A layout naming an absent plugin needs no new code path.** This is
structurally the same problem `sanitize()` already solves when a `layout.json`
naming `chat` is loaded by a build without it: the leaf resolves to `None`, the
split collapses onto its surviving child, and the "at least one editor pane"
rule still forces a fallback rather than an empty window.

The resulting contract, stated so it is chosen rather than discovered: **a
disabled plugin's panel disappears and its space is reclaimed; re-enabling the
plugin does not bring the panel back.** The user re-adds it from the View menu.
That is worse than VS Code, which remembers — but the alternative is persisting
ghost panels for plugins that may never return, accumulating in `layout.json`
forever.

A silent disappearance is, however, exactly the *"silently halves the feed"*
failure the Claude driver counts skipped lines to avoid. The host records how
many plugin leaves it dropped and says so.

<!--@ crates/sc-craft-ui/src/layout.rs -->

## Failure, and the Plugins panel

A plugin can be missing, refuse to start, hang during handshake, crash mid-
session, or send nonsense. None of these may take the editor with them.

Spawn failure distinguishes *not installed* from every other error, because the
first is a thing the user can fix and "program not found" alone does not tell
them how — the Claude driver already makes exactly this distinction. A crash
tears down the plugin's panels to a tombstone and leaves the rest of the app
untouched. Unknown message types are counted, not fatal.

All of which is only useful if it is visible somewhere, so **the Plugins panel
is a prerequisite, not a follow-up**: per-plugin status, log tail, handshake
timing, unknown-message counts, dropped-layout-panel counts, and a Restart
button. It is built in rather than a plugin — a plugin that reports plugin
failures cannot report its own.

## Security: what this does not protect you from

Stated plainly, because the alternative is implying a sandbox that does not
exist:

> **A plugin is a subprocess running with the user's full privileges.** It can
> read every file on the machine, write to them, and make network calls. There
> is no sandbox, no permission prompt, and no capability enforcement at the OS
> level. Plugins are trusted code, installed deliberately, exactly like a shell
> script.

The capability negotiation in the handshake is an *API* mechanism — it decides
what the host will answer — not a security boundary. A plugin that wants to
ignore it and read `~/.ssh` directly can.

This directness is owed to the reader for the same reason [22](22-claude-code.md)
declines to offer `bypassPermissions`: a one-click path to unsupervised action
is not a considered decision. The eventual answer here is a WASM plugin host,
where capabilities can be enforced rather than declared. The present answer is
honesty.

## How the agent migrates

The 59 fields and 72 message variants overstate the problem, because most of
them are not agent *state* — they are agent *UI* state. `claude_input`,
`claude_menu`, `claude_filter`, `chat_editors`, `chat_sig`, `selected_coder` and
their kin move into the plugin process and never cross the boundary at all;
their `Message` variants become `panel.event` frames and vanish from the host's
enum.

What genuinely crosses is much narrower: the feed and thread (as `list` and
`text` content), the composer text (inbound events), the workspace (which the
host already publishes), and the approval decisions.

Three pieces of luck make this cheaper than it looks.

**The approval seam is already written for this failure mode.** `bridge.rs`
blocks a worker thread on a reply channel and already documents what to do when
the answering UI is gone: deny the confirmation, abort the gate, rather than
hanging the worker. *"If the UI has gone away, deny rather than hang"* maps onto
*"if the host disconnects, deny rather than hang"* without changing the rule.

<!--@ crates/sc-win/src/bridge.rs -->

**The agent already writes files as a foreign process.** `sc-iterate` writes to
disk, and the editor already copes with a file changing under a dirty buffer.
Moving the agent out of process changes nothing structurally — and offers an
upgrade, because the agent plugin *could* route edits through `buffer.edit` and
get version checking and undo integration it does not have today.

**The remote mirror gets better.** It binds a port and tees events to a phone; it
never touched the editor. Moving it out means the editor stops ticking at 50ms
whenever a mirror is live.

Two losses, named here rather than discovered later. `iced` widget state cannot
cross a process boundary, so the per-turn `text_editor` widgets that make chat
messages drag-selectable become markdown blocks with a copy affordance — a small
regression in selection fidelity. And the amber pulse over lines the agent is
working on needs decorations, which are deferred; the agent plugin ships without
it, and becomes the concrete case that justifies designing them properly.

## Order of work

1. **Host and protocol** in `sc-craft-ui`, plus `sc-plugin-proto`, plus the
   Plugins panel.
2. **One proving plugin**: Git Blame. Not a hello-world — it exercises panels,
   commands and two event types for real reasons, it is a genuinely missing
   feature (`gitdiff.rs` is 856 lines with no blame), and it has an honest
   latency problem (a bare git spawn costs ~26ms here before git does anything)
   that forces the async design to be right on day one. Read-only, so the first
   plugin is not the one that finds a bug in `buffer.edit`.
3. **The agent as a new shell** over the same core, running beside the in-process
   one. `sc-win` keeps working and keeps shipping throughout.
4. **Claude Code as a plugin** — the easiest migration, because `claudecode.rs`
   is already a pure translator and `claude.rs` already a subprocess driver. It
   is mostly a change of destination.
5. **Delete the in-process agent** once the plugin reaches parity.
6. **`smart-coder-crafter` becomes the only desktop binary.** At this point
   `Product` exists only to pick a state directory, and may not need to exist at
   all.

**v1 is done when a second, unrelated plugin can be written without changing
`sc-plugin-proto`.** If Blame ships and a TODO scanner needs three new message
types, v1 is not v1 yet.

## Open questions

- **Startup spawn cost on Windows.** ~26ms per bare process spawn was measured
  on the development machine, Defender-dominated. Ten plugins is ~260ms before
  anything appears, on the startup path, on the primary target platform. If it
  is bad, lazy activation moves from v2 into v1.
- **Push cadence against the 50ms tick.** A plugin streaming a feed faster than
  the tick drains needs batching, the way `Session::drain_events` already
  batches. Probably fine; worth watching.
- **Two products, two plugin directories.** [21](21-craft-mode.md) deliberately
  does not share state directories, which means installing a plugin twice. Step
  6 dissolves this by dissolving the second product.
