//! `sc-gateway` — one tool surface over many capabilities.
//!
//! The premise: a small model gets *worse* as the tool menu and the observation
//! text grow. It burns context on schemas it will not use, picks the wrong tool
//! from a menu of sixteen, and drowns in output where three lines mattered. The
//! fix is not a better model — it is to stop showing it the machinery.
//!
//! So the model sees exactly one tool with one free-text argument, and three
//! components behind it each know one thing:
//!
//! 1. [`classify`] — knows what every capability does, and *only* that. Routes
//!    a need to one capability, or refuses. Deterministic first; a model is
//!    consulted only for a genuine tie, and only over a shortlist.
//! 2. [`execute`] — each capability knows what *it* does and nothing else. They
//!    run against the existing tool registry, the retrieval index, a live model,
//!    or the web.
//! 3. [`simplify`] — reduces the capability output to what the model needs, and
//!    names everything it removed.
//!
//! Two rules hold this together, and both exist because of failures that are
//! invisible from the outside:
//!
//! * **A refusal beats a guess.** A misroute produces a plausible answer from
//!   the wrong capability, and nothing downstream can detect it. `Unknown` is a
//!   first-class outcome.
//! * **Nothing is dropped silently.** Every reduction is named in the [`Trace`],
//!   so "the capability found nothing" and "the simplifier ate it" are
//!   distinguishable at 2am.
//!
//! Read-only by design: every capability here answers questions. Mutation stays
//! on the discrete tool surface with its permission gates, because a router that
//! can guess wrong must never be able to guess into a write.
//!
//! # Status
//!
//! Not wired into the agent loop. Nothing in `sc-core` calls this crate; it is
//! built and tested standalone until the numbers say it earns its place.
//!
//! ```no_run
//! use std::path::Path;
//! use sc_gateway::{Gateway, Need};
//!
//! let gw = Gateway::new();
//! let answer = gw.ask(&Need::new("read crates/sc-core/src/lib.rs"), Path::new("."));
//! if answer.is_unknown() {
//!     eprintln!("refused: {}", answer.trace.reason);
//! }
//! ```

pub mod bench;
mod capability;
mod classify;
mod execute;
mod simplify;
mod types;

pub use capability::{contains_word, requirements_met, score, table, Capability, Requirement};
pub use classify::{classify, classify_with_model, Route, AMBIGUITY_MARGIN, MIN_SCORE};
pub use execute::{available, Ctx, Verify, WebSearch};
pub use simplify::{simplify, Level, MAX_RAW_CHARS};
pub use types::{Answer, Class, Need, Trace};

use std::path::Path;

/// The single tool surface.
///
/// Holds the capability table and the policy knobs; the per-call context (the
/// workspace, the model, the web seam) is passed in, so one gateway serves many
/// calls across many workspaces.
pub struct Gateway {
    table: Vec<Capability>,
    level: Level,
    /// Escalate an ambiguous need to the model router rather than refusing.
    ///
    /// Off by default: the router is a model call on the hot path, and a
    /// refusal the caller can see beats a guess it cannot.
    escalate: bool,
}

impl Default for Gateway {
    fn default() -> Self {
        Self::new()
    }
}

impl Gateway {
    /// A gateway with the built-in table, extraction-only simplification, and no
    /// model escalation — the configuration with no hidden model calls at all.
    pub fn new() -> Self {
        Self {
            table: capability::table(),
            level: Level::Extract,
            escalate: false,
        }
    }

    /// Replace the capability table. For tests, and for trying a different
    /// vocabulary without touching the classifier.
    pub fn with_table(mut self, table: Vec<Capability>) -> Self {
        self.table = table;
        self
    }

    /// Set the simplification level. [`Level::Summarize`] adds a model call and
    /// makes the output lossy in a way nothing downstream can verify.
    pub fn with_level(mut self, level: Level) -> Self {
        self.level = level;
        self
    }

    /// Allow the model router to break ties that deterministic scoring refused.
    pub fn with_escalation(mut self, escalate: bool) -> Self {
        self.escalate = escalate;
        self
    }

    /// The capability table this gateway routes over.
    pub fn table(&self) -> &[Capability] {
        &self.table
    }

    /// Answer a need with no model, no web access and no verification.
    ///
    /// Those capabilities are unavailable here and refuse rather than falling
    /// back to something that would answer confidently and wrongly.
    pub fn ask(&self, need: &Need, workspace: &Path) -> Answer {
        let ctx = Ctx {
            workspace,
            model: None,
            web: None,
            verify: None,
        };
        self.ask_with(need, &ctx)
    }

    /// Answer a need with the full context: classify, run, simplify.
    ///
    /// The one entry point. Every stage records into the same [`Trace`], so the
    /// returned answer carries its own provenance.
    pub fn ask_with(&self, need: &Need, ctx: &Ctx<'_>) -> Answer {
        let (index, mut trace) = match self.route(need, ctx) {
            Ok(pair) => pair,
            Err(trace) => {
                return Answer {
                    text: format!("Cannot answer that: {}.", trace.reason),
                    trace,
                }
            }
        };

        let cap = &self.table[index];

        // Refuse a capability whose backing seam is absent, rather than letting
        // the executor fail halfway and report it as a capability error.
        if !execute::available(cap.class, ctx) {
            let reason = format!("{} is unavailable in this context", cap.name);
            let considered = std::mem::take(&mut trace.considered);
            return Answer {
                text: format!("Cannot answer that: {reason}."),
                trace: Trace::unknown(reason, considered),
            };
        }

        match execute::run(cap, need, ctx) {
            Ok(raw) => {
                let text = simplify::simplify(&raw, self.level, &mut trace, ctx.model);
                Answer { text, trace }
            }
            // An executor failure is reported as itself, with the route intact,
            // so it reads as "the right capability found nothing" rather than
            // being confused with a refusal to route.
            Err(why) => {
                trace.raw_bytes = why.len();
                trace.out_bytes = why.len();
                Answer {
                    text: format!("{}: {why}", cap.name),
                    trace,
                }
            }
        }
    }

    /// Route a need, escalating to the model router only if configured and only
    /// on a genuine ambiguity.
    fn route(&self, need: &Need, ctx: &Ctx<'_>) -> Result<(usize, Trace), Trace> {
        match classify::classify(need, &self.table) {
            Route::To { index, trace } => Ok((index, trace)),
            Route::Unknown { trace } => {
                if !self.escalate {
                    return Err(trace);
                }
                let Some(model) = ctx.model else {
                    return Err(trace);
                };
                // Shortlist: everything the deterministic pass considered
                // plausible. The router never sees the whole table.
                let shortlist: Vec<usize> = trace
                    .considered
                    .iter()
                    .filter_map(|(name, _)| self.table.iter().position(|c| c.name == *name))
                    .collect();
                match classify::classify_with_model(need, &self.table, &shortlist, model) {
                    Some(index) => Ok((
                        index,
                        Trace {
                            route: Some(self.table[index].name.to_string()),
                            class: Some(self.table[index].class),
                            reason: "llm-router".to_string(),
                            considered: trace.considered,
                            ..Trace::default()
                        },
                    )),
                    None => Err(trace),
                }
            }
        }
    }
}
