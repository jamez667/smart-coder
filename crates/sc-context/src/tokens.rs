//! Token accounting (spec 05 — accurate accounting).
//!
//! The Context Manager budgets against *real* token counts where the backend
//! exposes a tokenizer, and a conservative heuristic estimator otherwise. The
//! estimator deliberately **over**-counts a little: on a tiny window, the
//! expensive failure is silently overflowing and truncating the most recent
//! (most important) content, so erring high keeps us inside the budget.

use sc_model::ModelBackend;

/// Counts tokens for budgeting, preferring the backend's tokenizer.
pub struct TokenCounter<'a> {
    backend: Option<&'a dyn ModelBackend>,
}

impl<'a> TokenCounter<'a> {
    /// Count using `backend`'s tokenizer when it has one, else the estimator.
    pub fn new(backend: &'a dyn ModelBackend) -> Self {
        Self {
            backend: Some(backend),
        }
    }

    /// A counter with no backend — always uses the heuristic estimator. Handy for
    /// tests and for budgeting before a backend is chosen.
    pub fn estimator() -> Self {
        Self { backend: None }
    }

    /// Token count for `text`: exact when available, else estimated.
    pub fn count(&self, text: &str) -> usize {
        if let Some(b) = self.backend {
            if let Some(n) = b.count_tokens(text) {
                return n;
            }
        }
        estimate_tokens(text)
    }
}

/// Heuristic token estimate with a safety margin.
///
/// Tokenizers split on subword boundaries, so a token is shorter than a word but
/// longer than a character. We estimate from character count at ~3.5 chars/token
/// (BPE-ish for code, which has many short tokens: punctuation, brackets, short
/// identifiers) and round **up**, then add 1 so empty-ish strings still cost a
/// little. This reliably sits at or above real counts for code and prose.
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    // chars (not bytes) so multibyte text isn't over-counted wildly.
    let chars = text.chars().count();
    // ceil(chars / 3.5) == ceil(chars * 2 / 7)
    (chars * 2).div_ceil(7) + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_model::{Capabilities, GenerateRequest, GenerateResponse, ToolCalling};
    use sc_proto::Result;

    #[test]
    fn estimator_is_zero_for_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimator_grows_with_length_and_over_counts_slightly() {
        // 70 chars -> ceil(140/7)=20, +1 = 21 tokens. A real tokenizer would put
        // ~70 chars of code near 18-22 tokens, so we're in range and not under.
        let s = "a".repeat(70);
        assert_eq!(estimate_tokens(&s), 21);
        // Monotonic.
        assert!(estimate_tokens("short") < estimate_tokens("a much longer string here"));
    }

    /// A backend that reports an exact, deliberately-distinctive token count so we
    /// can prove the counter prefers it over the estimator.
    struct ExactBackend;
    impl ModelBackend for ExactBackend {
        fn name(&self) -> &str {
            "exact"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                max_context_tokens: 8192,
                tool_calling: ToolCalling::None,
                on_device: false,
            }
        }
        fn generate(&self, _req: &GenerateRequest) -> Result<GenerateResponse> {
            Ok(GenerateResponse {
                content: String::new(),
            })
        }
        fn count_tokens(&self, text: &str) -> Option<usize> {
            Some(text.split_whitespace().count()) // 1 token per word — distinctive
        }
    }

    #[test]
    fn prefers_backend_tokenizer_when_present() {
        let b = ExactBackend;
        let counter = TokenCounter::new(&b);
        // "one two three" -> 3 words via the backend, not the char estimate (~5).
        assert_eq!(counter.count("one two three"), 3);
    }

    #[test]
    fn falls_back_to_estimator_without_a_tokenizer() {
        let counter = TokenCounter::estimator();
        assert_eq!(
            counter.count("one two three"),
            estimate_tokens("one two three")
        );
    }
}
