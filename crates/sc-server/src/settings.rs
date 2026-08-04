//! What this server does, decided from a page rather than a stack.
//!
//! ## Why these moved off the environment
//!
//! Changing an environment variable means editing a stack and redeploying, which
//! restarts the process and drops whatever was in flight. That is a fair price
//! for *where to listen* and *which volume to use* — facts that cannot change
//! while running anyway. It is a bad price for a mail key, a daily cap, or a
//! screening model, which are ordinary operational decisions.
//!
//! ## Seeds, not sources of truth
//!
//! Every value here can be seeded from the environment **once**, guarded by
//! [`Settings::seeded`], exactly as [`crate::roster`] is. Without that flag a
//! redeploy would silently revert every change made through the UI — the same
//! failure "it takes effect on the next request" exists to prevent, arriving by
//! the back door of a restart.
//!
//! ## Secrets are sealed, and never given back
//!
//! Three values here have to be *replayed* rather than compared — a mail key
//! goes to Brevo, a client secret goes to GitHub — so they cannot be hashed the
//! way [`crate::auth`] hashes everything else. They are sealed instead; see
//! [`crate::seal`] for why that keeps the volume safe to copy.
//!
//! **Nothing reads a secret back to a page.** The settings surface renders
//! presence and a date, never a value, which removes the class of leak rather
//! than gating it.

use serde::{Deserialize, Serialize};

use crate::seal::{SealKey, Sealed};

/// Everything this server decides for itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// The absolute address people reach this server at.
    ///
    /// Sign-in links are built from it and it decides whether cookies carry
    /// `Secure`, so it is validated by the same function the environment was —
    /// see [`crate::config::check_base_url`].
    #[serde(default)]
    pub base_url: String,
    /// The GitHub OAuth application. Both halves or neither: an id without a
    /// secret is a sign-in button that always fails.
    #[serde(default)]
    pub github_client_id: String,
    #[serde(default)]
    pub github_client_secret: Sealed,
    /// Set the first time the environment is applied. See the module doc.
    #[serde(default)]
    pub seeded: bool,
}

impl Settings {
    /// Is there a usable GitHub application?
    ///
    /// Both halves, because half an application is a button that sends somebody
    /// to GitHub and then cannot finish — worse than no button.
    pub fn has_github(&self) -> bool {
        !self.github_client_id.is_empty() && self.github_client_secret.is_set()
    }

    /// The client secret, if it can be read.
    ///
    /// `None` when unset *or* when the sealing key is missing or wrong. The
    /// caller cannot act differently on those, and the startup check in
    /// [`crate::seal::usable`] is what turns the second into a refusal to boot
    /// rather than a mystery here.
    pub fn github_secret(&self, key: Option<&SealKey>) -> Option<String> {
        crate::seal::open(key?, &self.github_client_secret)
    }

    /// Record a GitHub application.
    pub fn set_github(&mut self, key: &SealKey, id: &str, secret: &str, now_ms: u64) {
        self.github_client_id = id.trim().to_string();
        self.github_client_secret = crate::seal::seal(key, secret.trim(), now_ms);
    }

    /// Apply the environment **once**, the first time this volume is used.
    ///
    /// `true` when this call was the seeding, so the caller knows to write.
    /// The flag is set even when nothing was supplied, for the reason
    /// [`crate::roster::Roster::seed`] gives: otherwise the first boot *with*
    /// something configured would seed a volume somebody had already
    /// administered.
    pub fn seed(&mut self, from: Seed<'_>, key: Option<&SealKey>, now_ms: u64) -> bool {
        if self.seeded {
            return false;
        }
        if let Some(base) = from.base_url {
            self.base_url = base.trim().to_string();
        }
        // Sealed only if there is a key to seal with. Without one the id is
        // still recorded, which leaves `has_github` false — visibly incomplete
        // on the settings page rather than silently half-applied.
        if let (Some(id), Some(secret), Some(key)) = (from.github_id, from.github_secret, key) {
            self.set_github(key, id, secret, now_ms);
        } else if let Some(id) = from.github_id {
            self.github_client_id = id.trim().to_string();
        }
        self.seeded = true;
        true
    }
}

/// What the environment offered at first boot.
///
/// A struct rather than a long parameter list, so adding a setting later is one
/// field rather than a change every caller has to be re-checked against.
#[derive(Debug, Default, Clone, Copy)]
pub struct Seed<'a> {
    pub base_url: Option<&'a str>,
    pub github_id: Option<&'a str>,
    pub github_secret: Option<&'a str>,
}

/// The settings, and the file state they were read from.
///
/// The same mtime-keyed cache as [`crate::roster::RosterCache`], and for the
/// same reason: a change has to take effect on the next request, and a parse on
/// every request would pay for it on every request.
#[derive(Debug, Default)]
pub struct SettingsCache {
    mtime: Option<std::time::SystemTime>,
    value: std::sync::Arc<Settings>,
}

impl SettingsCache {
    /// The settings as they are on disk right now.
    ///
    /// A failed read yields whatever was last good, so a transient error does
    /// not momentarily un-configure the server.
    pub fn current(&mut self, path: &std::path::Path) -> std::sync::Arc<Settings> {
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if mtime != self.mtime {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(parsed) = serde_json::from_str::<Settings>(&text) {
                    self.value = std::sync::Arc::new(parsed);
                }
            } else if mtime.is_none() {
                self.value = std::sync::Arc::new(Settings::default());
            }
            self.mtime = mtime;
        }
        std::sync::Arc::clone(&self.value)
    }

    /// Forget what was read, so the next look re-parses. Called after a write,
    /// because a coarse filesystem timestamp can hide one.
    pub fn invalidate(&mut self) {
        self.mtime = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_key() -> SealKey {
        SealKey::parse(&crate::auth::mint_secret()).unwrap()
    }

    #[test]
    fn a_github_application_round_trips_without_the_secret_being_stored() {
        let key = a_key();
        let mut s = Settings::default();
        s.set_github(&key, "client-id", "client-secret", 1);

        assert!(s.has_github());
        assert_eq!(
            s.github_secret(Some(&key)).as_deref(),
            Some("client-secret")
        );

        // And the file holds no secret.
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("client-secret"), "{json}");
        // The id is not a secret and is stored plainly, which the page shows.
        assert!(json.contains("client-id"));
    }

    #[test]
    fn half_an_application_is_not_an_application() {
        // An id without a secret is a sign-in button that sends somebody to
        // GitHub and cannot finish — worse than no button at all.
        let mut s = Settings {
            github_client_id: "client-id".into(),
            ..Settings::default()
        };
        assert!(!s.has_github());

        let key = a_key();
        s.set_github(&key, "client-id", "secret", 1);
        assert!(s.has_github());
    }

    #[test]
    fn a_secret_sealed_with_a_lost_key_reads_as_absent_but_stays_set() {
        // The distinction matters: the page must not invite re-entering a secret
        // that was never lost, and `seal::usable` turns this into a refusal to
        // boot rather than a blank page.
        let mut s = Settings::default();
        s.set_github(&a_key(), "id", "secret", 1);
        assert!(s.github_client_secret.is_set());
        assert_eq!(s.github_secret(Some(&a_key())), None);
        assert_eq!(s.github_secret(None), None, "and with no key at all");
    }

    #[test]
    fn the_seed_is_applied_once_and_never_again() {
        // Without the flag a redeploy silently reverts every change made through
        // the UI — the restart back door.
        let key = a_key();
        let seed = Seed {
            base_url: Some("https://one.example"),
            github_id: Some("id-one"),
            github_secret: Some("secret-one"),
        };

        let mut s = Settings::default();
        assert!(s.seed(seed, Some(&key), 1));
        assert_eq!(s.base_url, "https://one.example");

        // The administrator changes it through the UI, then the container
        // restarts with the old environment still in place.
        s.base_url = "https://two.example".into();
        assert!(!s.seed(seed, Some(&key), 2), "a later boot does not");
        assert_eq!(s.base_url, "https://two.example", "the edit survived");
    }

    #[test]
    fn an_empty_seed_still_marks_the_volume_administered() {
        let mut s = Settings::default();
        assert!(s.seed(Seed::default(), None, 1));
        assert!(s.seeded);
        assert!(!s.seed(
            Seed {
                base_url: Some("https://late.example"),
                ..Seed::default()
            },
            None,
            2
        ));
        assert!(s.base_url.is_empty());
    }

    #[test]
    fn seeding_an_application_with_no_sealing_key_leaves_it_visibly_incomplete() {
        // Rather than silently half-applied: `has_github` is false, so the
        // sign-in button does not render and the settings page shows the gap.
        let mut s = Settings::default();
        s.seed(
            Seed {
                github_id: Some("id"),
                github_secret: Some("secret"),
                ..Seed::default()
            },
            None,
            1,
        );
        assert_eq!(s.github_client_id, "id");
        assert!(!s.has_github());
    }

    #[test]
    fn the_cache_sees_a_change_on_the_next_look() {
        let dir = std::env::temp_dir().join(format!(
            "sc-settings-{}-{}",
            std::process::id(),
            &crate::auth::mint_secret()[..12]
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        let mut s = Settings {
            base_url: "https://one.example".into(),
            ..Settings::default()
        };
        std::fs::write(&path, serde_json::to_string(&s).unwrap()).unwrap();

        let mut cache = SettingsCache::default();
        assert_eq!(cache.current(&path).base_url, "https://one.example");

        s.base_url = "https://two.example".into();
        std::fs::write(&path, serde_json::to_string(&s).unwrap()).unwrap();
        cache.invalidate();
        assert_eq!(cache.current(&path).base_url, "https://two.example");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_an_unconfigured_server_and_not_an_error() {
        let mut cache = SettingsCache::default();
        let nowhere = std::env::temp_dir().join("sc-settings-does-not-exist.json");
        assert!(cache.current(&nowhere).base_url.is_empty());
    }

    #[test]
    fn a_file_written_before_a_field_existed_still_loads() {
        // The data volume outlives any one image tag.
        let old = r#"{"base_url":"https://one.example","seeded":true}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.base_url, "https://one.example");
        assert!(!s.has_github());
        assert!(s.seeded);
    }
}
