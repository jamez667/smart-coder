//! Who is allowed to do what.
//!
//! Two parties, authenticating differently because they *are* different things
//! (spec 18): a browser is a person, and a daemon is a long-lived machine
//! credential.
//!
//! ## Credentials are hashed at rest
//!
//! Nothing here stores a credential in the clear. The server keeps a hash and
//! compares hashes, so the data volume — the thing a Portainer user backs up and
//! copies around — never contains anything that grants access. `sc-web`'s existing posture writes tokens to `remote-sessions.jsonl`
//! in the clear; spec 18 says explicitly that this path must not inherit it.
//!
//! Hashing also fixes a subtler thing. `sc-web`'s `ct_eq` returns early when
//! lengths differ, leaking the credential's *length* through timing. Comparing
//! fixed-width hashes removes the early return entirely — every comparison does
//! the same work regardless of what was submitted.
//!
//! ## Two hashes, and reaching for the wrong one is the mistake
//!
//! [`hash`] is SHA-256, unsalted and fast. That is **correct** for everything it
//! protects — session tokens, claim codes, daemon keys — because those are
//! [`mint_secret`] output: 256 bits of CSPRNG, with nothing to brute-force. A
//! fast hash costs an attacker the same as a slow one when there is no guessing
//! to be done.
//!
//! [`hash_password`] is argon2id, salted and deliberately slow. A password is
//! chosen by a person, so speed *is* the attack: an unsalted fast hash of one is
//! a password list waiting for somebody to copy the volume.
//!
//! Both are here rather than one, because the same word covers two different
//! problems and the wrong choice fails silently — the volume would still hold
//! "a hash", and the test asserting nothing is stored in the clear would still
//! pass, while the property it names had quietly weakened.

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

/// The shortest password this server will store.
///
/// Refused at entry rather than accepted and regretted: a short password looks
/// configured while being guessable, which is the same rule the daemon key and
/// the sealing key already apply. Twelve rather than eight because there is no
/// second factor behind it — this credential is the whole way in.
pub const MIN_PASSWORD: usize = 12;

/// Hash a password for storage.
///
/// **Not [`hash`] above, and the distinction is the point.** SHA-256 is fast by
/// design, which is correct for everything else this module protects: those are
/// [`mint_secret`] outputs, 256 bits of CSPRNG, with nothing to brute-force. A
/// password is chosen by a person, so speed is the attacker's friend — an
/// unsalted fast hash of one is a password list waiting for somebody to copy the
/// volume.
///
/// argon2id at the crate's defaults, which salt per password and are
/// deliberately slow. The returned PHC string carries the salt and parameters,
/// so a later change of parameters still verifies old passwords.
pub fn hash_password(plain: &str) -> std::result::Result<String, String> {
    if plain.len() < MIN_PASSWORD {
        return Err(format!(
            "a password needs at least {MIN_PASSWORD} characters — this is the \
             only way in, and there is no second factor behind it"
        ));
    }
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    let salt = SaltString::generate(&mut OsRng);
    argon2::Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("could not hash the password: {e}"))
}

/// Does `presented` match a stored [`hash_password`] result?
///
/// Constant-time comparison is the crate's, not ours — it reads the salt and
/// parameters out of the stored string and compares the derived key itself.
///
/// A malformed stored value is `false` rather than an error. The caller cannot
/// act differently on "wrong password" and "unreadable record", and offering two
/// answers would tell somebody which they had hit.
pub fn password_matches(presented: &str, stored: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    let Ok(parsed) = PasswordHash::new(stored) else {
        return false;
    };
    argon2::Argon2::default()
        .verify_password(presented.as_bytes(), &parsed)
        .is_ok()
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
    /// Carries the operator's label for that machine, matching `Account` rather
    /// than being a bare unit variant. Without it the server
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

    #[test]
    fn a_password_round_trips_and_a_wrong_one_does_not() {
        let stored = hash_password("correct-horse-battery").unwrap();
        assert!(password_matches("correct-horse-battery", &stored));
        assert!(!password_matches("correct-horse-batterz", &stored));
        assert!(!password_matches("", &stored));
    }

    #[test]
    fn a_stored_password_is_not_the_password() {
        // The rule this module rests on, applied to the one credential a person
        // chooses rather than the server mints.
        let plain = "correct-horse-battery";
        let stored = hash_password(plain).unwrap();
        assert!(!stored.contains(plain), "{stored}");
        assert!(stored.starts_with("$argon2id$"), "not argon2id: {stored}");
    }

    #[test]
    fn the_same_password_hashes_differently_every_time() {
        // **Salted.** Equal hashes for equal passwords would mean a stolen file
        // shows which accounts share one, and lets a single cracked hash open
        // all of them.
        let a = hash_password("correct-horse-battery").unwrap();
        let b = hash_password("correct-horse-battery").unwrap();
        assert_ne!(a, b);
        // And both still verify.
        assert!(password_matches("correct-horse-battery", &a));
        assert!(password_matches("correct-horse-battery", &b));
    }

    #[test]
    fn a_short_password_is_refused_at_entry() {
        // Refused rather than accepted and regretted: this credential is the
        // whole way in, with no second factor behind it.
        let err = hash_password("short").unwrap_err();
        assert!(err.contains(&MIN_PASSWORD.to_string()), "{err}");
        assert!(hash_password(&"x".repeat(MIN_PASSWORD - 1)).is_err());
        assert!(hash_password(&"x".repeat(MIN_PASSWORD)).is_ok());
    }

    #[test]
    fn a_malformed_stored_value_is_a_refusal_and_not_a_panic() {
        // Everything here is read from a file somebody may have edited, and one
        // answer for every failure: the caller cannot act differently on "wrong
        // password" and "unreadable record".
        for bad in [
            "",
            "nonsense",
            "$argon2id$broken",
            &hash("not-a-phc-string"),
        ] {
            assert!(!password_matches("anything", bad), "{bad:?}");
        }
    }

    #[test]
    fn the_two_hashes_are_not_interchangeable() {
        // The mistake this module is shaped to prevent. A password stored with
        // `hash` would leave the volume holding "a hash" and every existing
        // test passing, while the property they name had quietly weakened.
        let plain = "correct-horse-battery";
        assert!(
            !password_matches(plain, &hash(plain)),
            "a fast hash verified as a password hash"
        );
        assert!(
            !matches(plain, &hash_password(plain).unwrap()),
            "a password hash verified as a fast hash"
        );
    }
}
