# 27 — Versioning: what the client is, and what it will load

## Principle

A build that cannot say what it is cannot be supported. The desktop client
currently reports no version anywhere — not in a title bar, not on a command
line, not in the handshake it sends to every plugin — while the repository sits
hundreds of commits past its newest tag. Every bug report about it is therefore
about an unknown build.

> **Three things carry versions, and they are not the same thing.** The
> *protocol* is a contract between a host and a plugin. The *client* is a
> product someone installed. A *plugin* is a separate artifact with its own
> release cadence. Conflating any two of them is what makes an update story
> impossible to reason about.

> **A version is only worth carrying if something reads it.** A field nobody
> interprets is decoration that drifts. Each version this spec introduces has
> exactly one consumer named here, or it does not get added.

The protocol this spec ranges over is [25](25-plugins.md)'s, and the two clients
it versions are [21](21-craft-mode.md)'s product split. 25 owns the protocol's
content and what a plugin may declare; this spec owns only which versions load
and what the client calls itself.

## What is already there, and what is not

The wire protocol is genuinely versioned. A constant declares it, a plugin
declares the version it speaks in its manifest, and the host refuses a mismatch
by name rather than failing on the first message it cannot parse.

<!--@ crates/sc-plugin-proto/src/lib.rs -->
<!--@ crates/sc-plugin-proto/src/manifest.rs -->

Three gaps sit behind that.

**The host's compatibility promise is not implemented.** `sc-plugin-proto`'s
module documentation states that the host supports every version it has ever
shipped, and calls it a real and unbounded commitment. The check is an
inequality against the current constant.

<!--@ crates/sc-craft-ui/src/plugin/mod.rs -->

Both v2 and v3 are documented as purely additive, every addition defaulting to
the v1 behaviour, so that a v1 plugin runs on a later host unchanged. It does
not: a v3 host refuses it. The documentation describes the design intent and the
code implements a stricter rule, which is the worst arrangement available —
third-party plugin authors will read the promise.

**The client has no version identity at all.** `sc-win` and `sc-craft-ui`
reference no package version. The crate is pinned at `0.0.0`. The handshake
sends a protocol version but no client version, so a plugin cannot tell which
client it is talking to, and cannot degrade against a known-bad build.

**A shipped plugin carries no version on disk.** The manifest type has a
`version` field, marked *not interpreted*, populated from the plugin's own
package version. But `plugin.json` — the only thing on disk before a plugin is
spawned — holds `command`, optional `args`, and optional `enabled`. That is what
the launch parser reads; the release pipeline writes only `command`, and the
installer adds `enabled` only when carrying an existing flag through.

<!--@ scripts/install-plugins.ps1 -->

Because the plugin crate versions are hand-maintained and unrelated to the
repository tag that ships them, two different releases emit plugins that both
call themselves the same version.

**The manifest shape is written in four places.** Three checked-in
`plugin.json` files, the install script, and the release pipeline, which
generates its own inline rather than copying the crate's. Any field added to
that file must be added four times, and a field added in three of them is a
field that silently disappears depending on how the plugin was installed. This
is the strongest practical argument for the decision below that `plugin.json`
gains nothing.

## One version for the repository, not one per crate

<!--@ crates/sc-plugin-agent/Cargo.toml -->

Every crate except the three plugins is pinned at `0.0.0`, and the tag is the
real version. That is the right arrangement and this spec keeps it: these crates
are not published to a registry and are never consumed independently, so
per-crate semver would be ceremony maintained by hand and wrong within a month —
which is exactly what the plugins' stale `0.1.0` demonstrates.

So: **the tag is the version of everything the pipeline builds from it**,
clients and bundled plugins alike. The plugin crates drop to `0.0.0` with the
rest, and their manifest version comes from the build, not from `Cargo.toml`.

This deliberately gives up independent plugin release cadence. That cadence does
not exist today — one pipeline builds all five binaries from one tag — and
inventing version numbers to describe a freedom nothing uses is how the current
`0.1.0` became meaningless. A genuinely independent plugin, built elsewhere by
someone else, sets its own version and this spec does not constrain it.

### Where the number comes from

A build stamps the tag, the commit, and whether the tree was dirty. Untagged
developer builds are the common case and must be describable, so the format is
the one `git describe` already produces, which the repository is already using:

```text
v0.1.5              a tagged release build
v0.1.5-241-g4405eca a development build, 241 commits past v0.1.5
v0.1.5-241-g4405eca-dirty uncommitted changes
```

A build script resolves this once and exposes it as a constant. When git is
unavailable — a source tarball, a container with no `.git` — the value is the
literal `unknown`, never a fabricated number and never a build failure.

The client already has a build script, which today embeds a Windows icon and
nothing else. It is the natural hook, and it means this costs no new machinery.

<!--@ crates/sc-win/build.rs -->
<!--@ crates/sc-craft-ui/src/config.rs -->

**`unknown` must stay legible rather than being papered over.** A build that
cannot identify itself is a fact worth showing, and a plausible-looking wrong
version is far more expensive than an honest absence.

## Who reads the client version

Four consumers, which is the justification for adding the field:

| Consumer | Why it needs it |
| --- | --- |
| `--version` on both binaries | The first question asked of any bug report |
| The Plugins panel and the About affordance | The user can read it without a terminal |
| The handshake sent to every plugin | A plugin can degrade against a known build |
| Crash and failure reports | A report naming no build is unactionable |

The handshake gains one optional field carrying the client's product name and
version string. It is added to the initialize message, which today carries the
protocol version, the workspace and the host capabilities — and nothing
identifying the host. Optional, because a plugin built against an older protocol
must still parse the message — the same forward-compatibility rule the
capability and subscription enumerations already follow with their catch-all
variants.

The client version is **advisory to plugins and never a gate**. Compatibility is
the protocol version's job. A plugin refusing to run against a client version it
dislikes would make the client's own release cadence a breaking change for every
plugin, which is the coupling the protocol version exists to prevent.

## What the host will load

The equality check becomes a range, which is what the documentation already
promises.

```text
MIN_PROTOCOL_VERSION = 1
PROTOCOL_VERSION     = 3   (the newest the host speaks)
```

A plugin declaring anything in that inclusive range is accepted. Below the
minimum, or above the host's own version, it is refused — still by name, still
naming both numbers, because the existing failure message is the model for what
a rejection should read like.

<!--@ crates/sc-craft-ui/src/plugin/mod.rs -->

**The daemon↔server link already does this properly, and is the precedent to
copy.** That protocol is a separate, unrelated integer, but its check names both
versions *and says which side is older*, which is the difference between a
message a user can act on and one they can only report. A rejection that says a
plugin is too old tells them to update the plugin; one that says it is too new
tells them to update the editor.

<!--@ crates/sc-proto/src/wire.rs -->

**A plugin speaking an older version is not translated.** Both v2 and v3 were
designed so that every addition defaults to the v1 behaviour, so an old plugin
simply never sends the newer messages and never receives them. The host sends
newer message kinds only to a plugin whose declared version includes them —
which means the negotiated version must be retained per plugin after the
handshake, not discarded once it has been checked.

That retention is the substance of this change. The check is three lines; the
per-plugin gate on what the host is allowed to send is the part that makes the
compatibility promise true.

**Raising the minimum is a deliberate, documented act.** When translation for an
old version becomes a genuine burden, the minimum rises in a release that says
so. It does not drift upward as a side effect of adding a message.

## What goes on disk, and what does not

`plugin.json` answers exactly one question — how to start this process —
because everything a plugin *contributes* arrives in the handshake, where it
cannot drift from what the plugin actually does.

<!--@ crates/sc-craft-ui/src/plugin/discover.rs -->

That argument applies with full force to a version, and this spec does not
weaken it: **`plugin.json` gains no version field.** A version written there
would be a second declaration of a fact the handshake already carries, and the
one on disk would be the stale one — the precise failure the split exists to
prevent.

The cost is real and is accepted: the host cannot know a plugin's version
without spawning it. So an update check that wanted to compare installed
versions offline cannot read them from disk.

The answer is that the host already learns every plugin's version at startup, in
the handshake, before anything a user would call an update check could run. It
records what it learned in its own state — a host-owned cache, not a plugin-owned
declaration, and therefore not a second source of truth. If it is stale or
absent, the host spawns and asks, which it was going to do anyway.

Anything writing a plugin manifest continues to write only the launch keys, and
continues to preserve keys it does not recognise, which is already tested.

## Failure, stated rather than crashed

Every version failure names both sides and says which thing to update. The
existing protocol-mismatch message is the standard: it names the plugin's
version and the host's, so the user knows whether to update the plugin or the
editor. The range check keeps that shape and gains a direction — too old, or too
new.

A missing client version reads as `unknown` everywhere it appears. A missing
plugin version reads as absent rather than as `0.0.0`, because a plugin that
declined to say and a plugin claiming a real number are different facts.

## What this deliberately is not

**Not an auto-updater.** Nothing in this spec downloads, installs, or replaces a
binary. The release pipeline publishes tarballs and prunes to the newest three;
a user installs by copying files. Knowing what you are running is a prerequisite
for updating it, not the same project, and the prerequisite is missing.

**Not a plugin registry or a compatibility matrix.** Three first-party plugins
ship from one pipeline. A registry that resolved versions for three artifacts
built from the same tag would be infrastructure with no user.

**Not semver for the wire protocol.** It is a monotonic integer and stays one.
There is no meaningful minor/patch distinction for a message set where every
change is either additive or breaking, and the integer makes the range check
trivial.

**Not per-crate versioning.** Covered above: the tag is the version.

## Order of work

1. **The build stamp.** `git describe` resolved in a build script and exposed as
   a constant, degrading to `unknown` without git. `sc-win` already has a build
   script embedding the app icon, so this extends one rather than adding one.
   Nothing depends on it yet, so it lands and is verifiable on its own.
2. **`--version` on both binaries, and the version in the UI.** The two clients
   answer the first question a bug report asks.
3. **The range check.** `MIN_PROTOCOL_VERSION`, the inequality replaced, the
   negotiated version retained per plugin, and the host gated on what it may
   send to an older plugin. Tests for a v1 plugin on a v3 host, for one below
   the minimum, and for one above the host's version.
4. **The client version in the handshake.** Optional field, older plugins
   unaffected, surfaced in the Plugins panel.
5. **The plugin crates drop to `0.0.0`,** and their manifest version comes from
   the build stamp, so a shipped plugin names the tag that shipped it.

**Done when a user can say what they are running, and a plugin built against
protocol 1 still loads,** on a build made from a source tarball with no git
history.

## Open questions

- **Whether the handshake should carry the product identity as well as the
  version.** The two clients are the same editor with different state
  directories and plugin directories, and a plugin that behaves differently per
  product is arguably a design smell worth refusing rather than enabling.
- **Whether the host-owned version cache is worth building before anything
  consumes it.** It is only needed by an update check that does not yet exist,
  and the startup handshake already supplies the same facts.
- **What a third-party plugin's version means for support.** This spec sets the
  first-party rule; an external plugin sets its own version, and nothing
  currently distinguishes the two in the Plugins panel.
