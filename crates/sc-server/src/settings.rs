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
//! Two values here have to be *replayed* rather than compared — a mail key goes
//! to Brevo, a screening key goes to its provider — so they cannot be hashed the
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
    /// Whether the public surface exists at all.
    ///
    /// **This used to be an environment variable on purpose**: a server that
    /// could open its own public surface from a UI was a different security
    /// posture. The claim changed the premise — the only caller who can reach
    /// this proved they can read the container's logs, which is the same proof
    /// the stack editor stood in for and better than holding a cookie. So the
    /// switch moved, and a freshly claimed server starts with it **off**.
    #[serde(default)]
    pub public: bool,
    /// What the masthead calls this site. A label, not a routing key.
    #[serde(default)]
    pub site_name: String,
    /// May a filer read the spec drafted from their own request?
    ///
    /// `Option` so "never set" is distinguishable from "set to false", which is
    /// what lets the default be *on* without that default overwriting a
    /// deliberate no on every read.
    #[serde(default)]
    pub show_spec: Option<bool>,

    /// How sign-in links are sent. Empty provider means no mail is configured.
    #[serde(default)]
    pub mail_provider: String,
    #[serde(default)]
    pub mail_key: Sealed,
    #[serde(default)]
    pub mail_from: String,
    #[serde(default)]
    pub mail_from_name: String,

    /// The spam screener. Empty key means filings are not screened.
    #[serde(default)]
    pub screen_key: Sealed,
    #[serde(default)]
    pub screen_url: String,
    #[serde(default)]
    pub screen_model: String,

    /// The four spend ceilings. `None` means "use the built-in default", so a
    /// value never set does not have to be re-stated to keep working.
    #[serde(default)]
    pub max_daily_filings: Option<usize>,
    #[serde(default)]
    pub max_daily_drafts: Option<usize>,
    #[serde(default)]
    pub max_accounts: Option<usize>,
    #[serde(default)]
    pub max_outstanding_links: Option<usize>,

    /// Set the first time the environment is applied. See the module doc.
    #[serde(default)]
    pub seeded: bool,
}

impl Settings {
    /// Is mail configured well enough to send a sign-in link?
    pub fn has_mail(&self) -> bool {
        !self.mail_provider.is_empty() && self.mail_key.is_set() && !self.mail_from.is_empty()
    }

    /// Is screening configured?
    ///
    /// A key alone is enough — the URL and model have defaults, and demanding
    /// all three would make the common case (Gemini) three fields instead of
    /// one.
    pub fn has_screening(&self) -> bool {
        self.screen_key.is_set()
    }

    /// The mail settings, if they are complete and readable.
    pub fn mail(&self, key: Option<&SealKey>) -> Option<crate::config::MailConfig> {
        if !self.has_mail() {
            return None;
        }
        let provider = crate::mail::Provider::parse(&self.mail_provider)?;
        Some(crate::config::MailConfig {
            provider,
            api_key: crate::seal::open(key?, &self.mail_key)?,
            from: self.mail_from.clone(),
            from_name: if self.mail_from_name.is_empty() {
                crate::config::DEFAULT_MAIL_FROM_NAME.to_string()
            } else {
                self.mail_from_name.clone()
            },
        })
    }

    /// The screening settings, if they are complete and readable.
    pub fn screen(&self, key: Option<&SealKey>) -> Option<crate::config::ScreenConfig> {
        if !self.has_screening() {
            return None;
        }
        Some(crate::config::ScreenConfig {
            api_key: crate::seal::open(key?, &self.screen_key)?,
            url: if self.screen_url.is_empty() {
                crate::config::DEFAULT_SCREEN_URL.to_string()
            } else {
                self.screen_url.clone()
            },
            model: if self.screen_model.is_empty() {
                crate::config::DEFAULT_SCREEN_MODEL.to_string()
            } else {
                self.screen_model.clone()
            },
        })
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

        self.public = from.public;
        if let Some(name) = from.site_name {
            self.site_name = name.trim().to_string();
        }
        self.show_spec = from.show_spec;

        if let (Some(mail), Some(key)) = (from.mail, key) {
            self.mail_provider = mail.provider.slug().to_string();
            self.mail_key = crate::seal::seal(key, &mail.api_key, now_ms);
            self.mail_from = mail.from.clone();
            self.mail_from_name = mail.from_name.clone();
        }
        if let (Some(screen), Some(key)) = (from.screen, key) {
            self.screen_key = crate::seal::seal(key, &screen.api_key, now_ms);
            self.screen_url = screen.url.clone();
            self.screen_model = screen.model.clone();
        }

        self.max_daily_filings = from.max_daily_filings;
        self.max_daily_drafts = from.max_daily_drafts;
        self.max_accounts = from.max_accounts;
        self.max_outstanding_links = from.max_outstanding_links;

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
    pub site_name: Option<&'a str>,
    /// Whether the environment described a public surface at all.
    pub public: bool,
    pub show_spec: Option<bool>,
    pub mail: Option<&'a crate::config::MailConfig>,
    pub screen: Option<&'a crate::config::ScreenConfig>,
    pub max_daily_filings: Option<usize>,
    pub max_daily_drafts: Option<usize>,
    pub max_accounts: Option<usize>,
    pub max_outstanding_links: Option<usize>,
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
    fn the_seed_is_applied_once_and_never_again() {
        // Without the flag a redeploy silently reverts every change made through
        // the UI — the restart back door.
        let key = a_key();
        let seed = Seed {
            base_url: Some("https://one.example"),
            ..Seed::default()
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
        assert!(s.seeded);
    }
}
