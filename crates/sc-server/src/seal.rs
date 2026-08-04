//! Secrets that have to be given back, kept safe on a volume that travels.
//!
//! ## Why this exists, when [`crate::auth`] already refuses to store secrets
//!
//! Everything `auth` holds is **compared, never replayed** — a device token, an
//! enrolment code, a daemon key. Comparing needs only a hash, so the volume can
//! hold hashes and grant nobody anything. That is the posture `auth`'s own
//! module doc describes, and it is the right one wherever it fits.
//!
//! It does not fit a mail API key. The server has to *send* that key to Brevo,
//! so it has to be able to read it back, so a hash is no use. The same is true
//! of a screening key. These are the first secrets in
//! this server that are reversible by necessity rather than by carelessness.
//!
//! Until now they were environment variables, which kept them off the volume by
//! keeping them out of every file. Once they are editable from a page, they must
//! live somewhere — and the somewhere is the volume, which is *"the thing a
//! Portainer user backs up and copies around"*.
//!
//! ## So the volume holds ciphertext, and the key is the one thing not on it
//!
//! `SC_SERVER_SECRET_KEY` stays in the stack. A copied volume without it is
//! inert, which restores the property `auth` established: **a copy of the data
//! directory grants nobody anything.** It is arguably stronger than what it
//! replaces — an environment variable sits in plaintext in a stack editor,
//! readable forever by anyone with access to it, whereas this needs two things
//! held in two places.
//!
//! ## Authenticated, not merely encrypted
//!
//! ChaCha20-Poly1305 rather than a bare stream cipher, and the tag is the point.
//! Secrecy is the obvious half; **detecting tampering is the half that matters
//! more here.** An attacker who can write the volume but cannot read the key
//! could otherwise flip bits in a screening URL or a mail endpoint and have the
//! server obediently talk to it. With an AEAD that is a failed open, not a
//! silent redirection.
//!
//! ## A fresh nonce per value, never derived
//!
//! Every [`seal`] draws twelve random bytes and stores them beside the
//! ciphertext. Not a counter, not a hash of the field name, not the field name
//! itself: **reusing a nonce under one key is the failure mode of this
//! construction**, and it leaks plaintext rather than degrading gracefully.
//! Deriving from the field name would guarantee reuse the moment a key were
//! edited twice.
//!
//! ## What is deliberately not here
//!
//! No key rotation, no key derivation, no per-value keys. One key, taken as
//! given, used directly. Rotation is a redeploy plus re-entering three fields —
//! rare, and cheap enough that machinery to automate it would be more code to be
//! wrong about than the thing it replaces.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use serde::{Deserialize, Serialize};

/// How many bytes the key is, before hex encoding.
pub const KEY_BYTES: usize = 32;

/// How many hex characters `SC_SERVER_SECRET_KEY` must be.
pub const KEY_HEX_LEN: usize = KEY_BYTES * 2;

const NONCE_BYTES: usize = 12;

/// The key this server seals with, parsed once at startup.
///
/// Holds the parsed bytes rather than the hex string, so a malformed key is
/// refused at load and every later use is infallible. The alternative — parsing
/// per call — puts a failure that is really a *configuration* error onto the
/// path of every request that reads a setting.
#[derive(Clone)]
pub struct SealKey([u8; KEY_BYTES]);

impl std::fmt::Debug for SealKey {
    /// Never prints the key.
    ///
    /// `Config` derives `Debug` and is logged in places; a key that renders
    /// itself into a diagnostic is a key in a log file. This is the one thing
    /// this type owes the rest of the program.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SealKey(…)")
    }
}

impl PartialEq for SealKey {
    fn eq(&self, other: &Self) -> bool {
        // Constant-time, for the same reason `auth::matches` is: this is a
        // secret, and an early-returning compare on secrets is a habit worth not
        // having even where no attacker is obviously watching.
        self.0
            .iter()
            .zip(other.0.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }
}

impl Eq for SealKey {}

impl SealKey {
    /// Parse the hex form an operator sets.
    ///
    /// Refuses anything but exactly [`KEY_HEX_LEN`] hex characters. A short key
    /// looks configured while being guessable — the same rule the daemon key and
    /// the screening key already apply, and the same reason.
    pub fn parse(hex: &str) -> std::result::Result<SealKey, String> {
        let hex = hex.trim();
        if hex.len() != KEY_HEX_LEN {
            return Err(format!(
                "a sealing key is {KEY_HEX_LEN} hex characters and this one is {}. \
                 Generate one with `openssl rand -hex {KEY_BYTES}`, or read the one \
                 this server logs on a first start.",
                hex.len()
            ));
        }
        let mut out = [0u8; KEY_BYTES];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                .map_err(|_| "a sealing key is hexadecimal, and this one is not".to_string())?;
        }
        Ok(SealKey(out))
    }

    fn cipher(&self) -> ChaCha20Poly1305 {
        ChaCha20Poly1305::new(&self.0.into())
    }
}

/// One secret, encrypted.
///
/// Stored as hex rather than base64 because everything else this server writes
/// is hex, and one encoding is one fewer thing to get wrong when reading a file
/// by hand at three in the morning.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sealed {
    /// Fresh per seal. See the module doc on why this is never derived.
    #[serde(default)]
    pub nonce: String,
    #[serde(default)]
    pub ciphertext: String,
    /// When this value was last written, for the page that says "changed 3 days
    /// ago" without ever saying *what* changed to.
    #[serde(default)]
    pub set_ms: u64,
}

impl Sealed {
    /// Is there anything here?
    ///
    /// What the settings page asks to render "set" or "not set". Deliberately
    /// **not** "does it open" — a value sealed with a key this server no longer
    /// has is still *set*, and reporting it as absent would invite somebody to
    /// re-enter it when the real fix is the key.
    pub fn is_set(&self) -> bool {
        !self.ciphertext.is_empty()
    }
}

/// Encrypt a secret for the volume.
pub fn seal(key: &SealKey, plaintext: &str, now_ms: u64) -> Sealed {
    let mut nonce_bytes = [0u8; NONCE_BYTES];
    getrandom::fill(&mut nonce_bytes)
        .expect("the OS random source is unavailable; refusing to seal with a predictable nonce");
    let nonce = Nonce::from(nonce_bytes);

    // Infallible in practice for this cipher: the only documented error is a
    // payload too large for the construction, and these are API keys.
    let ciphertext = key
        .cipher()
        .encrypt(&nonce, plaintext.as_bytes())
        .expect("sealing a secret of this size cannot fail");

    Sealed {
        nonce: hex(&nonce_bytes),
        ciphertext: hex(&ciphertext),
        set_ms: now_ms,
    }
}

/// Read a secret back.
///
/// `None` covers every failure alike — wrong key, tampered bytes, malformed hex.
/// The caller cannot act differently on any of them and a distinguishing error
/// would only find its way into a log.
pub fn open(key: &SealKey, sealed: &Sealed) -> Option<String> {
    if !sealed.is_set() {
        return None;
    }
    let nonce_bytes: [u8; NONCE_BYTES] = unhex(&sealed.nonce)?.try_into().ok()?;
    let ciphertext = unhex(&sealed.ciphertext)?;
    let plain = key
        .cipher()
        .decrypt(&Nonce::from(nonce_bytes), ciphertext.as_ref())
        .ok()?;
    String::from_utf8(plain).ok()
}

/// Can this key read what is already on the volume?
///
/// **The check that must happen at startup, not at first use.** A key that is
/// missing or wrong makes every sealed value unreadable, and the shape of that
/// failure matters enormously: without this, a server would boot happily,
/// report no mail provider and no screener, and send nothing — which is
/// indistinguishable from a server nobody has configured yet. The operator
/// would go looking at the settings page and find it apparently blank.
///
/// So the question is asked once, against a value known to be present, and the
/// answer is a refusal to start with the setting named.
///
/// `probe` is any sealed value the volume already holds. `Ok(())` when there is
/// nothing to check — a fresh install has no secrets, so no key is needed yet.
pub fn usable(key: Option<&SealKey>, probe: Option<&Sealed>) -> std::result::Result<(), String> {
    let Some(probe) = probe.filter(|p| p.is_set()) else {
        return Ok(());
    };
    match key {
        None => Err(format!(
            "this volume holds sealed settings and {} is not set. Set it to the \
             key they were sealed with, or clear the settings to enter them again.",
            crate::config::env::SECRET_KEY
        )),
        Some(key) if open(key, probe).is_none() => Err(format!(
            "the settings on this volume were sealed with a different key than \
             {}. Restore the original key, or clear the settings to enter them \
             again — they cannot be recovered without it.",
            crate::config::env::SECRET_KEY
        )),
        Some(_) => Ok(()),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_key() -> SealKey {
        SealKey::parse(&crate::auth::mint_secret()).unwrap()
    }

    #[test]
    fn a_sealed_value_comes_back_the_same() {
        let key = a_key();
        let sealed = seal(&key, "xkeysib-not-a-real-key", 1_000);
        assert_eq!(
            open(&key, &sealed).as_deref(),
            Some("xkeysib-not-a-real-key")
        );
        assert_eq!(sealed.set_ms, 1_000);
    }

    #[test]
    fn the_plaintext_is_nowhere_in_what_gets_written() {
        // **The claim the whole change rests on.** Asserted against the
        // serialized form rather than the struct, because the file is what
        // leaves this machine.
        let key = a_key();
        let secret = "xkeysib-not-a-real-key";
        let json = serde_json::to_string(&seal(&key, secret, 1)).unwrap();
        assert!(!json.contains(secret), "{json}");
        for fragment in ["xkeysib", "not-a-real"] {
            assert!(!json.contains(fragment), "{fragment} survived: {json}");
        }
    }

    #[test]
    fn sealing_one_value_twice_gives_two_different_ciphertexts() {
        // The fresh nonce, observable. Equal ciphertexts would mean the nonce
        // was reused, which under one key is what leaks plaintext — so this is
        // the test that would fail if somebody "simplified" the nonce to
        // something derived.
        let key = a_key();
        let a = seal(&key, "same", 1);
        let b = seal(&key, "same", 1);
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.ciphertext, b.ciphertext);
        // And both still open.
        assert_eq!(open(&key, &a).as_deref(), Some("same"));
        assert_eq!(open(&key, &b).as_deref(), Some("same"));
    }

    #[test]
    fn another_key_does_not_open_it() {
        let sealed = seal(&a_key(), "secret", 1);
        assert_eq!(open(&a_key(), &sealed), None);
    }

    #[test]
    fn a_tampered_value_fails_to_open_rather_than_opening_differently() {
        // **Why an AEAD and not a stream cipher.** Somebody who can write the
        // volume but cannot read the key must not be able to steer what the
        // server talks to — flipping bits in a sealed screening URL has to be a
        // failure, not a redirection.
        let key = a_key();
        let mut sealed = seal(&key, "https://real.example", 1);

        let mut bytes = unhex(&sealed.ciphertext).unwrap();
        bytes[0] ^= 0x01;
        sealed.ciphertext = hex(&bytes);
        assert_eq!(open(&key, &sealed), None, "a flipped bit still opened");

        // The nonce is covered too: it is an input to the tag, so moving it
        // breaks the check rather than shifting the keystream silently.
        let mut sealed = seal(&key, "https://real.example", 1);
        let mut nonce = unhex(&sealed.nonce).unwrap();
        nonce[0] ^= 0x01;
        sealed.nonce = hex(&nonce);
        assert_eq!(open(&key, &sealed), None);
    }

    #[test]
    fn an_unset_value_opens_to_nothing_and_is_not_an_error() {
        // The resting state of every fresh install.
        assert_eq!(open(&a_key(), &Sealed::default()), None);
        assert!(!Sealed::default().is_set());
    }

    #[test]
    fn a_value_sealed_with_a_lost_key_still_reads_as_set() {
        // "Set" is about whether somebody entered one, not about whether this
        // server can read it. Reporting it absent would invite re-entering the
        // secret when the real fix is restoring the key.
        let sealed = seal(&a_key(), "secret", 1);
        assert!(sealed.is_set());
        assert_eq!(open(&a_key(), &sealed), None, "and it does not open");
    }

    #[test]
    fn a_key_has_to_be_the_right_length_and_hexadecimal() {
        assert!(SealKey::parse("").is_err());
        assert!(SealKey::parse("abcd").is_err());
        // 64 characters, but not hex.
        assert!(SealKey::parse(&"z".repeat(KEY_HEX_LEN)).is_err());
        // A minted secret is exactly what this wants, which is why the operator
        // is told to use one.
        assert!(SealKey::parse(&crate::auth::mint_secret()).is_ok());
        // Surrounding whitespace is an artefact of copying, not a different key.
        let key = crate::auth::mint_secret();
        assert_eq!(
            SealKey::parse(&format!("  {key}\n")).unwrap(),
            SealKey::parse(&key).unwrap()
        );
    }

    #[test]
    fn a_key_never_prints_itself() {
        // `Config` derives `Debug` and gets logged. A key that renders into a
        // diagnostic is a key in a log file.
        let key = crate::auth::mint_secret();
        let shown = format!("{:?}", SealKey::parse(&key).unwrap());
        assert!(!shown.contains(&key[..16]), "{shown}");
        assert_eq!(shown, "SealKey(…)");
    }

    #[test]
    fn a_wrong_key_is_a_refusal_and_not_a_blank_settings_page() {
        // **The failure shape that matters most here.** Without this check a
        // server boots, opens nothing, and reports no mail provider and no
        // screener — which looks exactly like a server nobody has configured.
        // The operator would go to the settings page, find it apparently empty,
        // and re-enter secrets that were never actually lost.
        let sealed = seal(&a_key(), "secret", 1);

        let wrong = a_key();
        let err = usable(Some(&wrong), Some(&sealed)).unwrap_err();
        assert!(err.contains(crate::config::env::SECRET_KEY), "{err}");
        assert!(err.contains("different key"), "{err}");

        // Missing is its own message: the fix is different from a wrong one.
        let err = usable(None, Some(&sealed)).unwrap_err();
        assert!(err.contains("is not set"), "{err}");
    }

    #[test]
    fn a_volume_with_no_secrets_needs_no_key() {
        // The resting state of a fresh install, and of every deployment that
        // never stores a secret through the UI. Demanding a key from those
        // would be a setting that exists only to be satisfied.
        assert!(usable(None, None).is_ok());
        assert!(usable(None, Some(&Sealed::default())).is_ok());
        // And the right key on a real value is, of course, fine.
        let key = a_key();
        let sealed = seal(&key, "secret", 1);
        assert!(usable(Some(&key), Some(&sealed)).is_ok());
    }

    #[test]
    fn garbage_hex_does_not_panic() {
        // Everything here is read from a file somebody may have edited.
        let key = a_key();
        for bad in ["nonsense", "0", "zz", ""] {
            let sealed = Sealed {
                nonce: bad.to_string(),
                ciphertext: bad.to_string(),
                set_ms: 0,
            };
            assert_eq!(open(&key, &sealed), None);
        }
    }
}
