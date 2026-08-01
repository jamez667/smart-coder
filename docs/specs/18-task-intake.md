# 18 — Task intake & the public surface

> **One repository.** The server, the daemon and the protocol they share all live
> here <!--@ crates/sc-proto/src/wire.rs -->, so every claim in this spec is
> checkable in one pass.
>
> The server briefly shipped from its own repo. The split was reverted because
> the thing that made it attractive — a tiny dependency surface on the public
> server — came from `sc-server` depending on `sc-proto` alone, not from the
> repository boundary. `cargo build -p sc-server` compiles that tree and no
> other, so the separation that matters is the one in the dependency graph.
> Splitting bought a shorter clone and cost twenty-one verifiable anchors.

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

**No model decides admission** (principle 9, [00](00-overview.md)). Authentication
and rate limiting are decidable by code, so they are decided by code.

⚠️ **This previously read "No model is anywhere near this path", and the spam
screener makes that false.** Amended rather than left standing, because a
traceability marker that lies is worse than an honest widening.

What survives is the substance, and it is a real property rather than a wording
dodge: **a model may only *withhold* work from the queue, never introduce it.**
The screener's verdict type has two variants and the parser's fallback is
*admit*, so unreachable, timed out, garbled, wrong shape, no key configured —
every unexpected outcome is indistinguishable from approval, by construction
<!--@ crates/sc-server/src/screen.rs -->. Code admits; the model subtracts. And
what it withholds is **quarantined, not deleted**: visible to the developer and
released in one click.

#### The screener is measured, two ways

A spam filter nobody measures is one you cannot tell has stopped working — and
this one talks to a third-party model that can change without notice. So the
corpus <!--@ crates/sc-server/evals/screen.toml --> is checked twice, answering
different questions:

**Containment**, offline, on every commit. Exact-match rather than `contains`;
anything unexpected admits; markers stripped; the stored reason never model
output. These hold *whatever the model does*, including "the model has been
fully talked round", which is why they are what gates.

**Accuracy**, against the live model, on demand
<!--@ crates/sc-server/src/bin/screen-eval.rs -->. Deliberately **not** in
`check.sh`: a gate that costs money per run is a gate somebody disables.

✅ **Measured** — `gemini-2.5-flash-lite`, 2026-08-01: **precision 100%, recall
70%, zero legitimate requests held.**

Recall reads worse than it is, and the difference matters. All three misses are
cases where admitting is defensible, and two are the design working as intended:
a keyboard-mash case that is junk rather than spam; the fake-delimiter attack,
where the marker strip left inert text with nothing spammy to catch; and the
case that asks for a verbose reply, which exact-match then rejects into the
**intended** fail-open direction.

Precision is the number to watch. A false positive tells a real person their
report went through when it did not; a false negative costs one wasted drafting
run, visible in the queue and cheap to discard. The runner exits non-zero on a
false positive and not on a miss, for exactly that reason.

There are **three parties** now, and conflating them is the mistake to avoid.
They authenticate differently because they are different things: a daemon is a
long-lived machine credential, a *device* is the developer, and an *account* is a
member of the public who may file and read their own requests and nothing else.

### The daemon → server

- **A per-daemon API key**, held in `~/.smart-coder/daemon.json` — the same
  file that already carries the repository set
  <!--@ crates/sc-daemon/src/config.rs --> — and sent as
  `Authorization: Bearer`. One key per machine, so a lost laptop is revoked
  without locking the developer out of their desktop.

  ✅ **Built.** `smart-coder queue link <url> --key <key>` sets it and
  `queue serve` runs the dial-out loop; `queue run` is the offline twin that
  needs no server at all. Two commands rather than one flag, because they fail
  in different ways and a developer needs to know which one they are running
  when it stops.

  The link is validated **at the keyboard**, not at 3am in a poll loop: a key
  under 32 characters is refused with the server's own floor, and plain HTTP to
  a *remote* host is refused because it would send the key in the clear.
  Loopback over HTTP is allowed, because that is how a developer tries the
  server before deploying it and nothing leaves the machine.

  Because that file now holds a secret, it is written owner-only
  <!--@ crates/sc-daemon/src/atomic.rs -->, with the mode set on the temp file
  *before* the rename — setting it afterwards leaves a window another user can
  open it in, and an attacker only has to win that race once. `queue link` with
  no argument reports the URL and never the key, because that is the command
  someone runs while screen-sharing.
- The key authenticates **outbound** requests only. The daemon accepts no
  connections, so there is no inbound surface to authenticate.
- Short-lived exchanged tokens were considered and rejected for this pass: they
  limit the blast radius of a key leaked *on the wire*, which TLS already
  covers, at the cost of a refresh path and clock handling. Mutual TLS was
  rejected for the reason this spec rejects in-process TLS generally — cert
  issuance, renewal, and a private key on disk are three failure modes bought for
  one threat.

### The public → server

✅ **Built** <!--@ crates/sc-server/src/account.rs -->. The filing form is
reachable **with no credential at all**; reviewing stays behind device
enrolment. This is a real move of the trust boundary, so what makes it
defensible is worth stating rather than assuming.

**An account is the gate, not per-request verification.** An earlier design
emailed a verification link for every *request*, which made each filing an
unauthenticated mail send — structurally an open relay — and left rate limiting
nothing trustworthy to key on, since an address is chosen by whoever is typing.
Moving the mail to **once per person** fixes all three: the budget keys on an
account id the filer cannot mint more of, and abuse becomes *revocable* rather
than merely rate-limited.

**Magic links, no passwords.** No password storage, no reset flow, no hashing
choice to get wrong — and the session machinery is the one devices already use.
The link is single-use, expires in fifteen minutes, and is stored hashed like
every other credential here.

**A `GET` on a link changes nothing.** Mail scanners fetch every URL in a
message, often within seconds, so a `GET` that spent the token would burn it
before the human opened their inbox. The landing page renders a form; the `POST`
signs in. It renders identically for a valid, expired or fabricated token — a
404 on an invalid one would be a free validity oracle, cheaper than the `POST`
it guards.

**Asking for a link says the same thing every time** — unknown address, existing
account, revoked account, malformed input, over the outstanding-link cap. Only
what is *sent* differs, so the surface cannot be used to discover who has an
account. A revoked account is sent **nothing at all**: a "your account was
revoked" mail is one an attacker can trigger at a victim's address.

**The email body is fixed text with one URL in it** <!--@ crates/sc-server/src/mail.rs -->.
No request text, no name, nothing a stranger typed. That is what separates a
bounded notification mailer from a usable relay, and it costs nothing — the
person who asked for the link knows why. The **outstanding-link cap** is the real
ceiling on mail spend, refusing before the mailer is called.

**Emails are stored hashed**, with a `jo***@example.com` hint for the revoke
list. Honestly: this is *not* anonymisation — the address space is small enough
to brute-force. It means a copied volume is not a mailing list.

**A filer can read the spec drafted from their request**, and
`SC_SERVER_PUBLIC_SHOW_SPEC` defaults **on**. Worth understanding rather than
inheriting: that spec is model output produced by reading the developer's
repository, and the filer wrote the prompt that produced it — so the default
hands a stranger a description of code they cannot otherwise see, and steer.
The filer's page withholds `artifact_dir` (a path on the developer's machine),
`note` (daemon failure text naming repositories) and the repository name, but
**not the spec body**. Turn it off for a repository whose contents should not be
described to strangers.

✅ **Revocation has a surface** <!--@ crates/sc-server/src/routes.rs -->:
`/accounts` lists who can file and one POST stops an account, ending every
session it holds at once. Revoked accounts stay listed rather than vanishing —
a list that silently shrinks cannot answer "did I already deal with that?".
Built rather than documented as a gap, because the amendment above leans on
revocation being *the* lever, and a lever reachable only by hand-editing
`accounts.json` on the volume is not one anybody pulls at the moment they need
it.

✅ **Two spend ceilings, built in the order they actually bite.**

**A per-account filing cap** — 20 requests per rolling 24 hours by default
<!--@ crates/sc-server/src/routes.rs -->. This is the ceiling on *model* spend:
every filing that clears the screener costs a full drafting run on the
developer's machine, and the per-credential rate limit (240/min) is no defence
against something that expensive.

Counted from the request records rather than a tally, because a counter is state
that can drift from the thing it counts and the filer who discovers the drift is
the one who benefits. **Every state counts**, including `Discarded` and
`Quarantined` — a filing the screener rejected still cost a screening call, and
letting either refund the budget would make file-then-discard a way around the
limit. The window **rolls**: "resets at midnight" invites waiting for midnight,
and midnight in whose timezone has no good answer on a server holding no locale.

The count and the write happen **under the same lock** the account paths hold.
Without it a filer with two sessions, or one script issuing parallel POSTs,
would have every request read the same pre-write total and every one of them
pass — an overshoot bounded by concurrency rather than by the cap.

**The cap keys on the account id**, so requests the developer files from an
enrolled device — which carry no account — are outside it. The ceiling bounds
what strangers spend of the developer's budget, not what the developer spends of
their own.

**A ceiling on how many accounts exist** — 1,000 by default. This is what the
filing cap *rests on*, and it is the deeper of the two: an id an attacker cannot
*vary* is one they can **re-mint**, and a script with a hundred disposable
addresses would otherwise hold a hundred budgets. A cap built on an unbounded
account count is not a cap.

The ceiling is on **creation**, so somebody who already has an account still
signs in after it is reached — a signup wall that locked out existing filers
would be an outage. Refused signups are logged for the operator, because a wall
you have hit is something you need to know about; the page says only that it did
not work.

**The count includes revoked accounts, and that has a consequence worth stating
plainly: revoking does not make room.** A revoked address can never be
re-created, so a slot it freed could only be taken by a *different* address —
counting only live accounts would let an attacker's burned identities be swapped
one for one under a wall that looks intact. At the ceiling the lever is raising
`PUBLIC_MAX_ACCOUNTS`, not revoking.

A cap of `0` is **refused at startup**: a public surface that accepts nothing
reads as a broken feature rather than a setting, and "off" is expressed by
leaving `PUBLIC_REPO` unset, which turns the whole surface off honestly.

*Still reactive:* revocation remains the answer to an account that is inside both
ceilings and still unwelcome, and the developer finds that one by looking. What
the ceilings buy is that the finding-out is not a bill.

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
>
> ✅ **Built** <!--@ crates/sc-server/src/routes.rs -->. All three are returned
> from one function and written on **every** response, because a header added per
> route is a header eventually missing from one. Beyond them, a drafted spec is
> rendered as escaped text in a `<pre>` — never as Markdown and never as HTML —
> which removes the class rather than filtering it.

✅ **Built** <!--@ crates/sc-server/src/auth.rs -->, with two deviations recorded
below.

Credentials are **hashed at rest** — SHA-256 of each device token, never the
token. The data volume is the thing a Portainer user backs up and copies around,
so it contains nothing that grants access. That also removes the length leak in
`sc-web`'s `ct_eq`, which returns early on a length mismatch: comparing
fixed-width hashes makes every comparison the same work.

Enrolment is a **single-use** code, spent on the device it enrols. Each device
gets its own credential and can be revoked alone, and a revoked device is *kept*
rather than deleted so a list can say it was revoked.

**Deviation — CSRF.** This spec asks for a required header on write routes. The
surface is server-rendered HTML forms with no script at all (the CSP is
`default-src 'none'`), and a form cannot set a header — so requiring one would
mean requiring script, which is the thing that makes a rendered model-authored
spec dangerous. The defence is `SameSite=Strict` plus `form-action 'self'`
instead. That is a genuinely weaker guarantee on very old browsers, and it is the
trade this pass took deliberately rather than by omission.

**Deviation — no `?k=` bootstrap.** The spec permits a query parameter to
bootstrap, exchanged immediately. None is implemented: the enrolment code is
typed into a form. The QR-code ergonomics are lost; the credential is never in a
URL at all, so there is nothing to exchange and nothing to leak into a log.

*Not built:* the daemon-side key is still absent from `DaemonConfig`, which
carries only `repos`.

*Not built:* **revocation has primitives but no surface.** `revoke` and the device
list exist <!--@ crates/sc-server/src/auth.rs --> and are tested, but no route
reaches them — so the lost-phone case that *justifies* per-device credentials
cannot yet be acted on without editing `credentials.json` on the volume by hand.
The argument for per-device credentials is only half-delivered until that route
exists, and this is the most valuable outstanding piece of the auth work.

*Not built:* **there is no way to arm a second enrolment code.** One is minted at
first start and printed to the container log; once spent, enrolling a second
device means an operator with access to the data volume. A `smart-coder enrol`
subcommand is the obvious home for this and does not exist in any crate.

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
| **Repository** | A name the daemon resolves against its configured set — never a free path. Typed, not picked: the server holds no copy of that set. |
| **Agent profile** | Chosen from a named list (below). |

Three of these are closed sets, and that is the security-relevant part.

A filed request can also be **discarded** from the surface
<!--@ crates/sc-server/src/routes.rs -->, which drops it before approval. That is
a queue operation on the request, not a fifth gate decision — [19](19-queue-and-runner.md)
already separates the two — so [20](20-remote-review.md)'s four review decisions
are unchanged by it.

### Four kinds, and only three become specs

✅ **Built** <!--@ crates/sc-proto/src/intake.rs -->. A bug and a feature are not
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

### A repository is named, never pathed

✅ **Built** <!--@ crates/sc-daemon/src/config.rs -->, *on the daemon side*. An
arbitrary path in a request body is rejected outright rather than
canonicalised-and-checked. That is the difference between a path-traversal bug
being *mitigated* and being *unreachable* — there is no path-handling code on the
request path at all, because a request carries no path.

**The heading used to say "chosen, never typed", and the hosted architecture
broke the first half of that.** The server holds no configuration and no copy of
the repository set — that is the property that makes it safe to expose — so it
cannot render a picker. The *device* form asks for a free-text **name**
<!--@ crates/sc-server/src/page.rs -->, and the closed set is enforced one hop
later, when the daemon resolves it. The **public** form has no repository field
at all: a public filing takes `PUBLIC_REPO` from the server's own configuration
<!--@ crates/sc-server/src/config.rs -->, so a stranger cannot name a repository
and the public surface serves exactly one. Asserted as *ignored*, not merely
hidden — a repo submitted in the body is discarded.

The security claim survives intact, because it never rested on the picker: a name
is not a path, whether it was chosen or typed. What is lost is ergonomic — a
mistyped name is filed and fails at the daemon rather than being impossible to
enter. A future pass could have the daemon publish its names on poll, which is
the right way to get the picker back; inventing a server-side repository list
would not be.

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

✅ **Built** <!--@ crates/sc-server/src/page.rs -->, **with the transport
deviating**: the page is server-rendered HTML with **no script and no polling at
all** — forms and links, refreshed by the developer pulling down.

That went further than this section asked, and the reason is the section above
it. Forbidding remote subresources is only half the exfiltration defence; the
other half is forbidding *script*, and a CSP can only say `default-src 'none'` if
nothing on the page needs script. Polling with a cursor needs script. So the
choice was between a live-updating page with a weaker CSP and a static page with
the strongest one, and on the surface that renders model-authored text the static
page wins.

What is lost is live updates: a spec that finishes drafting while the developer
is looking at the list does not appear until they reload. On a phone on a train
that is a pull-to-refresh, which is the gesture people already make. It also
means the page renders on one round trip with nothing else to fetch, which is the
better behaviour on the connection this feature was designed for.

The single-file rule is honoured in spirit rather than by `include_str!`: the
markup is generated in Rust because it renders per-request state, and the CSS is
one inlined constant. No build step, no bundler, no `node_modules`.

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

✅ **Built** <!--@ crates/sc-server/src/serve.rs -->. The server holds an idle
poll open for `POLL_TIMEOUT`, re-checking every 250ms so a request filed
mid-poll is picked up in well under a second rather than waiting out the window.
The protocol constants are shared with the daemon <!--@ crates/sc-proto/src/wire.rs -->
rather than restated, so the two ends cannot drift.

### The protocol lives in `sc-proto`

✅ **Built.** `wire` and `IntakeKind` sit in `sc-proto` — a crate whose only
dependency is `serde` — and `sc-daemon` re-exports them, so nothing in this
workspace changed at its call sites.

**`sc-server` depends on `sc-proto` and nothing else**
<!--@ crates/sc-server/Cargo.toml -->: no `sc-daemon`, and through it no
`sc-model`, no `sc-workflow`, no `sc-core`. The claim that no model is anywhere
near the public server becomes *literally* true rather than true in spirit, and
the image stops compiling the entire local agent to obtain two type definitions.

**That dependency line is the whole of the separation that matters.** The server
and the desktop agent share a workspace and are still strangers in the build
graph, because `cargo build -p sc-server` compiles that tree and no other — which
is why the brief experiment of splitting the server into its own repository
bought nothing and was reverted. One definition of the protocol, one workspace,
no drift — the failure [17](17-spec-traceability.md) exists to catch, prevented
rather than detected.

The server cannot reuse the existing `Hub`. It is an in-memory, monotonically
growing event vector scoped to **one run** — the wrong shape for a durable queue of
many requests that outlives any process, and an unbounded allocation in something
meant to run for weeks.

## Deployment

**A separate Docker image, installed in Portainer on its own.**
<!--@ deploy/sc-server.stack.yml --> It shares a repository with the rest of this
workspace and nothing else: no dependency on the desktop client, the daemon, or a
model, and — because the image pipeline is **path-filtered**
<!--@ .woodpecker/image.yml --> — no reason for a change to any of them to
redeploy it.

The filter must list every input the server's build actually reads: its crate
tree, `sc-proto`, `Cargo.toml`, **`Cargo.lock`**, `rust-toolchain.toml`, the
`Dockerfile` and `deploy/`. A filter narrower than the build's real inputs ships
a **stale image**, which is silent and worse than an extra rebuild — a bare
`cargo update` changes what the server links without touching a line of its
source.

- **One port, one volume.** All state — requests, drafted specs, credentials —
  lives under a single directory <!--@ crates/sc-server/src/store.rs -->. State
  split across paths is a footgun, because the backup that misses one looks like
  it worked.
- **Configured entirely from environment variables**
  <!--@ crates/sc-server/src/config.rs -->, because a Portainer stack editor is
  where a user configures a container. A config file baked into an image cannot
  be edited without rebuilding, and mounting one to override it makes two sources
  of truth.
- **It refuses to start without a daemon key**, and refuses one shorter than 32
  characters. Running open is not a degraded mode; it is the failure this whole
  design exists to prevent, and a short key looks configured while being
  guessable. `sc-web`'s `--no-token` has no equivalent here.
- **Non-root, fixed uid.** The uid is pinned so a volume written by one image tag
  stays readable by the next — an image that changes it on upgrade greets the
  developer with permission errors on data that was fine yesterday.
- **A fresh install is usable but never open.** With no enrolment code
  configured, one is minted at first start and printed to the container log. It
  is stored hashed, so that log line is the only place it ever appears.

## Anti-goals

- **No execution.** The server files requests and displays specs. There is no
  route that builds, and no parameter that could reach one — the daemon
  constructs a spec-only pipeline, so the later phases are *unreachable* rather
  than declined ([19](19-queue-and-runner.md)).

  ✅ **Held** <!--@ crates/sc-server/src/routes.rs -->. The server holds text: it
  has no repository, no path to one, no model, and no way to reach the daemon —
  the daemon dials *out*. The record has no path field, so traversal is
  unreachable rather than mitigated, and a test asserts the build-ish routes 404.
- **No widening for the private surface.** A private surface with the full agent
  is a separate, later thing. It may share the queue and the credential
  primitives, but it gets its own routes and its own trust boundary. Adding
  capability *here* on the grounds that the private one will need it is exactly
  how the boundary erodes.
- ~~**No multi-user, for now.**~~ **Revisited, deliberately.** This paragraph said
  the claim was "a scope decision to revisit deliberately rather than a property
  the architecture enforces" — and that revisit has happened: the public surface
  is self-serve, so anyone with a mailbox can hold an account.

  What it bought is worth naming, because "multi-user" sounds like a widening and
  the useful part is a *narrowing*: filing is now attributable and **revocable**.
  Before, the only lever against abuse was a rate limit that punished everyone.

  The boundary that did *not* move: **there is still one developer.** Roles do not
  exist, sharing does not exist, and an account holder can file and read their own
  requests and nothing else — every review verb is 401 for them, asserted against
  a shared constant so a verb added later is covered without anyone remembering.
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
