# 18 — Task intake & the remote surface

## Principle

A task can be filed from anywhere. **The work only ever happens on the machine
that owns the code.**

This is the spec for a surface that is reachable when the developer is not at
their desk — file a task from a phone on the train, come back to a drafted spec.
It is deliberately *not* a hosted product. The website is an **edge**: it takes
text in and shows artifacts back. It holds no workspace, runs no model, and
executes nothing.

The commitment this spec protects:

> **The remote surface never gains an execution path.** It enqueues and it
> displays. Every byte of code and every model call stays on the developer's own
> machine.

Once there is a web UI, "just let it run the task server-side" is a small diff
away, and it would convert a private local assistant ([00](00-overview.md)) into
someone else's cloud. The separation is architectural — the edge process has no
`sc-workflow` dependency at all — precisely so that diff is never small.

## The trust boundary

```
   phone / browser
        │
        │  HTTPS  (Tailscale serve, or a tunnel)
        ▼
 ┌──────────────────────┐
 │   remote surface     │   intake · status · review
 │   (sc-web)           │   no workspace · no model · no FS
 └──────────┬───────────┘
            │  loopback only
            ▼
 ┌──────────────────────────────────────────────┐
 │  sc-daemon — the developer's own machine     │
 │  queue · runner · filesystem · model backend │   ([19](19-queue-and-runner.md))
 └──────────────────────────────────────────────┘
```

Everything above the loopback line is untrusted input. Everything below it is the
existing local product. The daemon is the only component that reads the
workspace, and it is never reachable from the network: the surface binds
`127.0.0.1`, and exposure is delegated to `tailscale serve` as a reverse proxy
that terminates TLS.

Two cautions about that sentence, because the existing code is weaker than it
reads. `serve_mirror` binds **whatever address its caller passes**
<!--@ crates/sc-web/src/mirror_server.rs --> — loopback is a convention of the
desktop's call site, not a property the server enforces. And there is no tailscale
code anywhere in `sc-web`; the proxy is an instruction to the developer. For a
daemon, the bind must be enforced at the bind, and the surface **cannot
distinguish `tailscale serve` from `tailscale funnel`** — the latter publishes to
the open internet, and both arrive as loopback traffic. A funnelled surface with a
guessable-in-principle token, no rate limit, and a `POST /approve` route is the
worst case this design admits, and the daemon should refuse to start when it can
detect one.

This is the same posture as the existing remote mirror
<!--@ crates/sc-web/src/mirror_server.rs -->, generalised: the mirror attaches a
phone to a *live desktop session*, so it dies with the GUI. This surface attaches
a phone to a *durable queue*, so it does not.

| | Remote mirror (built) | This surface |
| --- | --- | --- |
| Attaches to | A live `sc-win` session | A durable queue |
| Survives the GUI closing | No | Yes |
| Can *start* work | No — injects chat into a running loop | Yes — enqueues a task |
| Owner of the run | The desktop process | `sc-daemon` ([19](19-queue-and-runner.md)) |

## Authentication

**No model is anywhere near this path** (principle 9, [00](00-overview.md)).
Admission, authentication and rate limiting are decidable by code, so they are
decided by code.

The mechanism already exists and is proven in `sc-web` — this spec adopts it
rather than inventing a second one:

- A **256-bit bearer token** <!--@ sc_web::mint_token -->, minted per run rather
  than held as a long-lived secret.
- **Read routes** (`GET`) accept the token as a `?k=` query parameter, so a QR
  code or a pasted URL works on a phone with no keyboard ceremony.
- **Write routes** (`POST`) require `Authorization: Bearer <token>`. This is the
  CSRF defence: a hostile page can cause a cross-origin `GET` but cannot set that
  header.
- **Constant-time comparison** on both.

One caveat on adopting it wholesale: the posture has an escape hatch. `sc-web`'s
compliance server takes a `--no-token` flag that disables auth entirely, and the
swarm server authenticates nothing at all. Those are defensible for a
localhost-only dashboard the developer launches for a minute. **The daemon must
not inherit them** — there is no equivalent of "just this once, on my own machine"
for a surface that is reachable from a phone and can approve gates.

**Where the inherited posture is not sufficient.** The mechanism was built for a
process the developer restarts daily. Stretching it to a daemon that runs for
weeks exposes four gaps, and the spec is only honest if it names them as work
rather than assuming them solved:

- **The token is currently written to disk, in the clear.** Each mirror launch
  appends its connection URL — token included — to `remote-sessions.jsonl`
  <!--@ crates/sc-win/src/persist.rs -->, which never rotates or expires entries.
  For a per-run secret that dies at exit this is a convenience; for a daemon
  credential it is an append-only log of every key ever minted, readable by any
  process running as the user and swept up by any backup. **The daemon must not
  reuse this path.**
- **The token must leave the query string.** `?k=` is good ergonomics for a QR
  code and bad storage for a long-lived credential: it lands in browser history,
  in the phone's bookmark, and in the access log of every reverse proxy in front
  of it — including the `tailscale serve` that terminates TLS. A bookmarked URL
  is a credential at rest on the phone. The query parameter is acceptable only to
  *bootstrap* a session, exchanging it immediately for a cookie scoped to the
  surface; it must never be the standing credential on a write route.
- **Per-device tokens, not one shared secret.** This is a direct conflict with the
  "one developer, one token" instinct, and the conflict resolves against it:
  with a single token, rotation is indistinguishable from re-enrolment, so the
  developer who rotates *because* they are away from their desk locks themselves
  out and needs physical access to recover. Per-device tokens make revocation
  granular and rotation survivable. One *user*, several *devices*.
- **A bounded intake rate.** Unspecified here beyond the requirement, but it must
  exist before the surface is reachable, and it must be enforced per token rather
  than per IP — behind a proxy every request shares one address.

> **Rendering model-authored Markdown is an exfiltration path.** The surface
> displays artifacts a model wrote ([20](20-remote-review.md)). A drafted spec
> containing a remote image reference is enough to leak the page URL — and with it
> a `?k=` token — through the `Referer` header. This is one hallucination away, not
> a theoretical chain. The surface must send `Referrer-Policy: no-referrer`,
> `Cache-Control: no-store`, and a CSP that forbids remote subresources. None of
> these headers exist in `sc-web` today.

*Not built:* all of this is greenfield, and the four gaps above are the reason
the daemon cannot simply call `serve_mirror`. The token primitives
(`mint_token`, the constant-time compare, the read/write split) are sound and
reusable; the session lifecycle around them is not yet built.

> **On TLS.** There is no TLS in the Rust process and there should not be.
> Terminating TLS in-process means certificate handling, renewal, and a private
> key on disk — three new failure modes to solve a problem `tailscale serve`
> already solves correctly. If the developer wants exposure beyond their tailnet,
> that is a reverse proxy's job, not this crate's.

## Filing a task

A task is a small, deliberately boring record:

| Field | Notes |
| --- | --- |
| **Text** | The request, free-form. The same string an interactive user would type. |
| **Project** | Chosen from the daemon's configured workspaces — never a free path. |
| **Agent profile** | Chosen from a named list (below). |
| **Ceremony** | `minimal` / `standard` / `full` ([09](09-workflow-and-checkpoints.md)). |

Two of these are closed sets, and that is the security-relevant part.

**Project is chosen, never typed.** The surface offers the workspaces the daemon
knows about; an arbitrary path in a request body is rejected outright rather than
canonicalised-and-checked. That is the difference between a path-traversal bug
being *mitigated* and being *unreachable*.

The mirror is **not** precedent for this, despite appearances. Its `/open` route
performs no validation at all — it queues the path, and the allow-list check
happens in the desktop consumer against the GUI's recents list
<!--@ crates/sc-win/src/app/logic_c.rs -->. A headless daemon has no such consumer
to defer to, so this defence is new work, and the allowlist it checks against does
not exist yet: `UiState` holds a recents list for convenience, which is not the
same thing as a security boundary and must not be pressed into service as one.

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

Transport is **polling with a cursor**, as the existing pages do
<!--@ crates/sc-web/src/dashboard.html --> — note this is despite `sc-web`
describing itself as SSE; the SSE frame helper is dead relative to the pages that
ship. A phone on a train loses its connection constantly, and polling reconnects by
asking again, which is the failure mode that needs no code.

The daemon cannot reuse the existing `Hub` as-is, though. It is an in-memory,
monotonically growing event vector scoped to **one run** — the wrong shape for a
durable queue of many tasks that outlives the process, and an unbounded allocation
in something meant to run for weeks.

## Anti-goals

- **No multi-user.** There is one developer and one token. Accounts, roles, and
  sharing are absent by design — they would each imply a server-side identity
  model that this surface has no business holding.
- **No workspace browsing.** The surface shows *artifacts from runs it filed*, not
  the repository. It is not a code host, and a read-any-file route is exactly how
  it would become one.
- **No secrets in transit.** Keys stay in `config.json`. The surface never
  displays a key, even masked — a masked key still confirms which key is set.
- **No cross-repo work.** One task, one project ([00](00-overview.md) refuses
  monorepo-scale indexing). Filing against several projects is sequential, not a
  fan-out.

## Relationship to other specs

- **Not a platform client.** [12](12-platform-clients.md) governs *thin shells
  around the core, on the machine that holds the code* — full filesystem, full
  effects, the permission layer. This is the opposite: a surface with no effects
  at all, running away from the workspace. Blurring the two is how the remote
  surface would acquire a filesystem.
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
