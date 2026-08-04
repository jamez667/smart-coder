//! Who owns this server, decided once and kept on the volume.
//!
//! ## Why a claim rather than a setting
//!
//! The obvious way to name an administrator is an environment variable, and it
//! is a trap. A typo'd login starts cleanly and nobody can *ever* administer the
//! server — and because the answer lives in the environment, there is nothing on
//! the volume to repair. The only fix is a redeploy, which means the recovery
//! path for "I mistyped my own username" is the same as the one for a lost
//! machine.
//!
//! So the server is **claimed**. A fresh volume arms a one-time code, the code
//! is spent at `/setup`, and the username chosen on the step after is written
//! here. Getting it wrong is a file to delete rather than a stack to edit, and
//! the thing that proves ownership is *reading the container's logs* — which is
//! the same proof a stack editor was standing in for, and better evidence than
//! holding a cookie.
//!
//! ## What the claim rests on, stated plainly
//!
//! This used to be two proofs: the code proved you could read the logs, and a
//! GitHub sign-in proved who you were. The password chosen at `/setup` is chosen
//! by the same person who typed the code, so the second proof has collapsed into
//! the first. **Whoever can read the container's log owns the server.**
//!
//! That is a real reduction and worth naming rather than glossing. It is
//! defensible because reading the log already meant being able to deploy the
//! thing — the two capabilities were never far apart — and because the
//! alternative cost two days of a sign-in that never completed: a callback
//! pointing at a hostname with no DNS, an OAuth application per deployment, and
//! a third party who could take the only way in away.
//!
//! What follows from it is that [`Admin::spend`] and the setup token matter
//! *more* than before, not less. The wizard is still multi-step, and the step in
//! between now decides a password rather than an application — the same threat
//! with a worse consequence.
//!
//! ## Why not first-login-wins
//!
//! This surface is on a public hostname. Without a code, whoever reached
//! `/setup` first would own the server, and losing that race once is permanent.
//! The code is what makes the race unwinnable by a stranger.
//!
//! ## What this deliberately is not
//!
//! Not a list. Two administrators is two people who can hand the server to each
//! other, and revoking one would not revoke what the other granted. One login,
//! transferable by the holder, recoverable by deleting a file.

use serde::{Deserialize, Serialize};

use crate::auth::{hash, matches, mint_code};

/// How long a minted claim code stays usable.
///
/// The same thirty minutes an enrolment code had, for the same reason: the code
/// is logged in the clear because it has to be, and the container log goes
/// wherever the host ships logs. Its value is bounded by *time* rather than by
/// the log's audience. A restart re-arms one while the server is unclaimed, so
/// letting it lapse costs nothing.
pub const CLAIM_TTL_MS: u64 = 30 * 60 * 1000;

/// Who administers this server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Admin {
    /// The administrator's username, lowercased. `None` means unclaimed.
    ///
    /// A login rather than an account id, matching [`crate::roster::OwnerRecord`]:
    /// somebody types a name they recognise. It has to keep matching the
    /// [`crate::account::Account`] that carries the password — which is why
    /// [`crate::account::Accounts::create_login`] refuses a duplicate, since two
    /// accounts answering to one name would make [`Admin::is`] match whichever
    /// the iteration reached first. If it ever stops matching, the result is a
    /// person who cannot sign in and says so — the safe direction — and the
    /// recovery is a file on the volume rather than a support ticket.
    #[serde(default)]
    pub login: Option<String>,
    #[serde(default)]
    pub claimed_ms: u64,
    /// SHA-256 of the outstanding claim code. Hashed, like every other
    /// credential here — the volume never holds one that grants anything.
    #[serde(default)]
    pub claim_code_hash: Option<String>,
    /// Unix ms after which the outstanding code is refused.
    ///
    /// `Option` so a file written before this field existed still loads, reading
    /// as `None`, which [`Admin::claim_expired`] treats as **expired**. Failing
    /// closed is right: a code armed by an older build is exactly the standing
    /// credential the expiry exists to end.
    #[serde(default)]
    pub claim_expires_ms: Option<u64>,
    /// SHA-256 of the token held by the browser that spent the code.
    ///
    /// **Setup is more than one step, and the code is spent at the first.**
    /// Without this, everything after step one is guarded only by the server
    /// being unclaimed — so a half-finished setup on a public hostname is open
    /// to whoever arrives next, and they can set their own password and take the
    /// server. **Worse than before**: the step this guards used to name a GitHub
    /// application, which at least required the interloper to own one. It now
    /// chooses a credential outright.
    ///
    /// That is not hypothetical on a *migrated* volume: seeding fills in the
    /// address, which made step one look already done to everybody.
    ///
    /// Hashed like every other credential here, and cleared by the claim.
    #[serde(default)]
    pub setup_token_hash: Option<String>,
    /// Unix ms after which a half-finished setup has to start again.
    ///
    /// Shares [`CLAIM_TTL_MS`] with the code, because it is the same window
    /// seen from the other side: an abandoned setup must not leave the rest of
    /// the wizard standing open indefinitely.
    #[serde(default)]
    pub setup_expires_ms: Option<u64>,
}

impl Admin {
    /// Has somebody claimed this server?
    pub fn claimed(&self) -> bool {
        self.login.is_some()
    }

    /// Is this login the administrator?
    ///
    /// Lowercased on the way in, so a caller who typed their name with different
    /// capitalisation than they registered it still matches.
    pub fn is(&self, login: &str) -> bool {
        match &self.login {
            Some(mine) => mine == &login.to_ascii_lowercase(),
            None => false,
        }
    }

    /// Arm a fresh claim code, replacing any outstanding one.
    ///
    /// **Refuses once claimed.** Re-arming on a claimed server would leave a
    /// standing way to take it over that the administrator never asked for —
    /// every restart would print a fresh key to their own front door.
    pub fn arm(&mut self, code: &str, now_ms: u64) -> bool {
        if self.claimed() {
            return false;
        }
        self.claim_code_hash = Some(hash(code));
        self.claim_expires_ms = Some(now_ms.saturating_add(CLAIM_TTL_MS));
        true
    }

    /// Is there no code that would still be accepted at `now_ms`?
    pub fn claim_expired(&self, now_ms: u64) -> bool {
        match (&self.claim_code_hash, self.claim_expires_ms) {
            (None, _) => true,
            (Some(_), None) => true,
            (Some(_), Some(expires)) => now_ms >= expires,
        }
    }

    /// Spend the claim code, returning the token that carries the rest of the
    /// wizard.
    ///
    /// `None` when the code was wrong, expired, or already spent.
    ///
    /// **Spending is still separate from claiming, but no longer for the reason
    /// it was.** It used to be two proofs — the code for the logs, a GitHub
    /// sign-in for who you are. The username and password are now chosen by the
    /// same person who typed the code, so the code alone does decide who owns the
    /// server. What the split still buys is the [`setup_token_hash`] binding:
    /// spending returns a token, and every step after this one demands it, so the
    /// wizard cannot be finished by a second browser that merely arrived while it
    /// was half-done.
    ///
    /// [`setup_token_hash`]: Admin::setup_token_hash
    ///
    /// An expired code is refused *before* it is compared and cleared on the way
    /// out, so a code that outlived its window stops being a credential rather
    /// than merely being unlucky.
    pub fn spend(&mut self, code: &str, now_ms: u64) -> Option<String> {
        if self.claimed() || self.claim_expired(now_ms) {
            self.claim_code_hash = None;
            self.claim_expires_ms = None;
            return None;
        }
        let expected = self.claim_code_hash.as_ref()?;
        if !matches(code, expected) {
            // Left armed: a wrong guess must not spend somebody else's code, or
            // a stranger who cannot read the log could still deny the claim to
            // the person who can.
            return None;
        }
        self.claim_code_hash = None;
        self.claim_expires_ms = None;

        // **The rest of the wizard belongs to this browser.** Minted here
        // rather than at the claim, because the step in between — choosing the
        // username and password — is exactly what somebody else arriving mid-way
        // would supply in order to take the server.
        let token = crate::auth::mint_secret();
        self.setup_token_hash = Some(hash(&token));
        self.setup_expires_ms = Some(now_ms.saturating_add(CLAIM_TTL_MS));
        Some(token)
    }

    /// Is this the browser that spent the code?
    ///
    /// **Not merely "is the server unclaimed".** Setup has more than one step
    /// and the code is spent at the first, so without this every later step is
    /// open to whoever arrives — and on a migrated volume, where seeding fills
    /// in the address, step one looks already done to everybody.
    pub fn setting_up(&self, token: Option<&str>, now_ms: u64) -> bool {
        let Some(expected) = self.setup_token_hash.as_ref() else {
            return false;
        };
        match self.setup_expires_ms {
            // No expiry recorded is a file written by an older build. Treated
            // as expired, so an upgrade asks somebody to start again rather
            // than honouring a session nobody can account for.
            None => false,
            Some(expires) if now_ms >= expires => false,
            Some(_) => token.is_some_and(|t| matches(t, expected)),
        }
    }

    /// Record who owns this server.
    ///
    /// Used by the end of setup and by a later transfer alike — the write is the
    /// same act, and what differs is who is allowed to reach it.
    pub fn claim(&mut self, login: &str, now_ms: u64) {
        self.login = Some(login.trim().to_ascii_lowercase());
        self.claimed_ms = now_ms;
        self.claim_code_hash = None;
        self.claim_expires_ms = None;
        // The wizard is over, so its token stops meaning anything.
        self.setup_token_hash = None;
        self.setup_expires_ms = None;
    }
}

/// A code for a fresh server to print.
///
/// The same shape as an enrolment code and for the same reasons — read off a
/// terminal and typed into a browser, so it trades length for typeability, which
/// is safe only because it is single-use and short-lived.
pub fn mint_claim_code() -> String {
    mint_code()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_server_is_unclaimed_and_nobody_is_the_administrator() {
        let admin = Admin::default();
        assert!(!admin.claimed());
        assert!(!admin.is("jamez667"));
        assert!(!admin.is(""));
    }

    #[test]
    fn the_administrator_is_matched_whatever_case_they_type() {
        // Registered one way, typed another. A sign-in form is where this
        // happens, and a mismatch would lock out the one account that matters.
        let mut admin = Admin::default();
        admin.claim("JameZ667", 1);
        assert!(admin.is("jamez667"));
        assert!(admin.is("JAMEZ667"));
        assert!(!admin.is("someone-else"));
    }

    #[test]
    fn a_claim_code_is_single_use() {
        // An intercepted code must not be replayable. Spending it here means a
        // second attempt fails, and the person who claimed it sees the server is
        // already theirs rather than silently sharing it.
        let mut admin = Admin::default();
        assert!(admin.arm("ABC-123", 0));
        assert!(admin.spend("ABC-123", 1).is_some());
        assert!(admin.spend("ABC-123", 2).is_none(), "spent twice");
    }

    #[test]
    fn a_wrong_guess_does_not_spend_the_code() {
        // **Otherwise a stranger who cannot read the log could still deny the
        // claim** to the person who can, by burning the code with any guess.
        let mut admin = Admin::default();
        admin.arm("ABC-123", 0);
        assert!(admin.spend("WRONG-1", 1).is_none());
        assert!(
            admin.spend("ABC-123", 2).is_some(),
            "the real code still works"
        );
    }

    #[test]
    fn a_claim_code_expires() {
        let mut admin = Admin::default();
        admin.arm("ABC-123", 0);
        assert!(admin.spend("ABC-123", CLAIM_TTL_MS).is_none());
        assert!(
            admin.claim_code_hash.is_none(),
            "an expired code is cleared, not merely refused"
        );
    }

    #[test]
    fn a_code_armed_without_an_expiry_is_treated_as_expired() {
        // What a file written by an older build reads as. Failing closed is
        // right: a code with no window is exactly the standing credential the
        // expiry exists to end.
        let admin = Admin {
            claim_code_hash: Some(hash("ABC-123")),
            claim_expires_ms: None,
            ..Admin::default()
        };
        assert!(admin.claim_expired(1));
    }

    #[test]
    fn a_claimed_server_arms_nothing_and_spends_nothing() {
        // **The property that keeps a claim permanent.** Re-arming on every
        // restart would print a fresh key to the administrator's own front door,
        // and anyone reading the logs could take the server from them.
        let mut admin = Admin::default();
        admin.claim("jamez667", 1);

        assert!(!admin.arm("NEW-COD", 2), "armed a claimed server");
        assert!(admin.claim_code_hash.is_none());
        assert!(admin.spend("NEW-COD", 3).is_none());
        assert!(admin.is("jamez667"), "still theirs");
    }

    #[test]
    fn claiming_clears_any_outstanding_code() {
        // The code has done its job. Leaving it armed would be a second way in
        // that nobody is watching.
        let mut admin = Admin::default();
        admin.arm("ABC-123", 0);
        admin.claim("jamez667", 1);
        assert!(admin.claim_code_hash.is_none());
        assert!(admin.claim_expires_ms.is_none());
        assert_eq!(admin.claimed_ms, 1);
    }

    #[test]
    fn a_transfer_replaces_rather_than_adds() {
        // One login, always. Two administrators could hand the server to each
        // other, and revoking one would not revoke what the other granted.
        let mut admin = Admin::default();
        admin.claim("jamez667", 1);
        admin.claim("somebody-else", 2);
        assert!(admin.is("somebody-else"));
        assert!(!admin.is("jamez667"));
        assert_eq!(admin.claimed_ms, 2);
    }

    #[test]
    fn nothing_that_grants_access_is_serialized() {
        // The same rule every store here keeps: the file holds hashes, so a
        // copy of the volume lets nobody claim the server or finish somebody
        // else's setup.
        //
        // **Asserted on the values, not on field names.** Scanning the JSON for
        // the word "token" was a proxy that broke the moment a field was called
        // `setup_token_hash` — and a proxy that fails on a correct change is
        // one that will eventually be silenced rather than read.
        let mut admin = Admin::default();
        admin.arm("ABC-123", 0);
        let setup = {
            let mut a = admin.clone();
            a.spend("ABC-123", 1).expect("spent")
        };

        let json = serde_json::to_string(&admin).unwrap();
        assert!(!json.contains("ABC-123"), "the claim code is in {json}");

        let mut spent = Admin::default();
        spent.arm("ABC-123", 0);
        let token = spent.spend("ABC-123", 1).expect("spent");
        let json = serde_json::to_string(&spent).unwrap();
        assert!(!json.contains(&token), "the setup token is in {json}");
        assert!(json.contains(&hash(&token)), "but its hash is not: {json}");
        let _ = setup;

        // And the claim clears both, so a claimed server carries neither.
        spent.claim("jamez667", 2);
        let json = serde_json::to_string(&spent).unwrap();
        assert!(!json.contains("ABC-123"), "{json}");
        assert!(!json.contains(&token), "{json}");
    }

    #[test]
    fn a_file_written_before_these_fields_existed_still_loads() {
        // The data volume outlives any one image tag.
        let old = r#"{"login":"jamez667","claimed_ms":7}"#;
        let admin: Admin = serde_json::from_str(old).unwrap();
        assert!(admin.is("jamez667"));
        assert!(admin.claim_expired(0), "and no code is outstanding");
    }

    #[test]
    fn an_empty_file_is_an_unclaimed_server_and_not_an_error() {
        let admin: Admin = serde_json::from_str("{}").unwrap();
        assert!(!admin.claimed());
    }
}
