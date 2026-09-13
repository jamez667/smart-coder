# 26 — The container manager: the sandbox becomes visible

## Principle

[25](25-plugins.md) made the IDE something that loads plugins, and set the bar
for what counts as one: **v1 is done when a second, unrelated plugin can be
written without changing `sc-plugin-proto`.** This is that second plugin, chosen
because it is unrelated in the way that matters — it is not a model, not a chat
surface, and not a text tool.

It exists because of a hole the split left. Smart Coder runs containers today —
one long-lived sandbox per workspace, ephemeral ones per verify run — and there
is nowhere in the product to see them:

> **The sandbox is a real machine the user cannot look at.** Commands run on a
> Linux userland they did not start, cannot list, cannot inspect, and cannot
> stop except by closing the app. A container that is running, wedged, or
> holding 4 GB is invisible until Docker Desktop is opened beside the IDE.

The commitment that shapes everything below:

> **The manager never invents a container.** It shows what Docker reports, and
> the containers it can act on are the ones this product created. A general
> Docker GUI is a different product, and one that already exists.

## What is already there, and what is not

The container *vocabulary* is written and tested. What is missing is every read.

`sc-verify` builds commands and never runs them — `start_command`,
`exec_command`, `stop_command` all return a `Command` for the caller to spawn,
which is what makes the argument construction testable on a machine with no
Docker installed.

<!--@ crates/sc-verify/src/run.rs -->

The naming is the load-bearing part, and it is what makes this plugin possible
at all. A session container is named from a hash of the workspace path:

```text
sc-ws-1f4c8a09b3e27d55
```

Stable for the same project, unique across projects, and — critically —
**recognisable**. `sc-ws-` is how the manager tells our containers from the
user's own, without keeping a registry that can drift from reality.

Nothing in the workspace runs `docker ps`, `docker inspect`, `docker logs` or
`docker stats`. Every read this plugin needs is new work, and it is the whole of
the new work.

## Why a plugin, and not the editor

The editor's guarantee is a dependency tree with no path to a model
([21](21-craft-mode.md)), and Docker has nothing to do with models — so on that
test alone this could live in `sc-craft-ui`. Three things say otherwise.

**Docker is absent more often than present.** The editor must open and work on a
machine with no Docker daemon, exactly as [24](24-profiler.md) requires the flame
viewer to work with no profiler installed. Absence is a state, not an error. As a
plugin, absence is expressed by the plugin not being installed — the strongest
form of that statement, and one the user can act on.

**It is the natural home for the terminal's lost container mode.** When the agent
left, `sc-win` lost its container exec modes because they needed
`sc_verify::Sandbox`, an agent-tree crate. The seam that survived is deliberately
narrow — `ExecMode::Container { name: String }`, a bare docker name, no
`sc-verify` type in sight:

<!--@ crates/sc-craft-ui/src/terminal.rs -->

That comment records why the variant was kept rather than deleted: the Crafter
keeps a working sandboxed terminal if something can hand it a name. **This plugin
is that something.** It is the reason the seam was left open.

**It has a genuine failure surface.** A daemon that is not running, an image that
was never built, a container that exited three seconds ago, a `docker` binary
that is not on `PATH`. Every one is a state the user must be shown rather than a
crash — which makes this a better exercise of the plugin failure model than a
read-only tool would be.

## What it shows

One panel, `containers`, pushed on a timer and after every action.

The content model has four kinds — `List`, `Text`, `Form`, `Stack` — and no
table. That is a constraint worth stating rather than working around: a
container row is a `ListItem` with `text`, muted `detail`, and a `command` plus
`args`, which is exactly enough.

<!--@ crates/sc-plugin-proto/src/content.rs -->

```text
● sc-ws-1f4c8a09b3e27d55        smart-coder-pyenv · up 14m · 312 MB
○ sc-ws-9d2b177e0aa4c318        smart-coder-pyenv · exited (0) 2h ago
● smart-coder-web               running · 1.1 GB
```

State reads as `severity`, not as a colour the plugin picks — the one concession
to appearance in the model, and it is a meaning the host renders. A container
that exited non-zero is `Error`; a healthy one is ordinary. That is the same rule
the Problems panel already follows.

Rows carry the container id in `args`, so a click is
`container.inspect <id>` and the plugin never has to correlate a click back to a
row index that may have moved between the push and the tap.

### The actions

Declared as commands, so they appear in the palette as well as on rows:

| Command | What it does |
| --- | --- |
| `container.refresh` | Re-read `docker ps -a` now |
| `container.start` | Start a stopped container |
| `container.stop` | `docker stop`, then `rm -f` if it will not go |
| `container.restart` | Stop and start, for a wedged sandbox |
| `container.logs` | Last N lines into the panel as `Text` |
| `container.inspect` | Image, mounts, ports, created-at |
| `container.prune-ours` | Remove every stopped `sc-ws-*` container |
| `container.open-terminal` | Point the IDE terminal at this container |

`prune-ours` is deliberately not `docker prune`. It removes stopped containers
whose names begin `sc-ws-`, and says how many it will remove before it does it.
A plugin that offers to prune a developer's whole Docker install is a plugin that
deletes someone's database on a Tuesday.

## Reading Docker without a Docker library

**Shell out to the CLI and parse `--format` JSON.** No `bollard`, no API socket.

```bash
docker ps -a --format '{{json .}}' --no-trunc
```

One JSON object per line — the same NDJSON shape the CLI's own `run --json`
already emits and the same line-delimited discipline the plugin protocol uses.
Three reasons this beats a client library:

- **It matches what the product already does.** Every container command in
  `sc-verify` is an argument vector handed to `docker`. A second, entirely
  different access path would mean two ways to be wrong about the daemon.
- **It works wherever `docker` works** — Docker Desktop, a remote context, a
  rootless daemon, Podman aliased to `docker` — because context resolution is the
  CLI's job and it is already solved.
- **It is testable without Docker.** Parsing is a pure function from a captured
  line to a row, proven against recorded fixtures on a machine with no daemon.
  This is [24](24-profiler.md)'s folded-stack parser again, and
  [25](25-plugins.md)'s `parse_line()` rule: translation is pure and lives apart
  from spawning.

An unparseable line is skipped and counted, never fatal. Docker adds fields
between versions, and a manager that dies on an unfamiliar column is worse than
one that shows a row it does not fully understand.

### Every spawn is windowless

The plugin shells out constantly — a poll every few seconds, plus every action.
On Windows a plain `Command::new` allocates a console window per child, and the
symptom is hundreds of black terminals flashing across the screen. This is a bug
the desktop app has already had once, and the fix is already written twice:

<!--@ crates/sc-plugin-agent/src/proc.rs -->

The container manager copies those twenty lines for the reason that file states:
a plugin that linked the editor to obtain a `Command` builder would defeat being
a separate process.

### Polling, not watching

`docker events` is a live stream and it is the wrong first choice. Polling
`docker ps -a` every 3 seconds while the panel is *visible*, and not at all when
it is not, is one spawn per tick against a daemon that answers in single-digit
milliseconds.

This codebase has paid for the alternative twice — the file tree is cached
because re-walking per frame made filtering laggy, and the workspace sync was
slowed from 500 ms to 2 s because a steady drip of git spawns is felt as typing
lag. A permanently-attached event stream is a fourth long-lived subprocess to
supervise, restart, and explain when it dies quietly. Revisit it when polling is
measured to be a problem.

## What it needs from the host, and what it does not

The plugin declares **no capabilities**.

That is worth dwelling on, because it is what makes this a good second plugin.
The six capabilities are `BufferRead`, `BufferEdit`, `FileRead`, `Diagnostics`,
`EditorOpen` and `RunCommand`:

<!--@ crates/sc-plugin-proto/src/manifest.rs -->

A container manager wants none of them. It reads no buffers, edits nothing,
publishes no diagnostics, and opens no files. It renders panels and receives
command invocations — the two things every plugin gets for free — and everything
else it does, it does to Docker. **The whole feature fits inside v1 with no
protocol change**, which is the bar [25](25-plugins.md) set for calling v1 done.

One host gap is visible from here, and it does not block v1. The other was a gap
when this spec was written and is not one now.

**`RunCommand` is implemented, and this spec is why.** It was declared in
`host_capabilities` and answered `Unsupported`, which made the capability a
promise rather than an API. Writing this spec found the first real consumer, so
the host now resolves a command id against every loaded plugin's manifest —
first declaration wins, matching the claim order `command_collisions` already
reports — dispatches `CommandInvoked`, and acknowledges the requester:

<!--@ crates/sc-win/src/app/plugin_requests.rs -->

An id nothing declares is `NotFound` rather than `Unsupported`: the host owns no
commands of its own, so "nothing owns this" is the honest answer, and it lets a
plugin tell a missing peer from a host that cannot serve the request at all.

**Nothing lets a plugin set the terminal's exec mode.** That is a new capability
(`TerminalExec`, one string), and it should be designed against this plugin
rather than in the abstract — the same rule [25](25-plugins.md) applies to
decorations. It is the one thing here that will eventually need a protocol
addition, and capabilities are negotiated at handshake, so adding it needs no
version bump.

## Installing it

The discovery contract is already fixed: a directory under
`<state_dir>/plugins/` containing `plugin.json`, which says only how to *start*
the process.

<!--@ crates/sc-craft-ui/src/plugin/discover.rs -->

```text
%APPDATA%\smart-coder-crafter\plugins\
    containers\
        plugin.json
        sc-plugin-containers.exe
```

Everything the plugin contributes — panels, commands — arrives in the handshake
instead, because two declarations of the same panel list drift and the one on
disk is the stale one.

## Failure, stated rather than crashed

Four failures, each a sentence in the panel naming the fix, none of them an
empty panel:

- **No `docker` on `PATH`** — "Docker was not found. Install Docker Desktop, or
  remove this plugin." The same shape as the profiler's `Missing::reason()`,
  which ends every sentence by naming the import path.
- **Daemon not running** — "Docker is installed but not running." This is the
  common transient case on Windows, so the panel keeps polling and recovers on
  its own without the user touching anything.
- **No containers** — an empty list is a normal state, not an error. A fresh
  install has no sandbox until a project is opened.
- **An action failed** — the container was removed between the push and the
  click, or the daemon refused. The row's stderr goes into the panel as `Text`,
  because "stop failed" without the reason is a bug report nobody can act on.

The plugin never kills the editor: a subprocess that panics is a dead plugin the
Plugins panel reports, which is the process-boundary property
[25](25-plugins.md) chose the transport for.

## What this deliberately is not

**Not a Docker GUI.** No image management, no compose, no networks, no volumes
beyond showing what a container has mounted. Docker Desktop, `lazydocker` and
Portainer all exist and are better at it. The scope is *the containers this
product runs*, and the moment a feature request starts with "while we're here,
could it also…", the answer is that the other tool is one alt-tab away.

**Not the ops repo's manager.** The model backends run from `../smart-coder-ops`
compose and the web system ships as a separate image installed in Portainer.
Those are deployments with their own lifecycle, not this IDE's sandboxes. The
manager will *show* a `smart-coder-web` container if it is running, because
Docker reports it — but it does not own it, and `prune-ours` will not touch it.

**Not a security boundary.** [25](25-plugins.md) is direct that a plugin is a
subprocess with the user's full privileges. A plugin that can run `docker` can
run anything, because `docker run -v /:/host` is root on the host by design.
This changes nothing about the threat model; it just makes it worth restating
where a reader will look for it.

## Order of work

1. **The parser.** `docker ps -a --format '{{json .}}'` → rows, as a pure
   function against recorded fixtures. No daemon required to test it, and it is
   the half most likely to be wrong.
2. **The read-only panel.** List, poll while visible, the four failure states.
   Read-only first, exactly as Git Blame was chosen so the first plugin is not
   the one that finds a bug in something destructive.
3. **The safe actions** — `logs`, `inspect`, `restart`, `stop`.
4. **`prune-ours`**, with its count-before-acting confirmation.
5. **`open-terminal`**, which needs the host's `TerminalExec` capability and is
   the point at which this plugin stops being read-mostly. It also re-homes the
   container exec mode the editor's terminal lost — the payoff for keeping
   `ExecMode::Container` alive through the split.

**Done when a wedged sandbox can be found, understood and restarted without
leaving the IDE**, on a machine where Docker is sometimes not running at all.

## Open questions

- **Poll interval against panel visibility.** 3 s while visible is a guess. The
  host does not currently tell a plugin whether its panel is on screen, so the
  first version polls whenever the plugin is running — which is the wasteful
  case this section exists to fix.
- **`docker stats` is expensive.** Memory in the row is the nicest column and
  the costly one: `stats --no-stream` is noticeably slower than `ps`. It may
  need a separate, slower cadence, or to appear only in `inspect`.
- **Podman and remote contexts.** Both should work through the CLI without the
  plugin knowing. Neither has been tried, and "should work" is not "does".
- **Whether `TerminalExec` is one capability or two.** Setting the mode and
  reading the current one are different privileges, and a plugin that can
  silently redirect the user's terminal into a container is worth a second
  thought before it ships.
