# 05 — Context management

## Why this is the most important component

A frontier model with a 200k window can absorb sloppy context. **Gemma 3n E4B
class models often have an effective window of a few thousand tokens** and
degrade fast as it fills. For `dumb-coder`, deciding *what goes into each prompt*
is the difference between a working agent and a confused one. The Context Manager
(`dc-context`) treats the window as a scarce, hard-budgeted resource.

## The budget

Every prompt is assembled to fit a **hard token budget** derived from the
backend's real context size ([02](02-model-backends.md)), minus a reserve for
the model's response. The budget is split into zones with priorities:

```
┌─────────────────────────────────────────────────────────┐
│  System prompt (role, current step, tool schemas)        │  fixed, minimal
├─────────────────────────────────────────────────────────┤
│  Task anchor (the user's original request, verbatim)     │  always present
├─────────────────────────────────────────────────────────┤
│  Retrieved context (only the snippets relevant NOW)      │  budgeted, ranked
├─────────────────────────────────────────────────────────┤
│  Recent observations (last tool result(s))               │  budgeted
├─────────────────────────────────────────────────────────┤
│  History summary (compacted older turns)                 │  budgeted, optional
└─────────────────────────────────────────────────────────┘
```

If zones don't fit, the Context Manager evicts from lowest priority up
(old history → older retrieved snippets), never dropping the task anchor or the
current step.

## Strategies

### 1. Retrieval over inclusion
Never dump whole files "just in case." The retrieval index (`dc-index`) over the
repo lets the manager pull **only the relevant chunks**:

- Index files into chunks (function/section granularity where the language
  allows) with lightweight symbol extraction.
- Rank by relevance to the current step (keyword/symbol match first; embeddings
  optional and pluggable — a small local embedder, or none on constrained
  hardware).
- Inject the top-K chunks that fit the retrieval zone, each labeled with
  `path:line` so the model can ask to read more precisely.

### 2. Just-in-time, step-scoped context
Context is rebuilt **per step**, not accumulated forever. When the loop moves to
a new step, stale snippets from the previous step are dropped and fresh ones
retrieved for the new step. The window reflects *what matters right now*.

### 3. Aggressive observation truncation
Tool results are summarized to fit before re-entering the prompt
([04](04-tools.md)): head+tail of long output, error lines prioritized,
line-numbered file slices instead of whole files. Truncation is always flagged
so the model knows it can request more.

### 4. History compaction (rolling summary)
Older turns are compressed into a short running summary ("decisions made, files
changed, what's verified") rather than kept verbatim. The summary itself is
budgeted and refreshed. Recent turns stay verbatim; distant ones become summary.

### 5. Structured state instead of prose
Plan status, working-set file list, and budgets are rendered as compact
structured text (small token cost, high signal) rather than narrated, so the
model reliably knows where it is.

## Accurate accounting

The manager budgets against real token counts from the gateway's tokenizer
([02](02-model-backends.md)), with a safety margin — never a naive char/4
guess at the edges, because overflowing a small window silently truncates the
*most recent* (most important) content on many runtimes.

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

## Tuning knobs (config)

- `context_tokens` cap and response reserve.
- Retrieval top-K and ranking method (lexical / embedding / hybrid).
- Observation truncation limits.
- History compaction threshold (when to start summarizing).

Defaults are conservative for tiny windows; users on a roomier 12B model can
loosen them.
