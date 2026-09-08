//! Token accounting (spec 05 — accurate accounting).
//!
//! The Context Manager budgets against *real* token counts where the backend
//! exposes a tokenizer, and a conservative heuristic estimator otherwise. The
//! estimator deliberately **over**-counts a little: on a tiny window, the
//! expensive failure is silently overflowing and truncating the most recent
//! (most important) content, so erring high keeps us inside the budget.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

use sc_model::ModelBackend;

/// Not yet asked the backend for a count.
const UNPROBED: u8 = 0;
/// The backend answered: every count comes from its tokenizer.
const EXACT: u8 = 1;
/// The backend declined (no tokenizer, or it stopped answering): estimating.
const ESTIMATED: u8 = 2;

/// Counts exact per-text results remembered before the memo is cleared. A run's
/// prompt is rebuilt every turn from mostly the same segments, so a bound this
/// size holds a whole run; clearing when full is simpler than eviction and the
/// cost of a miss is one tokenizer call.
const CACHE_CAPACITY: usize = 4096;

/// Counts tokens for budgeting, preferring the backend's tokenizer.
///
/// Exact counts are memoised by text, so the stable prefix of a prompt (the
/// system preamble, the task anchor, a pinned focus file) is tokenized once per
/// run rather than once per turn. The estimator is not cached: it is as cheap as
/// hashing the text.
pub struct TokenCounter<'a> {
    backend: Option<&'a dyn ModelBackend>,
    /// One of `UNPROBED` / `EXACT` / `ESTIMATED`, settled by the first count.
    state: AtomicU8,
    /// Exact counts by content hash.
    cache: Mutex<HashMap<u64, usize>>,
}

impl<'a> TokenCounter<'a> {
    /// Count using `backend`'s tokenizer when it has one, else the estimator.
    pub fn new(backend: &'a dyn ModelBackend) -> Self {
        Self {
            backend: Some(backend),
            state: AtomicU8::new(UNPROBED),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// A counter with no backend — always uses the heuristic estimator. Handy for
    /// tests and for budgeting before a backend is chosen.
    pub fn estimator() -> Self {
        Self {
            backend: None,
            state: AtomicU8::new(ESTIMATED),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Token count for `text`: exact when available, else estimated.
    ///
    /// An exact count is the tokenizer's answer and carries NO safety margin --
    /// the margin in [`estimate_tokens`] exists to cover that function's own
    /// error, and there is nothing to cover when the server counted. The
    /// per-message chat-template cost ([`MESSAGE_OVERHEAD_TOKENS`]) is a separate
    /// matter: it is real markup the request carries either way, and the budget
    /// adds it on top of whichever path answered here.
    pub fn count(&self, text: &str) -> usize {
        if let Some(b) = self.backend {
            if self.state.load(Ordering::Relaxed) != ESTIMATED {
                let key = content_hash(text);
                if let Some(n) = self.lock_cache().get(&key) {
                    return *n;
                }
                if let Some(n) = b.count_tokens(text) {
                    self.state.store(EXACT, Ordering::Relaxed);
                    let mut cache = self.lock_cache();
                    if cache.len() >= CACHE_CAPACITY {
                        cache.clear();
                    }
                    cache.insert(key, n);
                    return n;
                }
                // The backend has no tokenizer, or it stopped answering. Either way
                // it is asked nothing more: a hosted provider costs one refused
                // request, not one per segment per turn.
                self.state.store(ESTIMATED, Ordering::Relaxed);
            }
        }
        estimate_tokens(text)
    }

    /// Are counts coming from the backend's tokenizer (true) or the heuristic
    /// estimator (false)?
    ///
    /// Settled by the first count; asking before any count has been made probes
    /// the backend with a short string so the answer is definite. Once the
    /// backend has declined, this stays false for the life of the counter.
    pub fn is_exact(&self) -> bool {
        if self.state.load(Ordering::Relaxed) == UNPROBED {
            self.count("probe");
        }
        self.state.load(Ordering::Relaxed) == EXACT
    }

    fn lock_cache(&self) -> std::sync::MutexGuard<'_, HashMap<u64, usize>> {
        // A poisoned memo is still a valid memo: a panic mid-insert leaves at
        // worst a missing entry, never a wrong one.
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The memo key for a piece of text.
fn content_hash(text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// Heuristic token estimate with a safety margin.
///
/// Tokenizers split on subword boundaries, so a token is shorter than a word but
/// longer than a character. We estimate from character count at ~3.5 chars/token
/// (BPE-ish for code, which has many short tokens: punctuation, brackets, short
/// identifiers) and round **up**, then add 1 so empty-ish strings still cost a
/// little.
///
/// **This counts TEXT, not a request.** A chat request wraps every message in
/// template markup -- role headers, turn delimiters, a generation prompt -- that
/// the server tokenizes too and this function never sees. Measured against
/// llama.cpp: a prompt this estimated at 26,516 tokens was counted by the server
/// at 34,237, a 23% undercount, and the request was rejected. Callers sizing a
/// REQUEST must add [`MESSAGE_OVERHEAD_TOKENS`] per message on top.
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    // chars (not bytes) so multibyte text isn't over-counted wildly.
    let chars = text.chars().count();
    // ceil(chars / 3.5) == ceil(chars * 2 / 7)
    (chars * 2).div_ceil(7) + 1
}

/// Per-message cost of the chat template, in tokens.
///
/// Every message in a chat request carries markup the model tokenizes but the
/// message text does not contain: a role header, a turn delimiter, and for the
/// final message a generation prompt. The exact shape is template-specific, so
/// this is a deliberate over-estimate -- being wrong high costs a little prompt
/// room, being wrong low costs the whole request with an HTTP 400.
pub const MESSAGE_OVERHEAD_TOKENS: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;
    use sc_model::{Capabilities, GenerateRequest, GenerateResponse, ToolCalling};
    use sc_proto::Result;
    use std::cell::Cell;

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
            Ok(GenerateResponse::new(String::new()))
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
        assert!(counter.is_exact());
    }

    #[test]
    fn falls_back_to_estimator_without_a_tokenizer() {
        let counter = TokenCounter::estimator();
        assert_eq!(
            counter.count("one two three"),
            estimate_tokens("one two three")
        );
        assert!(!counter.is_exact());
    }

    /// **An exact count carries no margin.**
    ///
    /// The estimator rounds up and adds one to cover its own error. When the
    /// tokenizer answered there is no error to cover, and stacking the margin on
    /// top would put the budget back where it was: under-filling the window to
    /// make room for a mistake nobody made.
    #[test]
    fn an_exact_count_bypasses_the_estimator_margin() {
        let b = ExactBackend;
        let counter = TokenCounter::new(&b);
        // Three long words: the backend says 3, the estimator (3.5 chars/token,
        // rounded up, plus one) says 15.
        let text = "internationalization localization considerations";
        let exact = 3;
        assert!(
            estimate_tokens(text) > exact,
            "premise: the estimator over-counts this text"
        );
        assert_eq!(
            counter.count(text),
            exact,
            "the tokenizer's answer, untouched"
        );
    }

    /// A backend that counts how often it is asked, and answers what it is told to.
    struct CountingBackend {
        calls: Cell<usize>,
        answer: Option<usize>,
    }
    impl ModelBackend for CountingBackend {
        fn name(&self) -> &str {
            "counting"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                max_context_tokens: 8192,
                tool_calling: ToolCalling::None,
                on_device: false,
            }
        }
        fn generate(&self, _req: &GenerateRequest) -> Result<GenerateResponse> {
            Ok(GenerateResponse::new(String::new()))
        }
        fn count_tokens(&self, _text: &str) -> Option<usize> {
            self.calls.set(self.calls.get() + 1);
            self.answer
        }
    }

    /// **A stable segment is tokenized once per run, not once per turn.**
    ///
    /// The tokenizer is an HTTP round-trip; the system preamble and the task
    /// anchor are the same text on every turn.
    #[test]
    fn a_cache_hit_does_not_ask_the_backend_again() {
        let b = CountingBackend {
            calls: Cell::new(0),
            answer: Some(7),
        };
        let counter = TokenCounter::new(&b);
        assert_eq!(counter.count("the same prefix"), 7);
        assert_eq!(counter.count("the same prefix"), 7);
        assert_eq!(b.calls.get(), 1, "second count served from the memo");

        // Different text is a different question.
        assert_eq!(counter.count("something else"), 7);
        assert_eq!(b.calls.get(), 2);
        assert!(counter.is_exact());
    }

    /// A backend that declines is asked exactly once; after that the counter
    /// estimates without a round-trip, and says so.
    #[test]
    fn a_declining_backend_is_asked_once_then_estimated() {
        let b = CountingBackend {
            calls: Cell::new(0),
            answer: None,
        };
        let counter = TokenCounter::new(&b);
        assert_eq!(
            counter.count("one two three"),
            estimate_tokens("one two three")
        );
        assert_eq!(counter.count("four five"), estimate_tokens("four five"));
        assert_eq!(b.calls.get(), 1, "declined once, never asked again");
        assert!(!counter.is_exact());
    }

    /// Asking whether counts are exact before any count has been made settles the
    /// question with a probe rather than guessing.
    #[test]
    fn is_exact_probes_an_unasked_backend() {
        let b = CountingBackend {
            calls: Cell::new(0),
            answer: Some(1),
        };
        let counter = TokenCounter::new(&b);
        assert!(counter.is_exact());
        assert_eq!(b.calls.get(), 1);
        // And the probe's own answer is memoised like any other.
        counter.count("probe");
        assert_eq!(b.calls.get(), 1);
    }
}
