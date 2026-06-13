# 10 — Prior art & references

`dumb-coder` is not inventing the agentic-coding wheel. This doc records the
systems and techniques we deliberately borrow from, what we take from each, and
the open debates we're walking into. It exists so design choices in the other
specs can point at evidence rather than assertion.

> Snapshot: mid-2026. The field moves fast; treat specifics as of this date.

## The target model: Gemma 4 E4B

Worth pinning down, since the whole project is sized around it:

- **~4.5B effective / 8B total params**; the "E" is "effective" (edge-optimized).
- **128K context window** — far larger than first assumed. This relaxes (but does
  **not** remove) the context-discipline burden: effective usable context on
  small models is reliably *less* than advertised, and quality degrades as the
  window fills, so [05 — Context management](05-context-management.md) still
  applies.
- **Native function-calling** + a **built-in step-by-step reasoning mode**, with
  "enhanced coding and agentic capabilities."
- Runs on **~5GB RAM at 4-bit** (15GB full) — genuinely laptop/phone-class.
- A larger **Gemma 4 31B** is a natural fit for the orchestrator profile
  ([08](08-orchestration-and-swarm.md)).

Sources: [Ollama `gemma4:e4b`](https://ollama.com/library/gemma4:e4b),
[Google AI — Gemma 4 overview](https://ai.google.dev/gemma/docs/core),
[InfoWorld](https://www.infoworld.com/article/4156597/googles-gemma-4-shines-on-local-systems-both-big-and-small.html).

## On-device Android: AICore vs LiteRT-LM

Two ways to run Gemma 4 locally on Android, both feeding the on-device adapter
([02](02-model-backends.md), M8 in [07](07-roadmap.md)):

- **AICore (OS-managed).** Android's **AICore** system service ships and manages
  the model on the device's behalf; on supported devices **Gemma 4 runs as
  Gemini Nano 4** — so AICore *is* a Gemma 4 path, not a departure from it. No
  weights to bundle, hardware-accelerated, ~4× faster / ~60% less battery than
  the prior gen. **Catch:** flagship-only (~12GB RAM, supported SoC, Gemini Nano
  v3+ on board). The recommended *production* path where the hardware exists.
- **LiteRT-LM (self-hosted).** Ship and run **Gemma 4 E4B/E2B** ourselves via
  **LiteRT-LM** (the recommended successor to the now-maintenance-mode MediaPipe
  LLM Inference API). Models are published ready-to-run
  (`litert-community/gemma-4-E4B-it-litert-lm`). Broader device reach and full
  control; cost is bundling weights.

**Our posture:** prefer AICore when present, fall back to self-hosted LiteRT-LM —
"runs broadly on modest hardware" beats "fastest on flagships only."

Sources: [Announcing Gemma 4 in the AICore Developer Preview](https://android-developers.googleblog.com/2026/04/AI-Core-Developer-Preview.html),
[Gemma 4 = engine behind Gemini Nano on Android](https://gadgetbond.com/google-gemma-4-android-local-agentic-ai-intelligence/),
[LLM Inference guide for Android (Google AI Edge)](https://ai.google.dev/edge/mediapipe/solutions/genai/llm_inference/android),
[gemma-4-E4B-it-litert-lm](https://huggingface.co/litert-community/gemma-4-E4B-it-litert-lm).

## Feasibility evidence

- **Harness >> raw model.** Surveys are blunt: "every model performs
  significantly better inside a structured agent harness than in raw chat mode —
  that investment isn't optional." This is the core `dumb-coder` thesis, and it's
  the consensus. ([MindStudio](https://www.mindstudio.ai/blog/best-open-source-llms-agentic-coding-2026))
- **The small-model gap is real.** Purpose-built 7B coding models score ~18–23%
  on SWE-bench Verified (SWE-Dev-7B: 23.4%); the field recommends 27B+ for
  *serious* autonomous coding. E4B is smaller but agent-tuned. **Implication:**
  scope work tightly, verify constantly, keep humans at the gates — exactly the
  rest of these specs. ([arXiv: Skywork-SWE](https://arxiv.org/pdf/2506.19290))
- **Build the eval early.** A SWE-bench-style fixed task suite is the only honest
  way to answer "is E4B good enough on scoped subtasks?" Tracked from M1
  ([07](07-roadmap.md)).

## Systems we borrow from

### aider — repo map & edit discipline
- **PageRank repo map:** a tree-sitter tag index over the repo builds a symbol
  dependency graph; identifiers mentioned in the conversation get a ~10× boost,
  chat files ~50×; output is token-budgeted. Beats naive file inclusion on edit
  accuracy by precomputing relevance from the code's actual structure instead of
  asking the model to navigate. → adopt in [05](05-context-management.md) /
  `dc-index`.
- **Auto test/lint-repair loop** and **precise edit formats** → [03](03-agent-loop.md)
  (verify) and [04](04-tools.md) (`edit_file`).
- Refs: [Building a better repository map with tree-sitter](https://aider.chat/2023/10/22/repomap.html),
  [Repository Mapping System (DeepWiki)](https://deepwiki.com/Aider-AI/aider/4.1-repository-mapping).

### OpenHands — event-stream architecture
- All agent↔environment interaction flows as **typed events through a central
  hub**; execution sandboxed (Docker); a composable, model-agnostic Agent SDK.
  Our event log ([01](01-architecture.md)) is the same idea — adopt it
  deliberately. MIT-licensed; works with open models (Qwen, Devstral) as well as
  proprietary.
- Refs: [OpenHands SDK](https://docs.openhands.dev/sdk),
  [Software Agent SDK paper](https://arxiv.org/pdf/2511.03690).

### SWE-agent — the Agent-Computer Interface (ACI)
- Core lesson: **the tool/interface surface matters as much as the model.**
  Design tools *for* the agent (clear, narrow, good feedback). This is exactly
  [04 — Tools](04-tools.md)'s stance. Single-agent.
- Ref: [Open-source coding agents survey](https://airesponsibly.substack.com/p/open-source-ai-coding-agents-a-survey).

### Constrained / grammar-based decoding
- Restrict generation to a grammar/schema so output is valid **by construction**;
  turns the typical 1–5% parse-error rate into 0 and "can enable relatively
  small models to perform comparably to much larger alternatives." Tooling:
  **Outlines**, **llama.cpp GBNF**, **vLLM** structured output. NVIDIA showed it
  specifically lifts *small* models on structured generation. → [02](02-model-backends.md)'s
  tool-call strategy ladder.
- **Caveat — the alignment tax.** Over-constraining can degrade a small model's
  reasoning ("structure snowballing"). Mitigation: constrain only the *tool-call
  envelope*; let the model reason freely in an unconstrained scratchpad first.
- Refs: [A guide to structured generation](https://www.aidancooper.co.uk/constrained-decoding/),
  [NVIDIA: grammar-constrained decoding for small models](https://developer.nvidia.com/blog/improving-bash-generation-in-small-language-models-with-grammar-constrained-decoding/),
  [Alignment-tax paper](https://arxiv.org/pdf/2604.06066).

### Multi-agent orchestration with git worktrees
- The **worktree-per-agent** pattern (each agent: own worktree + branch + PR;
  orchestrator handles merges/CI/conflicts) is now standard. Reference impls:
  **Composio agent-orchestrator**, **Claude Code**'s built-in worktrees + shared
  task list, **ccswarm** (notably **written in Rust**, with specialized agent
  pools — Frontend/Backend/DevOps/QA — in worktree-isolated environments; the
  closest existing system to [08](08-orchestration-and-swarm.md), and in our
  language — study it).
- Refs: [Composio agent-orchestrator](https://github.com/ComposioHQ/agent-orchestrator),
  [Claude Code shared task list](https://www.mindstudio.ai/blog/claude-code-agent-teams-shared-task-list),
  [Git worktrees for parallel AI agents](https://www.mindstudio.ai/blog/git-worktrees-parallel-ai-coding-agents).

## The open debate we're walking into: does multi-agent help?

Cognition's "**Don't Build Multi-Agents**" argues multi-agent systems are
**fragile because agents lack a shared global context** — passing context between
agents loses critical information and yields incoherent results. They later
softened, but to a *specific* pattern we should adopt: **"writes stay
single-threaded; multiple agents contribute intelligence"** (e.g. parallel
coding + review loops, not parallel writers).

**What this means for `dumb-coder`:** our worktree-per-worker model is the
*optimistic* form of multi-agent. We therefore default to the conservative
posture in [08](08-orchestration-and-swarm.md) — **parallelize exploration and
proposals, serialize integration/writes through the orchestrator** — with full
parallel-write as an opt-in we validate empirically against the eval suite. The
mandatory integration-verification gate is the backstop, not the only defense.

- Refs: [Don't Build Multi-Agents](https://cognition.ai/blog/dont-build-multi-agents),
  [Multi-Agents: What's Actually Working](https://cognition.ai/blog/multi-agents-working).

## How this maps onto our specs

| Borrowed idea | Lands in |
| --- | --- |
| PageRank tree-sitter repo map | [05](05-context-management.md), `dc-index` |
| Event-stream architecture | [01](01-architecture.md) |
| Agent-Computer Interface (tools for the model) | [04](04-tools.md) |
| Grammar-constrained tool calls (+ envelope-only caveat) | [02](02-model-backends.md) |
| Worktree-per-agent orchestration | [08](08-orchestration-and-swarm.md) |
| Writes-single-threaded concurrency posture | [08](08-orchestration-and-swarm.md) |
| Auto test/lint-repair, precise edits | [03](03-agent-loop.md), [04](04-tools.md) |
| Eval-suite-first feasibility check | [07](07-roadmap.md) M1 |
