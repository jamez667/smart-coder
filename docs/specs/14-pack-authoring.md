# 14 — Pack authoring

## Principle

**A deterministic lint beats a model opinion every time.**

Spec 13 made framework packs the unit of extension: adding ISO 27001 is an
authoring task, not an engineering one. That moves the bottleneck, and with it
the risk. A pack encodes human judgment about what each control means and what
counts as evidence for it — and one field, `on_no_files`, decides whether the
resulting report tells the truth about what was never examined.

So the assistant that helps author packs is mostly *not* a model. Most of what
makes a pack wrong is mechanically provable: a regex that cannot match, a glob
that selects nothing, a secret pattern that hits the detector's own sources, an
organizational control that can nonetheless resolve to `pass`. Those are lints.
The model is reserved for the one genuinely semantic question — *does this check
evidence what the control text claims?* — which no static analysis can answer.

**This tooling never runs during an audit.** That is enforced structurally, not
by convention: `sc-comply` has no dependency on `sc-model`, so the audit path
cannot reach a model even by accident. An evidence pack must be reproducible;
that property is the entire reason anyone should trust one.

## Shape

```
crates/sc-comply-author
  ├─► sc-comply   (Pack, CheckKind, Glob, validate, scan_workspace)
  ├─► sc-model    (ModelBackend — the drafting path ONLY)
  └─► sc-core     (extract_json_array — the house tolerant-JSON scan)

crates/sc-comply  ──X──►  sc-model      (never)
```

Two jobs, in descending order of value:

| Job | Model? | Entry point |
|---|---|---|
| **Critique** an existing pack | no | `smart-coder comply-lint --pack <path>` |
| **Draft** checks for a new framework | yes | `sc_comply_author::draft_control` |

## The lints

Sixteen, in three families. Each is a pure function over a parsed pack plus an
optional sample workspace, so the whole surface is testable without a network.

**Outcomes** — the `on_no_files` family, and the reason this crate exists:

| Lint | Severity |
|---|---|
| `indeterminate-maps-to-pass` (explicit) | critical |
| `indeterminate-maps-to-pass` (via the default, on a negative scan) | high |
| `provider-side-path-without-on-no-files` | high |
| `symbol-check-without-on-no-files` | high |
| `absent-target-without-on-no-files` | medium |
| `all-outcomes-identical` | medium |

The severity split on `indeterminate-maps-to-pass` is deliberate and was learned
by writing the lint wrong first. For a `regex-must-not-match`, `on_no_match =
"pass"` is *correct* — searching real files and finding no secret is a genuine
pass. The defect is only ever that `on_no_files` inherits it, so searching *zero*
files also reports pass. Flagging the former would tell an author to break a
working control; the first version of this lint did exactly that.

**Patterns** — can this check ever fire, and will it fire on the wrong thing:
`regex-no-look-around` (high), `self-referential-pattern` (high),
`glob-matches-nothing` (medium), `must-not-match-without-exclusions` (medium),
`pattern-matches-nothing-in-sample` (low), `untracked-only-evidence` (low).

**Structure** — `org-control-claims-determinism` (high),
`weighted-band-too-narrow` / `too-wide` (low), `severity-without-remediation`
(low), `any-of-single-check` (low), `missing-intent` (low).

`org-control-claims-determinism` is the second-most-important lint. Roughly 85%
of a framework like SOC 2 is organizational — board oversight, vendor contracts,
incident records — and none of it is visible in a repository. A control that
resolves to `pass` because it found a Markdown file has confused *documented*
with *operating*. Declaring such a control `unknown` is the correct answer;
omitting it would imply a coverage the pack does not have.

> **Sample workspaces.** Several lints need real files. Without one they *skip*
> rather than guess, and the report says so — a clean result from a run that
> could not look is not a clean bill of health.

## The self-critique test

`the_shipped_soc2_pack_has_no_blocking_findings` runs every lint against
`crates/sc-comply/packs/soc2-tsc.toml` with this repository as the sample. It
failed on its first run and found five real defects in a pack that had already
shipped:

- Four checks (`CC6.1/no-committed-private-keys`,
  `CC6.1/no-hardcoded-cloud-credentials`, `CC6.6/no-tls-verification-disabled`,
  `CC6.6/no-plaintext-http-endpoints`) let `on_no_files` inherit
  `on_no_match = "pass"`. An empty or renamed tree would have reported green
  having read nothing.
- `CC6.6/no-tls-verification-disabled` matched this tool's own test fixtures,
  reporting `danger_accept_invalid_certs(true)` as a real TLS defect.

That is the test earning its place: a linter whose author-facing claim is "this
catches the mistakes I make" has to be able to catch the ones already made.

## Drafting

The model emits **JSON, not TOML**. `sc-model` has no structured-output mode, so
either way this is prompt-and-parse — but a model that invents a check kind then
fails at *our* deserialization into the closed `CheckKind` enum, with a precise
error to feed back, rather than emitting plausible TOML that breaks at audit
time. Rendering the TOML ourselves also keeps drafts in the house style and
guarantees the provenance marker is present.

Parsing follows the house pattern: `sc_core::extract_json_array` tolerates
markdown fences and surrounding prose by construction. It departs from the
planner's convention in one way — a parse failure is *reported*, never silently
degraded. The planner can fall back to a generic step and keep moving; a drafting
tool that quietly produces nothing has wasted the author's time and tokens.

The loop:

```
model → extract_json_array → deserialize into CheckKind
      → render TOML → Pack::from_toml_str (runs validate())
      → deterministic lints
      → on failure: feed the exact errors back, retry (max 2)
      → still failing: emit the attempt with a REJECTED banner
```

Lint findings are written as instructions precisely so they work as retry
feedback. Two retries and stop: a model that cannot satisfy an eight-kind closed
vocabulary in three attempts will not manage it on the fourth.

**Provenance is not optional.** Every drafted check carries a
`# DRAFT (<model>, <timestamp>) — REVIEW BEFORE USE` comment plus a reminder to
verify `on_no_files`. An auditor must be able to tell a machine-drafted check
from a human-authored one; they carry different weight. The marker is a comment,
so it survives review-and-edit and disappears only when a human deliberately
deletes it — an explicit act of taking ownership.

## Anti-goals

1. Never write into `crates/sc-comply/packs/`. Landing a pack is a human `git add`.
2. Never commit.
3. Never modify an existing pack in place. Critique emits a *report*, never an edit.
4. Never run during an audit — enforced by the dependency graph.
5. Never emit a draft that fails validation without marking it rejected.
6. Never auto-fix a lint. The whole point is that a human decides.

## The judgment collector (not built)

A later, separate piece: an LLM collector for controls the deterministic
vocabulary cannot express — "is authorization enforced on *every* admin route?"
is a whole-program dataflow question, and regex finds the routes that *have* the
guard, never the one that is missing it.

Its authority is already settled: **`Gap` or `Unknown`, never `Pass`.** A model
that can turn a control green makes that control unauditable, reintroducing
exactly what spec 13 exists to prevent. A model saying "looks fine" is not
evidence; a model saying "the route at `admin.rs:412` has no guard" is a lead a
human verifies in one click.

The constraint belongs in the *collector*, which returns
`Observation { matched: Some(false) | None }` and never `Some(true)` — not in the
pack, since a pack author could otherwise map `on_no_match = "pass"` and route
around it.

## Relationship to other specs

- [13 — Compliance evidence](13-compliance-evidence.md): the engine this tooling
  authors for. The `on_no_files` semantics and the status lattice are defined
  there; this spec is about getting them right in a pack.
- [02 — Model backends](02-model-backends.md): the drafting path rides
  `OpenAiBackend`. Note `with_detected_context()` must **not** be chained for a
  hosted provider — it probes for llama.cpp's `n_ctx` and silently falls back to
  8192.
- [03 — Agent loop](03-agent-loop.md): the tolerant-JSON convention
  (`extract_json_array`, parse failures degrade rather than panic) comes from
  there; this spec explains where it deliberately does not degrade.
- [11 — Testing and TDD](11-testing-and-tdd.md): the self-critique test is this
  crate's oracle — not "does the code run" but "does the tool catch the mistakes
  its author already made".
