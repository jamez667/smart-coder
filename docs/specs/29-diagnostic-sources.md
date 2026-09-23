# 29 — Diagnostic sources, and the events a plugin never receives

## Principle

[25](25-plugins.md) committed to a plugin protocol whose most demanding consumer
would find its limits first. Three consumers later — Claude Code, the agent,
compliance — the protocol has been revised twice and every one of those plugins
is a **text feed**. They push `list` and `text` content, they declare commands,
and all three declare `capabilities: Vec::new()`.

That is not a representative sample, and it has hidden something. Spec 25's own
exit criterion says so:

> **v1 is done when a second, unrelated plugin can be written without changing
> `sc-plugin-proto`.**

The first genuinely unrelated plugin — an external graphics engine reporting
shader compile errors ([void-engine](#the-plugin-that-found-this), spec
`docs/plugin-spec.md` in that repo) — needs nothing from the wire format. It
needs three things the host declares and does not do:

> **The protocol is not the gap. The host is.** `BufferEvent`, `BufferChanged`,
> `SelectionChanged` and `WorkspaceChanged` are defined in
> `sc-plugin-proto` and constructed **nowhere**. `PublishDiagnostics` is
> received and dropped. `Manifest::subscriptions` is parsed and read by
> nothing.

This spec closes those three, and fixes the version check while it is in the
file. It adds **no new message types**, which is the point: it is the spec that
lets spec 25 claim its exit criterion rather than the one that revises it again.

## What the host actually sends today

Every `HostMessage` construction site in `sc-win` and `sc-craft-ui`:

| Message | Sites |
|---|---|
| `Initialize` | `plugin/mod.rs:180` |
| `Shutdown` | `plugin/host.rs:205` |
| `CommandInvoked` | `app/update.rs:137`, `app/plugin_requests.rs:267` |
| `PanelEvent` | `app/update.rs:159` |
| `Response` / `ErrorResponse` | `app/plugin_requests.rs` (six sites) |

And that is the whole list. Five of the twelve `HostMessage` variants are live;
the notification half of the protocol — the half spec 25's message taxonomy
calls **Notifications** — has never sent a single message.

All three shipped plugins subscribe to `WorkspaceChanged` and handle it. None of
them has ever received it. This went unnoticed because each one also cancels its
run on `Shutdown`, and a workspace change in practice arrives as a restart.

## Part 1 — Diagnostics gain a source

### The problem

`PublishDiagnostics` cannot be wired to the Problems panel as it stands, because
the panel is backed by a **single-producer slot**:

<!--@ crates/sc-win/src/app/types.rs -->

```rust
pub(crate) compile_report: Option<sc_win::diagnostics::CompileReport>,
```

One `Option`, written by `Message::CompileDone` and read by `view_panels.rs:398`.
A plugin publishing into it would erase `cargo`'s diagnostics, and the next
compile would erase the plugin's. Whoever wrote last would win, and the panel
would silently show a fraction of what is wrong with the code.

This is the real work in this spec. Everything else is an emit site.

### The change

Diagnostics become keyed by who produced them:

```rust
/// Who produced a set of diagnostics.
///
/// `Ord` matters: it is the display order in the Problems panel, and
/// `Compile` sorts first because the compiler is the authority on whether
/// the code builds. Plugins sort after, among themselves by id, so the
/// order does not change when a plugin restarts.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticSource {
    Compile,
    Plugin(String),
}

pub(crate) diagnostics: BTreeMap<DiagnosticSource, Vec<Diagnostic>>,
```

`compile_report` keeps its other fields (the summary line, the ok/error counts,
the command that ran) — it loses only its `diagnostics` vector, which moves into
the map under `DiagnosticSource::Compile`.

**Publishing is wholesale replacement per source, never a merge.** A
`PublishDiagnostics { path, diagnostics }` replaces everything that source
previously said *about that path*, and an empty vector is how a plugin says
"this file is clean now". There is no delete message and there does not need to
be one — this is the same rule LSP settled on, for the same reason: a plugin
that crashes mid-update leaves a stale set, not a corrupt one.

**A compile replaces everything it said, not one path at a time.** It is the one
source that knows its own full output: a fresh run supersedes the previous one
entirely, *including for files it no longer mentions*. Per-path replacement would
leave a fixed file's errors on screen until something happened to mention it
again. So the store offers both, and `Message::CompileDone` replaces the whole
`Compile` key while plugins publish per path.

**A stopped plugin's diagnostics are dropped**, unlike its panel content. These
are opposite cases and the difference is deliberate: panel content is *evidence
of what the plugin was doing when it died* and spec 25 keeps it for that reason,
whereas a diagnostic is a *claim about the current state of a file* that nothing
is left to retract. A dead plugin's errors pointing at lines the user has since
fixed is exactly the "stale diagnostics against a different project" failure
`app/mod.rs:269` already guards the compile path against.

### Bounds

A plugin can push arbitrarily many diagnostics, and the Problems panel is a
widget per row. `Content::element_count` exists because *"a plugin that pushes
200,000 list items would otherwise freeze the UI thread"*; the same applies
here and the same answer is taken:

```rust
/// Diagnostics kept per source. Beyond this the newest are dropped and the
/// panel says so.
pub const MAX_DIAGNOSTICS_PER_SOURCE: usize = 1_000;
```

Truncation is reported in the panel **and names the source**, not silent: a note
that does not say whose problems were cut is not actionable when several sources
are listed. A plugin that trips it has a bug, and hiding that makes it harder to
find.

**The cap is per source across every file, not per path.** A plugin that spread
200,000 rows over a thousand files would slip a per-path cap while freezing the
UI thread just the same. Truncation drops the newest, so what other files already
said survives a later flood about one of them.

### Path handling

Every path in a `PublishDiagnostics` goes through `sc_fsutil::safe_join`, for
the reason `plugin_requests.rs:8-19` already gives for reads and edits: a plugin
can do what it likes in its own process, but it must not get the *host* to act
on a path outside the workspace on its behalf. A diagnostic whose path escapes
the workspace is dropped, and the drop is logged to the Plugins panel.

Paths are workspace-relative on the wire, matching `BufferRead` and `FileRead`.

### A third severity

The wire has carried `error`, `warning` and **`info`** since v1; the host's own
`Severity` had two. The missing one has to land somewhere, and the two available
answers were both wrong: folding `info` into `warning` makes `summary()` report a
problem the plugin never claimed, and dropping it discards something a plugin
deliberately sent.

So `sc_win::diagnostics::Severity` gains `Info`. It is **never produced by
`parse`** — no supported toolchain emits it, and a compiler that did would be
talking about the build rather than the code — and it is carried through rather
than promoted, so a hint never inflates the error or warning counts. The panel
renders it in the default foreground colour, which is what the Plugins panel
already does with a plugin's `info` rows.

## Part 2 — The notification half starts sending

### `BufferEvent::Saved`

There is **no file-watcher dependency in the workspace**, and this spec does not
add one. That is the right call independently: a watcher fires on the editor's
own writes and on `target/` churn, and every consumer then needs to tell its own
saves apart from someone else's. The editor already knows exactly when a file is
written, and there is exactly one place it happens.

<!--@ crates/sc-win/src/app/logic_save.rs -->

`save_tab` is the single choke point — `Message::SaveFile`, `SaveAndClose`,
`SaveAllAndQuit` and the agent's own writes all funnel through it. One emit
site, after the write succeeds, covers every route. A save that is refused
(`SaveVerdict::Conflict`) or skipped (a clean buffer) emits nothing, because
nothing changed on disk.

`Opened` and `Closed` emit from the tab lifecycle for completeness. `Saved` is
the one this spec exists for.

### `WorkspaceChanged`

Emitted where the workspace root changes. Three plugins already handle it by
cancelling their run, which is why this is a fix and not a feature: they have
been written against a message that never arrives, so the behaviour they
implement has never once executed.

### `BufferChanged` and `SelectionChanged` are deliberately **not** sent

Both are defined in the protocol; neither is emitted by this spec.

`BufferChanged` fires per keystroke. Spec 25 already paid for this mistake twice
— the file tree is cached because re-walking per frame *"made filtering laggy"*,
and workspace sync was slowed from 500ms to 2s because a drip of git spawns *"is
felt as typing lag"*. A JSON line per keystroke per subscribed plugin is the
same error in a third place. When something needs it, it arrives debounced and
that debounce is specified then.

`SelectionChanged` has no consumer. Adding an event with no consumer is how the
current situation arose.

**This is the point of `subscriptions` being enforced** (below): a plugin that
asks for `BufferEvents` gets saves; one that does not, gets nothing. Without
enforcement every plugin pays for every event, and the cost of adding one later
is charged to plugins that never wanted it.

### Subscriptions are honoured

`Manifest::subscriptions` is parsed and read by nothing. It becomes the filter
on the above: a notification is sent only to plugins that declared the matching
`Subscription`. `Subscription::Other` — the forward-compatibility catch-all —
matches nothing, which is correct: a plugin asking for an event this host does
not have must not silently receive a different one.

Requests and responses are unaffected. A subscription gates notifications only.

## Part 3 — The version check matches its documentation

<!--@ crates/sc-craft-ui/src/plugin/mod.rs -->

```rust
if manifest.protocol_version != PROTOCOL_VERSION {
    return Err(Failure::ProtocolMismatch { theirs: ..., ours: ... });
}
```

Strict equality, against a documented promise that *"the host supports every
version it has ever shipped"*. [27](27-versioning.md) already calls this *"the
worst arrangement available, which is that third-party plugin authors will read
the promise"*. Both v2 and v3 were additive; a v1 plugin would run correctly on
this host and is refused anyway.

This spec implements 27's planned rule, because it is four lines and because
this spec is what invites the first third-party plugin:

```rust
pub const MIN_PROTOCOL_VERSION: u32 = 1;
pub const PROTOCOL_VERSION: u32 = 3;
```

Inclusive range. The rejection message names both numbers and which side is
older. **The negotiated version is retained per plugin** rather than discarded
after the check, so the host is able to refuse sending a v1 plugin a v2 or v3
message.

Nothing reads it yet, and that is worth saying plainly rather than implying a
gate that does not exist: `Ask` and `Answered` have no host-side send site today,
so the first *emitter* of a version-gated message is also the first consumer of
this field. Retaining it now is what makes that emitter a four-line change rather
than a re-litigation of the handshake.

Raising `MIN_PROTOCOL_VERSION` stays a deliberate, documented release act.

## What this does not do

No new message types. No `Content` kinds. In particular **no image or canvas
kind** — the graphics plugin that motivated this spec renders in its own
window, and spec 25's refusal of a widget tree stands untouched. If an embedded
viewport is ever wanted, it is a protocol bump argued on its own merits, not a
thing this spec smuggles in.

No hot-swap, no plugin registry, no auto-update. No sandbox: a plugin remains a
subprocess with the user's full privileges, and the capability list remains an
API negotiation rather than a security boundary. Publishing diagnostics does not
change that — a plugin could already write to any file on the machine.

## Order of work

1. **`DiagnosticSource` and the keyed store.** Pure host refactor, no plugin
   involvement. The Problems panel merges sources and sorts. Ships alone and is
   worth shipping alone — it is what any future LSP plugin needs too.
2. **`PublishDiagnostics` wired** into that store, `safe_join`-gated and
   bounded.
3. **Subscriptions enforced**, then `BufferEvent` and `WorkspaceChanged`
   emitted. In that order — filtering before the firehose, so the first event
   ever sent is already gated.
4. **Version range**, retained per plugin.

1 and 2 are the substance. 3 is small. 4 is four lines and a test.

## The plugin that found this

void-engine is a wgpu graphics engine in a separate repository. Its plugin
subscribes to `BufferEvents`, validates `.wgsl` files through `naga` on save,
publishes the resulting errors as diagnostics, and hot-reloads its own render
pipeline. Its viewport is its own window; it asks the host for nothing but
events, diagnostics and `EditorOpen`.

It is a useful plugin, and it is a more useful *test*: it shares no dependency,
no domain and no author-side assumption with the agent, and it exercises
precisely the half of the protocol three text feeds never touched. Its spec
lives in that repo (`docs/plugin-spec.md`) and pins the same protocol version
this one does.

**When it runs against this host unchanged, spec 25's exit criterion is met.**
