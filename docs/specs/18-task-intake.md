# 18 — Task intake & the public surface

## Principle

A task can be filed from anywhere. **The work only ever happens on the machine
that owns the code.**

This is the spec for a surface that is reachable when the developer is not at
their desk — file a request from a phone on the train, come back to a drafted
spec. It is a **hosted server**, deployed separately from the IDE and the local
agent, and it holds nothing but text: requests in, drafted specs out. No
workspace, no model, no filesystem.

The commitment this spec protects:

> **The public surface never gains an execution path.** It accepts requests and
> displays specs. Every byte of code and every model call stays on the
> developer's own machine.

Once there is a web UI, "just let it run the task server-side" is a small diff
away, and it would convert a private local assistant ([00](00-overview.md)) into
someone else's cloud. The separation is architectural — the server has no
`sc-workflow` dependency at all, and no path to a repository — precisely so that
diff is never small.

**Scope of this first pass: specs only.** The public surface drafts Phase 1 and
nothing else. Approving marks a spec `Ready` and writes it into the repository;
the developer builds it in their IDE when they choose. A *private* surface with
the full agent is a separate, later thing, and it does not get built by widening
this one.

## The trust boundary

The daemon **dials out**. It is not a server, and nothing reaches it.

```
   phone / browser
        │
        │  HTTPS
        ▼
 ┌────────────────────────────────────────┐
 │  hosted web server                     │  intake · status · review
 │  queue of requests + drafted specs     │  no workspace · no model · no FS
 └────────────────────────────────────────┘
        ▲                     │
        │  long-poll: any     │  push: the drafted spec
        │  work for me?       ▼
 ┌──────────────────────────────────────────────┐
 │  sc-daemon — the developer's own machine     │
 │  repos · model backend · filesystem          │   ([19](19-queue-and-runner.md))
 └──────────────────────────────────────────────┘
```

Everything above is untrusted input. Everything below is the existing local
product. **The daemon has no listening socket**: it opens outbound HTTPS to the
server, asks whether there is work, drafts locally, and pushes the result back.

This inverts an earlier design in which the surface bound loopback and exposure
was delegated to `tailscale serve`, and the inversion is a strict improvement.
That design leaned on two things the code did not actually provide:
`serve_mirror` binds **whatever address its caller passes**
<!--@ crates/sc-web/src/mirror_server.rs -->, so loopback was a convention of the
desktop's call site rather than a property; and a loopback surface **cannot
distinguish `tailscale serve` from `tailscale funnel`**, the latter publishing to
the open internet with both arriving as loopback traffic. A funnelled surface
with an approve route was the worst case that design admitted.

Dialling out removes the class rather than guarding it. There is no bind address
to get wrong, no tunnel to misconfigure, and no local surface that could be
published by accident — because the surface is *meant* to be public and the
daemon is unreachable by construction. It also works from a coffee shop or behind
corporate NAT, needing no inbound firewall hole and no stable address.

> **Tailscale belongs to the Android agent connection only.** That is the phone
> attaching to a *live desktop session* — the remote mirror
> <!--@ crates/sc-web/src/mirror_server.rs -->, which dies with the GUI. It is not
> part of this surface, the daemon, or the hosted API, none of which should grow a
> `tailscale serve` hint, a funnel guard, or a MagicDNS lookup.

The mirror is still worth contrasting, because the two are easily conflated:

| | Remote mirror (built) | This surface |
| --- | --- | --- |
| Attaches to | A live `sc-win` session | A hosted queue |
| Reached over | Tailscale, phone → PC | Ordinary HTTPS to a server |
| Direction | Phone dials the PC | The PC dials the server |
| Survives the GUI closing | No | Yes |
| Can *start* work | No — injects chat into a running loop | Yes — files a request |
| Owner of the run | The desktop process | `sc-daemon` ([19](19-queue-and-runner.md)) |

## Authentication

**No model is anywhere near this path** (principle 9, [00](00-overview.md)).
Admission, authentication and rate limiting are decidable by code, so they are
decided by code.

There are **two parties**, and conflating them is the mistake to avoid. They
authenticate differently because they are different things: a browser is a person
who might lose their phone, and a daemon is a long-lived machine credential.

### The daemon → server

- **A per-daemon API key**, held in `~/.smart-coder/daemon.json` — the same
  file that already carries the repository set
  <!--@ crates/sc-daemon/src/config.rs --> — and sent as
  `Authorization: Bearer`. One key per machine, so a lost laptop is revoked
  without locking the developer out of their desktop.
- The key authenticates **outbound** requests only. The daemon accepts no
  connections, so there is no inbound surface to authenticate.
- Short-lived exchanged tokens were considered and rejected for this pass: they
  limit the blast radius of a key leaked *on the wire*, which TLS already
  covers, at the cost of a refresh path and clock handling. Mutual TLS was
  rejected for the reason this spec rejects in-process TLS generally — cert
  issuance, renewal, and a private key on disk are three failure modes bought for
  one threat.

### The browser → server

- **Per-device credentials, not one shared secret.** This conflicts with the "one
  developer, one token" instinct, and resolves against it: with a single token,
  rotation is indistinguishable from re-enrolment, so the developer who rotates
  *because* they are away from their desk locks themselves out and needs physical
  access to recover. One *user*, several *devices*.
- **The credential never lives in a query string.** `?k=` is good ergonomics for
  a QR code and bad storage for a standing credential: it lands in browser
  history, in the phone's bookmark, and in every proxy access log along the way.
  A bookmarked URL is a credential at rest on the phone. A query parameter is
  acceptable only to *bootstrap* a session, exchanged immediately for a cookie
  scoped to the surface.
- **Write routes require a header**, not just a cookie. That is the CSRF defence:
  a hostile page can cause a cross-origin request but cannot set an arbitrary
  header.
- **Constant-time comparison** everywhere a secret is checked.
- **A bounded intake rate**, enforced **per credential** rather than per IP —
  behind a proxy every request shares one address.

`sc-web`'s existing token primitives are sound and worth reusing:
`mint_token` <!--@ sc_web::mint_token --> is a real CSPRNG, and the constant-time
compare and read/write split are correct. What is *not* reusable is the session
lifecycle around them, which assumes a process the developer restarts daily.

Two escape hatches in that code must **not** be inherited. `sc-web`'s compliance
server takes a `--no-token` flag that disables auth entirely, and the swarm server
authenticates nothing at all. Both are defensible for a localhost dashboard
launched for a minute; neither has any equivalent on a public server. And each
mirror launch appends its connection URL — token included — to
`remote-sessions.jsonl` <!--@ crates/sc-win/src/persist.rs -->, which never
rotates: fine for a secret that dies at exit, an append-only log of every key ever
minted for anything longer. **No credential on this path is written to that file.**

> **Rendering model-authored Markdown is an exfiltration path.** The surface
> displays artifacts a model wrote ([20](20-remote-review.md)). A drafted spec
> containing a remote image reference is enough to leak the page URL — and with it
> any credential in it — through the `Referer` header. This is one hallucination
> away, not a theoretical chain. The surface must send `Referrer-Policy:
> no-referrer`, `Cache-Control: no-store`, and a CSP that forbids remote
> subresources. None of these headers exist in `sc-web` today.

*Not built:* none of the credential handling exists. `DaemonConfig` does, but
carries only `repos`, so the key is a new field on an existing file rather than
a new file.

> **On TLS.** A hosted server terminates TLS the ordinary way, at the edge, with
> whatever its deployment already uses. The *daemon* holds no certificate and
> listens on nothing — it is an HTTPS client, which is the whole reason this
> architecture needs no tunnel, no reverse proxy, and no bind-address discipline.

## Filing a request

A request is a small, deliberately boring record:

| Field | Notes |
| --- | --- |
| **Text** | The request, free-form. The same string an interactive user would type. |
| **Kind** | `bug` / `feature` / `improvement` / `feedback` (below). |
| **Repository** | Chosen from the daemon's configured set — never a free path. |
| **Agent profile** | Chosen from a named list (below). |

Three of these are closed sets, and that is the security-relevant part.

### Four kinds, and only three become specs

✅ **Built** <!--@ crates/sc-daemon/src/intake.rs -->. A bug and a feature are not
the same request wearing different labels, so each shapes the drafting prompt:

- **bug** — what happens now, what should happen, how to reproduce, what else the
  same root cause might affect. A report with no reproduction must *say so* rather
  than have steps invented for it; a spec that guesses sends someone hunting the
  wrong bug.
- **feature** — goals, explicit **non**-goals, constraints. The non-goals matter
  as much as the goals, or a feature spec sprawls into everything adjacent.
- **improvement** — specified against the *current* behaviour: what it does today,
  what is wrong with that, what better looks like. Respecifying from scratch loses
  the baseline that makes an improvement reviewable.

This is [16](16-post-integration-review.md)'s lens reasoning applied to intake: a
model asked one question answers it far better than one asked four.

**Feedback is the fourth kind and never becomes a spec.** It is a note — "this
annoys me", "that flow feels wrong" — not a request. Drafting a spec for it would
manufacture a work item nobody asked for and push phone-typed text into a
repository through a gate whose job is to decide whether a *spec* is right. It
costs no model call and writes to no repository: it is kept under
`~/.smart-coder/feedback/<repo>/` <!--@ crates/sc-daemon/src/feedback.rs -->,
outside every working tree — so a repository the daemon later stops serving is
left with no litter in it — and acknowledging
it keeps the note rather than deleting it — a list that silently shrinks cannot
show what has already been considered, so the same point gets raised again.

### A repository is chosen, never typed

✅ **Built** <!--@ crates/sc-daemon/src/config.rs -->. The surface offers the
names the daemon serves; an arbitrary path in a request body is rejected outright
rather than canonicalised-and-checked. That is the difference between a
path-traversal bug being *mitigated* and being *unreachable* — there is no
path-handling code on the request path at all, because a request carries no path.

Canonicalisation happens at *configuration* time, by the developer at their own
keyboard. By the time a network request is handled there is only a name to look up.

The mirror is **not** precedent for this, despite appearances. Its `/open` route
performs no validation at all — it queues the path, and the allow-list check
happens in the desktop consumer against the GUI's recents list
<!--@ crates/sc-win/src/app/logic_c.rs -->. A hosted server has no such consumer to
defer to, and `UiState`'s recents list is a convenience, not a security boundary,
and must not be pressed into service as one.

The daemon serves **any repository the developer configures**, which is the point:
it is not tied to the workspace it was built in.

**Agent profile is chosen, never configured.** See below.

## Agent profiles

The developer names their agents in `config.json`; the surface picks from that
list. A profile binds **stage routing only** — which connection serves the coder,
planner and advisor stages — under a label:

```
  config.json                          the website shows
  ─────────────────────────────        ──────────────────
  profiles:                            Agent: [ local-coder  ▾ ]
    local-coder    → Local                     gemini-planner
    gemini-planner → Gemini/Local              advisor-only
    advisor-only   → Local, no tools
```

This is the tiered assignment of [02](02-model-backends.md) given names. *Not
built:* named profiles do not exist — `UiConfig`
<!--@ sc_win::config::types::UiConfig --> today holds a fixed `Local`/`Gemini`
pair plus per-stage scalars, with no keyed list. The routing concept is
established; the naming and the list are new work.

`Task` today carries id, text, repository, kind, state and artifact fields but
no profile <!--@ crates/sc-daemon/src/task.rs -->, so the request record's fourth
row has nowhere to land yet. Adding that field belongs to the profile work, not
to the hosted server.

**A profile must carry routing and nothing else.** `UiConfig` also holds `yolo`,
`allow`, `dry_run` and the sandbox override — and a profile that bound the whole
struct would make the *permission posture* selectable from a phone, directly
contradicting [19](19-queue-and-runner.md)'s requirement that an unattended run
takes the most restrictive policy in the system. The permission posture of a
background run is a property of it being unattended, never of which agent the
task picked.

What the surface must **never** expose is a free-form endpoint URL, model name, or
API key field. Two independent reasons, either sufficient:

1. **Security.** A credential input on a network-reachable form is a credential
   you have chosen to have stolen. Keys live in `config.json` on the developer's
   machine and are never transmitted, displayed, or echoed back.
2. **The product thesis.** [00](00-overview.md) refuses a frontier-model escape
   hatch — "the constraints are the product." A free model field is exactly that
   hatch, wearing a web form. Named profiles keep backend choice a deliberate
   local configuration decision rather than a per-task impulse.

The ≤12B ceiling is a property of what the developer puts in `config.json`. This
spec does not police it; it declines to provide a way around it.

## The page itself

**One self-contained HTML file, served via `include_str!`.** No build step, no
bundler, no `node_modules`, no npm audit surface.

This is unwritten convention today, enforced only by the fact that every existing
page follows it — `dashboard.html`, `swarm_dashboard.html`, `comply_dashboard.html`
are each a single file. Writing it down here makes it a decision rather than a
habit, because this is the surface most likely to attract a "we should really use
React for this one" argument.

The cost of that argument being won is not aesthetic. It is a JS toolchain, a
dependency tree, and a supply chain, added to the one component that faces the
network, in a project whose entire premise is a self-contained local binary.

**Browser transport is polling with a cursor**, as the existing pages do
<!--@ crates/sc-web/src/dashboard.html --> — note this is despite `sc-web`
describing itself as SSE; the SSE frame helper is dead relative to the pages that
ship. A phone on a train loses its connection constantly, and polling reconnects by
asking again, which is the failure mode that needs no code.

**Daemon transport is long-polling.** The daemon asks "is there work for me?" and
the server holds the request open until there is, or until a timeout of about
thirty seconds. That gives near-instant pickup with almost no idle traffic, and it
is one ordinary HTTP call in each direction — no persistent connection to keep
alive, no reconnect logic, and no async runtime, which this workspace has nowhere
outside the GUI.

Fixed-interval polling was rejected because it forces a choice between latency and
wasted requests: a request filed on the train would wait an interval for no reason.
WebSockets and SSE were rejected for the machinery they add to solve a problem
long-polling already solves at this scale.

The server cannot reuse the existing `Hub`. It is an in-memory, monotonically
growing event vector scoped to **one run** — the wrong shape for a durable queue of
many requests that outlives any process, and an unbounded allocation in something
meant to run for weeks.

## Anti-goals

- **No execution.** The server files requests and displays specs. There is no
  route that builds, and no parameter that could reach one — the daemon
  constructs a spec-only pipeline, so the later phases are *unreachable* rather
  than declined ([19](19-queue-and-runner.md)).
- **No widening for the private surface.** A private surface with the full agent
  is a separate, later thing. It may share the queue and the credential
  primitives, but it gets its own routes and its own trust boundary. Adding
  capability *here* on the grounds that the private one will need it is exactly
  how the boundary erodes.
- **No multi-user, for now.** One developer, several devices. Accounts, roles and
  sharing are absent by design. Note this is a weaker claim than it was when the
  surface bound loopback: a hosted server *does* hold an identity, so "no
  multi-user" is a scope decision to revisit deliberately rather than a property
  the architecture enforces.
- **No workspace browsing.** The surface shows *specs from requests it took*, not
  the repository. It is not a code host, and a read-any-file route is exactly how
  it would become one. The server has no filesystem access to a repository at all,
  so this one is structural.
- **No secrets in transit.** Model keys stay in `config.json` on the developer's
  machine. The surface never displays one, even masked — a masked key still
  confirms which key is set.
- **No cross-repo work.** One request, one repository ([00](00-overview.md)
  refuses monorepo-scale indexing). Filing against several is sequential, not a
  fan-out.

## Relationship to other specs

- **Not a platform client.** [12](12-platform-clients.md) governs *thin shells
  around the core, on the machine that holds the code* — full filesystem, full
  effects, the permission layer. This is the opposite: a surface with no effects
  at all, running away from the workspace. Blurring the two is how the public
  surface would acquire a filesystem.
- **Not the Android agent.** That phone-to-PC connection is Tailscale's job and
  keeps it; this surface is ordinary HTTPS to a hosted server, and the daemon
  reaches it outbound. The two share no transport and should share no
  configuration.
- The execution half is [19](19-queue-and-runner.md); the approval half is
  [20](20-remote-review.md). This spec deliberately owns only intake and trust.
- Reuses the token, hub, and single-file-HTML patterns already in `sc-web`, which
  spec 17 would currently classify as `UNGOVERNED` — this spec is where that code
  acquires a governing document ([17](17-spec-traceability.md)).
- Agent profiles are [02](02-model-backends.md)'s tiering with labels attached.
- **Retires a v1 non-goal.** [06](06-cli-ux.md) listed "Remote/daemon mode or web
  UI" as out of scope for v1 — a line already overtaken when M5 shipped `sc-web`.
  It is struck through *in 06 itself*, pointing here, so the retirement is recorded
  on both sides rather than asserted only from this one.
