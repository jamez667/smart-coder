//! Advisor escalation — "junior asks senior" (spec 02 tiered models, spec 00
//! "scale out, tier by difficulty").
//!
//! When the small coder model (T2) gets stuck — looping, thrashing, or it
//! explicitly calls `ask_user` — the harness consults a *larger* advisor model
//! (T1, up to the 12B ceiling) the way a junior engineer asks a senior for a
//! nudge. Crucially the advisor gives **advice, not the implementation**: a short
//! hint about what to try next. The junior still does the work. This keeps the
//! expensive model's role to the high-reasoning, low-volume judgment it's best at,
//! and the cheap model doing the edits.
//!
//! The advisor is optional. With none configured, escalation is a clean stop with
//! an [`crate::recovery::StopReason::Escalated`] — there's simply no senior to ask.

use dc_model::{GenerateRequest, Message, ModelBackend};

/// The context handed to the advisor: what the junior is stuck on.
pub struct Predicament<'a> {
    /// The original task.
    pub task: &'a str,
    /// The current plan rendering (where we are).
    pub plan: &'a str,
    /// A short recent-history summary (what's been tried).
    pub recent: &'a str,
    /// Why we're escalating (the stall reason or the junior's question).
    pub trigger: &'a str,
}

/// Consult `advisor` for a nudge. Returns a short piece of advice to inject back
/// into the junior's loop, or `None` if the advisor errored (degrade to a stop).
///
/// The prompt deliberately forbids writing code: we want direction, not a
/// solution the junior would just paste.
pub fn consult(advisor: &dyn ModelBackend, p: &Predicament) -> Option<String> {
    let system = "You are a senior engineer advising a junior coding agent that is \
        stuck. Give ONE short, concrete next-step hint (1-3 sentences) — a nudge in \
        the right direction. Do NOT write the code or the full solution; the junior \
        will do the work. Point at what to check, reconsider, or try differently."
        .to_string();
    let user = format!(
        "Task: {}\n\n{}\n\nWhat's been tried recently:\n{}\n\nThe junior is stuck: {}\n\n\
         Give one short hint for what to try next.",
        p.task, p.plan, p.recent, p.trigger
    );
    let req = GenerateRequest::new(vec![Message::system(system), Message::user(user)]);
    match advisor.generate(&req) {
        Ok(resp) => {
            let advice = resp.content.trim();
            if advice.is_empty() {
                None
            } else {
                Some(advice.to_string())
            }
        }
        Err(_) => None,
    }
}

/// Format advice as guidance to inject into the junior's next prompt.
pub fn advice_observation(advice: &str) -> String {
    format!("ADVICE from a senior engineer (a hint, not a solution — you do the work): {advice}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use dc_model::{Capabilities, GenerateResponse, MockBackend, ToolCalling};
    use dc_proto::Result;

    fn predicament() -> Predicament<'static> {
        Predicament {
            task: "make is_even correct",
            plan: "plan:\n  [~] 1. edit is_even",
            recent: "edited impl.sh twice, tests still red",
            trigger: "looping on the same edit",
        }
    }

    #[test]
    fn returns_advice_from_the_advisor() {
        let advisor = MockBackend::new(["Check the modulo: $1 % 2 should equal 0 for even."]);
        let advice = consult(&advisor, &predicament()).unwrap();
        assert!(advice.contains("modulo"));
        assert!(advice_observation(&advice).contains("ADVICE from a senior"));
    }

    #[test]
    fn the_advisor_sees_the_predicament() {
        // A backend that echoes back what it was asked, so we can assert the
        // predicament (task + trigger) reached the advisor prompt.
        struct Echo;
        impl ModelBackend for Echo {
            fn name(&self) -> &str {
                "echo"
            }
            fn capabilities(&self) -> Capabilities {
                Capabilities {
                    max_context_tokens: 8192,
                    tool_calling: ToolCalling::None,
                    on_device: false,
                }
            }
            fn generate(&self, req: &GenerateRequest) -> Result<GenerateResponse> {
                Ok(GenerateResponse {
                    content: req.messages.last().unwrap().content.clone(),
                })
            }
        }
        let advice = consult(&Echo, &predicament()).unwrap();
        assert!(advice.contains("make is_even correct"), "task missing");
        assert!(
            advice.contains("looping on the same edit"),
            "trigger missing"
        );
    }

    #[test]
    fn advisor_error_yields_no_advice() {
        let advisor = MockBackend::new(Vec::<String>::new()); // errors on generate
        assert!(consult(&advisor, &predicament()).is_none());
    }

    #[test]
    fn empty_advice_is_treated_as_none() {
        let advisor = MockBackend::new(["   "]);
        assert!(consult(&advisor, &predicament()).is_none());
    }
}
