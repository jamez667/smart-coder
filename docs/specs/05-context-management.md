# 05 — Context management

## Why this is the most important component

A frontier model with a 200k window can absorb sloppy context. Gemma 4 E4B
actually advertises a **128K window** ([10](10-prior-art.md)) — but the
**effective** usable context of a small model is reliably *less* than advertised,
and quality degrades as the window fills (a small model gets *confused* by
irrelevant context long before it runs out of tokens). So the discipline holds:
for `smart-coder`, deciding *what goes into each prompt* is the difference between
a working agent and a confused one. The Context Manager (`sc-context`) treats the
window as a scarce, hard-budgeted resource — budgeting against an **effective**
limit (`effective_context_fraction`, default 0.9 of the advertised window), not
the nominal max. The fraction is headroom for what the counter cannot see — the
estimator's error, residual template markup — not a law about the model; it
shrank from 0.75 once counts became exact, and shrinks further as the
accounting does.

## The budget

Every prompt is assembled to fit a **hard token budget** derived from the
backend's real context size ([02](02-model-backends.md)), minus a reserve for
the model's response. On a native function-calling backend the tool schemas do
not sit in the system prompt: they ride beside the messages as `tools`, and the
server tokenizes them into the same window. The strategy reports that text in
wire shape (`ToolCallStrategy::request_overhead_text`; ~2k tokens for eighteen
schemas) and the builder charges it before fitting
(`ContextBuilder::with_fixed_overhead`), so `tokens_used` is what the request
really costs. The budget is split into zones. Each zone has a
**priority** (what survives under pressure) and, separately, a **layout rank**
(where it sits in the prompt): the prompt reads System, Task anchor, Retrieved,
History summary, Focus file(s), Recent observations — the observation the model
must react to is always last — while eviction goes by priority alone:

```
┌─────────────────────────────────────────────────────────┐
│  System prompt (role, current step, tool schemas)        │  fixed, minimal
├─────────────────────────────────────────────────────────┤
│  Task anchor (the user's original request, verbatim)     │  always present
├─────────────────────────────────────────────────────────┤
│  Retrieved context (only the snippets relevant NOW)      │  budgeted, ranked
├─────────────────────────────────────────────────────────┤
│  History summary (the turns evicted from the window)     │  budgeted, optional
├─────────────────────────────────────────────────────────┤
│  Focus file(s) (pinned in full, hash-keyed)              │  sacred
├─────────────────────────────────────────────────────────┤
│  Recent observations (the verbatim recent window)        │  sacred, last
└─────────────────────────────────────────────────────────┘
```

If zones don't fit, the Context Manager evicts from lowest priority up
(old history → older retrieved snippets), never dropping the task anchor or the
current step.

## Strategies

### 1. Retrieval over inclusion
Never dump whole files "just in case." The retrieval index (`sc-index`) over the
repo lets the manager pull **only the relevant chunks**:

- Index files into chunks (function/section granularity where the language
  allows) with lightweight symbol extraction.
- Rank by relevance to the current step (keyword/symbol match first; embeddings
  are ruled out for the deterministic core — see [23](23-repo-intelligence.md)
  *Why not embeddings*; the sanctioned escape hatch is orchestrator-side query
  expansion).
- Inject the top-K chunks that fit the retrieval zone, each labeled with
  `path:line` so the model can ask to read more precisely.

**Borrowed algorithm — the PageRank repo map (aider).** Rather than ad-hoc
ranking, `sc-index` builds a **tree-sitter symbol-definition/-reference graph**
over the repo and runs **PageRank** to score how central each symbol is, with
boosts for identifiers mentioned in the current task/conversation (~10×) and for
files already in play (~50×). The output is a compact, token-budgeted "map" of
the *most-referenced* symbols — relevance precomputed from the code's actual
dependency structure instead of asking a small model to navigate. This measurably
beats naive file inclusion on edit accuracy and is a strong default for the
relevance ranking above. See [10 — Prior art](10-prior-art.md).

### 2. Prefix stability (append-only between evictions)
The retrieved zone (plan doc, repo map, ledger, imports, signature map, focus
files) is rendered once per run and re-rendered only when the workspace changes,
keyed by content hash; the recent window is whole turns, appended and never
trimmed by count. So turn N's messages are a byte-identical prefix of turn
N+1's (`crates/sc-core/tests/prefix_stability.rs`) and the backend's prefix KV
cache is reused instead of re-prefilling the whole prompt every turn
([02](02-model-backends.md)). That reuse is measured, not assumed: the prompt
tokens the harness *sends* are identical whether the prefix held or not, so the
evidence is the server's own split ([02](02-model-backends.md)), accumulated per
run as cached-versus-prefilled tokens and reported per ladder row
([11](11-testing-and-tdd.md)). A probe under
`evals/results/2026-09-08-cache-probe/` shows an appended turn reusing 694 of 721
tokens — 27 prefilled instead of 698, 132ms against 931ms. The one deliberate break is the repeat-dedup
nudge, which overwrites the newest user message in place.

### 3. Aggressive observation truncation
Tool results are summarized to fit before re-entering the prompt
([04](04-tools.md)): head+tail of long output, error lines prioritized,
line-numbered file slices instead of whole files. Truncation is always flagged
so the model knows it can request more.

### 4. History compaction (rolling summary)
Older turns are compressed into a short running summary ("decisions made, files
changed, what's verified") rather than kept verbatim. The summary covers exactly
the turns evicted from the recent window, so it changes only when an eviction
happens (which keeps the prefix stable). Recent turns stay verbatim; evicted
ones become summary.

### 5. Structured state instead of prose
Plan status, working-set file list, and budgets are rendered as compact
structured text (small token cost, high signal) rather than narrated, so the
model reliably knows where it is.

## Accurate accounting

The manager budgets against exact counts from the backend's tokenizer when it
has one ([02](02-model-backends.md); probed once per run, memoised by content so
a stable prefix is counted once, not once per turn), falling back to the
heuristic estimator — whose safety margin applies only on that path — when the
backend declines; never a naive char/4 guess at the edges, because overflowing a
small window silently truncates the *most recent* (most important) content on
many runtimes.

## What stays sacred

These are never evicted to make room:

- The **task anchor** (original request) — prevents goal drift.
- The **current step** definition and its tool schemas.
- The **most recent observation** the model must react to.

Everything else is negotiable under budget pressure.

## Inspectability

The exact assembled context for any turn is logged and viewable
([06](06-cli-ux.md), [01](01-architecture.md)). When the agent goes wrong, the
first question is "what did it actually see?" — and the answer is always
available.

The verbose prompt dump carries `budget` and `fixed_overhead` alongside `tokens`,
so a log can say how close a turn came to the ceiling — which is
`budget - fixed_overhead`, since the native `tools` JSON is charged against the
same window (453 tokens for the six-tool eval registry, counted by the server's
tokenizer). Without them a recorded 18,653 tokens cannot be told apart from a
comfortable turn or one a token short of eviction.

Its `zones_evicted` list names only whole non-sacred zones the builder dropped.
Because the recent window is sacred, **history compaction happens in the loop and
never appears there**: an empty `zones_evicted` does not mean nothing was evicted.
Measured on a run driven to saturation, the history summary appeared on turn 12
with `zones_evicted` empty on every one of the 15 turns.

## Tuning knobs (config)

- `context_tokens` cap and response reserve. `response_reserve_tokens` (default
  2048) is sized to ~1.5x the measured peak reply (`AgentReport::peak_reply_tokens`;
  1,328 on the ladder), never guessed — every reserved token is one the prompt
  loses on every turn. A reply that hits the cap raises the `ReplyTruncated`
  harness fault, printed loudly in the run summary; that fault and a rising
  `peak_reply_tokens` are the only reasons to raise it.
- Retrieval top-K (ranking is lexical; see [23](23-repo-intelligence.md)).
- Observation truncation limits.
- `keep_recent_turns` — the **minimum** number of whole turns kept verbatim (a
  floor, not a cap). The window grows freely while the prompt fits; only when
  the built prompt is over budget is the oldest whole turn (action, observation,
  any attached harness note) evicted into the summary, one at a time, never
  below the floor.

Defaults are conservative for tiny windows; users on a roomier 12B model can
loosen them.
