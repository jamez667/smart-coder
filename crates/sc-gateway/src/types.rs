//! The shapes that cross the gateway: what a model asks for, what it gets back,
//! and the trace that says how the answer was produced.
//!
//! The model only ever sees [`Answer::text`]. Everything else here exists so a
//! *human* can tell a misroute apart from a bad capability apart from a
//! simplifier that ate the detail — three failures that look identical from the
//! outside once they're behind one tool.

use serde::Serialize;

/// What the model asks for: one free-text need, optionally scoped to a path.
///
/// Deliberately not a query language. A small model composing structured query
/// syntax is the same failure as a small model composing a path — it goes wrong
/// silently and there's no schema to catch it. One string, and the classifier
/// owns the hard part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Need {
    /// The natural-language need, verbatim from the model.
    pub text: String,
    /// Optional path scope, when the caller already knows where to look.
    pub scope: Option<String>,
}

impl Need {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            scope: None,
        }
    }

    pub fn scoped(text: impl Into<String>, scope: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            scope: Some(scope.into()),
        }
    }
}

/// Which capability class answered (or would answer) a need.
///
/// The split is by *cost and determinism*, not by subject matter: that's what
/// decides caching, ordering, and whether a repeat call can be trusted to give
/// the same answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// Pure function of the workspace. Cacheable, repeatable, free.
    Deterministic,
    /// Retrieval over an index. Cheap, repeatable while the index is fixed.
    Retrieval,
    /// Runs the project verification suite. Deterministic in principle, but it
    /// spawns processes and touches build artifacts, so it is neither free nor
    /// safely cacheable — a rerun can legitimately give a different answer.
    Verify,
    /// A live model call. Costs tokens, not repeatable, not cacheable.
    Live,
    /// An outbound network call. Costs latency, not repeatable.
    Web,
}

impl Class {
    /// Can two identical calls be assumed to give the same answer?
    ///
    /// `Verify` is excluded deliberately: the whole point of rerunning a suite
    /// is that the answer may have changed.
    pub fn repeatable(self) -> bool {
        matches!(self, Class::Deterministic | Class::Retrieval)
    }

    /// Is this class free to run (no tokens, no network)?
    pub fn free(self) -> bool {
        self == Class::Deterministic
    }
}

/// What the gateway returns. The model sees `text` and nothing else.
#[derive(Debug, Clone)]
pub struct Answer {
    /// The minimal text handed back to the model.
    pub text: String,
    /// How it was produced. For humans and tests, never for the model.
    pub trace: Trace,
}

impl Answer {
    /// Did this answer come back as an honest "I don't know"?
    ///
    /// A refusal is a *successful* outcome of the classifier — the alternative
    /// is a confident answer from the wrong capability, which the caller cannot
    /// detect. See [`crate::classify`].
    pub fn is_unknown(&self) -> bool {
        self.trace.route.is_none()
    }
}

/// The full record of one gateway call: what was chosen, why, what ran, and
/// what was dropped on the way out.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Trace {
    /// The capability chosen, or `None` if the classifier refused.
    pub route: Option<String>,
    /// The class of the chosen capability.
    pub class: Option<Class>,
    /// Why this route was chosen — a deterministic rule name, or `llm-router`.
    pub reason: String,
    /// Alternatives the classifier considered and their scores, best first.
    pub considered: Vec<(String, u32)>,
    /// Bytes the capability produced before simplification.
    pub raw_bytes: usize,
    /// Bytes handed to the model after simplification.
    pub out_bytes: usize,
    /// What the simplifier removed, named rather than silent.
    pub dropped: Vec<String>,
    /// Whether an LLM was used to summarize the output (lossy, unverifiable).
    pub summarized: bool,
}

impl Trace {
    /// A refusal: no route, and a reason a human can act on.
    pub fn unknown(reason: impl Into<String>, considered: Vec<(String, u32)>) -> Self {
        Self {
            route: None,
            class: None,
            reason: reason.into(),
            considered,
            ..Self::default()
        }
    }

    /// How much of the capability's raw output survived to the model, 0..=100.
    ///
    /// The headline number for "is the simplifier earning its place" — and the
    /// number to watch for the failure where it earns too much by dropping what
    /// mattered.
    pub fn retained_percent(&self) -> u32 {
        if self.raw_bytes == 0 {
            return 100;
        }
        ((self.out_bytes as f64 / self.raw_bytes as f64) * 100.0).round() as u32
    }
}
