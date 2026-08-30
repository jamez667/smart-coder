# 13 — Compliance evidence

## Principle

**An evidence pack is an argument, not a verdict.**

`sc-comply` evaluates a workspace against a compliance framework and hands an
auditor two things: citations for what it found, and an honest map of what it
could not see. It never claims a codebase is compliant, because no static
analysis can establish that — compliance is a property of an organization
operating controls over time, and a repository is one artifact among many.

This is a sharper constraint than it sounds. Spec 11 says the test is the
oracle: `sc-verify` answers *is it correct?* and a machine can settle it. There
is no equivalent oracle here. Most of what a framework asks about is invisible
to source inspection, so the honest engine is the one that says so loudly and
routes the rest to a human.

Three design consequences follow, and they drive everything else:

1. **`Unknown` is a first-class status.** "We didn't find it" and "it isn't
   there" are different claims carrying different weight. Collapsing them
   toward pass produces a false attestation; collapsing them toward fail
   produces alert fatigue and the tool gets switched off after the third audit.
2. **There is no headline compliance percentage.** A single number invites the
   exact misreading — "we're 78% SOC 2 ready" — that the tool exists to prevent.
3. **Pack-driven commands are off by default.** A pack is data that may have
   been downloaded from a vendor.

## Shape

One engine, many packs. Frameworks differ in *content*, not *mechanism*, so a
crate per framework would be thirty copies of the same walking and matching
code. `crates/sc-comply` is the engine; each framework is a TOML file under
`packs/`. Adding ISO 42001 is an authoring task, not an engineering one.

```
pack (TOML)  ─┐
              ├─►  engine  ─►  collectors  ─►  observations
workspace    ─┘                                     │
                                                    ▼
                                     outcome policy (per check)
                                                    │
                                                    ▼
                                       aggregation (per control)
                                                    │
                                                    ▼
                                        EvidencePack ─► md / json / sarif
```

The split between *observation* and *policy* is the load-bearing one. A
collector reports only what it saw — matched, didn't match, or couldn't tell.
The pack decides what that means. This is why one regex collector serves both
`regex-match-in-glob` and `regex-must-not-match`, and why both halves are
independently testable.

## The status lattice

| Status | Meaning |
|---|---|
| `NotApplicable` | Out of scope for this codebase. Excluded from scoring — **not** counted as a pass. |
| `Pass` | Evidence found and it satisfies the control. |
| `Unknown` | Could not be determined from source. The auditor's worklist. |
| `Gap` | Evidence found that violates the control, or required evidence definitively absent. |
| `Error` | A collector failed. A *tool* failure, not a compliance judgment. |

The variant order is load-bearing: `Ord` is derived, so `all` aggregation is
`.max()` — worst wins. `NotApplicable` sits lowest so it can never drag a
control down. `Error` outranks `Gap` because a crashed collector means we do not
know whether there is a gap; reporting one is as wrong as reporting a pass.

## Aggregation

Where compliance tools are quietly wrong. The recurring rule: **partial evidence
is not a verdict**.

Pre-rules, before any aggregate: `not_applicable_if` fires and no check runs ·
N/A checks leave the scoring set · **any `Error` makes the control `Error` under
every aggregate**, including `any` — a control cannot be declared satisfied when
the collector that might have found the gap is the one that broke.

| Aggregate | Rule |
|---|---|
| `all` | Worst status wins. |
| `any` | Any `Pass` → `Pass`. `Gap` **only if every** check is a `Gap`; one `Unknown` → `Unknown`. |
| `weighted` | `earned / observable`, then thresholds. |
| `majority` | >50% pass and gaps do not dominate. |

Two subtleties in `weighted` worth stating explicitly:

- **The denominator excludes `Unknown` weight.** Dividing by total weight
  penalizes the codebase for the *tool's* blind spots. `max_unknown_share`
  separately vetoes the whole result when too much was unobservable. Two
  mechanisms answering two distinct questions: "of what I saw, how much
  passed?" and "did I see enough to have an opinion?"
- **The middle band resolves to `Unknown`, not `Gap`.** Partial evidence means
  "an auditor should look at this", not "this failed".

Framework roll-up reports counts plus two ratios that must be read together:
`coverage` (passed / in-scope) and `determinacy` (passed+gaps / in-scope).
Coverage without determinacy is meaningless — 100% coverage at 12% determinacy
means almost nothing was verified and everything verified happened to pass.

## Check vocabulary

`file-exists` · `file-absent` · `regex-match-in-glob` · `regex-must-not-match` ·
`symbol-exists` · `toml-path` · `json-path` · `command-exit-code`.

Every check declares `on_match`, `on_no_match`, and — where "we could not look"
differs from "we looked and it was absent" — `on_no_files`. The canonical case
is branch protection: it lives in the VCS provider's API, so the absence of a
settings file is emphatically not evidence that review is unenforced. Letting
`on_no_files` default is how a compliance tool lies without anyone deciding to.

> A note on match sense: `file-absent` "matches" when the path **exists**. The
> inversion is easy to get backwards, and getting it backwards inside
> `not_applicable_if` silently removes a control from the scored denominator.

**What this vocabulary cannot express** — and must therefore declare as
`Unknown` rather than omit: organizational controls (SOC 2 CC1–CC5), anything
in an identity provider or cloud console, incident and vendor records, and
whole-program semantic properties. "Is authorization enforced on *every* admin
route?" is a dataflow question; regex finds the routes that have the annotation,
never the one that is missing it. That last category is the strongest argument
for the retrieval collector below.

For SOC 2 this yields real deterministic evidence for roughly five of the
thirty-three common-criteria points. **That number belongs in the report.** An
auditor told "15% deterministic, 85% flagged for manual evidence" trusts the
tool; one told "92% compliant" does not.

## Deterministic first, retrieval later

Roughly 70% of code-relevant controls are mechanically decidable, and for those
a language model makes the output strictly worse: non-reproducible, unciteable,
and unauditable, which is disqualifying when the deliverable *is* an audit
trail. So the built-in collectors are deterministic, and `Collector` is shaped
so a retrieval-backed one drops in without reshaping a type:

```rust
// Illustrative.
pub trait Collector {
    fn name(&self) -> &'static str;
    fn handles(&self, kind: &CheckKind) -> bool;
    fn collect(&self, check: &Check, ctx: &AuditContext<'_>) -> Result<Observation>;
}
```

Object-safe, so the registry is `Vec<Box<dyn Collector>>`. Sync, because the
whole core is (only `sc-win` pulls in tokio, for its GUI loop) and a future
`ModelBackend` call is sync too. Fallible, because a model backend can be down
and that must surface as `Error` rather than silently becoming "no evidence
found". `handles()` rather than enum dispatch is what lets a later collector be
*added* rather than *integrated* — registry order decides precedence, so an LLM
fallback can shadow `symbol-exists` for a language `sc-index` cannot parse.

Worth noting: the eventual retrieval corpus is *framework text* — control
catalogs, ISO clauses — not the codebase. That corpus is small, stable, and a far
easier retrieval problem than code RAG.

## Safety

`command-exit-code` is gated behind `ComplyOptions::allow_commands`, default
`false`. A pack that can run shell commands turns the format into an attack
vector: download a vendor's `soc2.toml`, run it against a checkout, get owned.
When disabled, such checks yield `Unknown` with a stated reason and the
capability is named in the report — never silently skipped, because a silent
skip reads as clean.

`command-exit-code` runs through a POSIX `sh` wherever one exists, on every
platform, falling back to `cmd` only on a Windows host with no `sh` on PATH. Pack
commands are written POSIX (`test -f x && grep -q y x`, pipes, `2>/dev/null`),
which `cmd` rejects outright — so without the preference a pack that passes on
Linux CI fails on a Windows desktop with a shell error that reads as a failed
*check* rather than as `Error`. That fallback is the one case where a check's
outcome depends on the host, and a pack relying on POSIX syntax should expect
`Error` there rather than a verdict.

SARIF output covers only `Gap` findings that carry a `file:line` anchor.
Unknowns are excluded deliberately: a code-scanning UI has no way to render "we
could not tell", so emitting them would collapse the distinction the lattice
exists to preserve.

## Reuse and deliberate divergence

`sc-comply` reuses `sc-index`'s tree-sitter extraction for `symbol-exists`,
where the language filter is the point — a symbol named in a comment is not a
definition. It does **not** reuse `collect_sources` for the general scan, which
accepts only `.rs`, `.py` and `.cs` and would silently drop the `.tf`, `.yml`,
`Dockerfile` and `.gitignore` files where most real evidence lives. The scan is
modelled on `sc-tools`' `search_code` walker instead.

Globs compile to `regex` at pack load rather than adding `globset`: the crate
stays on workspace dependencies only, the translation is unit-testable, and a
malformed glob becomes a load error rather than a surprise mid-audit.

## Relationship to other specs

- [01 — Architecture](01-architecture.md): a portable core crate with no client
  dependencies; the evidence pack is a plain data structure, renderers are pure.
- [04 — Tools](04-tools.md): the command gate is the same instinct as the tool
  permission classes — capability off by default, named when withheld.
- [05 — Context management](05-context-management.md): `sc-index` reuse and the
  Rust/Python/C# language limit that `symbol-exists` inherits.
- [11 — Testing and TDD](11-testing-and-tdd.md): the contrast that defines this
  spec. There, a test is an oracle that settles correctness; here there is no
  oracle, so the engine reports what it cannot settle.
- [12 — Platform clients](12-platform-clients.md): the report is a
  client-agnostic artifact — Markdown for humans, JSON for diffing across
  quarters, SARIF for code-scanning surfaces.
