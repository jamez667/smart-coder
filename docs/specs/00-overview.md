# 00 — Overview, goals & non-goals

## Vision

`dumb-coder` is a terminal-based agentic coding assistant whose defining
constraint is that it runs on **small** language models — never larger than 12B
parameters, and ideally on a model as small as **Gemma 3n E4B**.

The thesis is that a well-engineered harness can extract competent,
*reliable* coding behavior from a model that, on its own, is a poor agent. We
treat the model as a narrow, fallible reasoning unit and surround it with
structure: constrained tools, decomposed tasks, aggressive context discipline,
schema-enforced outputs, and verification loops.

## Why this is hard (and the design follows from it)

Small models differ from frontier models in specific, predictable ways. Every
major design decision in `dumb-coder` is a direct response to one of these:

| Small-model weakness | Consequence | Design response |
| --- | --- | --- |
| Shallow multi-step reasoning | Loses the plot on long tasks | Explicit planner that decomposes work into small, single-purpose steps (see [03](03-agent-loop.md)) |
| Small context window (often 4k–32k) | Can't hold a large repo or long history | Retrieval + summarization + strict context budget (see [05](05-context-management.md)) |
| Unreliable free-form tool/JSON output | Malformed tool calls, hallucinated args | Constrained decoding / grammar-enforced tool schemas, with a repair loop (see [04](04-tools.md)) |
| Weak instruction-following under load | Ignores parts of long prompts | Short, single-responsibility prompts; one decision per turn |
| Poor self-correction | Repeats the same failed action | Harness-side loop detection, verification gates, and budgets |
| Limited world knowledge | Wrong API usage, stale assumptions | Ground every step in actual file contents and command output, never memory |

## Goals (v1)

1. **Run a real coding task end-to-end** on Gemma 3n E4B class models: read a
   repo, make a focused change across a few files, run tests, iterate.
2. **Backend-agnostic.** The same agent runs against Ollama, a llama.cpp
   server, vLLM, or an on-device Android runtime with only config changes.
3. **Deterministic, auditable loop.** Every model turn, tool call, and context
   decision is logged and replayable. The user can always see *why* the agent
   did something.
4. **Graceful degradation.** When the model produces garbage, the harness
   detects it and recovers (repair, retry, re-plan, or ask the user) rather
   than charging ahead.
5. **Fast inner loop.** Interactive on commodity hardware; sub-second to
   first token where the backend allows.

## Non-goals (v1)

- **No large/frontier-model support path.** The constraints are the product. We
  will not add "just use GPT-4 for the hard parts." (A backend *could* point at
  a big model, but the harness is tuned for and tested against small ones.)
- **No editor/IDE extension.** CLI only for v1 (see [06](06-cli-ux.md)).
- **No autonomous, unattended operation.** v1 is human-in-the-loop. Long-running
  background autonomy is future work.
- **No multi-repo / monorepo-scale indexing.** v1 targets a single repository
  that fits a modest retrieval index.
- **No fine-tuning or training.** We adapt to off-the-shelf models via prompting
  and harness design, not weight changes. (Adapters/LoRA are future work.)

## Target users

- Developers who want a private, local coding assistant with no API dependency.
- People on constrained or offline hardware (laptops, homelabs, phones).
- Researchers probing how far small models can be pushed on agentic tasks.

## Guiding principles

1. **The harness is the product, not the model.** Assume the model is dumb;
   make the surrounding system smart.
2. **Ground everything.** Decisions reference real file bytes and real command
   output, never the model's recollection.
3. **Small steps, verified.** Prefer many tiny, checkable actions over one big
   speculative one.
4. **Fail loud and recover.** Detect bad model output early; never silently act
   on it.
5. **Everything is inspectable.** Plans, prompts, tool calls, and context
   windows are all visible and logged.
