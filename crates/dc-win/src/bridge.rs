//! The worker↔UI bridge: channel-backed implementations of the two harness-owned
//! decision seams, plus the message protocol the UI drains.
//!
//! Both seams follow the same shape: the worker thread (where the agent/swarm runs)
//! calls a blocking `decide`/`confirm`, which **sends a request to the UI and blocks
//! on a reply channel** until a button click answers it. The UI thread never blocks —
//! it only receives requests and sends replies. This file has no iced types, so the
//! whole protocol is host-testable: a test plays the UI by draining requests and
//! sending replies on a background thread (see the tests below).
//!
//! - [`ChannelConfirmer`] implements [`dc_core::Confirmer`] — the new Part-A seam for
//!   confirm-gated `run_command`.
//! - [`ChannelGate`] implements [`dc_workflow::Gate`] — the existing spec-09 workflow
//!   checkpoints (approve / revise / send-back / abort).

use std::sync::mpsc::{Receiver, Sender};

use dc_core::{Confirmation, Confirmer};
use dc_workflow::{Artifact, Decision, Gate, Phase};

/// A request the worker raises for the UI to answer. Each variant carries a
/// one-shot reply [`Sender`] the UI uses to unblock the worker.
pub enum Pending {
    /// A confirm-gated shell command awaits approval.
    Confirm {
        command: String,
        default_reason: String,
        reply: Sender<Confirmation>,
    },
    /// A workflow phase artifact awaits a checkpoint decision.
    Gate {
        phase: Phase,
        /// The artifact's full text (already persisted to disk by the runner; copied
        /// here so the UI can show it without a file read).
        content: String,
        reply: Sender<Decision>,
    },
}

/// A [`Confirmer`] that routes each request to the UI over `tx` and blocks on a
/// one-shot reply. If the UI side is gone (the reply channel drops), it falls back to
/// denying with the static reason — the run continues, it just doesn't get approval.
pub struct ChannelConfirmer {
    tx: Sender<Pending>,
}

impl ChannelConfirmer {
    pub fn new(tx: Sender<Pending>) -> Self {
        Self { tx }
    }
}

impl Confirmer for ChannelConfirmer {
    fn confirm_command(&self, command: &str, default_reason: &str) -> Confirmation {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let req = Pending::Confirm {
            command: command.to_string(),
            default_reason: default_reason.to_string(),
            reply: reply_tx,
        };
        // If the UI has gone away, deny (preserve the static decision) rather than
        // hanging the worker forever.
        if self.tx.send(req).is_err() {
            return Confirmation::Deny(default_reason.to_string());
        }
        reply_rx
            .recv()
            .unwrap_or_else(|_| Confirmation::Deny(default_reason.to_string()))
    }
}

/// A [`Gate`] that routes each phase checkpoint to the UI over `tx` and blocks on a
/// one-shot reply. If the UI side is gone, it aborts the workflow (keeping
/// approved-so-far artifacts, per spec 09) rather than hanging.
pub struct ChannelGate {
    tx: Sender<Pending>,
}

impl ChannelGate {
    pub fn new(tx: Sender<Pending>) -> Self {
        Self { tx }
    }
}

impl Gate for ChannelGate {
    fn decide(&self, phase: Phase, artifact: &Artifact) -> Decision {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let req = Pending::Gate {
            phase,
            content: artifact.content.clone(),
            reply: reply_tx,
        };
        if self.tx.send(req).is_err() {
            return Decision::Abort;
        }
        reply_rx.recv().unwrap_or(Decision::Abort)
    }
}

/// Convenience: a freshly-paired request channel. The worker keeps `tx` (cloned into
/// the confirmer/gate); the UI drains `rx`.
pub fn pending_channel() -> (Sender<Pending>, Receiver<Pending>) {
    std::sync::mpsc::channel()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::Receiver;
    use std::thread;

    /// Play the UI: on a background thread, drain one `Pending` and answer it. Returns
    /// the join handle so the test can assert what the UI saw.
    fn auto_answer(
        rx: Receiver<Pending>,
        confirm_with: Confirmation,
        gate_with: Decision,
    ) -> thread::JoinHandle<Option<String>> {
        thread::spawn(move || match rx.recv() {
            Ok(Pending::Confirm { command, reply, .. }) => {
                reply.send(confirm_with).ok();
                Some(command)
            }
            Ok(Pending::Gate { content, reply, .. }) => {
                reply.send(gate_with).ok();
                Some(content)
            }
            Err(_) => None,
        })
    }

    #[test]
    fn confirmer_blocks_until_ui_replies() {
        let (tx, rx) = pending_channel();
        let ui = auto_answer(rx, Confirmation::AllowOnce, Decision::Approve);

        // The worker thread blocks inside confirm_command until the UI answers.
        let confirmer = ChannelConfirmer::new(tx);
        let got = confirmer.confirm_command("rm -rf build", "shell blocked");

        assert_eq!(got, Confirmation::AllowOnce);
        assert_eq!(ui.join().unwrap().as_deref(), Some("rm -rf build"));
    }

    #[test]
    fn confirmer_remember_round_trips() {
        let (tx, rx) = pending_channel();
        let ui = auto_answer(
            rx,
            Confirmation::AllowRemember {
                prefix: "git ".to_string(),
            },
            Decision::Approve,
        );
        let got = ChannelConfirmer::new(tx).confirm_command("git push", "shell blocked");
        assert_eq!(
            got,
            Confirmation::AllowRemember {
                prefix: "git ".to_string()
            }
        );
        ui.join().unwrap();
    }

    #[test]
    fn confirmer_denies_when_ui_is_gone() {
        let (tx, rx) = pending_channel();
        drop(rx); // UI never starts.
        let got = ChannelConfirmer::new(tx).confirm_command("anything", "static reason");
        assert_eq!(got, Confirmation::Deny("static reason".to_string()));
    }

    #[test]
    fn gate_blocks_until_ui_replies() {
        let (tx, rx) = pending_channel();
        let ui = auto_answer(
            rx,
            Confirmation::AllowOnce,
            Decision::SendBack {
                target: Phase::Specs,
                notes: Some("tighten the scope".to_string()),
            },
        );

        let gate = ChannelGate::new(tx);
        let artifact = Artifact::draft(Phase::Architecture, "## Architecture\n...");
        let decision = gate.decide(Phase::Architecture, &artifact);

        assert_eq!(
            decision,
            Decision::SendBack {
                target: Phase::Specs,
                notes: Some("tighten the scope".to_string())
            }
        );
        // The UI saw the artifact content.
        assert_eq!(ui.join().unwrap().as_deref(), Some("## Architecture\n..."));
    }

    #[test]
    fn gate_aborts_when_ui_is_gone() {
        let (tx, rx) = pending_channel();
        drop(rx);
        let gate = ChannelGate::new(tx);
        let artifact = Artifact::draft(Phase::Specs, "draft");
        assert_eq!(gate.decide(Phase::Specs, &artifact), Decision::Abort);
    }
}
