# Implementation prompt — spec 23 (repo intelligence)

> **COMPLETED.** All seven milestones shipped: `0f4b2db` (M1 walker), `3bb2ea4`
> (M2 index), `6168050` (M3 search + retrieval eval), `4b97232` (M4 tools),
> `eecb550` (M5 traces), `fcb95c7` (M6 health + CLI), `db15a95` / `d85a479`
> (M7 leads + the A/B measurement). `docs/specs/23-repo-intelligence.md` is the
> source of truth for what exists; this file is kept as the record of what was
> asked for.
>
> Three things shipped differently from the plan below, each for a reason the spec
> records: the CLI subcommand is **`stack`**, not `trace` (which is spec
> traceability); the leads switch is the **`SC_INVESTIGATE_LEADS` env var**, not an
> `AgentConfig` field, because it exists to be flipped for one probe and compared;
> and trace files are **marked** in the file map rather than boosted, because that
> map is sorted rather than ranked and sorted won its own probe.

Paste the block below into a new session. The milestones are independent
commits; each one lands green on `scripts/check.sh` and pushes to `main`
before the next begins. If a milestone goes sideways, stop and re-plan rather
than pushing through.

---

Implement **spec 23 — repo intelligence** (`docs/specs/23-repo-intelligence.md`).
Read that spec in full first; it is the source of truth. Read spec 04 (tools)
and spec 05 (context management) alongside it. The investigate flow has no
spec of its own — its source of truth is the code listed below. Read these
files before writing any code — the design leans on all of them:

- `crates/sc-index/src/{symbols.rs,repomap.rs,workspace.rs,pagerank.rs}`
- `crates/sc-tools/src/builtin/{registry.rs,read.rs,util.rs,dispatch.rs}`
- `crates/sc-core/src/agent/{dispatch.rs,prompt.rs,config.rs}` and
  `crates/sc-core/src/agent/mod.rs:171-231`
- `crates/sc-win/src/session/agent.rs:203-476` (the investigate run and its
  measured config comments — the comments are load-bearing evidence)
- `crates/sc-eval/src/{task.rs,runner.rs}` and `evals/ladder/suite.toml`

## Ground rules (non-negotiable)

1. **No new dependencies** beyond what the workspace already carries
   (`serde`/`serde_json`, `regex`, `sha2`, tree-sitter + grammars). BM25-ish
   scoring and the trace parsers are hand-rolled, like `pagerank.rs`. The
   `regex` crate has no look-around — don't write patterns that need it.
2. **No scored registry grows.** `default_registry`, `read_only_registry`,
   `minimal_worker_registry`, and the eval solver's six-tool KEEP list are all
   untouched. `search_code` and `find_symbol` keep their names, schemas, and
   menu descriptions verbatim — the GBNF grammar output for the read-only
   registry must be byte-identical before and after (there is a dump example at
   `crates/sc-tools/examples/gbnf_dump.rs`; assert this in a test).
3. **Determinism is tested, not assumed.** Byte-identical goldens per the
   spec's Testing section. All index maps are `BTreeMap`; paths are stored
   workspace-relative with forward slashes; no `HashMap` iteration reaches any
   output.
4. **The sorted file map does not change.** Leads are additive, after the map,
   flag-gated, default **off**. Do not flip the default in this work; that flip
   is a separate probe-measured commit (M7 produces the numbers, the human
   flips the flag).
5. Windows is the dev box: every path test runs both `\` and `/` inputs; the
   index normalizes on write.
6. New code lands with symbol anchors in spec 23 as the symbols come into
   existence (spec 17 style, e.g. `<!--@ sc_index::RepoIndex -->`), and
   `sc-trace` stays green at every commit. Run the spec-guardian audit before
   each commit.

## Milestones

### M1 — the unified walker (pure refactor)

New `crates/sc-index/src/walk.rs`: one walker with one skip list (the union:
`.git` + all dotdirs, `target`, `node_modules`, `dist`, `build`,
`__pycache__`, `.venv`, `.smart-coder`), a parameterizable extension policy,
and a per-file byte cap (default 64 KiB, from `gather_sources`). Migrate the
existing walks to it:

- `sc-tools/src/builtin/util.rs` `source_files` (keep its test-file and
  workflow-artifact filters as a policy layered on the walker, not in it)
- `crates/sc-win/src/config/workspace.rs` `source_files` — a hand-synced
  mirror of the sc-tools one; delete it and call the shared walker
- `sc-tools/src/builtin/read.rs` `search_code`'s walk
- `sc-index/src/workspace.rs` `collect_sources`

`sc-core/src/agent/prompt.rs` `gather_sources` wraps `source_files` and needs
no migration of its own — just confirm its 64 KiB cap survives as the
walker's default. Move the existing regression tests with them (notably
`map_contents::binaries_and_logs_are_not_in_the_map` in sc-win must still
pass). Add a test asserting the shared skip list contains every entry from
each of the old lists. Behavior deltas (a walk now skipping `.venv` that
didn't before) are expected and fine; note them in the commit message.

### M2 — the persistent index

`crates/sc-index/src/store.rs` (+ types in `lib.rs`): `RepoIndex` with
`open(workspace) -> RepoIndex` (load → refresh stale → save → return) and the
per-file record from the spec: path, sha256, size, mtime, line count,
language, symbols (reuse `extract_symbols`), function spans/lengths, term
postings (M3 fills these — land the field now so the format doesn't churn),
outgoing references. Cache file: `.smart-coder/index.json`, with a format
version; any mismatch/corruption → silent full rebuild. Include a
test-only parse counter so incrementality is observable ("touch one file →
one re-parse"). Goldens: build twice → byte-identical; delete cache →
identical bytes.

### M3 — lexicon and search

`crates/sc-index/src/{lexicon.rs,search.rs}`:

- Tokenizer: split on non-alphanumerics and camelCase/snake_case boundaries,
  lowercase, drop 1-char tokens and a small **fixed** stopword list (commit
  the list as a `const`; it is part of the determinism surface).
- Fields with weights — symbol names ×4, comments ×3, string literals ×2,
  code ×1. Comments and strings come from tree-sitter nodes, not regex.
- BM25-shaped scoring aggregated per enclosing symbol span; ties broken
  score desc → path asc → line asc. Cap 25 hits. Render one line per hit
  exactly as the spec shows (no source-line quoting).
- **The model-free retrieval eval**: `evals/retrieval/suite.toml` with
  `[[queries]]` (question, fixture dir, expected paths/symbols, strict k=5 /
  loose k=25), a small runner in `sc-eval` (or a `sc-index` test — pick
  whichever is less machinery; do not build a parallel harness), wired into
  `scripts/check.sh`. Seed it with: the starfield question against the
  `evals/ladder/tasks/engine-*` fixture that contains trail/starfield code (check
  which; if none does, add a minimal fixture containing the real
  `starfield.rs` draw_trails function), the diagonal-path question, and at
  least four questions against smart-coder's own crates phrased the way a
  user would phrase them.

The unit test that matters most: a vague natural-language query whose words
appear only in comments must rank the right function first.

### M4 — re-back the tools

- `search_code` in `sc-tools/src/builtin/read.rs`: queries that contain regex
  metacharacters and compile as regex keep the existing literal grep path;
  everything else routes through `RepoIndex::open` + indexed search. Same
  `ToolSpec`, same menu text. Assert the read-only registry GBNF is unchanged.
- `find_symbol` routing in `sc-core/src/agent/dispatch.rs:136-139` re-backed
  by the persistent index; identical output format.
- Respect result caps: indexed output must sit comfortably inside
  `observation_line_cap` (200).

### M5 — stack-trace resolver

`crates/sc-index/src/trace.rs`: `resolve_trace(text, &index)` for Rust
panic + backtrace, Python traceback, .NET stack trace. Suffix-match frame
paths to indexed paths (both separators), annotate in-repo frames with the
enclosing function via `function_span`, render innermost-first per the spec.
Wire into investigate: in `sc-win/src/session/agent.rs`, scan the question
for a trace; on a hit, prepend resolved frames to the task anchor above the
file map and add the frame files to the in-play boosts that `build_repo_map`
already accepts. Fixture tests per format; a no-workspace-frames trace
degrades gracefully.

### M6 — health report and CLI

`crates/sc-index/src/health.rs`: line counts, >500/>1000 file flags, >120-line
functions (share one `const` with the existing `GIANT_FN_LINES` in
`sc-tools/src/builtin/read.rs` — hoist it, don't duplicate it), fn counts,
TODO/FIXME. Then CLI subcommands in `sc-cli`: `smart-coder index`,
`smart-coder search <query>`, `smart-coder trace` (reads stdin), and
`smart-coder health`. `search` prints exactly the bytes the model would see.
No GUI surface in this pass.

### M7 — investigate leads, flag-gated, measured

Add the `leads:` block (≤8 lines, after the sorted map) behind a new
`AgentConfig` field defaulting to `false`, set from `tune_for_investigation`
only when a config flag asks for it. Then produce the measurement, following
the existing probe pattern (`crates/sc-win/tests/investigate_probe.rs`,
`#[ignore]`, live backend): run the probe suite with and without leads,
record steps-to-answer and answered/not into `logs/` the way the
BEFORE/AFTER/FINAL transcripts were recorded. **Do not flip the default**;
report the numbers and stop — the flip is a human decision per the spec.

## Delivery order and hygiene

M1 → M2 → M3 → M4 are strictly sequential. M5 and M6 can follow in either
order. M7 is last. One commit (or a small series) per milestone, each green on
`scripts/check.sh`, each pushed to `main`. Use the repo's existing commit-
message voice (see `git log --oneline -10`). After M4 and again after M7,
update spec 23's anchors and run the spec-guardian audit.

## What done looks like

- `smart-coder search "why is the trail behind the stars thin before it gets
  thick"` prints `draw_trails` in the top hits, deterministically, twice in a
  row, byte-identical.
- `evals/retrieval` passes in `scripts/check.sh` with no model and no GPU.
- The read-only registry's GBNF and menu text are unchanged.
- The probe logs contain a with/without-leads comparison a human can read and
  act on.
