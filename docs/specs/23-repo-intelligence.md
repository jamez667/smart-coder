# 23 — Repo intelligence

## Principle

**A small model cannot find code. The harness finds code; the model reads it.**

A 35B model with a 24k window does not stumble on the starfield bug because it
cannot reason about line widths — the recorded probe runs show it reasons fine
once `starfield.rs` is in front of it. It stumbles on the twenty turns *before*
that: guessing filenames, re-listing directories, grepping for words that appear
in the question but not in the code. Every wasted turn shrinks the prompt and
feeds the amnesia loop where the model re-reads what it already saw.

So retrieval is a harness responsibility, and it is held to a harness standard:

> **Same repo bytes + same question → same evidence, byte for byte.**

Deterministic retrieval is not an aesthetic preference. It is what makes an
eval run reproducible, a failed investigation diagnosable ("the evidence was
wrong" vs "the model ignored it"), and a probe comparison meaningful. A vector
database with model-generated embeddings fails this standard three ways at once:
nondeterministic, another model to load on a box that is already VRAM-starved
running the coder, and a new endpoint the gateway has never spoken
(`sc-model` calls exactly two routes today: `chat/completions` and `/models`).
This spec is "RAG" in the useful sense — retrieval that augments generation —
with a deterministic lexical core instead of embeddings. The section
*Why not embeddings* makes the case in full.

## What exists today

This spec extends rather than invents. The current inventory:

- `sc-index` <!--@ crates/sc-index/src/lib.rs --> already holds a tree-sitter
  symbol extractor (Rust, Python, C#), a hand-rolled PageRank
  <!--@ crates/sc-index/src/pagerank.rs --> over a definition/reference graph,
  a rendered repo map <!--@ crates/sc-index/src/repomap.rs -->, and
  `find_symbol`. All of it is in-memory and rebuilt from scratch on every call.
- `search_code` <!--@ crates/sc-tools/src/builtin/read.rs --> is a flat regex
  grep: 50 hits, `file:line: text`, no ranking, no symbol context. When the
  question says "trail" and the code says `width_head`, it returns nothing.
- Four directory walks exist with **three disagreeing skip lists**:
  `source_files` (sc-tools, hand-mirrored in sc-win's
  `config::workspace::source_files`), `search_code`'s walk, and
  `collect_sources` (sc-index) — while `gather_sources` (sc-core) wraps
  `source_files`, adding only a 64 KiB cap. `collect_sources` skips `.venv`
  but not `dist` or `.smart-coder`; `search_code` skips `.smart-coder` but no
  dotdir besides `.git`; only the `source_files` pair skips `dist`/`build`.
  Which files the model can *see* depends on which code path asked.
- The investigate flow (`crates/sc-win/src/session/agent.rs`, no spec of its
  own yet) hands the model a **sorted**
  file map capped at 800 entries. Sorted is a measured decision, not a default:
  ranked maps answered 0/4 probe runs, sorted answered 2/2, because ranking
  scattered directory grouping and promoted filename coincidences. Any change
  this spec makes near that map inherits the same burden of measurement.

Three findings from the probe logs anchor the design:

| Finding | Evidence | Consequence here |
|---|---|---|
| The answer to a vague question is often *verbatim in a comment* | `starfield.rs:173` contains the words "thin" and "thick" — the exact words of the user's question | Index comments and string literals, not just identifiers |
| Small menus beat big menus | six tools: `run_command` used 12/12; sixteen tools: 3/12 | This spec adds **zero** tools to any scored registry |
| Ranking can hurt | sorted map 2/2, ranked map 0/4 | New retrieval output is *additive* and shipped behind a flag until probed — see *Investigate leads* |

## Architecture

Everything below lives in `sc-index`, which grows from "symbol scan + PageRank"
into the repo-intelligence crate. No new crate: `sc-tools` already depends on
`sc-index`, the dependency graph stays exactly as it is, and `sc-index` keeps
its property of having no model dependency — retrieval is code, per the
standing rule that the harness resolves paths and the model never composes one.

```
                    ┌───────────────────────────────────────────┐
                    │                 sc-index                  │
   workspace ──────▶│  walk ─▶ RepoIndex (persistent, incr.)    │
                    │            ├─ symbols   (tree-sitter)     │
                    │            ├─ lexicon   (split terms)     │
                    │            ├─ stats     (lines, fn sizes) │
                    │            └─ refs      (PageRank graph)  │
                    │  search(query)   trace(text)   health()   │
                    └───────┬───────────────┬───────────┬───────┘
                            │               │           │
                     search_code /    investigate    CLI / GUI
                     find_symbol      task anchor    reports
```

Five components: the unified walker, the persistent index, lexical search, the
stack-trace resolver, and the health report. Consumers change behind existing
interfaces; the model-facing surface is discussed last because it is the part
most constrained by evidence.

## The walker

One walk <!--@ sc_index::walk -->, one skip list
<!--@ sc_index::SKIP_DIRS -->, one extension policy — the union of the four current
lists: `.git` and all dotdirs, `target`, `node_modules`, `dist`, `build`,
`__pycache__`, `.venv`, `.smart-coder`, plus machine-generated lockfiles
<!--@ sc_index::SKIP_FILES -->, which contributed more index terms than the agent
loop and have never answered a question. A per-file size cap is available
<!--@ sc_index::PROMPT_MAX_FILE_BYTES --> but **opt-in** — see *What shipped*.
The three walks and the sc-win mirror migrate to it; the regression tests that
pinned their individual quirks (binaries and logs stay out of the file map,
session logs stay out of search) move with them and must keep passing against the
unified walker.

This is a prerequisite, not a feature: an index built by a fifth divergent walk
would make the visibility problem worse, not better.

## The index

`RepoIndex` <!--@ sc_index::RepoIndex --> is a persistent,
incrementally-refreshed snapshot of the workspace, stored as one serde_json file at
`.smart-coder/index.json` (a location every
walk already skips). No sqlite, no tantivy — a serialized struct, in the house
tradition of hand-rolled and tiny.

Per file: workspace-relative path (forward-slash normalized, so an index built
on Windows equals one built on Linux), content hash, size, mtime, line count,
language, symbols (name, kind, span — from the existing tree-sitter queries),
function lengths, term postings (below), and outgoing references (feeding the
existing PageRank graph).

Refresh discipline:

- **Staleness check**: size+mtime fast path, hash confirms. Only changed files
  re-parse; deleted files drop out. `RepoIndex::open(workspace)` loads, refreshes,
  saves, returns — cheap enough to call per tool invocation.
- **Version field**: format mismatch, unreadable file, or corrupt JSON → silent
  full rebuild. The cache is an accelerator, never a source of truth; deleting
  `.smart-coder/index.json` must never change any output, only timing.
- **Determinism**: all maps are `BTreeMap`, files serialize in path order, and
  the serialized bytes are a pure function of the tree's contents. Two builds
  of the same tree are byte-identical; a golden test enforces it.

Performance budget: first build under ~2s on a 1k-file repo, warm open under
~50ms. Both are asserted loosely (order-of-magnitude) in a marked test, not
tightly, because CI hardware varies.

## Smart search

The replacement for grep-as-retrieval. The query "why is the trail behind the
stars thin before it gets thick" must surface `Starfield::draw_trails` — and it
can, with no embeddings, because the lexical bridge is real once identifiers
are split and comments are indexed:

- *trail* → `draw_trails`, `base_len` ("trail length" comment)
- *thin*, *thick* → the comment at the flip point, verbatim
- *stars* → `self.stars`, `Starfield`

Mechanics, all deterministic <!--@ sc_index::search -->:

1. **Tokenization** <!--@ sc_index::tokenize --> (same for queries and
   documents): split on
   non-alphanumerics *and* camelCase/snake_case boundaries, lowercase, drop
   one-character tokens and a small fixed stopword list (question words: "why",
   "the", "is", "before", …), then fold a trailing plural. `width_head` indexes as
   `width`, `head`. The plural fold is **not a stemmer** — a stemmer turns
   `intensity` into `intens` and mangles identifiers — it is just enough that a
   user's "stars" reaches the code's `stars` and "trail" reaches `draw_trails`.
2. **Fields with weights**: symbol names (×4), comments (×3), string literals
   (×2), remaining code (×1). Comments outrank plain code deliberately — that
   is where authors write the words users use.
3. **Scoring**: hand-rolled BM25-shaped term weighting (saturating term
   frequency × inverse document frequency), ~100 lines, the same class of
   dependency-free math as `pagerank.rs`. Scores aggregate per **enclosing
   symbol span**, not per line — a hit is "this function", which is the unit a
   model can act on with `read_function`.
4. **Two multipliers after the BM25 sum.** *Coverage*: a hit matching three of
   the question's words beats one matching a single word forty times — the second
   is a rendering loop, the first is an answer. *Test damping* (×0.35): test names
   are long English sentences, exactly the shape a vague query matches by accident,
   and three of them crowded the top five for "why does a tool result get cut off".
   Damped rather than excluded, because a test is sometimes the clearest statement
   of intended behaviour, and applied per **symbol** rather than per path, because
   most tests here are inline `#[cfg(test)]` blocks inside the files they test.
   Both are constants in the diff, not runtime tuning, so they stay inside the
   determinism surface.
5. **Tie-breaks**: score descending, then path ascending, then line ascending.
   No randomness, no hash-order iteration anywhere in the pipeline.

Result rendering is built for a 200-line observation cap and a model that reads
top-down: at most 25 hits, one line each —

```
crates/void_engine/src/fx/starfield.rs:114  fn draw_trails  matched: trail, thin, thick, stars
crates/void_engine/src/fx/starfield.rs:60   fn update      matched: stars, trail
```

— densest evidence first, no raw source lines (the model has `read_file` and
`read_function` for that; search results that quote code tempt the model to
answer from fragments).

### Why not embeddings

Stated once, so the question stays settled until evidence reopens it:

1. **Determinism.** An embedding model is a black box whose output shifts with
   quantization, batch order, and version. The retrieval eval below would
   become unfalsifiable.
2. **Hardware.** The target box runs the 35B coder; probes already show the
   VRAM cliff (a second resident model drops 117 tok/s to 1.8). An embedding
   model is a second resident model.
3. **No endpoint.** The gateway speaks `chat/completions` and `/models`.
   Embeddings would be the first non-chat endpoint, a real surface expansion.
4. **The bridge already exists lexically.** The motivating bug is answered by
   comment indexing + identifier splitting. Ship the cheap thing, measure the
   gap, and let the retrieval eval — not intuition — justify anything heavier.

If the eval later shows a class of question lexical search cannot reach, the
sanctioned escape hatch is a **query-expansion call to the orchestrator model**
(the advisor/diagnose precedent: a second model call belongs in `sc-core`,
never in `sc-tools`, and never in the deterministic core). Expansion rewrites
the query; the index and scoring stay deterministic given the rewritten query.

## Stack traces

When a question contains a panic or traceback, the harness resolves it before
any model sees it — frame parsing is exactly the kind of mechanical work a
small model fumbles and a parser does not.

`resolve_trace(text, &index)` <!--@ sc_index::resolve_trace --> recognizes Rust
panics/backtraces, Python
tracebacks, and .NET stack traces; maps each frame's path to a
workspace-relative path by suffix match against the index; annotates in-repo
frames with the enclosing function via the existing `function_span`; marks
external frames as such; and renders innermost-first:

```
#0 crates/void_engine/src/fx/starfield.rs:153  in draw_trails   (workspace)
#1 <external> alloc::vec::Vec<T>::index
```

Wiring: the investigate path scans the question for a trace; on a hit, the
resolved frames are prepended to the task anchor above the file map, and every file
the trace named is **marked in the map** — `  src/fx/starfield.rs   <- in the stack
trace` — including one that falls outside the 800-entry cap, which would otherwise be
invisible precisely when it is the file we most know is implicated.

Marked, not moved. The obvious design was to boost those files the way in-play files
are boosted in `build_repo_map`, and that is what this spec first said. But the
investigate map is not built by `build_repo_map`; it is a sorted list, and sorted is
a measured decision (2/2 against a ranked map's 0/4) because sorting groups the map
by directory and ranking scatters that. Reordering to boost would trade a measured
win for an unmeasured one. A marker carries the same information and keeps the
grouping.
No new tool — a model that can be handed resolved frames should never be asked
to parse a backtrace with `search_code`.

## Line counts and smells

A deterministic health report <!--@ sc_index::health -->, computed from data the
index already holds:

- per-file line counts; files over 500 lines flagged *warn*, over 1000 *split
  required* (no gate in this repo enforces these today; this report becomes
  their single source of truth),
- functions over the existing 120-line giant-function threshold,
- per-file function counts, TODO/FIXME counts.

Test symbols are excluded from the function count and from giant-hunting: a long
test is a long test, and recommending its split is not the same advice as splitting a
300-line function that production code calls. The report lists only files with
something to say — over a threshold, holding a giant, or carrying a TODO — sorted by
lines descending, then path.

This is a size-and-attention report, not a linter: no style opinions, no
model calls, no configurable rule packs. Its consumer today is the
`smart-coder health` CLI; a GUI surface is not built. Smells do **not** feed search
ranking in this spec — that is an unmeasured idea, and unmeasured ranking changes are exactly
what the sorted-map lesson warns against.

## Model-facing surface

The part where restraint is the design. Rule: **no scored registry grows.**
The six-tool result is one of the few things in this repo with a 12/12-vs-3/12
measurement behind it; this spec spends its improvements *behind* existing tool
names.

1. **`search_code` keeps its name and its one-param schema** (`query: String` —
   the GBNF grammar, the menu text, and the model's habits all survive
   unchanged) but is re-backed by indexed search. The model asks the same vague
   question it always asked; the answers get better. Regex queries still work:
   a query that *looks* like a pattern — a regex metacharacter, or `::`/`.`/`_`
   in a query of three words or fewer — falls through to the literal grep path, so
   precise queries keep their precision. Whether it compiles is deliberately not
   the test: almost any prose compiles as a regex matching itself, so "does it
   parse" would send every question to grep and change nothing. An indexed search
   that finds nothing also falls back to grep, so the tool never returns worse than
   it did before.
2. **`find_symbol` is re-backed by the persistent index** — same output, no
   per-call rescan.
3. **Investigate leads, behind a flag.** After the sorted file map (which does
   not change — sorted won its probe and stays), the task anchor may append a
   bounded block:

   ```
   leads (indexed search over your question):
     crates/void_engine/src/fx/starfield.rs:114  fn draw_trails  matched: trail, thin, thick
     ...
   ```

   at most 8 lines <!--@ crates/sc-win/src/session/agent.rs -->, additive, off by
   default <!--@ sc_win::session::agent::leads_enabled -->. It ships default-on
   only if the probe says so — the acceptance bar is the investigate probe
   answering in **fewer steps** with leads than without, and the sorted-map
   regression (2/2) still holding. A retrieval feature that costs anchor tokens
   without shortening runs is deleted, not tuned.

   **The probe has now run** (`logs/leads-probe.md`, tiel-coder-35b against
   void-claim, the user's original question verbatim):

   | | answered | steps | tool calls | elapsed |
   |---|---|---|---|---|
   | leads OFF | yes | 10 | 9 | 59s |
   | leads ON | yes | 5 | 5 | 18s |

   Both arms reached the same correct answer — `starfield.rs`, the two `batch.line`
   widths, the fix being to swap them. Leads halved the run, and the OFF transcript
   shows exactly the behaviour this spec's principle describes: steps 1 and 2 were
   spent reading `crates/void_claim/src/hyperspace.rs` and
   `crates/void_claim/src/starfield.rs`, **neither of which exists**. The model was
   guessing filenames. With leads, `crates/void_engine/src/fx/starfield.rs:114
   fn draw_trails` was the first line of the block and the first file it opened.

   The bar is met, so the default *should* flip — but flipping it is a human
   decision, and one probe on one question is one data point. The switch stays off
   until a person looks at that file.

   The switch is the `SC_INVESTIGATE_LEADS` environment variable rather than a
   `config.json` field, following `SC_INVESTIGATE_VERBOSE`. It exists to be
   flipped for one probe run and compared; a setting that persists in a config
   file is a setting somebody forgets is on while reading the numbers. It becomes
   a real field the day the measurement says it should be the default. The A/B
   itself is `crates/sc-win/tests/leads_probe.rs`
   <!--@ crates/sc-win/tests/leads_probe.rs -->, which runs both arms and writes a
   verdict to `logs/leads-probe.md` — and deliberately asserts nothing about which
   arm wins, because an assertion encoding the expected result would make the
   measurement decorative.

`repo_health` and `resolve_trace` never enter a model registry. The CLI gets
`smart-coder index | search | health | stack` subcommands
<!--@ crates/sc-cli/src/cmd/index.rs --> (`stack`, not `trace`, because `trace` is
spec traceability and one verb should not mean two things). Each takes `--json` for
scripts; `stack` reads the trace from stdin (`cargo test 2>&1 | smart-coder stack`),
and `search <query...>` joins its remaining arguments so a question can be typed
unquoted, the way it would be said. — for humans, for
scripts, and above all for debugging: `smart-coder search "<question>"` prints
*exactly* what the model would have been shown, which turns "why did the
investigation go sideways" into a reproducible one-liner.

## What shipped, and what the build changed

Three things the design did not anticipate, each forced by evidence and each worth
carrying forward:

- **The 64 KiB cap is opt-in, not the walk's default.** It was `gather_sources`'
  prompt-size guard applied *after* its walk, never a walk policy. Five source files
  in this repo exceed it, `crates/sc-core/src/agent/mod.rs` among them; defaulting it
  on would have quietly deleted the project's biggest files from the repo map and from
  `find_symbol`.
- **Postings anchor to the enclosing definition, and a doc comment counts as part of
  the definition below it.** Search aggregates per symbol span anyway, so per-line
  detail was resolution nobody read and most of the file (716k postings became 397k).
  The doc-comment rule matters more: that comment is outside the span but is the most
  valuable text in the file for retrieval, because it is where the author wrote in the
  user's language.
- **Test code is damped, per symbol rather than per path.** Test names are long
  English sentences, exactly the shape a vague query matches by accident; three tests
  crowded the top five for "why does a tool result get cut off". Per symbol because
  most tests here are inline `#[cfg(test)]` blocks inside the files they test.

Two more rules came from running the CLI against this repo for the first time.
**Agent transcripts are not source**: `logs/` is skipped by name, because the
recorded probe transcripts contain the canonical question *and* its answer, so the
first `smart-coder search` returned seven copies of `investigate-probe.md` above any
code. And **a frame or hit landing on a doc comment is about the function below
it** <!--@ crates/sc-index/src/store.rs -->: a stack frame two lines above a
function reported a bare line number until it could say `in draw_trails`.

Measured once, on this repo at 619 files: 2.6s cold build, 171ms warm open, a
~21 MB cache. A snapshot of a before/after, not a property — the repo grows, and
the only figure a test pins is the shape that matters (`warm × 3 ≤ cold`). Reaching that took removing two redundant tree-sitter parses —
`function_span` per symbol was ~13s on the largest file alone — and declining to index
lockfiles, which contributed more terms than the agent loop and have never answered a
question.

## Measurement

The genuinely new instrument: a **model-free retrieval eval** that runs in CI.

`evals/retrieval/suite.toml` <!--@ evals/retrieval/suite.toml --> holds
`[[queries]]` entries <!--@ sc_eval::RetrievalSuite --> — a natural-language
question, a fixture directory, and the expected file(s)/symbol(s). Grading is
"expected target appears in the top-k hits" (k=5 strict, k=25 loose). Because
search is deterministic, this suite needs no model, no GPU, and no flakiness
allowance; it runs inside `scripts/check.sh` like any unit test.

A query may also carry `expect = "miss"`, asserting the lexical index is expected
*not* to find the target. That reads backwards until you consider the alternative:
a suite of only-winnable questions measures nothing, and quietly becomes a suite
somebody tuned the ranker against. A declared miss keeps the gap visible and turns
the suite red the day it starts working — which is news worth having.

Fixture strategy: the `evals/ladder/tasks/engine-*` fixtures are already in-repo
slices of the same game codebase as the starfield bug — the vague-question set
starts there ("why does the diagonal path cut corners", phrased the way a user
would), plus questions against smart-coder itself. The starfield question is
the canonical entry, run against the engine fixture that contains the file.

Beyond CI, two model-in-the-loop measurements gate the flag flips: the
investigate probes (steps-to-answer, with/without leads) and, later, ladder
rungs in the `rung:symptomatic` family over multi-file fixtures — the rungs
that grade diagnostic distance are precisely the ones retrieval should move.

## Testing

- **Determinism goldens**: build the index twice over a fixture tree →
  byte-identical; same query twice → byte-identical results; delete the cache
  file → identical results, only slower.
- **Walker unification**: the migrated regression tests from all four old
  walks pass against the shared walker; a test asserts the skip list is the
  union of the old four.
- **Search**: tokenizer table tests (camelCase, snake_case, stopwords); a
  fixture test asserting the starfield-shaped query ranks the trail-drawing
  function first in a miniature fixture; field-weight ordering (a comment hit
  outranks a body hit at equal frequency).
- **Trace resolver**: one fixture per format (Rust panic, Rust backtrace,
  Python, .NET); suffix mapping with Windows and POSIX path separators; a
  trace with zero in-repo frames degrades to "no workspace frames" without
  error.
- **Incrementality**: touch one file → exactly one record re-parses (observed
  via a parse counter); corrupt the cache → full rebuild, same bytes.
- **Retrieval suite**: `evals/retrieval` green in `scripts/check.sh`.

## What this is not

- Not a vector database, an embedding pipeline, or a semantic-similarity
  service — see *Why not embeddings*; the escape hatch is query expansion via
  the orchestrator, and it is not in this spec's scope.
- Not new tools in any scored registry. If a future menu wants `repo_health`,
  that is a new measured decision against the six-tool evidence.
- Not a linter or a code reviewer — smells are size signals; judgment lives in
  `sc-review` ([16](16-post-integration-review.md)).
- Not a rewrite of the sorted file map. The map stays sorted; leads are
  additive and flag-gated.
- Not cross-repo. One workspace, one index, matching the tool sandbox.

## Relationship to other specs

- **[04](04-tools.md) Tools** — `search_code`/`find_symbol` contracts are
  unchanged; only their implementations move.
- **[05](05-context-management.md) Context** — leads and resolved traces enter
  through the task anchor / Retrieved zone and live inside existing budgets;
  nothing here adds a new zone.
- **[17](17-spec-traceability.md) Traceability** — this spec anchors only code
  that exists at writing time; implementation commits add symbol anchors
  (`sc_index::RepoIndex`, `sc_index::search`, …) as the symbols land, keeping
  `sc-trace` green at every commit.
- **Investigate** (`crates/sc-win/src/session/agent.rs`, no spec of its own
  yet) — the primary consumer; its probe suite
  (`crates/sc-win/tests/investigate_probe.rs`) is this spec's acceptance
  instrument.
- **[11](11-testing-and-tdd.md) / evals** — the retrieval suite joins the
  check gate; ladder rungs follow once the flag work settles.
