//! The live swarm *topology*: the advisor, the orchestrator, and each coder as nodes,
//! with edges that light up when a message flows between them (orchestrator→coder on
//! dispatch/integration, advisor→coder on consult). Folded from the [`SwarmEvent`]
//! stream into pure data — no iced types — so it's host-testable; the canvas widget
//! in `app.rs` just draws what's here.
//!
//! Animation without a clock in the model: every event is folded with an explicit
//! `now` (monotonic seconds). An edge remembers *when* it last fired; its glow
//! `intensity(now)` decays from 1.0 to 0.0 over [`FLOW_FADE_SECS`]. The app passes the
//! real elapsed time each frame; tests pass synthetic values.

use std::collections::BTreeMap;

use dc_swarm::SwarmEvent;

/// How long (seconds) a message-flow glow takes to fade from full to nothing.
pub const FLOW_FADE_SECS: f32 = 1.5;

/// A coder node's lifecycle state (drives its box colour).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoderState {
    /// Actively running its subtask.
    Working,
    /// Finished its proposal; awaiting integration.
    Done,
    /// Being retried (its integration left tests red).
    Retrying,
    /// Integrated successfully.
    Integrated,
    /// Reverted/rejected.
    Reverted,
}

impl CoderState {
    /// Whether this state reads as a problem (for colouring).
    pub fn is_bad(self) -> bool {
        matches!(self, CoderState::Retrying | CoderState::Reverted)
    }
}

/// One coder (worker) node, keyed by its subtask id.
#[derive(Debug, Clone)]
pub struct Coder {
    pub subtask: String,
    pub goal: String,
    pub state: CoderState,
    /// The latest advice the advisor gave this coder, if any (shown near the edge).
    pub last_advice: Option<String>,
    /// The full single-shot prompt this coder was handed (what it "saw"). `None` until
    /// it starts.
    pub prompt: Option<String>,
    /// The full file content this coder proposed back. `None` until it finishes.
    pub proposal: Option<String>,
}

/// Which endpoint a coder edge connects to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Peer {
    /// The orchestrator (right) — dispatch and integration.
    Orchestrator,
    /// The advisor (left) — consultation.
    Advisor,
}

/// A directed message-flow on an edge: who, to which coder, and when it last fired.
#[derive(Debug, Clone)]
pub struct Flow {
    pub peer: Peer,
    pub coder: String,
    /// Monotonic seconds when this flow last fired.
    fired_at: f32,
}

impl Flow {
    /// The glow intensity in `[0,1]` at time `now`, decaying linearly to 0 over
    /// [`FLOW_FADE_SECS`]. A just-fired flow is 1.0; an old one is 0.0.
    pub fn intensity(&self, now: f32) -> f32 {
        let age = (now - self.fired_at).max(0.0);
        (1.0 - age / FLOW_FADE_SECS).clamp(0.0, 1.0)
    }
}

/// The whole topology at a point in time.
#[derive(Debug, Default, Clone)]
pub struct Topology {
    /// Coders in first-seen order (BTreeMap keeps lookups simple; render order is the
    /// insertion order tracked separately).
    coders: BTreeMap<String, Coder>,
    order: Vec<String>,
    /// Recent flows; old (faded) ones are pruned on insert.
    flows: Vec<Flow>,
    /// Whether the orchestrator has decomposed yet (so the canvas can show it active).
    pub decomposed: bool,
    /// Whether the advisor has ever been consulted (so the canvas can dim it until used).
    pub advisor_used: bool,
    /// Set when the swarm finishes.
    pub done: bool,
    /// The orchestrator's raw decomposition reply, for the detail panel.
    pub orchestrator_reply: Option<String>,
    /// Whether the decomposition fell back to a trivial single subtask.
    pub fell_back: bool,
}

impl Topology {
    /// Fold one event into the topology at monotonic time `now` (seconds).
    pub fn apply(&mut self, ev: &SwarmEvent, now: f32) {
        use SwarmEvent::*;
        match ev {
            Decomposed { .. } => self.decomposed = true,
            OrchestratorPrompt {
                reply, fell_back, ..
            } => {
                self.decomposed = true;
                self.orchestrator_reply = Some(reply.clone());
                self.fell_back = *fell_back;
            }
            WorkerStarted {
                subtask,
                goal,
                prompt,
            } => {
                self.upsert(subtask, |c| {
                    c.goal = goal.clone();
                    c.state = CoderState::Working;
                    c.prompt = Some(prompt.clone());
                });
                // Orchestrator dispatched this coder.
                self.fire(Peer::Orchestrator, subtask, now);
            }
            WorkerFinished {
                subtask, proposal, ..
            } => {
                self.upsert(subtask, |c| {
                    c.state = CoderState::Done;
                    c.proposal = Some(proposal.clone());
                });
            }
            SubtaskRetry { subtask, .. } => {
                self.upsert(subtask, |c| c.state = CoderState::Retrying);
            }
            AdvisorConsulted { subtask, advice } => {
                self.advisor_used = true;
                self.upsert(subtask, |c| c.last_advice = Some(advice.clone()));
                // Advisor → coder message.
                self.fire(Peer::Advisor, subtask, now);
            }
            Integrated {
                subtask, accepted, ..
            } => {
                let state = if *accepted {
                    CoderState::Integrated
                } else {
                    CoderState::Reverted
                };
                self.upsert(subtask, |c| c.state = state);
                // Orchestrator integrated (or reverted) this coder's work.
                self.fire(Peer::Orchestrator, subtask, now);
            }
            SwarmDone { .. } => self.done = true,
        }
    }

    /// Look up a coder by its subtask id.
    pub fn coder(&self, subtask: &str) -> Option<&Coder> {
        self.coders.get(subtask)
    }

    /// Coders in first-seen (render) order.
    pub fn coders(&self) -> Vec<&Coder> {
        self.order
            .iter()
            .filter_map(|id| self.coders.get(id))
            .collect()
    }

    /// All flows currently glowing at `now` (intensity > 0), for the canvas to draw.
    pub fn active_flows(&self, now: f32) -> Vec<(&Flow, f32)> {
        self.flows
            .iter()
            .map(|f| (f, f.intensity(now)))
            .filter(|(_, i)| *i > 0.0)
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Insert (or update) a coder, applying `f`.
    fn upsert(&mut self, subtask: &str, f: impl FnOnce(&mut Coder)) {
        if !self.coders.contains_key(subtask) {
            self.order.push(subtask.to_string());
            self.coders.insert(
                subtask.to_string(),
                Coder {
                    subtask: subtask.to_string(),
                    goal: String::new(),
                    state: CoderState::Working,
                    last_advice: None,
                    prompt: None,
                    proposal: None,
                },
            );
        }
        f(self.coders.get_mut(subtask).expect("just inserted"));
    }

    /// Record a fresh message-flow on the (peer, coder) edge. There's at most one flow
    /// per edge: drop any prior one (stale or still glowing) and push the fresh one, so
    /// the vector stays bounded and the newest fire wins.
    fn fire(&mut self, peer: Peer, coder: &str, now: f32) {
        self.flows.retain(|f| !(f.peer == peer && f.coder == coder));
        self.flows.push(Flow {
            peer,
            coder: coder.to_string(),
            fired_at: now,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(id: &str) -> SwarmEvent {
        SwarmEvent::WorkerStarted {
            subtask: id.to_string(),
            goal: format!("do {id}"),
            prompt: format!("Task: do {id}"),
        }
    }

    #[test]
    fn worker_start_creates_a_coder_and_an_orchestrator_flow() {
        let mut t = Topology::default();
        t.apply(&started("t1"), 0.0);
        assert_eq!(t.coders().len(), 1);
        assert_eq!(t.coders()[0].state, CoderState::Working);
        // A fresh orchestrator→coder flow glows at full intensity.
        let flows = t.active_flows(0.0);
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].0.peer, Peer::Orchestrator);
        assert!((flows[0].1 - 1.0).abs() < 1e-6);
    }

    #[test]
    fn flow_glow_decays_to_zero_over_the_fade_window() {
        let mut t = Topology::default();
        t.apply(&started("t1"), 0.0);
        // Halfway through the fade → ~0.5 intensity.
        let mid = t.active_flows(FLOW_FADE_SECS / 2.0);
        assert!((mid[0].1 - 0.5).abs() < 0.05, "got {}", mid[0].1);
        // Past the window → gone.
        assert!(t.active_flows(FLOW_FADE_SECS + 0.1).is_empty());
    }

    #[test]
    fn advisor_consult_lights_an_advisor_edge_and_records_advice() {
        let mut t = Topology::default();
        t.apply(&started("t2"), 0.0);
        assert!(!t.advisor_used);
        t.apply(
            &SwarmEvent::AdvisorConsulted {
                subtask: "t2".to_string(),
                advice: "split the helper out first".to_string(),
            },
            10.0,
        );
        assert!(t.advisor_used, "advisor box lights up once consulted");
        let coder = t.coders()[0];
        assert_eq!(
            coder.last_advice.as_deref(),
            Some("split the helper out first")
        );
        // The advisor→coder flow is fresh at now=10.
        let adv: Vec<_> = t
            .active_flows(10.0)
            .into_iter()
            .filter(|(f, _)| f.peer == Peer::Advisor)
            .collect();
        assert_eq!(adv.len(), 1);
        assert_eq!(adv[0].0.coder, "t2");
    }

    #[test]
    fn lifecycle_states_track_the_events() {
        let mut t = Topology::default();
        t.apply(&started("t1"), 0.0);
        t.apply(
            &SwarmEvent::WorkerFinished {
                subtask: "t1".to_string(),
                summary: "done".to_string(),
                proposal: "body".to_string(),
            },
            1.0,
        );
        assert_eq!(t.coders()[0].state, CoderState::Done);

        t.apply(
            &SwarmEvent::Integrated {
                subtask: "t1".to_string(),
                accepted: true,
                files: vec!["a.rs".to_string()],
            },
            2.0,
        );
        assert_eq!(t.coders()[0].state, CoderState::Integrated);
        assert!(!t.coders()[0].state.is_bad());

        t.apply(
            &SwarmEvent::SubtaskRetry {
                subtask: "t1".to_string(),
                attempt: 1,
                max: 2,
                failing_tests: vec!["x".to_string()],
            },
            3.0,
        );
        assert!(t.coders()[0].state.is_bad());
    }

    #[test]
    fn refiring_an_edge_replaces_the_stale_flow_not_appends() {
        let mut t = Topology::default();
        t.apply(&started("t1"), 0.0); // orchestrator flow at 0
        t.apply(
            &SwarmEvent::Integrated {
                subtask: "t1".to_string(),
                accepted: true,
                files: vec![],
            },
            0.2,
        ); // orchestrator flow refired at 0.2
        let orch: Vec<_> = t
            .active_flows(0.2)
            .into_iter()
            .filter(|(f, _)| f.peer == Peer::Orchestrator && f.coder == "t1")
            .collect();
        assert_eq!(
            orch.len(),
            1,
            "one flow per (peer,coder), refreshed not duplicated"
        );
    }
}
