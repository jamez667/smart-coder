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

use serde::{Deserialize, Serialize};
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

/// How long a minted enrolment code stays usable.
///
/// The code is logged in the clear — it has to be, since it is the only way into
/// a fresh install — and the container log goes wherever the host ships logs. On
/// a machine running a log aggregator that makes it readable by anyone who can
/// read the aggregator, so its value must be bounded by *time* rather than by
/// the log's audience.
///
/// Thirty minutes: long enough to walk to a phone after a deploy, short enough
/// that a code sitting in a log tomorrow is inert. A restart re-arms one, so
/// letting it lapse costs nothing but a redeploy.
pub const ENROL_TTL_MS: u64 = 30 * 60 * 1000;

/// A short, human-typeable enrolment code.
///
/// Read off a terminal and typed into a phone, so it trades length for
/// typeability — which is safe only because it is **single-use and short-lived**
/// (it enrols one device, then is spent, and it expires after
/// [`ENROL_TTL_MS`] regardless). It is never a standing credential.
pub fn mint_enrol_code() -> String {
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

/// A browser that has enrolled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// A stable id for the device, so it can be named and revoked.
    pub id: String,
    /// What the developer calls it: "phone", "work laptop".
    pub label: String,
    /// SHA-256 of the device's token. **Never the token.**
    pub token_hash: String,
    /// Unix ms when it enrolled.
    pub enrolled_ms: u64,
    /// Revoked devices are kept rather than deleted, so a list can show that a
    /// lost phone *was* revoked. A list that silently shrinks cannot answer
    /// "did I already deal with that?".
    #[serde(default)]
    pub revoked: bool,
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
    /// An enrolled browser. **The only caller that may review.**
    Device { id: String },
    /// A signed-in member of the public.
    ///
    /// May file and read their own requests, and nothing else. Kept a separate
    /// variant rather than a flag on `Device` so the review routes can be
    /// unreachable by *type* rather than by remembering to check a boolean.
    Account { id: String },
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

/// The credential store — hashes only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub devices: Vec<Device>,
    /// SHA-256 of the pending one-time enrolment code, if one is outstanding.
    #[serde(default)]
    pub enrol_code_hash: Option<String>,
    /// Unix ms after which the outstanding code is refused.
    ///
    /// `Option` and `#[serde(default)]` so a `credentials.json` written before
    /// this field existed still loads — it reads as `None`, which
    /// [`Credentials::enrol_expired`] treats as *expired*. A code armed by the
    /// older build is exactly the standing credential this field exists to end,
    /// so failing closed and making the operator restart is the right default.
    #[serde(default)]
    pub enrol_code_expires_ms: Option<u64>,
}

impl Credentials {
    /// Arm enrolment with a fresh one-time code, replacing any outstanding one.
    ///
    /// Expires [`ENROL_TTL_MS`] after `now_ms`.
    pub fn set_enrol_code(&mut self, code: &str, now_ms: u64) {
        self.enrol_code_hash = Some(hash(code));
        self.enrol_code_expires_ms = Some(now_ms.saturating_add(ENROL_TTL_MS));
    }

    /// Is there no code that would still be accepted at `now_ms`?
    ///
    /// True when none is armed, when one is armed without an expiry (written by
    /// a build older than the TTL), or when its window has passed.
    pub fn enrol_expired(&self, now_ms: u64) -> bool {
        match (&self.enrol_code_hash, self.enrol_code_expires_ms) {
            (None, _) => true,
            (Some(_), None) => true,
            (Some(_), Some(expires)) => now_ms >= expires,
        }
    }

    /// Spend the enrolment code and register a device, returning its token.
    ///
    /// The token is returned **once** and never stored — only its hash is kept,
    /// so a leaked data volume grants nothing.
    ///
    /// The code is single-use: consuming it here means an intercepted code
    /// cannot be replayed to enrol a second device, and the developer sees
    /// enrolment fail rather than silently sharing access.
    ///
    /// An expired code is refused **before** it is compared, and cleared on the
    /// way out — so a code that outlived its window stops being a credential
    /// rather than merely being unlucky.
    pub fn enrol(&mut self, code: &str, label: &str, now_ms: u64) -> Option<(Device, String)> {
        if self.enrol_expired(now_ms) {
            self.clear_enrol_code();
            return None;
        }
        let expected = self.enrol_code_hash.as_ref()?;
        if !matches(code, expected) {
            return None;
        }
        self.clear_enrol_code();

        let token = mint_secret();
        let device = Device {
            // Random, not time-derived: two devices enrolled in the same
            // millisecond would share an id, and revoking one would revoke the
            // other — the exact failure per-device credentials exist to prevent.
            id: format!("dev-{}", &mint_secret()[..16]),
            label: if label.trim().is_empty() {
                "a device".to_string()
            } else {
                label.trim().to_string()
            },
            token_hash: hash(&token),
            enrolled_ms: now_ms,
            revoked: false,
        };
        self.devices.push(device.clone());
        Some((device, token))
    }

    /// Forget the outstanding code, expiry and all.
    ///
    /// Both fields together: a hash left without its expiry would read as
    /// expired, but a stale expiry left without its hash is a field that means
    /// nothing and invites a later reader to trust it.
    fn clear_enrol_code(&mut self) {
        self.enrol_code_hash = None;
        self.enrol_code_expires_ms = None;
    }

    /// Which device holds this token, if any live one does.
    pub fn device_for(&self, token: &str) -> Option<&Device> {
        self.devices
            .iter()
            .find(|d| !d.revoked && matches(token, &d.token_hash))
    }

    /// Revoke a device. `true` if one was live and now is not.
    pub fn revoke(&mut self, id: &str) -> bool {
        match self.devices.iter_mut().find(|d| d.id == id && !d.revoked) {
            Some(d) => {
                d.revoked = true;
                true
            }
            None => false,
        }
    }

    /// Devices that can still act.
    pub fn live(&self) -> Vec<&Device> {
        self.devices.iter().filter(|d| !d.revoked).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_is_never_stored_in_the_clear() {
        // The data volume is backed up and copied around; it must contain
        // nothing that grants access (spec 18 — the daemon must not inherit
        // `remote-sessions.jsonl`'s posture).
        let mut creds = Credentials::default();
        creds.set_enrol_code("ABC-123", 0);
        let (device, token) = creds.enrol("ABC-123", "phone", 1).unwrap();

        let serialized = serde_json::to_string(&creds).unwrap();
        assert!(
            !serialized.contains(&token),
            "the device token must not be serialized"
        );
        assert!(
            !serialized.contains("ABC-123"),
            "the enrol code must not be serialized"
        );
        assert_eq!(device.token_hash, hash(&token));
    }

    #[test]
    fn an_enrolment_code_is_single_use() {
        // An intercepted code must not enrol a second device. Consuming it means
        // the developer sees a failure rather than silently sharing access.
        let mut creds = Credentials::default();
        creds.set_enrol_code("ABC-123", 0);

        assert!(creds.enrol("ABC-123", "phone", 1).is_some());
        assert!(
            creds.enrol("ABC-123", "attacker", 2).is_none(),
            "the code was already spent"
        );
        assert_eq!(creds.live().len(), 1);
    }

    #[test]
    fn an_enrolment_code_expires() {
        // The code is logged in the clear, and the log goes wherever the host
        // ships logs. Time is what bounds its value, since its audience cannot
        // be.
        let mut creds = Credentials::default();
        creds.set_enrol_code("ABC-123", 1_000);

        assert!(
            creds
                .enrol("ABC-123", "phone", 1_000 + ENROL_TTL_MS - 1)
                .is_some(),
            "inside the window it still works"
        );

        let mut creds = Credentials::default();
        creds.set_enrol_code("ABC-123", 1_000);
        assert!(
            creds
                .enrol("ABC-123", "attacker", 1_000 + ENROL_TTL_MS)
                .is_none(),
            "the moment it expires, the correct code is worth nothing"
        );
        assert!(creds.devices.is_empty());
    }

    #[test]
    fn an_expired_code_is_cleared_rather_than_left_lying_around() {
        // It stops being a credential, instead of merely being refused — so a
        // clock correction cannot bring a lapsed code back to life.
        let mut creds = Credentials::default();
        creds.set_enrol_code("ABC-123", 0);
        assert!(creds.enrol("ABC-123", "attacker", ENROL_TTL_MS).is_none());

        assert!(creds.enrol_code_hash.is_none());
        assert!(creds.enrol_code_expires_ms.is_none());
        assert!(creds.enrol_expired(0), "even back inside the old window");
    }

    #[test]
    fn a_code_stored_before_expiry_existed_is_treated_as_expired() {
        // The migration case: a `credentials.json` written by an older build has
        // a hash and no expiry. That is precisely the standing credential the
        // TTL exists to end, so it fails closed — the operator restarts and gets
        // a fresh one.
        let older = r#"{"devices":[],"enrol_code_hash":"__replace__"}"#
            .replace("__replace__", &hash("ABC-123"));
        let mut creds: Credentials =
            serde_json::from_str(&older).expect("an older file still loads");

        assert!(creds.enrol_expired(0));
        assert!(creds.enrol("ABC-123", "phone", 0).is_none());
    }

    #[test]
    fn a_wrong_code_enrols_nothing_and_does_not_spend_the_real_one() {
        let mut creds = Credentials::default();
        creds.set_enrol_code("ABC-123", 0);

        assert!(creds.enrol("XYZ-999", "attacker", 1).is_none());
        assert!(creds.devices.is_empty());
        // The developer's own code still works — a wrong guess must not lock
        // them out, or guessing becomes a denial of service.
        assert!(creds.enrol("ABC-123", "phone", 2).is_some());
    }

    #[test]
    fn enrolment_is_impossible_when_no_code_is_armed() {
        // The resting state is closed. A server with no outstanding code enrols
        // nobody, whatever they present.
        let mut creds = Credentials::default();
        assert!(creds.enrol("", "attacker", 1).is_none());
        assert!(creds.enrol("ABC-123", "attacker", 1).is_none());
    }

    #[test]
    fn revoking_one_device_leaves_the_others_working() {
        // The whole reason for per-device credentials: losing a phone must not
        // mean losing the desktop, or the developer away from their desk cannot
        // safely rotate.
        let mut creds = Credentials::default();
        creds.set_enrol_code("A", 0);
        let (phone, phone_token) = creds.enrol("A", "phone", 1).unwrap();
        creds.set_enrol_code("B", 0);
        let (_laptop, laptop_token) = creds.enrol("B", "laptop", 2).unwrap();

        assert!(creds.revoke(&phone.id));
        assert!(
            creds.device_for(&phone_token).is_none(),
            "the lost phone no longer acts"
        );
        assert!(
            creds.device_for(&laptop_token).is_some(),
            "the laptop is unaffected"
        );
        // Revoking twice is not an error, but reports nothing changed.
        assert!(!creds.revoke(&phone.id));
    }

    #[test]
    fn a_revoked_device_is_kept_so_the_list_can_say_it_was_revoked() {
        // A list that silently shrinks cannot answer "did I already deal with
        // that?" — so the developer revokes it again, or worries they never did.
        let mut creds = Credentials::default();
        creds.set_enrol_code("A", 0);
        let (phone, _) = creds.enrol("A", "phone", 1).unwrap();
        creds.revoke(&phone.id);

        assert_eq!(creds.devices.len(), 1, "kept");
        assert!(creds.live().is_empty(), "but not live");
        assert!(creds.devices[0].revoked);
    }

    #[test]
    fn two_devices_enrolled_in_the_same_millisecond_get_distinct_ids() {
        // A time-derived id collides here, and then revoking the lost phone
        // revokes the laptop too — the exact failure per-device credentials
        // exist to prevent. Caught for real by the routes test, whose fixture
        // pins the clock.
        let mut creds = Credentials::default();
        creds.set_enrol_code("A", 0);
        let (phone, phone_token) = creds.enrol("A", "phone", 1_000).unwrap();
        creds.set_enrol_code("B", 0);
        let (laptop, laptop_token) = creds.enrol("B", "laptop", 1_000).unwrap();

        assert_ne!(phone.id, laptop.id);
        creds.revoke(&phone.id);
        assert!(creds.device_for(&phone_token).is_none());
        assert!(
            creds.device_for(&laptop_token).is_some(),
            "the laptop must survive"
        );
    }

    #[test]
    fn an_unknown_token_matches_nothing() {
        let mut creds = Credentials::default();
        creds.set_enrol_code("A", 0);
        creds.enrol("A", "phone", 1).unwrap();
        assert!(creds.device_for("not-a-real-token").is_none());
        assert!(creds.device_for("").is_none());
    }

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
    fn an_enrol_code_avoids_the_characters_people_mistype() {
        // It is read off a terminal and typed into a phone. I/L/O/U are the
        // classic misreads, so the alphabet excludes them.
        for _ in 0..50 {
            let code = mint_enrol_code();
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
    fn credentials_round_trip_through_json() {
        let mut creds = Credentials::default();
        creds.set_enrol_code("A", 0);
        creds.enrol("A", "phone", 1).unwrap();
        let json = serde_json::to_string(&creds).unwrap();
        assert_eq!(serde_json::from_str::<Credentials>(&json).unwrap(), creds);
    }

    #[test]
    fn a_store_written_before_a_field_existed_still_loads() {
        // The data volume outlives any one image tag; an upgrade must not make
        // the developer's enrolled devices unreadable.
        let creds: Credentials = serde_json::from_str("{}").unwrap();
        assert!(creds.devices.is_empty());
        assert!(creds.enrol_code_hash.is_none());
    }
}
