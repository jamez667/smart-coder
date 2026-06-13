# 02 — Model backends & abstraction

## Goal

One agent, many runtimes. The same `dumb-coder` session must run against
**Ollama, llama.cpp's server, vLLM, an on-device Android runtime, or any
OpenAI-compatible endpoint** with nothing but a config change. The harness is
tuned for small models (≤ 12B, ideally Gemma 4 E4B), but it must not be
coupled to *how* those models are served.

## The `ModelBackend` trait

All inference goes through one trait in `dc-model`. Concrete adapters implement
it; `dc-core` only ever sees the trait.

```rust
// Illustrative — not final signatures.
#[async_trait]
pub trait ModelBackend: Send + Sync {
    /// Static description of what this backend can do (see Capabilities).
    fn capabilities(&self) -> Capabilities;

    /// One non-streaming generation. Returns the full assistant message.
    async fn generate(&self, req: GenerateRequest) -> Result<GenerateResponse, ModelError>;

    /// Streaming variant; yields tokens/deltas for live rendering.
    async fn stream(&self, req: GenerateRequest)
        -> Result<BoxStream<'_, Result<Delta, ModelError>>, ModelError>;

    /// Optional: token count for budgeting. Falls back to an estimator if None.
    async fn count_tokens(&self, text: &str) -> Option<usize>;
}
```

`GenerateRequest` carries: messages, sampling params (temp, top_p, seed, max
tokens), an optional **output constraint** (JSON schema or GBNF grammar for
tool calls), and stop sequences.

## Capabilities negotiation

Backends differ in what they support. The harness adapts at runtime based on a
declared capability set rather than assuming a feature exists.

```rust
pub struct Capabilities {
    pub max_context_tokens: usize,        // e.g. 8192, 32768
    pub streaming: bool,
    pub native_tool_calling: ToolCalling, // None | OpenAiStyle | Custom
    pub constrained_decoding: Constrained,// None | JsonSchema | Gbnf
    pub supports_seed: bool,
    pub tokenizer: TokenizerInfo,         // for accurate budgeting
}
```

The harness uses capabilities to choose its strategy for the hardest small-model
problem — **getting a valid tool call out of the model**:

| If backend supports… | Tool-call strategy |
| --- | --- |
| GBNF grammar (llama.cpp) | Constrain decoding to the exact tool-call grammar. Strongest guarantee. |
| JSON-schema mode (vLLM, some Ollama models) | Constrain to the tool's JSON schema. |
| OpenAI-style function calling | Use native `tools`/`tool_choice`. |
| Nothing (plain completion) | Prompt for a fenced JSON block + a parser + a **repair loop** (re-prompt with the parse error). |

This is central: small models emit malformed tool calls far more often than
large ones, so wherever the backend can *enforce* structure, we use it.
Constrained decoding turns the typical 1–5% parse-error rate into 0 and lets a
small model perform like a much larger one on structured output — well-evidenced
prior art ([10](10-prior-art.md)). Our **primary target, Gemma 4 E4B, has native
function-calling**, so it can use the OpenAI-style path directly; the grammar/
schema paths matter most for models that lack it.

> **Caveat — the alignment tax.** Over-constraining can *degrade* a small model's
> reasoning ("structure snowballing", [10](10-prior-art.md)). So constrain only
> the **tool-call envelope** — let the model reason freely in an unconstrained
> scratchpad first, then emit the structured call. Don't grammar-constrain the
> thinking, only the action.

## Planned adapters (v1)

1. **OpenAI-compatible HTTP** — covers vLLM, LM Studio, llama.cpp's
   `--api`, text-generation-webui, and Ollama's OpenAI-compat endpoint. One
   adapter, broad coverage. *Primary path.*
2. **Ollama native** — `/api/chat` + `/api/generate`, model pull/list, and
   Ollama-specific options. First-class because it's the easiest local setup.
3. **llama.cpp (direct/server)** — to expose **GBNF grammar** constrained
   decoding, which gives the most reliable tool calls on tiny models.
4. **On-device / Android** — run the model in-process or via a local runtime
   (e.g. an MLC/llama.cpp build) so the tool can operate fully offline on a
   phone. Likely behind a feature flag; thinnest viable adapter first.

> A backend can technically point at a large model, but `dumb-coder` is
> developed and benchmarked against small ones — that's the whole premise.

## Model configuration

Backends and models are selected by config (file + CLI flags + env), not code.

```toml
# ~/.config/dumb-coder/config.toml  (illustrative)
[model]
backend = "ollama"            # ollama | openai | llamacpp | android
model   = "gemma4:e4b"
context_tokens = 8192         # override / cap the window we actually use
temperature = 0.2
seed = 42

[model.openai]               # used when backend = "openai"
base_url = "http://localhost:8000/v1"
api_key_env = "DC_API_KEY"   # optional, for remote OpenAI-compat servers
```

Multiple named profiles are allowed (e.g. a fast `planner` model and a separate
`coder` model), so the harness can route different loop phases to different small
models if desired. This is the mechanism behind the **orchestrator vs. worker**
split in the swarm — the orchestrator is just a larger profile (up to the 12B
ceiling) and workers are tiny profiles ([08](08-orchestration-and-swarm.md)).

## Tokenizer & budgeting

Accurate token counts matter more on small models because the window is tiny.
The gateway prefers, in order: (1) a backend-provided `count_tokens`, (2) a
bundled tokenizer matching the model family, (3) a heuristic estimator with a
safety margin. The Context Manager ([05](05-context-management.md)) always
budgets against this count.

## Backend behavior the harness must tolerate

- **No streaming** → fall back to blocking `generate`, render on completion.
- **No seed** → mark the session non-replayable; warn but continue.
- **Smaller real context than advertised** → respect the configured cap; the
  Context Manager treats the budget as hard.
- **Transient errors / timeouts** → bounded retry with backoff at the adapter
  layer, surfaced as a typed `ModelError` if exhausted.
