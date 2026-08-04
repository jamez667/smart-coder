//! Who is allowed to do what.
//!
//! Two parties, authenticating differently because they *are* different things
//! (spec 18): a browser is a person who might lose their phone, and a daemon is a
//! long-lived machine credential.
//!
//! ## Credentials are hashed at rest
//!
//! Nothing here stores a credential in the clear. The server keeps SHA-256 of
//! each device token and compares hashes, so the data volume — the thing a
//! Portainer user backs up and copies around — never contains anything that grants
//! access. `sc-web`'s existing posture writes tokens to `remote-sessions.jsonl`
//! in the clear; spec 18 says explicitly that this path must not inherit it.
//!
//! Hashing also fixes a subtler thing. `sc-web`'s `ct_eq` returns early when
//! lengths differ, leaking the credential's *length* through timing. Comparing
//! fixed-width hashes removes the early return entirely — every comparison does
//! the same work regardless of what was submitted.
//!
//! ## Per-device, not one shared secret
//!
//! With a single token, rotation is indistinguishable from re-enrolment: the
//! developer who rotates *because* they are away from their desk locks themselves
//! out and needs physical access to recover. One user, several devices, each
//! revocable alone.

use sha2::{Digest, Sha256};

/// A 256-bit secret, hex-encoded.
///
/// Generated from the OS CSPRNG. On the vanishingly unlikely failure of
/// `getrandom`, this panics rather than falling back to a weaker seed: a
/// predictable credential on a public server is worse than a server that will
/// not start, and `sc-web`'s time+pid fallback is not defensible here.
pub fn mint_secret() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .expect("the OS random source is unavailable; refusing to mint a guessable credential");
    hex(&bytes)
}

/// A short, human-typeable one-time code.
///
/// Read off a terminal and typed into a browser, so it trades length for
/// typeability — safe only because every use of it is **single-use and
/// short-lived**. It is never a standing credential; see
/// [`crate::admin::CLAIM_TTL_MS`] for the window it lives in.
pub fn mint_code() -> String {
    let mut bytes = [0u8; 6];
    getrandom::fill(&mut bytes).expect("the OS random source is unavailable");
    // Crockford-ish alphabet: no I, L, O, U — the characters people mistype.
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let code: String = bytes
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect();
    format!("{}-{}", &code[..3], &code[3..])
}

/// SHA-256, hex-encoded — what is stored instead of a credential.
pub fn hash(secret: &str) -> String {
    let mut h = Sha256::new();
    h.update(secret.as_bytes());
    hex(&h.finalize())
}

/// Does `presented` match the stored `hash`?
///
/// Constant-time over fixed-width hashes, so neither the value nor its length
/// leaks through timing.
pub fn matches(presented: &str, stored_hash: &str) -> bool {
    ct_eq(hash(presented).as_bytes(), stored_hash.as_bytes())
}

/// Constant-time byte compare. No early return on length: the inputs here are
/// always two hex hashes of equal width.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Who a request turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// The developer's daemon, holding one of the API keys.
    ///
    /// Carries the operator's label for that machine, matching `Device` and
    /// `Account` rather than being a bare unit variant. Without it the server
    /// cannot tell two daemons apart, and three things that ought to be
    /// per-machine collapse onto one credential: the rate budget, the holder of
    /// a claim, and revocation.
    Daemon { label: String },
    /// The one account this server was claimed by. **The only caller that may
    /// review.**
    ///
    /// A variant of its own rather than a flag on [`Caller::Owner`], even though
    /// both now arrive as a GitHub session. The safety argument below is that no
    /// value of `Owner` satisfies the gate's pattern — a boolean would turn that
    /// back into a check somebody has to remember, and would silently widen
    /// every existing `Some(Caller::Owner { .. })` match to include the
    /// administrator.
    ///
    /// Carries the login rather than a session id, so the rate budget is per
    /// person: an administrator on a phone and a laptop is one human, and the
    /// per-device budget died with per-device credentials.
    Admin { login: String },
    /// A signed-in member of the public.
    ///
    /// May file and read their own requests, and nothing else. Kept a separate
    /// variant rather than a flag on `Device` so the review routes can be
    /// unreachable by *type* rather than by remembering to check a boolean.
    Account { id: String },
    /// Somebody the configuration names as an owner of particular repositories.
    ///
    /// **A third variant, not a flag and not an `Admin`.** An owner may decide
    /// *against* work — send it back, discard it — and may not decide *for* it.
    /// The verbs that admit work live behind a `Caller::Admin` match, so an
    /// owner cannot reach them: not because a check refuses, but because there
    /// is no value of this variant that satisfies that pattern.
    ///
    /// The asymmetry is not that accepting touches the repository — it does
    /// not; it flips a state and writes one file, and the web never builds
    /// anything. It is that **accepting is the signal a spec is settled and fit
    /// to be picked up in the IDE**, and that is a decision the developer has
    /// not delegated. Declining fails towards *lost* work, which the filer can
    /// re-file and the developer can see; accepting fails towards work taken as
    /// agreed by somebody who did not have to build it.
    ///
    /// Carries the repositories the configuration says are theirs, resolved once
    /// when the caller is identified. Passing the *names* rather than the login
    /// means every filtering site asks "is this request's repository in the
    /// caller's set?" instead of re-deriving the answer from the login — and a
    /// site that forgot to re-derive would show an owner everything.
    Owner { login: String, repos: Vec<String> },
}

/// Why a request was refused.
///
/// Deliberately coarse: the response says "unauthorized" and nothing more. A
/// message distinguishing "no credential" from "wrong credential" tells an
/// attacker which half they got right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denied {
    /// No credential, an unknown one, or a revoked device.
    Unauthorized,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparison_is_over_fixed_width_hashes() {
        // `sc-web`'s ct_eq returns early on a length mismatch, leaking the
        // credential's length. Hashing first makes every comparison the same
        // width, so the early return is unreachable for real inputs.
        let secret = mint_secret();
        assert_eq!(hash(&secret).len(), 64, "SHA-256 hex is always 64 chars");
        assert_eq!(hash("").len(), 64, "…whatever the input length");
        assert_eq!(hash(&"x".repeat(10_000)).len(), 64);

        assert!(matches(&secret, &hash(&secret)));
        assert!(!matches("wrong", &hash(&secret)));
    }

    #[test]
    fn minted_secrets_are_unique_and_full_width() {
        let a = mint_secret();
        let b = mint_secret();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64, "256 bits, hex-encoded");
    }

    #[test]
    fn a_one_time_code_avoids_the_characters_people_mistype() {
        // It is read off a terminal and typed into a phone. I/L/O/U are the
        // classic misreads, so the alphabet excludes them.
        for _ in 0..50 {
            let code = mint_code();
            assert_eq!(code.len(), 7, "XXX-XXX: {code}");
            assert!(code.contains('-'));
            for c in code.chars().filter(|c| *c != '-') {
                assert!(
                    !"ILOU".contains(c),
                    "{code} contains a character people mistype"
                );
            }
        }
    }

    #[test]
    fn a_credential_is_never_stored_in_the_clear() {
        // **The rule the module doc rests on**, moved rather than dropped when
        // the device store went: sessions are the credential now, and the
        // volume is still the thing a Portainer user backs up and copies.
        let mut accounts = crate::account::Accounts::default();
        let account = accounts.create(&hash("a@b.test"), "a***@b.test", 1_000);
        let token = accounts.open_session(&account.id, 1_000);

        let json = serde_json::to_string(&accounts).unwrap();
        assert!(!json.contains(&token), "the session token is in {json}");
        assert!(json.contains(&hash(&token)), "but its hash is");
        // And the address itself never appears, only the hint.
        assert!(!json.contains("a@b.test"), "{json}");
    }
}
