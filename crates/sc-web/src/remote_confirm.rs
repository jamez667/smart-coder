//! Remote approve/deny: a [`Confirmer`] whose answer arrives from an HTTP POST
//! rather than a local button click.
//!
//! This is the web/phone analog of `sc_win`'s `ChannelConfirmer` (the desktop GUI
//! seam). Same invariant: the agent worker thread blocks inside `confirm_command`
//! on a one-shot reply channel until a human answers, and if the answer never comes
//! (server tearing down, or an explicit cancel) it falls back to `Deny` rather than
//! hanging the run forever.
//!
//! The difference from `ChannelConfirmer` is *where* the reply comes from. There is
//! no separate UI thread draining a `Pending` queue — the resolver is the HTTP
//! handler for `POST /approve` | `POST /deny`. So this type owns the waiter map
//! directly: `confirm_command` registers a `reply_tx` under a fresh `id` and blocks;
//! [`RemoteConfirmer::resolve`] (called from the POST handler) looks the `id` up and
//! sends the answer.
//!
//! The pending prompt is announced to viewers by pushing an
//! [`AgentEvent::ConfirmPending`] into the [`Hub`], so it rides the *same* replay
//! path as every other event: a phone that polls `/events` sees it, and a phone that
//! reconnects mid-approval replays it (with no matching `ConfirmResolved`) and
//! re-renders its buttons. `resolve` pushes [`AgentEvent::ConfirmResolved`] so every
//! connected viewer clears the prompt.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use sc_core::{AgentEvent, Confirmation, Confirmer};

use crate::hub::Hub;

/// The waiter table: each blocked `confirm_command` registers its one-shot reply
/// sender under a monotonic id the inbound POST addresses.
#[derive(Default)]
struct PendingConfirms {
    next_id: u64,
    waiters: HashMap<u64, Sender<Confirmation>>,
}

/// A [`Confirmer`] answered over HTTP. Clone-shares its state (`Arc`), so the copy
/// wired into the agent's `AgentConfig` and the copy held by the HTTP server both
/// see the same pending table and hub.
#[derive(Clone)]
pub struct RemoteConfirmer {
    hub: Hub,
    pending: Arc<Mutex<PendingConfirms>>,
}

impl RemoteConfirmer {
    pub fn new(hub: Hub) -> Self {
        Self {
            hub,
            pending: Arc::new(Mutex::new(PendingConfirms::default())),
        }
    }

    /// Register a pending confirmation whose reply channel is owned *elsewhere* (the
    /// desktop mirror: the `App` already holds the `Sender<Confirmation>` from its gate
    /// bar). Announces `ConfirmPending{id,..}` on the hub for remote clients and returns
    /// the `id` so a later `/approve`/`/deny` resolves it. This is the non-blocking
    /// counterpart to `confirm_command` (which the worker calls and blocks on).
    pub fn register(&self, command: &str, reason: &str, reply: Sender<Confirmation>) -> u64 {
        let id = {
            let mut g = self.pending.lock().unwrap();
            let id = g.next_id;
            g.next_id += 1;
            g.waiters.insert(id, reply);
            id
        };
        self.hub.push(AgentEvent::ConfirmPending {
            id,
            command: command.to_string(),
            reason: reason.to_string(),
        });
        id
    }

    /// Answer a pending confirmation from the HTTP layer. Returns `true` if `id`
    /// named a still-pending waiter (so the POST handler can `404`/`409` a replayed
    /// or unknown id — the removal makes every approval strictly single-use, so a
    /// replayed `POST /approve` can never be applied to a later, different command).
    pub fn resolve(&self, id: u64, answer: Confirmation) -> bool {
        let tx = {
            let mut g = self.pending.lock().unwrap();
            g.waiters.remove(&id)
        };
        match tx {
            Some(tx) => {
                let allowed = !matches!(answer, Confirmation::Deny(_));
                // Send may fail only if the worker already gave up (channel dropped);
                // either way the waiter is gone, so the resolve "succeeded" in the
                // sense that this id is now settled.
                let _ = tx.send(answer);
                self.hub.push(AgentEvent::ConfirmResolved { id, allowed });
                true
            }
            None => false,
        }
    }

    /// Deny every outstanding waiter — used on cancel/shutdown so a run blocked on a
    /// human can unwind instead of hanging. Each denied waiter also emits a
    /// `ConfirmResolved` so viewers clear their prompts.
    pub fn deny_all(&self, reason: &str) {
        let drained: Vec<u64> = {
            let mut g = self.pending.lock().unwrap();
            let ids: Vec<u64> = g.waiters.keys().copied().collect();
            for id in &ids {
                if let Some(tx) = g.waiters.remove(id) {
                    let _ = tx.send(Confirmation::Deny(reason.to_string()));
                }
            }
            ids
        };
        for id in drained {
            self.hub
                .push(AgentEvent::ConfirmResolved { id, allowed: false });
        }
    }
}

impl Confirmer for RemoteConfirmer {
    fn confirm_command(&self, command: &str, default_reason: &str) -> Confirmation {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let id = {
            let mut g = self.pending.lock().unwrap();
            let id = g.next_id;
            g.next_id += 1;
            g.waiters.insert(id, reply_tx);
            id
        };
        // Announce the pending prompt on the event stream so viewers can render
        // approve/deny buttons (and a reconnecting viewer replays it).
        self.hub.push(AgentEvent::ConfirmPending {
            id,
            command: command.to_string(),
            reason: default_reason.to_string(),
        });
        // Block until an HTTP handler resolves us. If the sender is dropped without a
        // send (server gone), deny with the static reason rather than hanging — the
        // exact fallback `ChannelConfirmer` uses.
        reply_rx
            .recv()
            .unwrap_or_else(|_| Confirmation::Deny(default_reason.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn confirm_blocks_until_resolved_and_announces() {
        let hub = Hub::new();
        let confirmer = RemoteConfirmer::new(hub.clone());

        // The worker blocks inside confirm_command on a background thread.
        let worker_confirmer = confirmer.clone();
        let worker = thread::spawn(move || {
            worker_confirmer.confirm_command("rm -rf build", "shell blocked")
        });

        // The pending prompt lands on the hub as ConfirmPending(id=0) — poll until it
        // shows up (the worker races us to register + push).
        let mut announced = None;
        for _ in 0..200 {
            let (events, _, _) = hub.since(0);
            if let Some(AgentEvent::ConfirmPending { id, command, .. }) = events
                .iter()
                .find(|e| matches!(e, AgentEvent::ConfirmPending { .. }))
            {
                assert_eq!(command, "rm -rf build");
                announced = Some(*id);
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let id = announced.expect("ConfirmPending was announced on the hub");

        // Resolve it; the blocked worker returns the answer.
        assert!(confirmer.resolve(id, Confirmation::AllowOnce));
        assert_eq!(worker.join().unwrap(), Confirmation::AllowOnce);

        // A ConfirmResolved(allowed=true) was announced, and a replayed resolve is a no-op.
        let (events, _, _) = hub.since(0);
        assert!(events.iter().any(
            |e| matches!(e, AgentEvent::ConfirmResolved { id: rid, allowed: true } if *rid == id)
        ));
        assert!(
            !confirmer.resolve(id, Confirmation::AllowOnce),
            "replayed resolve is a no-op"
        );
    }

    #[test]
    fn deny_answer_marks_not_allowed() {
        let hub = Hub::new();
        let confirmer = RemoteConfirmer::new(hub.clone());
        let wc = confirmer.clone();
        let worker = thread::spawn(move || wc.confirm_command("git push", "shell blocked"));

        let id = wait_for_pending(&hub);
        assert!(confirmer.resolve(id, Confirmation::Deny("nope".into())));
        assert_eq!(worker.join().unwrap(), Confirmation::Deny("nope".into()));
        let (events, _, _) = hub.since(0);
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::ConfirmResolved { allowed: false, .. })));
    }

    #[test]
    fn deny_all_unblocks_a_waiting_worker() {
        let hub = Hub::new();
        let confirmer = RemoteConfirmer::new(hub.clone());
        let wc = confirmer.clone();
        let worker = thread::spawn(move || wc.confirm_command("anything", "static reason"));

        wait_for_pending(&hub);
        confirmer.deny_all("run cancelled");
        assert_eq!(
            worker.join().unwrap(),
            Confirmation::Deny("run cancelled".into())
        );
    }

    #[test]
    fn resolve_unknown_id_is_false() {
        let confirmer = RemoteConfirmer::new(Hub::new());
        assert!(!confirmer.resolve(999, Confirmation::AllowOnce));
    }

    fn wait_for_pending(hub: &Hub) -> u64 {
        for _ in 0..200 {
            let (events, _, _) = hub.since(0);
            if let Some(AgentEvent::ConfirmPending { id, .. }) = events
                .iter()
                .find(|e| matches!(e, AgentEvent::ConfirmPending { .. }))
            {
                return *id;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("no ConfirmPending appeared on the hub");
    }
}
