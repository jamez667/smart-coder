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
nothing else. The spec is written into the repository when it is *drafted*, by
the daemon on the developer's own machine; accepting marks it `Accepted` and
settles it, so it leaves the review list. **Accepting builds nothing** — the
developer opens their IDE and runs the pipeline when they choose. A *private*
surface with the full agent is a separate, later thing, and it does not get
built by widening this one.

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

There are **four parties** now, and conflating them is the mistake to avoid.
They authenticate differently because they are different things: a daemon is a
long-lived machine credential, the *administrator* is whoever claimed this
server and signs in with GitHub, an *account* is a member of the public who may
file and read their own requests and nothing else, and an *owner* is somebody
the administrator has promoted, at `/owners`, as responsible for a repository.

The last is the newest and the one whose boundary matters most:

| | may file | may read others' specs | may decline | may release | may **accept** | may administer |
| --- | --- | --- | --- | --- | --- | --- |
| Account | own repo's | no | no | no | no | no |
| Owner | — | their repositories' | yes | their repositories' | **no** | no |
| Admin | yes | all | yes | yes | yes | yes |

**Accepting is the one an owner does not get** <!--@ sc_server::routes::OWNER_VERBS -->,
and the reason is not that it reaches the repository. It does not: the spec was
already written there when it was drafted, and accepting flips a state and
writes one file <!--@ crates/sc-server/src/store.rs -->.

The reason is that **nothing here builds anything**. Accepting *settles* a
request — it says this is done and drops it out of the review list. Turning a
spec into work means opening the IDE and running the pipeline, on the
developer's machine, by hand. That is the one decision the web has never been
able to make and the one this design does not try to delegate.

**The line is not "an owner may not admit work."** It used to be, and `release`
broke it: releasing a quarantined request puts it back in the claimable queue,
so a daemon *will* draft it. Owners have it deliberately — the screener is a
model reading a stranger's text, so it holds things it should not, and an owner
who can see their repository's queue but not unblock it has to ask the developer
about every false positive, which makes the role decorative. What makes it
affordable is that re-admitting is bounded per repository rather than trusted:
see the third ceiling below.

Declining still fails towards **lost** work — visible on the page, and the filer
can file again. Releasing fails towards **spent** work, which is what the
ceiling is for.

That rule is **structural**. The private surface is entered through
`let Some(Caller::Admin { .. }) = caller` <!--@ crates/sc-server/src/routes.rs -->
and `accept` lives past that line, so there is no value of `Caller::Owner` which
reaches it. Not a check inside a handler that a later reader could tidy away.

**Both now arrive as a GitHub session**, so the whole burden of telling them
apart sits in `identify` — one function, one branch. The administrator is
checked **before** the roster and returns immediately, because an administrator
who also appears in `owners.json` (easy: the seed may have put them there) would
otherwise match `owner_for` first and be identified as an owner, losing their
own server to a file they can edit from the UI.

The same line does the same work twice more. Administering **owners**
<!--@ crates/sc-server/src/routes.rs --> and **repositories**
<!--@ sc_server::routes::private_route::REPOS --> lives past it too, which is
what makes "an owner cannot promote an owner" a property of the type rather than
a rule somebody has to remember. It matters more than it looks: somebody who may
promote may promote an accomplice, and revoking the first would not revoke the
second.

### The daemon → server

- **A per-daemon API key**, held in `~/.smart-coder/daemon.json` — the same
  file that already carries the repository set
  <!--@ crates/sc-daemon/src/config.rs --> — and sent as
  `Authorization: Bearer`. One key per machine, so a lost laptop is revoked
  without locking the developer out of their desktop.

  The server holds them as `label:key` pairs in `SC_SERVER_DAEMON_KEYS`
  <!--@ crates/sc-server/src/config.rs -->, keeping only a hash of each. **The
  label is what makes several daemons possible at all**: without it the server
  cannot tell two machines apart, and three things collapse onto one credential —
  the rate budget, so a daemon retrying in a loop on one host starves every
  other; revocation, which becomes all-or-nothing; and the holder of a claim,
  which is what stops a late report from a machine presumed dead landing on top
  of a draft another is still writing.

  Labels must be unique and no two daemons may share a key. Both are refused at
  startup rather than resolved, because either would make "revoke that one
  machine" quietly untrue.

  The singular `SC_SERVER_DAEMON_KEY` is still read, filed under the label
  `default`, so a deployment predating this upgrades with no stack edit — and
  both together are a union, which is how a second machine is added before the
  first is migrated.

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
reachable **with no credential at all**; reviewing stays behind the
administrator's GitHub sign-in. This is a real move of the trust boundary, so what makes it
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

✅ **Three spend ceilings, built in the order they actually bite.**

**A per-account filing cap** — 20 requests per rolling 24 hours by default
<!--@ crates/sc-server/src/routes.rs -->. This is the ceiling on what *filing*
spends: every filing that clears the screener costs a full drafting run on the
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
administrator — which carry no account — are outside it. The ceiling bounds
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

A cap of `0` is **refused at startup**: a surface that accepts nothing reads as
a broken feature rather than a setting, and "off" is expressed by leaving
`SC_SERVER_PUBLIC_REPOS` unset, which turns the whole surface off honestly.
(Disabling every *repository* from the admin page reaches a similar-looking
state deliberately and says why — see [below](#a-repository-is-named-never-pathed).
The difference is that one is a configuration nobody meant and the other is a
developer between configurations, with a page saying so.)

**A per-repository drafting cap** — 60 re-admissions per rolling 24 hours by
default <!--@ sc_server::routes::drafting_budget -->. The filing cap does not
reach this: it is checked when something is *filed*, keyed on the filer, and a
request already filed has paid its filing. Every **re-admission** after that is
free — and `send-back` and `release` both put a request back in the claimable
queue, so each one buys another full drafting run.

That loop was open. An owner could send back, wait for the redraft, and send it
back again for ever, and the cost lands on somebody else's laptop.

**Keyed on the repository, not on the caller**, and the three obvious
alternatives are all wrong for the same reason: they key on who pressed the
button, and the owner is the trusted party. What is being spent is drafting runs
against a project, which is the number the developer actually cares about — and
it stays true when a second owner is added. The developer's own verbs are inside
it too, deliberately: the cap states what a project may cost in a day, and a
send-back loop from a stuck redraft is as easy for the developer to cause.

Counted from the request records for the same reason the filing cap is, and
taken by one helper every re-admitting verb calls, so a fourth added later either
calls it or visibly does not.

*Still reactive:* revocation remains the answer to an account that is inside every
ceiling and still unwelcome, and the developer finds that one by looking. What
the ceilings buy is that the finding-out is not a bill.

### Claiming the server

✅ **Built** <!--@ crates/sc-server/src/admin.rs -->. A fresh volume arms a
one-time code and logs it; `/setup` <!--@ crates/sc-server/src/routes.rs -->
spends it and walks three steps; the GitHub login that finishes is written to
`admin.json` and administers the server from then on.

**Why a claim and not a setting.** The obvious way to name an administrator is
an environment variable, and it is a trap: a typo'd login starts cleanly and
nobody can *ever* administer the server — and because the answer lives in the
environment, there is nothing on the volume to repair. The only fix is a
redeploy, which makes "I mistyped my own username" cost the same as losing a
machine. A record can be deleted; a variable has to be re-entered somewhere the
running server cannot reach.

**Why a code and not first-login-wins.** This surface is on a public hostname.
Without a code, whoever reached `/setup` first would own the server, and losing
that race once is permanent. Reading the code out of the container's log is the
proof of ownership — the same proof a stack editor stood in for, and better
evidence than holding a cookie.

**Two proofs, deliberately separate.** The code proves you can read the logs;
the GitHub sign-in proves who you are. Spending the code claims nothing on its
own, so a code lifted from a log aggregator is not enough to take the server.
The claim rides the OAuth state — already single-use, already expiring, already
what the callback validates — rather than a cookie, which would be a second
claim to authority that nothing spends.

The three steps are in dependency order because each needs the one before it:
the callback URL is absolute, so it cannot be shown until the address is known,
and the sign-in cannot run until an application exists. One screen would show a
URL that changed as somebody typed.

**Everything past the first step belongs to the browser that spent the code**
<!--@ crates/sc-server/src/admin.rs -->. Spending it mints a token, held
in a cookie of its own and hashed on the volume like every other credential
here.

That is not decoration. The code is spent at step *one*, so without it the
later steps are guarded only by the server being unclaimed — and step two is
where a GitHub application is named, which decides which account can finish the
claim. An interloper reaching it would supply their own and own the server.

It bit hardest on a **migrated** volume rather than a fresh one. Seeding fills
in the address, so "step one is already done" was true for everybody from the
first boot, with no code ever spent — which is exactly the state a server
upgrading into this design starts in. Found by looking at a real deployment
mid-migration rather than by a test, which is recorded here because the test now
exists and the reasoning that missed it is worth not repeating: the code was
treated as guarding *the wizard*, when it guards one step of it.

The token shares the code's thirty-minute window, so an abandoned setup stops
standing open rather than leaving that step reachable indefinitely.

`secure_cookies` stays **derived** from that address and the page says what it
decided <!--@ sc_server::config::secure_for -->. "Is this a private network" is
a question people answer wrong, and answering it wrong drops `Secure` from every
session cookie without a word.

A claimed server arms nothing however often it restarts, and `/setup` **404s**
rather than refusing — so a stranger cannot tell a claimed server from one that
never had the route. To start again: delete `admin.json` and restart.

### What every browser request gets

Requirements this surface owes whoever is on the other end of it, administrator and stranger alike.

- **Write routes require a header**, not just a cookie. That is the CSRF defence:
  a hostile page can cause a cross-origin request but cannot set an arbitrary
  header.
- **Constant-time comparison** everywhere a secret is checked.
- **A bounded intake rate**, enforced **per credential** rather than per IP —
  behind a proxy every request shares one address.

**The CSP is per-surface** <!--@ sc_server::routes::Policy -->. What does not
vary is `default-src 'none'`, so no remote subresource is reachable from either
half. What varies is `script-src`: `'self'` on the public surface, absent
everywhere else.

The policy rides on the response and is stamped in **one** place, at the
dispatch site that already decides public-or-not, so a public handler cannot be
written without it and a private one cannot accidentally acquire it.
`Policy::Strict` is the `Default`, which fixes the direction the mistake falls
in: a handler that forgets produces a public page whose script does not run —
visible at once — rather than a private page that quietly permits one.

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
>

### The administrator → server

✅ **Built.** One identity, signing in with GitHub from any browser
<!--@ crates/sc-server/src/oauth.rs -->.

**This replaced per-device enrolment**, and the argument for that model
dissolved rather than being answered. It held a credential per enrolled browser
so that rotating one did not lock out the others — a real problem when there was
no other way to prove who the developer was. GitHub sign-in already existed for
owners, so the server had an identity provider and was still asking people to
copy a code out of a container log into each browser separately. There is now
nothing per-device to rotate, and "revoke this device" is "sign out", which
GitHub already owns.

**What survives is the rule underneath**: the volume holds hashes, never
credentials <!--@ crates/sc-server/src/auth.rs -->. Comparing fixed-width hashes
also removes the length leak in `sc-web`'s `ct_eq`, which returns early on a
length mismatch.

**The cost, stated plainly.** `accounts.json` is now the only credential store,
so it is read on every cookie-bearing request — including one carrying a cookie
that matches nothing, which is what a guesser sends, and which is resolved
*before* the rate limiter runs. That is why it is cached the same way the roster
is <!--@ sc_server::account::AccountsCache -->: a `stat` on every request and a
parse only when the file changed. `max_accounts` now does double duty, bounding
signup *and* bounding what one request can be made to parse.

**Changing a secret needs a fresh sign-in** <!--@ sc_server::routes::SENSITIVE_VERBS -->.
An administrator's session reaches accept, discard and owner promotion — the
same blast radius the device cookie had, and acceptable for the same reasons.
Secrets are where it stops being acceptable: somebody holding a stolen cookie
must not be able to rotate the mail key and redirect every sign-in link. So a
secret change requires the browser to have proved itself against GitHub within
five minutes <!--@ sc_server::account::FRESH_AUTH_MS -->, which asks for the
attacker to hold GitHub *at that moment* rather than a cookie taken at some
point. Freshness is per **session**, not per account: proving yourself again on
a laptop must not privilege a phone that has sat signed in for a month.

**Deviation — CSRF.** This spec asks for a required header on write routes. The
surface is server-rendered HTML forms, and a form cannot set a header — so
requiring one would mean requiring script on the surface where script is exactly
what makes a rendered model-authored spec dangerous. The defence is
`SameSite` plus `form-action 'self'` instead. That is a genuinely weaker
guarantee on very old browsers, and it is the trade this pass took deliberately
rather than by omission.

Script is now permitted on the *public* surface, so "a form cannot set a header"
is no longer a hard constraint there. The deviation stands anyway, on the
narrower ground it should always have rested on: `form-action 'self'` plus the
cookie is the defence, and adding a header would not meaningfully strengthen it.
The pages remain forms that work with script disabled — the script is
progressive enhancement, not the transport.

**Deviation — the only way in is a third party.** If GitHub is unreachable, or
the OAuth application is deleted, nobody can administer this server until it
returns. That is the price of removing the second credential path, and it is
recorded here rather than discovered: the recovery is deleting `admin.json` and
claiming again, which needs the volume and a restart.

*Not built:* the daemon-side key is still absent from `DaemonConfig`, which
carries only `repos`.

> **On TLS.** A hosted server terminates TLS the ordinary way, at the edge, with
> whatever its deployment already uses. The *daemon* holds no certificate and
> listens on nothing — it is an HTTPS client, which is the whole reason this
> architecture needs no tunnel, no reverse proxy, and no bind-address discipline.

### The owner → server

✅ **Built** <!--@ crates/sc-server/src/oauth.rs -->. An owner signs in with
GitHub and reads the drafted specs for the repositories the developer named as
theirs, and may send them back, discard them, or release ones the screener held.
Magic links stay for filers: the two are different roles, not two spellings of
one, and they need not share an identity.

**The allowlist is the entire authorization model**, and that is worth stating
plainly rather than discovering. Nothing is checked against GitHub's API, so it
is the only thing standing between a GitHub account and every drafted spec for a
project — and a drafted spec is model output produced by reading the developer's
tree. Signing in proves *who somebody is*; the roster decides *what that is
worth*.

#### The roster lives on the volume

✅ **Built** <!--@ crates/sc-server/src/roster.rs -->. Owners are a record in
`owners.json`, administered at `/owners` by the administrator.

**They used to be an environment variable**, and that was defensible while the
only writer was somebody editing a Portainer stack. It is a bad fit for a list
that changes when people join and leave: every edit meant a redeploy, which
restarts the server and drops whatever was in flight.

**The property that had to survive the move** is the one that made configuration
right in the first place: *revocation takes effect on the next request*. Deleting
a line and redeploying was complete revocation — no session to hunt down, no
record that might disagree. A snapshot read at startup would lose that, and a
parse on every request would pay for it on every request. So the file's modified
time is checked each time and the contents parsed only when it has actually
changed — a `stat`, not a parse, on the requests that get that far.

The direction of promotion survives intact. The old guarantee was *"an owner is
an account the configuration promotes — never one that promotes itself"*.
Substituting **the developer** for the configuration keeps it exactly, because
the only writer is past the admin gate.

`SC_SERVER_OWNERS` is kept as a **seed applied once**, so an existing deployment
keeps its owners across the move and a fresh one can be bootstrapped without a
browser. It is guarded by a flag and the server logs that it is ignoring it
afterwards <!--@ crates/sc-server/src/serve.rs -->: re-applying it every boot
would resurrect an owner revoked through the UI, which is the failure
revocation-on-the-next-request exists to prevent arriving by the back door of a
restart. The flag is set even when the seed is empty, or the first boot *with*
owners configured would seed a volume somebody had already administered.

**Two things a record cannot do that a setting could**, and both had to be
answered rather than dropped.

The startup refusals are still there and still refuse — an owner naming a
repository this surface does not serve, an owner naming none, a duplicate login,
owners with no GitHub application to sign in with. What changed is their
*reach*: they validate the seed, which is the only part still coming from
configuration. A record read at runtime cannot refuse to boot, because the boot
already happened.

So the same two failures are answered where the record is read instead. An owner
naming something no longer served is **intersected** away — the roster and the
enabled set are separately editable, so a record can name something this surface
stopped collecting for, and granting it would be a permission that looks applied
and reaches nothing. The admin page marks the difference rather than hiding it.
And an owner of nothing reads on the page as promoted while granting nothing, so
the form refuses to write one.

Keyed on the **login**, lowercased, not the numeric id. The id is stabler, and
that argument does not reach here: somebody types a name they recognise, and
nobody knows their collaborator's numeric id. A rename makes the entry stop
matching — somebody who cannot sign in and says so, which is the safe direction.

The flow itself is an **interstitial page with a link**, not a redirect. `Res`
carries no `Location` header, and this surface already records an objection to
redirects on routes anyone can reach. A link also stays inside the CSP as
written, where `form-action 'self'` would refuse a form posting to github.com.
The state token is spent **before** the code is exchanged: a code surviving a
failed exchange is worth nothing, where a state surviving one could be replayed
out of browser history.

Every callback failure renders one page. A reader can only act on "that did not
work"; telling them whether the state was forged or merely expired would tell an
attacker which half they got right. The operator gets the real reason in the log.

*Not built:* the GitHub API is never asked whether a login can actually see a
repository. The roster is **asserted by the developer, not verified** — the same
gap as before the move, now behind an admin-only page rather than a stack edit.
Adding the check later reads the same record and calls the API in addition.

## Filing a request

A request is a small, deliberately boring record:

| Field | Notes |
| --- | --- |
| **Text** | The request, free-form. The same string an interactive user would type. |
| **Kind** | `bug` / `feature` / `improvement` / `feedback` (below). |
| **Repository** | A name the daemon resolves against its configured set — never a free path. Picked from the enabled set on the public form, typed on the device form; a name either way, and the daemon still resolves it. |
| **Agent profile** | Chosen from a named list (below). |

Three of these are closed sets, and that is the security-relevant part.

A filed request can also be **discarded** from the surface
<!--@ crates/sc-server/src/routes.rs -->, which drops it before it is accepted. That is
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
broke the first half of that.** The server held no copy of the repository set —
that was the property that made it safe to expose — so it could not render a
picker. The *device* form asks for a free-text **name**
<!--@ crates/sc-server/src/page -->, and the closed set is enforced one hop
later, when the daemon resolves it.

The security claim survives intact, because it never rested on the picker: a name
is not a path, whether it was chosen or typed.

**The picker is back**, and on the surface that lost it. ✅ **Built**: the public
form offers exactly the enabled set and a submitted name is checked against the
same set <!--@ sc_server::config::Repos -->.

The property this protects was always a **closed set**, never a
*set of size one* — the scalar was how the closed set happened to be
implemented, not what made it safe. So one server can collect for several
projects, which is what a second deployment per repository was previously the
only way to do: another stack, volume, hostname, certificate and daemon, none of
which paid for itself by the third repository.

A name outside the set is **refused**, not filed against a default. A fallback
would put work somewhere the filer did not choose with nothing on the page
saying so; a refusal is the honest failure. One repository renders no field at
all — a select with one option is a question with one answer.

This makes the public surface the first place this server holds a closed set of
repository *names*. Still names, never paths, and the daemon still resolves each
against its own configured set one hop later — which is where the closed set is
really enforced.

#### The set is administered here, and a daemon has to be serving one

✅ **Built** <!--@ sc_server::routes::private_route::REPOS -->. Repositories are
enabled and disabled at `/repos`, beside the owners and on the same volume.

**The on/off switch moved too, and the old argument did not survive.** This spec
used to say it stayed in configuration deliberately, because a server that could
open its own public surface from a UI is a different security posture. That was
written when the UI was reachable by any enrolled browser and nothing in the
system proved who had deployed it.

The claim changed the premise. The only caller who can flip it is the
administrator, who proved they can read this container's log — the same proof a
stack editor stood in for, and better evidence than holding a cookie. The
posture is preserved by *who* rather than by *where*.

`SC_SERVER_PUBLIC_REPOS` is a seed, on/off half included. **A freshly claimed
server has no public surface**, which is a safer default than the one it
replaces: naming a repository in a stack used to turn the surface on as a side
effect.

**Enabling asks whether a daemon is actually serving that name**
<!--@ sc_server::routes::enable_repo -->, and this is a **typo-catcher, not a
security gate** — built as one, because confusing the two makes it worse at
both. Enabling `smrt-coder` writes a name nothing will ever claim: filings pile
up against a repository that does not exist, and the surface looks broken rather
than misconfigured. Asking a machine that is polling right now catches that when
it is cheapest to fix.

It needs a **narrower question than the one the review page asks**
<!--@ crates/sc-server/src/daemons.rs -->. A daemon that declared nothing is
treated as covering everything, which is right for *"is this request stuck?"* —
it is serving something and the server cannot say it is not this — and wrong
here, where the same generosity would rubber-stamp a misspelling into a
permanent record.

And it **cannot be a refusal**, because nobody having said the name is not proof
of a typo: the register is empty for the first half-minute after a restart, and
a daemon on an older build declares nothing at all. So an unconfirmed name is
*questioned* — the page names the case, lists what is actually on offer, and
offers to proceed. Taking that override records that nobody confirmed it, so
"a machine vouched for this" and "I asserted it" stay distinguishable. A page
that showed them alike could not explain why nothing is being drafted.

**Disabling closes the door and keeps what came through it.** Deleting the
filings would make the button destructive in a way its name does not say, and
the developer's own review surface still shows them.

**A surface with nothing enabled serves, and says why it cannot take anything.**
That state is reachable the moment somebody disables the last repository, so it
had to mean something. Refusing to boot would put the page that fixes it out of
reach exactly when it is needed — the failure the enrolment bootstrap exists to
prevent, at another layer — and a 404 teaches a filer at a working address
nothing.

Which cost the repository set its **non-empty invariant**, and that was the right
trade. The invariant was load-bearing while parsing the developer's
configuration was the only
constructor: the empty set could not arise, so no reader had to handle it. Once
a developer can disable the last repository it is real, and a type that declares
it unrepresentable does not prevent it — it moves the failure from a value a
reader must handle into a panic. So the accessor returns an `Option` and the two
call sites that wanted "the only one" say so.

**The daemon also publishes its names on poll**, which this spec previously
called the right way to get the picker back, and it is — but it was built first
for a sharper reason. See below.

### Which daemon gets which work

✅ **Built** <!--@ crates/sc-server/src/query.rs -->. A daemon names the
repositories it serves when it polls, and the server hands it only those.

This exists because the alternative actively destroyed work. Several daemons can
poll one server — a machine holding one repository, a machine holding another —
and the claim used to be first-come-first-served regardless of what the asker
could reach. A daemon handed a repository it does not have reported a *failure*,
which is terminal, so the wrong machine claiming a request burned it. Not a
degraded outcome: the request was gone, and the daemon that could have drafted it
never saw it.

The declaration rides in the **query string**, and that is what makes it safe to
deploy either half first: a server predating it splits the path on `?` before
matching, so it ignores the declaration and answers exactly as before. A daemon
that declares nothing gets everything, which is what it always got. A new route
would have 404'd and a header would have vanished silently — neither degrades,
and both would force the two halves to move together.

`PROTOCOL_VERSION` is deliberately **not** bumped. The check is exact equality,
so a bump breaks every deployed peer — for a change that breaks none of them.

- **The screening gate is unaffected**, and structurally so: `is_claimable` stays
  a named method and the *first* predicate, with the served-repo filter beside
  it. Declaring a repository widens which *queued* work a daemon is offered; it
  can never make unqueued work claimable <!--@ crates/sc-server/src/store.rs -->.
- **A claim records which daemon holds it.** Guarding a late report on state
  alone left a real window: a daemon whose claim expired, whose work a second
  daemon has since claimed but not yet finished, found the request still
  `Claimed` and its stale draft was accepted over one in progress.
- **Work can be handed back.** `POST /api/v1/work/:id/released` requeues rather
  than failing. A failure is a statement about the *request*; a release is a
  statement about the *daemon*. Rare now that routing exists, but reachable: a
  daemon's configuration can change between the poll and the draft, and a path
  that is not a git repository is only discovered when drafting is attempted.
- **A request nothing serves is visible, not silent.** The server keeps an
  in-memory register of who polled for what
  <!--@ crates/sc-server/src/daemons.rs -->, so the review page distinguishes
  "no daemon has connected" from "one has, but not for this repository" and names
  the command that fixes each. *Waiting for a daemon to pick it up* is true of
  both and useless for either.

  Deliberately **not** a `RequestState`: it is a fact about who happens to be
  polling this minute, so it would flap — and in memory, because persisting it
  would survive a restart as a confident claim about daemons that are gone.

*Not built:* the picker **on the device form**, which still takes free text.
The public form has one, and `/repos` gave the server a set to render it from —
which is the thing this paragraph used to say would be the wrong way to get
there. It was, while the set would have been *invented*: a list the server made
up and the daemon had never confirmed. The set it holds now is one a daemon
declared or the developer knowingly asserted, and it is per-surface rather than
global. The device form is the developer's own, so a free-text name there costs
nobody else anything.

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

✅ **Built** <!--@ crates/sc-server/src/page -->, **with the transport
deviating**: the page is server-rendered HTML with **no script and no polling at
all** — forms and links, refreshed by the developer pulling down.

That went further than this section asked, and the reason is the section above
it. Forbidding remote subresources is only half the exfiltration defence; the
other half is forbidding *script*, and a CSP can only say `default-src 'none'` if
nothing on the page needs script. Polling with a cursor needs script. So the
choice was between a live-updating page with a weaker CSP and a static page with
the strongest one, and on the surface that renders model-authored text the static
page wins.

**Amended: the public surface now permits `script-src 'self'`**
<!--@ sc_server::routes::Policy -->. The paragraph above treated "renders
model-authored text" as one property of one surface. It is two, and separating
them is what changed the answer:

| | whose text it renders | script |
|---|---|---|
| private | **every** filer's, on one page | never |
| public | only the reader's own | `'self'` |

A script that misbehaves on the public surface reaches the data of the person who
filed it — who wrote the prompt that produced it in the first place. The same
script on the private surface reaches every filer's specs at once, which is a
cross-tenant leak with no equivalent on the other side. The blanket ban was
buying nothing on the public half that the reader could not already do to
themselves, and it was the reason a filer could not have a theme toggle or a
language they can read — both of which the section below now builds.

**Same-origin subresources are permitted; remote ones are not.** The policy also
carries `font-src 'self'` and, on the public half, `connect-src 'self'` — added
when the blanket ban turned out to be costing things it was never protecting.

The distinction is the whole argument. What makes a rendered spec an
exfiltration channel is a fetch that **leaves this server**: it tells a third
party the page was viewed, and hands them the URL — which identifies the request
— through `Referer`. A font served from `/public/`, or a `fetch()` back to this
same origin, tells nobody anything. Refusing those bought no security and cost
real typography and any live status on a filed request.

So the rule is now stated exactly: **every source in every directive is `'self'`
or `'none'`**, with `'unsafe-inline'` tolerated for styles alone because the
stylesheet ships inside the page. A test asserts that as an allowlist rather than
by grepping for `https:` — a bare domain like `fonts.gstatic.com` carries no
scheme and would have passed the old check.

The two faces are `include_bytes!`-ed into the binary
<!--@ crates/sc-server/assets -->, so the container has no asset directory to
mount and no file that can go missing. Both are SIL Open Font License 1.1, whose
text travels beside them as the licence requires.

Three things this does **not** relax, and they are what keep the deviation
narrow:

- `default-src 'none'` is unchanged on both, so no remote subresource is
  reachable from either. The exfiltration argument the section above makes is
  untouched — it is about *remotes*, not about script.
- `'self'` and not `'unsafe-inline'`. An inline-script allowance is also what a
  successful injection needs, and this is the surface rendering model-authored
  text. Script here must be a served file.
- The transport is still forms and links. The pages work with script disabled;
  what script is permitted *for* is presentation. The polling deviation this
  section records therefore still stands.

What is lost is live updates: a spec that finishes drafting while the developer
is looking at the list does not appear until they reload. On a phone on a train
that is a pull-to-refresh, which is the gesture people already make. It also
means the page renders on one round trip with nothing else to fetch, which is the
better behaviour on the connection this feature was designed for.

The single-file rule is honoured in spirit rather than by `include_str!`: the
markup is generated in Rust because it renders per-request state, and the CSS is
one inlined constant. No build step, no bundler, no `node_modules`.

### The public surface is designed, themed and translated

<!--@ crates/sc-server/src/page/public.rs -->

What the relaxed CSP was *for*. Three things the filer gets, and the reasoning
that decided each:

**A shared look with the GitHub Pages site**
<!--@ crates/sc-comply/src/report/site.rs -->. The same token names and values,
so a reader arriving from the docs meets one product rather than two. Copied
rather than factored into a shared crate: the alternative couples the compliance
reporter to the intake server, which have nothing else to do with each other, and
the drift it risks is cosmetic and visible.

**A theme control that is three radios and CSS, with no script.** Script is
permitted here now, but a theme that *needs* script flashes the wrong colours
before it runs, and the CSS-only form has no flash by construction. Three options
rather than two, because a radio cannot be un-checked: light/dark alone is a
one-way door out of following the system. The choice does not survive a page
load, which is the honest limit of doing this without a cookie, and the right
trade on a surface a reader passes through two or three pages at a time.

**A language switcher, and the catalogue behind it**
<!--@ sc_server::i18n::Strings -->.
The catalogue is a `struct` with one field per string, not a map keyed on a name.
That is the design and not a style choice: a map's missing key is a runtime
`None`, and the fallback renders English into the middle of a French page, which
nobody notices until a user says so. A missing *field* is a compile error naming
the language and the string.

Three properties the compiler cannot see, so three tests hold them instead: no
empty string, no string still identical to its English outside a named exception
list, and no markup or format placeholder anywhere in a catalogue. Strings that
need a value in the middle are split into two fields — a translator reordering
`{0}` and `{1}` is a runtime panic, and it is a mistake translation invites.

**Only the public half is translated.** The private review pages have exactly one
reader, and a catalogue for them is weight paid for nobody. The intake *kinds*
are untranslated too, and deliberately: the slug is what the form submits and
what the developer reads on the review page, so translating the visible text
would have a filer and a reviewer naming the same kind differently. Only the
field's label translates.

`POST /public/language` is reachable **signed out**, since somebody who cannot
read the sign-in page is precisely who needs it. It carries no `next=` parameter
and performs no redirect — a "return to where you were" field on a route anyone
can reach is an open redirect waiting to be found, and this surface is small
enough that landing on the sign-in page in the chosen language costs nothing.

The `sc_lang` cookie is **not** `HttpOnly` and is `SameSite=Lax`, both departing
from the session cookie: it holds a preference rather than a credential, nothing
authenticates on it, and arriving from an external link should still show the
language the reader chose. Its value is parsed against the catalogues that exist,
so nothing a caller writes there reaches a page except by selecting among them.

Looking at any of this without standing up a mail provider is
`cargo run -p sc-server --example render-public -- <dir>`
<!--@ crates/sc-server/examples/render-public.rs -->, which writes every public
page in every language through the real renderers.

#### Two concessions to running it locally, both derived rather than configured

Standing the container up to *use* the surface — rather than look at rendered
files — hit two things that behave correctly in production and make the feature
appear broken on `http://localhost`. Both are now decided from the base URL,
which already has to be `https://` before a deployed server will start.

**`Secure` comes off on loopback** <!--@ sc_server::config::PublicConfig -->. A
browser silently *discards* a `Secure` cookie that arrives over plain HTTP, so
locally the sign-in and the language switcher both appeared to do nothing: the
request succeeded, the cookie vanished, and the next page had forgotten. The
symptom reads as a bug in the feature rather than a property of the cookie.
Nothing else relaxes with it — `HttpOnly` and `SameSite` are unchanged, and a
test asserts that, since "drop `Secure` locally" is an easy edit to over-apply.

**Console mail is gone, and its concession with it.** A switch used to print
sign-in links to the container log so that trying the surface did not need an
API key for a third party. A sign-in link is a credential, so it was refused
unless the base URL was loopback — the containment being the *address*, not the
audience: the link pointed at `127.0.0.1`, which a reader of the log cannot
reach.

That was a sound guard and it is still the wrong shape of thing to keep. It made
"anyone who can read this log can sign in as anyone" one mistaken base URL away,
and the mistake was a stack edit rather than a code change. What replaced it
costs nothing: **a surface with no mail provider serves and says it cannot send
sign-in links**, the same way one with no repositories enabled says it cannot
take a filing. Configure mail at `/settings` afterwards, which is reachable
because the administrator signs in with GitHub rather than by email.

The POST refuses too, with a `503`
<!--@ crates/sc-server/src/routes.rs -->. Every other failure on that route is
deliberately indistinguishable — the page looks the same whether or not mail
went out, so it cannot be used to test whether an address has an account — and
that argument does not reach this one. "This server has no mail provider" is not
a fact about any person, and accepting an address nobody will act on is the
worse answer. It has to be refused there rather than merely hidden, because the
masthead's sign-in dialog is rendered by the shell on every page and knows
nothing about what is configured.

**Amended: a private network address counts too**
<!--@ sc_server::config::PublicConfig -->.
Both guards originally accepted only `localhost`, which refused a real case
found the first time this was deployed to Portainer on a LAN: `http://192.168.
0.100:8420` is neither localhost nor the internet, and the server would not start
the public surface there at all. The choice it forced — put a certificate in
front of a machine on your own network, or do not try the feature — is not one
worth imposing.

The rule being relaxed is *"a sign-in link is a credential in a URL, so plain
HTTP puts it in the clear"*. That is true on the internet and **not** true of a
link that cannot be routed off the network it was issued on. So the ranges are
RFC 1918 and RFC 4193 plus link-local, and nothing else: `10/8`, `172.16/12`,
`192.168/16`, `169.254/16`, `fc00::/7`, `fe80::/10`.

The boundary is exact rather than approximate, because an address one digit
outside a private range is the public internet and treating it as private would
leak credentials. Addresses are **parsed, not prefix-matched** —
`starts_with("10.")` calls `100.0.0.1` private, and `172.15` and `172.32` sit
just outside `172.16/12` on either side. A **name** is never private however it
resolves, since this sees a string and a hostname is not a promise about where it
points; `10.0.0.1.attacker.test` is a public host.

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

### A claim expires

<!--@ sc_server::store::CLAIM_TIMEOUT_MS -->

**Found by running it, not by reading it.** Claiming is serialised per
repository: `claim_next` skips any repo that already has something `Claimed`, so
two daemons never work the same tree. The consequence nobody had stated is that
an *abandoned* claim holds its repository **for ever** — nothing else for that
repo can ever be claimed again, no error is raised anywhere, and the only symptom
is a queue that quietly stops moving. A daemon killed mid-draft is enough; so is
an operator curling the work endpoint to see what it returns, which is exactly
how this was found.

A claim now carries `claimed_ms` and is returned to the queue after twenty
minutes, with a note saying why so the developer's page can answer "why is this
queued again".

Three decisions inside that, each the less obvious option:

**Reclaimed in `claim_next`, not on a background thread.** A stale claim has no
consequence until somebody asks for work, so checking at that moment costs one
scan on a request that already scans — no second thread, and no window in which a
sweep and a claim disagree about who holds a repository.

**Twenty minutes, not two.** The two failures are not symmetric: too short
reclaims a *live* draft and puts two daemons on one tree, too long delays a repo
whose daemon is already dead. Only the first corrupts anything, so the timeout
sits well clear of a plausible drafting run rather than close to it. It is not
configurable — an operator tuning it down for snappiness is choosing duplicate
work without the trade being visible — and a `const` assertion fails the **build**
if it is ever shortened past ten minutes.

**A late report is refused** <!--@ sc_server::store::Store -->.
This is the hazard the timeout *introduces*: a daemon that comes back after its
claim expired would otherwise overwrite whatever happened since — a spec another
daemon has drafted, a decision a reviewer has made. Reclaiming stale work is only
safe if the work it reclaimed can no longer write, so both daemon-facing verbs
require the request to still be `Claimed`.

**State alone was not enough**, and the gap only became reachable once several
daemons could poll one server. If the first daemon's claim expires, a *second*
claims the work and is still drafting, the request is `Claimed` — so a state-only
check passes and the first daemon's stale spec lands on top of one being written
right now. The original test missed it because its second daemon had already
finished, which moved the state past `Claimed` and made the check sufficient by
accident. A claim now records **which daemon holds it**, and a report is refused
unless it comes from that machine. A record written before the field existed has
no holder and matches any daemon, so upgrading does not reject a draft that is
legitimately in flight; the window is one claim timeout.

That guard also revealed two existing tests were exercising a transition the
state machine already forbade — a daemon redrafting straight over a spec sitting
in `AwaitingReview`. They now drive the real path (send back, requeue, reclaim),
which is the only way a redraft actually happens.

`claimed_ms` is dropped by `Store::put` on anything not `Claimed`, rather than
cleared at each of the ten transitions out of that state — a rule that holds
until the eleventh is added. A record written before the field existed is treated
as claimed *now* rather than as infinitely stale, so upgrading does not reclaim
every in-flight draft at once.

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

#### Deploying without a gap

A single container restarting shows a 502 for the second or two in between, and
closing that needs two things.

**The image checks itself** <!--@ ./Dockerfile -->. `sc-server --health` opens a
TCP connection to its own configured port. Without a healthcheck Docker reports
a container *running* the instant its process is spawned — before the port is
listening — so a proxy in front forwards to a socket nobody is on yet.

The check is a **TCP connect, not an HTTP request**, and that is deliberate:
every route here needs a credential or a configured public surface, so any URL
worth probing returns 401 or 404 on a perfectly healthy server. A check that has
to enumerate which non-200 responses are acceptable is one that eventually calls
a broken server healthy. It is also the binary rather than `curl`, because the
runtime layer is Alpine plus one static binary and adding an HTTP client to the
one component facing the internet buys nothing.

**A Swarm stack rolls forward rather than restarting**
<!--@ deploy/sc-server.swarm.yml -->. `order: start-first` starts the
replacement, waits for its healthcheck, and only then stops the old task.

> **One replica, and raising it corrupts data.** The state is one directory and
> the write lock is a `Mutex` *inside the process*, so two tasks on the same
> volume would both read-modify-write `accounts.json` and silently lose signups —
> and the per-repository claim serialisation would break the same way. This is
> zero-downtime *deployment*, not horizontal scaling; scaling needs the state
> moved somewhere that can arbitrate between processes.
>
> A start-first rollout does overlap the two tasks for a few seconds, which is
> survivable where two steady-state replicas are not: one is draining, the other
> starting, and the window is seconds rather than permanent. It is still a
> window, and a signup landing in it can lose its write.

Both deployment files are checked against the config's environment module in
both directions <!--@ crates/sc-server/src/config.rs -->, so a setting cannot be added to one
and forgotten in the other — which would leave an operator on the Swarm stack
with no box for a cap that exists.
- **Four environment variables, and the rest is administered from the server's
  own pages** <!--@ crates/sc-server/src/settings.rs -->. Where to listen, which
  volume, and the key the rest is sealed with. Nothing else.

  **This spec used to argue the opposite**, and the reversal is worth recording
  rather than quietly overwriting. It said configuration belonged in the stack
  because a Portainer editor is where a user configures a container, with the
  roster as "the deliberate exception". The exception ate the rule, and the
  reason it did is the one the exception was granted for: editing a stack means
  a redeploy, which restarts the process and drops what was in flight. That is a
  fair price for a bind address and a bad one for raising a ceiling that is
  refusing filings right now, or fixing a mail key at the moment sign-in is
  broken.

  What is left in the stack is what a running server cannot change about itself:
  the socket it listens on, the directory it opens, and the key it would need in
  order to read its own settings. Everything else is read per request from the
  volume, so an edit lands on the next request rather than the next restart
  <!--@ sc_server::settings::SettingsCache -->.

  **The old variables survive as seeds** — applied once, on a volume that has
  never been administered, so an existing deployment upgrades without
  re-entering anything. After that they are ignored and the server says so: a
  setting that is present and inert is one somebody will edit expecting an
  effect.

- **The three reversible secrets are sealed on the volume**
  <!--@ crates/sc-server/src/seal.rs -->. A mail key is *replayed* to Brevo
  rather than compared, so unlike every credential in `auth` it cannot be
  hashed. Storing it plainly would have made a copied volume leak live
  credentials for the first time — the exact property `auth`'s module doc
  claims. So it is encrypted with `SC_SERVER_SECRET_KEY`, which is the one
  secret that stays in the environment: a key beside its own ciphertext protects
  nothing.

  ChaCha20-Poly1305 rather than a bare cipher, and the tag is the point.
  Secrecy is the obvious half; detecting tampering is the half that matters
  more, because somebody who can write the volume but not read the key could
  otherwise flip bits in a stored screening URL and have the server talk to a
  host of their choosing.

  A wrong or missing key is a **refusal to start**
  <!--@ crates/sc-server/src/seal.rs -->, checked against a value known to be
  present. Without it the server would boot, open nothing, and report no mail
  provider and no screener — indistinguishable from a fresh install, and the
  operator would re-enter secrets that were never lost.
- **A daemon key is minted here, shown once, and stored hashed**
  <!--@ sc_server::routes::private_route::DAEMONS -->. A freshly claimed server
  has none, so nothing can claim work until one exists — the right resting
  state, and better than an environment variable that sits in a stack editor in
  plaintext for the life of the deployment. The seed still refuses one shorter
  than 32 characters, because a short key looks configured while being
  guessable. `sc-web`'s `--no-token` has no equivalent here.
- **Non-root, fixed uid.** The uid is pinned so a volume written by one image tag
  stays readable by the next — an image that changes it on upgrade greets the
  developer with permission errors on data that was fine yesterday.
- **A fresh install is usable but never open.** An unclaimed volume mints a
  claim code at startup and writes it to the container log. It is stored hashed,
  so that log line is the only place it ever appears — and that is a liability
  as much as a safeguard. **The container log's audience is whatever scrapes
  it.** On a host shipping logs to an aggregator, the code's exposure is the
  aggregator's exposure, and a code that never expired would be a standing
  credential published to everyone who can run a query.

  So it **expires** <!--@ sc_server::admin::CLAIM_TTL_MS -->. A minted code is
  good for thirty minutes, and an unclaimed server arms a fresh one on any
  start, so a lapsed code costs a restart rather than a lockout. The bound is
  time, because the audience cannot be bounded.

  **A claimed server arms nothing**, however often it restarts. Re-arming would
  leave a standing way to take the server from its administrator, refreshed on
  every deploy — and the code alone cannot claim anyway: it opens the door to
  the GitHub sign-in that decides who owns this.

### Logs

**One JSON object per line, on stdout** <!--@ crates/sc-server/src/log.rs -->.
The destination is a log aggregator, and that is what those parse — which turns
"grep the container log and hope" into a query over named fields rather than a
match against a sentence somebody may reword.

- **Every line carries `svc`.** A scraper labels lines with the Docker container
  name, and under Swarm that name carries a task id that changes on every
  redeploy — so anything pinned to it silently stops matching. `svc` does not
  move, and is what dashboards should key on.
- **Messages are fixed strings; the variable part goes in fields.** A message
  built by interpolation is one nobody can query for.
- **One access line per request** <!--@ crates/sc-server/src/serve.rs -->,
  emitted once per *request* rather than per dispatch — the long poll re-runs
  dispatch every 250ms, and logging there would mean four lines a second per idle
  daemon: a log of nothing happening, drowning the log of something happening.
- **The route is classified, not sanitised.** Each request maps onto one of a
  fixed set of labels, and anything unrecognised becomes `other`. A sign-in
  token is a path segment and a bearer credential; a query string is free-form
  caller input. A redactor decides what to remove and is wrong the first time it
  misses something, so this decides what to *keep* — enforced by the return
  type, which cannot borrow from the path.
- **Deliberately not logged:** query strings, bearer tokens, cookies, email
  addresses, and the client IP. Behind the reverse proxy this design assumes,
  the peer address is the proxy's own; reading `X-Forwarded-For` instead would
  put an attacker-controlled header carrying personal data into a log built to be
  shipped elsewhere.
- **No level setting.** Three levels, all of them emitted. A filter would need an
  environment variable, an entry in the stack file, a row in the drift test, and
  a paragraph here — to suppress lines from a server whose whole output is a
  startup banner and one line per request.

*Not built:* nothing authenticates the log itself. Anything that can write to the
aggregator can forge a line under any label, so the log is an operational record
and not evidence.

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
- The execution half is [19](19-queue-and-runner.md); the review half is
  [20](20-remote-review.md). This spec deliberately owns only intake and trust.
- Reuses the token, hub, and single-file-HTML patterns already in `sc-web`, which
  spec 17 would currently classify as `UNGOVERNED` — this spec is where that code
  acquires a governing document ([17](17-spec-traceability.md)).
- Agent profiles are [02](02-model-backends.md)'s tiering with labels attached.
- **Retires a v1 non-goal.** [06](06-cli-ux.md) listed "Remote/daemon mode or web
  UI" as out of scope for v1 — a line already overtaken when M5 shipped `sc-web`.
  It is struck through *in 06 itself*, pointing here, so the retirement is recorded
  on both sides rather than asserted only from this one.
