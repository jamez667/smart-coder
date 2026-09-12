# 24 — The profiler: a flame graph in the editor

## Principle

`smart-coder` exists to prove that **a well-engineered harness extracts reliable
coding behaviour from a small model** ([00](00-overview.md)). Craft mode
([21](21-craft-mode.md)) then makes a second promise: the editor is a real
editor with no model in it at all.

That second promise leaves a hole. "Why is my code slow" is a question you
cannot answer by reading, and in Craft mode there is nobody to ask. This spec
fills it with the one tool that answers it without a model:

> Reading a profile is a **local, deterministic act**. No model is contacted,
> no network is required, and nothing is uploaded. The panel is therefore *not*
> `needs_model()`, and survives Craft-mode pruning — it is one of the few
> surfaces that is arguably **more** useful with the agent switched off.

The commitment that shapes everything below: **the viewer must work on a machine
with no profiler installed.** That is not a degraded fallback, it is the common
case — checked while building this, the development machine had no
`cargo-flamegraph`, no `samply`, no `perf` and no `dtrace`.

## Two halves, deliberately separable

The obvious design is "run `cargo flamegraph`, show its SVG". It is wrong for
the same reason [13](13-compliance-evidence.md)'s model is optional: it welds
the *viewer* to one *producer*.

### Reading is always available

Nothing about drawing a flame graph needs a profiler to be present. Folded
stacks arrive from `perf script | stackcollapse-perf.pl`, from `dtrace`, from
`samply`, from CI, from a colleague's bug report. Parsing the text is the
load-bearing part, and it is pure.

<!--@ crates/sc-flame/src/lib.rs -->

### Recording is a tool that may not be there

`flame/tool.rs` is a **separate module the viewer must not depend on**. It knows
how to find a sampler and build a command; it is allowed to find nothing.

<!--@ crates/sc-craft-ui/src/flame/tool.rs -->

Absence is a **state, not an error** — the same shape `project::UnityMissing`
already uses for a missing editor. `Missing::reason()` returns a sentence
naming the fix (`cargo install samply`), and every one of those sentences ends
by pointing at the import path, so a missing tool never reads as a broken panel.

## The input format: folded stacks and nothing else

One line per unique stack: a semicolon-separated call path, whitespace, a
sample count.

```text
main;run_agent;model_call 58
```

**One parser is the single source of truth.** Recording asks its tool for folded
output (`--print-folded`) rather than scraping the SVG it also produces, so a
recorded profile and an imported one travel the identical path.

Parsing never fails as a whole, because real files carry banners and warnings:

- blank lines and `#` comments are skipped **silently**
- a missing or non-numeric count is skipped and **counted** into `Profile::skipped`
- `0`-sample stacks are dropped — they would draw zero-width frames
- empty path segments (`a;;b`) are dropped, so a trailing `;` is harmless
- identical stacks on several lines are **summed**, which is what makes the
  parser correct on un-deduplicated `perf script` output

The count is the **last whitespace-separated token**, never the first split.
Rust symbols contain spaces constantly — `<core::iter::Map<I,F> as
Iterator>::next` is one frame — and splitting on the first space shreds them.

`Profile::skipped` is surfaced in the panel. A file that parsed 3 stacks out of
4000 must not masquerade as a small profile, and a file that parsed *nothing* is
reported as "no readable stacks" with the expected shape, not as a blank graph.

## The pure core

Everything that can be **wrong** — percentages, rectangle geometry, merge order,
search totals — is a value-in/value-out function with no iced types, no
filesystem and no process spawning, so it is tested without a window.

| Concern | Function | The rule it enforces |
|---|---|---|
| Tree | `parse_folded` | repeated stacks merge; children sort by **name** |
| Self time | `Frame::self_samples` | derived, never stored, saturating |
| Geometry | `layout` | children tile the parent; self time is the **gap** |
| Zoom | `at_path` | a stale path returns `None`, never a wrong subtree |
| Search | `matched_percent` | counts **self** time, so nesting cannot double-count |
| The answer | `hot_frames` | one function's cost is one number across all stacks |

Two of these are worth stating as commitments rather than implementation notes.

**Children sort by name, not by weight.** A flame graph is read by *finding* a
function. A frame that jumps to a different x-position between two runs of the
same workload cannot be compared by eye; name ordering makes two profiles of the
same program line up.

**Self time is a gap, not a rectangle.** Nothing is emitted for it — the parent
showing through *is* the self time. This is why `layout` never needs a
synthetic "self" frame, and why the widths of a row can be checked to sum to
their parent.

## Recording

### Cargo only, and what every other project still gets

`profile_command` refuses a non-Cargo project by name (`Missing::NotCargo`).
Every other project kind keeps the whole viewer — this is a limit on the
*recorder*, never on the section.

### Which profiler, and why `samply` first

`detect()` prefers `samply` over `cargo flamegraph`. On Windows the latter
samples through `blondie`, which generally needs an Administrator shell;
`samply` does not. Preferring it means **the button that appears is the one more
likely to work when pressed**.

Probing is a `--version` spawn, not a `PATH` scan: a `PATH` entry that is a
broken symlink or a wrong-architecture binary passes a scan and fails on use.
The probe runs **once at boot**, beside the `claude` probe ([22](22-claude-code.md)) —
a spawn per frame would be absurd, and the answer cannot change without the user
installing something, which is a restart-shaped event.

The cargo subcommand's binary is `cargo-flamegraph`, not `flamegraph`. Probing
the wrong name reports "not installed" on a machine that has it.

### The command is built purely

`profile_command` is the second consumer of the *project type → command → parsed
output* seam that Part 5 of [21](21-craft-mode.md) established for compilation.
Like `compile_command`, it returns a `CompileCommand` and touches nothing, so
the argument list is asserted without either tool installed.

An unnamed bench or test selects **all** of them (`--benches` / `--tests`).
Emitting `--bench ""` would name a target that is the empty string, which cargo
rejects — and that state is reachable from the toolbar's cycle button.

### Cancellation kills the tree, not the child

A recording is `samply record -- cargo run -- prog`: three processes deep.
`Child::kill` terminates only the handle you hold, leaving the profiled binary
running with nobody waiting on it. Cancel therefore goes through
`proc::kill_tree` — `taskkill /T` on Windows, the negative pid (the process
group) elsewhere.

<!--@ crates/sc-craft-ui/src/proc.rs -->

Every spawn goes through `proc::command`, per [22](22-claude-code.md)'s rule, so
no console window flashes.

### Where the folded file lands

`target/sc-profile.folded` — under `target/` so it inherits the project's
existing `.gitignore`. A profile is build output, and nobody wants one committed.

Stdout is drained on a helper thread while exit is polled, mirroring
`run_compile`: a full pipe would otherwise deadlock a chatty child, and polling
is what makes Cancel responsive rather than waiting out the whole run.

When a run produces no usable stacks, the tool's **own words** are reported
rather than a paraphrase. On Windows that is where "requires Administrator"
surfaces, and hiding it would hide the fix.

## The agent's half: `profile_hotspots`

The panel is for a human. The same profile answers a question the **agent** gets asked —
*why is this slow* — so the parser is a crate, not a module: `sc-flame` has zero
dependencies and is shared by the desktop client and the tool registry. `sc-tools`
must never depend on a GUI crate to read a text file.

<!--@ crates/sc-tools/src/builtin/flame.rs -->

**The tool READS, it does not record.** Recording needs a sampler that may not be
installed, takes minutes, and often needs an elevated shell; a tool that fails for
environmental reasons teaches a small model to distrust the harness
([04](04-tools.md)). Reading is deterministic and instant. The human records — from
the panel, or by hand — and the agent answers which function is slow.

The observation leads with **percentages**, because a raw sample count means nothing
without the total, and it always accounts for the unlisted tail (`(everything else)`)
so the model can tell whether it has seen the whole story. A silly `limit` is
**clamped, not rejected**: that is a judgement error, not a syntax error, and an
answer beats a validation failure the model must spend a turn recovering from.

### It is not in the investigation six

`read_only_registry` derives itself from `SideEffect::ReadOnly`, so a read-only tool
joins the trimmed menu automatically. This one is excluded by name, alongside
`cargo_info`, for the reason that list already records: the six-tool menu is the one
model-facing contract here with a measurement behind it, and that measurement compared
six against sixteen — it says nothing about seven. `profile_hotspots` also needs a
profile file that usually does not exist, so on most repositories it would be a seventh
entry that can only answer "could not read". It joins when a probe says it earns the
slot.

## The panel

`PanelKind::Flame` — a peer of Files, Git and Editor in the panel tree
([21](21-craft-mode.md)), draggable and dockable like any other, persisted by
the slug `"flame"`.

### Zoom is a path, not a borrow

The zoom is stored as `Vec<String>`, not a `&Frame`, because the profile can be
replaced underneath it. A stale path resolves to `None` and the view falls back
to the whole profile — the harmless reading. Loading a new profile clears the
zoom and the hover outright: a stale zoom would silently show a different
subtree than the one named.

### The graph is a canvas

A real profile is thousands of frames. A widget tree that deep re-lays-out every
frame and makes hover a per-widget concern; a canvas draws the same picture in
one pass and hit-tests by walking the same vector.

<!--@ crates/sc-craft-ui/src/flamecanvas.rs -->

Three rules the rendering owes the reader:

- **Colour carries no meaning.** Warm hues vary by a hash of the *name*, never
  by cost. Width is already the measure; colouring by cost would say the same
  thing twice and invite "red = slow" when red means nothing. Hashing the name
  keeps a function the same colour across the graph and between runs. Search
  hits are the one deliberate exception, and they go blue.
- **Hit ranges are half-open**, so no pixel is claimed by two frames and none by
  neither.
- **Frames narrower than 1e-4 of the viewport are not drawn**, along with their
  subtrees. Sub-pixel rectangles cost time and change nothing on screen.

Hover publishes **only on change**. A cursor move within one frame fires
continuously, and redrawing the panel per pixel is how a profiler viewer ends up
slower than the code it is profiling.

### The picture and the answer, side by side

A flame graph shows *shape*. The question people actually arrive with — *which
function was it* — is answered by a sorted table, so `hot_frames` sits beside the
graph rather than behind a tab.

## Limits

Deliberately absent, and each would be a new decision:

- no differential or inverted (icicle) view
- no demangling beyond what the producing tool emits
- no attaching to an already-running process
- no allocation or wall-clock profiling — CPU samples only
- files above 256 MB are refused by name: far above any real profile, far below
  anything that wedges the app for minutes

## Relationship to other specs

- **[21](21-craft-mode.md)** — `Flame` is a `PanelKind` and is **not**
  `needs_model()`, so Craft mode keeps it. Part 5 owns compilation and
  diagnostics; this spec owns profiling and flame layout, and is the second
  consumer of the same project-type → command seam.
- **[22](22-claude-code.md)** — the spawn rule (`proc::command`, never bare
  `std::process::Command`) and the probe-once-at-boot pattern.
- **[12](12-platform-clients.md)** — the client this ships in.
- **[00](00-overview.md)** — no non-goal is amended. A profiler contacts no
  model, so the harness thesis is untouched.
