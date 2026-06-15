//! Interactive confirmation for confirm-gated shell commands (spec 04 / spec 06).
//!
//! The [`PermissionPolicy`](dc_tools::PermissionPolicy) decides *statically*:
//! allowlist, `--yolo`, frozen paths. When that static gate denies a `run_command`
//! merely because the command is *unapproved* (not forbidden), the loop may ask a
//! human via a [`Confirmer`] instead of auto-denying — the seam behind the GUI's
//! approve/deny buttons and the CLI's interactive prompt.
//!
//! Like [`Gate`](../../dc_workflow/gate/trait.Gate.html) in `dc-workflow`, the
//! confirmer is **harness-owned**: it lives outside the model, so a model can never
//! self-approve a shell command. It is threaded like an
//! [`EventSink`](crate::event::EventSink), and is `Send + Sync` because the agent
//! runs on a worker thread and a GUI impl blocks on a channel until a button click.
//!
//! **No confirmer wired ⇒ today's behavior exactly:** the static `Deny` stands and
//! the command is auto-denied. [`AutoDeny`] is that headless default made explicit.

/// A human's answer to "may this shell command run?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirmation {
    /// Run it this once; do not change the policy.
    AllowOnce,
    /// Do not run it. `reason` becomes the model's observation.
    Deny(String),
    /// Run it, and remember the approval for the rest of *this* run by adding
    /// `prefix` to the effective shell allowlist, so a later command that starts
    /// with `prefix` runs without prompting again.
    AllowRemember { prefix: String },
}

/// Asks a human whether an otherwise-unapproved shell command may run.
///
/// Mirrors `dc_workflow::Gate` — blocking, harness-owned, model cannot bypass — and
/// is threaded like [`EventSink`](crate::event::EventSink). `Send + Sync` so it can
/// be shared with the worker thread the agent runs on; a GUI impl blocks on a reply
/// channel inside [`confirm_command`](Confirmer::confirm_command).
pub trait Confirmer: Send + Sync {
    /// `command` is the full shell string the model proposed; `default_reason` is
    /// the `Deny` string the static policy produced (shown to the human, and the
    /// fallback `Deny` reason). Blocks until the human answers.
    fn confirm_command(&self, command: &str, default_reason: &str) -> Confirmation;
}

/// The headless default: never prompt — keep the static `Deny`. An absent confirmer
/// is equivalent to this, so the loop's behavior is identical with `None` or
/// `Some(AutoDeny)`.
#[derive(Debug, Default, Clone, Copy)]
pub struct AutoDeny;

impl Confirmer for AutoDeny {
    fn confirm_command(&self, _command: &str, default_reason: &str) -> Confirmation {
        Confirmation::Deny(default_reason.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn auto_deny_preserves_default() {
        // The headless contract: AutoDeny echoes the static reason and never allows.
        assert_eq!(
            AutoDeny.confirm_command("rm -rf /", "shell is blocked"),
            Confirmation::Deny("shell is blocked".to_string())
        );
    }

    #[test]
    fn confirmer_handle_is_send_sync() {
        // The worker thread moves the AgentConfig (hence the Arc) across threads, so
        // the trait object must be Send + Sync. This is a compile-time guard.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Arc<dyn Confirmer>>();
    }
}
