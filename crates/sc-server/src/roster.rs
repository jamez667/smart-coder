//! Who may review work, and for what — kept on the volume rather than in the
//! stack.
//!
//! ## Why this moved out of configuration
//!
//! Owners were an environment variable, and that was defensible while the only
//! writer was somebody editing a Portainer stack. It is a bad fit for a list
//! that changes when people join and leave: every edit meant a redeploy, which
//! restarts the server and drops whatever was in flight.
//!
//! **The property that had to survive the move** is the one that made
//! configuration right in the first place: *revocation takes effect on the next
//! request*. Deleting a line and redeploying was complete revocation — no
//! session to hunt down, no record that might disagree. A snapshot taken at
//! startup would lose that, so the file is re-read whenever it changes; see
//! [`RosterCache`].
//!
//! ## The direction of promotion is preserved
//!
//! The old guarantee was *"an owner is an account the configuration promotes —
//! never one that promotes itself"*. Substituting "the developer" for "the
//! configuration" keeps it exactly: the only writer is a route behind the device
//! gate, so an owner cannot add an owner. That matters more than it looks —
//! somebody who may promote may promote an accomplice, and revoking the first
//! would then not revoke the second.
//!
//! ## Why its own file
//!
//! `accounts.json` is written by anyone who signs in; this is written only by an
//! administrator. Sharing one file would put a self-serve write and a privileged
//! one under the same lock and the same rewrite — the same reasoning that gave
//! `oauth-states.json` a file of its own.

use serde::{Deserialize, Serialize};

/// Who may review what, and which repositories collect publicly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roster {
    #[serde(default)]
    pub owners: Vec<OwnerRecord>,
    /// Which repositories this surface collects for.
    ///
    /// Empty is a real state, not a broken one: a developer who can disable the
    /// last repository makes it reachable, and the form says why it cannot take
    /// anything rather than the server refusing to boot. Refusing would make
    /// the page that fixes it unreachable exactly when it is needed.
    #[serde(default)]
    pub repos: Vec<RepoRecord>,
    /// The credentials daemons authenticate with, one per machine.
    ///
    /// **Hashed, so nothing here is reversible** — which is what made these the
    /// safest secret to move off the environment. The server only ever asks "is
    /// this the same", never "what was it", so the volume can hold the answer
    /// and grant nobody anything.
    ///
    /// Minted here rather than invented by an operator: the server generates the
    /// key, shows it once, and keeps only the hash. That is strictly better than
    /// a stack variable, which sits in plaintext in an editor forever.
    #[serde(default)]
    pub daemons: Vec<DaemonRecord>,
    /// Set the first time a seed is applied.
    ///
    /// Without it the environment would re-apply on every boot, and an owner
    /// revoked through the UI would come back on the next restart — which is
    /// precisely the failure "revocation takes effect on the next request"
    /// exists to prevent, arriving by a different door.
    #[serde(default)]
    pub seeded: bool,
}

/// Somebody the **developer** promoted. Never somebody who promoted themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerRecord {
    /// The GitHub login, lowercased.
    ///
    /// Not the numeric id, for the same reason the configuration used a login:
    /// an administrator types a name they recognise, and nobody knows their
    /// collaborator's numeric id. A rename makes the entry stop matching — a
    /// person who cannot sign in and says so, which is the safe direction.
    pub login: String,
    /// Which repositories this owner may review.
    ///
    /// An empty set is treated as revoked rather than as an error. A record read
    /// at runtime cannot refuse to boot the way a setting could, so the
    /// unusable case has to mean something — and "sees nothing" is the reading
    /// that fails closed.
    #[serde(default)]
    pub repos: Vec<String>,
    pub added_ms: u64,
    /// Kept rather than deleted, so the page can say somebody *was* an owner —
    /// the same reasoning as a revoked device. A list that silently shrinks
    /// cannot answer "did I already deal with that?".
    #[serde(default)]
    pub revoked: bool,
}

/// One machine's credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonRecord {
    /// What the developer calls this machine: `laptop`, `office`.
    ///
    /// **Not a secret**, which is the point of it being separate from the key:
    /// it appears in the log and on a claim, so a human reading either can tell
    /// which machine did something.
    pub label: String,
    /// SHA-256 of the key. The key itself is never stored.
    pub key_hash: String,
    pub added_ms: u64,
    /// Kept rather than deleted, so the page can say a machine *was* trusted —
    /// the same reasoning as a revoked owner.
    #[serde(default)]
    pub revoked: bool,
}

/// A repository this surface collects for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRecord {
    /// Matched **exactly** against the daemon's own `queue add-repo` name, and
    /// against what a filer's form submits. A fuzzy match here would reintroduce
    /// the ambiguity a closed set exists to remove.
    pub name: String,
    /// Which daemon confirmed it was serving this when it was enabled.
    ///
    /// `None` records that nobody had — the developer enabled it anyway, past
    /// the check. Kept rather than dropped so the two cases stay
    /// distinguishable: "a machine said it serves this" and "I asserted it" are
    /// different claims, and a page that showed them alike could not explain why
    /// nothing is being drafted.
    #[serde(default)]
    pub served_by: Option<String>,
    pub added_ms: u64,
    /// Disabling stops collection without losing the record, so the page can
    /// say a name *was* served — the same reasoning as a revoked owner.
    #[serde(default)]
    pub disabled: bool,
}

impl OwnerRecord {
    /// May this owner review work for `repo`?
    pub fn owns(&self, repo: &str) -> bool {
        !self.revoked && self.repos.iter().any(|r| r == repo)
    }

    /// Can this record grant anything at all?
    pub fn live(&self) -> bool {
        !self.revoked && !self.repos.is_empty()
    }
}

impl Roster {
    /// The live record for a login, if the developer has promoted one.
    ///
    /// Lowercased on the way in so a caller holding GitHub's own casing matches
    /// a record written by hand.
    pub fn owner_for(&self, login: &str) -> Option<&OwnerRecord> {
        let login = login.to_ascii_lowercase();
        self.owners.iter().find(|o| o.login == login && o.live())
    }

    /// Add or replace an owner.
    ///
    /// Replaces rather than appends: two records for one person would make
    /// "revoke that owner" ambiguous, and deleting one would leave them with
    /// access from the other.
    pub fn set_owner(&mut self, login: &str, repos: &[String], now_ms: u64) {
        let login = login.trim().to_ascii_lowercase();
        self.owners.retain(|o| o.login != login);
        self.owners.push(OwnerRecord {
            login,
            repos: repos.to_vec(),
            added_ms: now_ms,
            revoked: false,
        });
    }

    /// Apply a configured owner list **once**, the first time a volume is used.
    ///
    /// `true` when this call was the seeding, so the caller knows to write.
    ///
    /// A seed and not a source of truth: re-applying every boot would resurrect
    /// an owner revoked through the UI — the failure "revocation takes effect on
    /// the next request" exists to prevent, arriving by the back door of a
    /// restart.
    ///
    /// The flag is set **even when the list is empty**. Otherwise the first boot
    /// with owners configured would seed a volume that had already been
    /// administered, and "I removed the last owner and one came back" is the
    /// same bug with more steps.
    pub fn seed(
        &mut self,
        owners: &[(String, Vec<String>)],
        repos: &[String],
        daemons: &[crate::config::DaemonKey],
        now_ms: u64,
    ) -> bool {
        if self.seeded {
            return false;
        }
        // **Hashes carry straight across.** The environment already holds these
        // hashed, so seeding copies the hash rather than re-deriving anything —
        // which is what lets a running daemon keep working across the upgrade
        // without touching its own configuration. Getting this wrong would
        // strand it, and the symptom would be work silently not being drafted.
        for key in daemons {
            self.set_daemon(&key.label, &key.key_hash, now_ms);
        }
        for name in repos {
            // `served_by: None` — no daemon has polled yet at startup, and
            // asserting one would be a claim nothing made. The configuration
            // *is* the developer's assertion, which is the same thing the
            // override records.
            self.enable(name, None, now_ms);
        }
        for (login, owned) in owners {
            self.set_owner(login, owned, now_ms);
        }
        self.seeded = true;
        true
    }

    /// The enabled repository names, in the order they were added — which is
    /// the order the picker offers.
    pub fn enabled(&self) -> Vec<String> {
        self.repos
            .iter()
            .filter(|r| !r.disabled)
            .map(|r| r.name.clone())
            .collect()
    }

    /// Enable a repository, or re-enable one that was disabled.
    ///
    /// Replaces rather than appends, for the reason [`set_owner`](Roster::set_owner)
    /// does: two records for one name would make "disable that repository"
    /// ambiguous.
    pub fn enable(&mut self, name: &str, served_by: Option<String>, now_ms: u64) {
        let name = name.trim().to_string();
        self.repos.retain(|r| r.name != name);
        self.repos.push(RepoRecord {
            name,
            served_by,
            added_ms: now_ms,
            disabled: false,
        });
    }

    /// Stop collecting for a repository. `true` if one was enabled and now is not.
    pub fn disable(&mut self, name: &str) -> bool {
        match self
            .repos
            .iter_mut()
            .find(|r| r.name == name && !r.disabled)
        {
            Some(r) => {
                r.disabled = true;
                true
            }
            None => false,
        }
    }

    /// The live daemon credentials, in the shape the poll path compares against.
    pub fn daemon_keys(&self) -> Vec<crate::config::DaemonKey> {
        self.daemons
            .iter()
            .filter(|d| !d.revoked)
            .map(|d| crate::config::DaemonKey {
                label: d.label.clone(),
                key_hash: d.key_hash.clone(),
            })
            .collect()
    }

    /// Record a machine's credential.
    ///
    /// Takes the hash, never the key — the caller mints and shows the key, and
    /// this never sees it. Replaces by label, so re-minting for a machine
    /// rotates rather than accumulating two live credentials for one name.
    pub fn set_daemon(&mut self, label: &str, key_hash: &str, now_ms: u64) {
        let label = label.trim().to_string();
        self.daemons.retain(|d| d.label != label);
        self.daemons.push(DaemonRecord {
            label,
            key_hash: key_hash.to_string(),
            added_ms: now_ms,
            revoked: false,
        });
    }

    /// Stop trusting a machine. `true` if one was live and now is not.
    pub fn revoke_daemon(&mut self, label: &str) -> bool {
        match self
            .daemons
            .iter_mut()
            .find(|d| d.label == label && !d.revoked)
        {
            Some(d) => {
                d.revoked = true;
                true
            }
            None => false,
        }
    }

    /// Revoke an owner. `true` if one was live and now is not.
    pub fn revoke(&mut self, login: &str) -> bool {
        let login = login.to_ascii_lowercase();
        match self
            .owners
            .iter_mut()
            .find(|o| o.login == login && !o.revoked)
        {
            Some(o) => {
                o.revoked = true;
                true
            }
            None => false,
        }
    }
}

/// The roster, and the file state it was read from.
///
/// **Not a copy taken at startup.** Revocation has to take effect on the request
/// after it — that is the property the move off configuration had to keep — and
/// a snapshot cannot do that. A parse on every identification could, but would
/// pay for one on every request.
///
/// So: the file's modified time is checked each time, and the contents are
/// parsed only when it has actually changed. Writes go through the same
/// `write_lock` as everything else and land atomically, so a reader never sees a
/// half-written file.
///
/// This does not disturb the ordering `identify` documents. That argument is
/// about who pays to parse an *attacker-sized* file — `accounts.json` is one,
/// because signup is self-serve. This is administrator-sized and read after the
/// account branch has already succeeded.
#[derive(Debug, Default)]
pub struct RosterCache {
    mtime: Option<std::time::SystemTime>,
    value: std::sync::Arc<Roster>,
}

impl RosterCache {
    /// The roster as it is on disk right now.
    ///
    /// A failed read yields whatever was last good rather than an empty roster:
    /// a transient error must not silently demote every owner, which would look
    /// exactly like a revocation nobody performed.
    pub fn current(&mut self, path: &std::path::Path) -> std::sync::Arc<Roster> {
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if mtime != self.mtime {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(parsed) = serde_json::from_str::<Roster>(&text) {
                    self.value = std::sync::Arc::new(parsed);
                }
            } else if mtime.is_none() {
                // No file yet — the resting state of a fresh install, and an
                // empty roster is the right answer rather than a stale one.
                self.value = std::sync::Arc::new(Roster::default());
            }
            self.mtime = mtime;
        }
        std::sync::Arc::clone(&self.value)
    }

    /// Forget what was read, so the next look re-parses.
    ///
    /// Called after a write. The mtime would usually catch it anyway, but a
    /// filesystem with coarse timestamps can record a write inside the same tick
    /// as the read before it — and an administrator who revokes somebody and
    /// sees no change would reasonably conclude it had not worked.
    pub fn invalidate(&mut self) {
        self.mtime = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_owner_is_matched_whatever_case_github_reports() {
        // The record is written by hand and the login arrives from GitHub, so
        // the two have to meet somewhere. A mismatch here grants nothing while
        // looking applied.
        let mut roster = Roster::default();
        roster.set_owner("JameZ667", &["alpha".into()], 1);
        assert!(roster.owner_for("jamez667").is_some());
        assert!(roster.owner_for("JAMEZ667").is_some());
    }

    #[test]
    fn setting_an_owner_twice_replaces_rather_than_duplicates() {
        // Two records for one person makes "revoke that owner" ambiguous, and
        // deleting one would leave them with access from the other.
        let mut roster = Roster::default();
        roster.set_owner("jamez667", &["alpha".into()], 1);
        roster.set_owner("jamez667", &["beta".into()], 2);
        assert_eq!(roster.owners.len(), 1);
        assert_eq!(roster.owner_for("jamez667").unwrap().repos, ["beta"]);
    }

    #[test]
    fn an_owner_of_nothing_grants_nothing() {
        // A record read at runtime cannot refuse to boot the way a setting
        // could, so the unusable case has to mean something — and "sees
        // nothing" is the reading that fails closed.
        let mut roster = Roster::default();
        roster.set_owner("jamez667", &[], 1);
        assert!(roster.owner_for("jamez667").is_none());
    }

    #[test]
    fn a_revoked_owner_is_kept_and_grants_nothing() {
        let mut roster = Roster::default();
        roster.set_owner("jamez667", &["alpha".into()], 1);
        assert!(roster.revoke("jamez667"));
        assert!(roster.owner_for("jamez667").is_none());
        // Kept, so the page can say they *were* one.
        assert_eq!(roster.owners.len(), 1);
        assert!(roster.owners[0].revoked);
        // And revoking twice is not a second event.
        assert!(!roster.revoke("jamez667"));
    }

    #[test]
    fn the_cache_sees_a_change_on_the_next_look() {
        // **The property the move off configuration had to keep.** Deleting a
        // line and redeploying was complete revocation; this has to be as
        // immediate, without the redeploy.
        let dir = std::env::temp_dir().join(format!("sc-roster-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("owners.json");

        let mut roster = Roster::default();
        roster.set_owner("jamez667", &["alpha".into()], 1);
        std::fs::write(&path, serde_json::to_string(&roster).unwrap()).unwrap();

        let mut cache = RosterCache::default();
        assert!(cache.current(&path).owner_for("jamez667").is_some());

        roster.revoke("jamez667");
        std::fs::write(&path, serde_json::to_string(&roster).unwrap()).unwrap();
        cache.invalidate();

        assert!(
            cache.current(&path).owner_for("jamez667").is_none(),
            "revocation takes effect on the next look"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_an_empty_roster_and_not_an_error() {
        // The resting state of every fresh install.
        let mut cache = RosterCache::default();
        let nowhere = std::env::temp_dir().join("sc-roster-does-not-exist.json");
        assert!(cache.current(&nowhere).owners.is_empty());
    }

    #[test]
    fn the_seed_is_applied_once_and_never_again() {
        // **The restart back door.** Re-applying the setting every boot would
        // resurrect an owner revoked through the UI — the same failure
        // revocation-on-the-next-request exists to prevent, arriving by a
        // different door and only on a restart, which is where it would go
        // unnoticed longest.
        let configured = vec![("jamez667".to_string(), vec!["intake".to_string()])];
        let repos = vec!["intake".to_string()];

        let mut roster = Roster::default();
        assert!(
            roster.seed(&configured, &repos, &[], 1),
            "the first boot seeds"
        );
        assert!(roster.owner_for("jamez667").is_some());
        assert_eq!(roster.enabled(), ["intake"]);

        // The developer revokes the owner and stops collecting for the
        // repository, then the container restarts with both settings still in
        // place.
        assert!(roster.revoke("jamez667"));
        assert!(roster.disable("intake"));
        assert!(
            !roster.seed(&configured, &repos, &[], 2),
            "a later boot does not"
        );
        assert!(
            roster.owner_for("jamez667").is_none(),
            "a revoked owner came back on a restart"
        );
        assert!(
            roster.enabled().is_empty(),
            "a disabled repository came back on a restart"
        );
    }

    #[test]
    fn seeding_carries_the_daemon_hashes_across_untouched() {
        // **Getting this wrong strands a running daemon**, and the symptom is
        // work silently not being drafted rather than an error anybody sees.
        // The environment already holds these hashed, so the seed copies the
        // hash rather than re-deriving anything.
        let hash = crate::auth::hash("the-real-key");
        let configured = vec![crate::config::DaemonKey {
            label: "laptop".into(),
            key_hash: hash.clone(),
        }];

        let mut roster = Roster::default();
        assert!(roster.seed(&[], &[], &configured, 1));

        let live = roster.daemon_keys();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].label, "laptop");
        assert_eq!(
            live[0].key_hash, hash,
            "the hash changed, so the daemon is locked out"
        );
        assert!(
            crate::auth::matches("the-real-key", &live[0].key_hash),
            "the key that worked before no longer does"
        );
    }

    #[test]
    fn a_revoked_machine_is_kept_and_claims_nothing() {
        let mut roster = Roster::default();
        roster.set_daemon("laptop", &crate::auth::hash("k"), 1);
        roster.set_daemon("office", &crate::auth::hash("j"), 1);
        assert_eq!(roster.daemon_keys().len(), 2);

        assert!(roster.revoke_daemon("laptop"));
        let live = roster.daemon_keys();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].label, "office", "revoking one took the other");
        // Kept, so the page can say a machine *was* trusted.
        assert_eq!(roster.daemons.len(), 2);
        // And revoking twice is not a second event.
        assert!(!roster.revoke_daemon("laptop"));
    }

    #[test]
    fn no_daemon_key_is_ever_serialized_in_the_clear() {
        // The rule every store here keeps. These are hashed, which is what made
        // them the safest secret to move off the environment.
        let mut roster = Roster::default();
        roster.set_daemon("laptop", &crate::auth::hash("the-real-key"), 1);
        let json = serde_json::to_string(&roster).unwrap();
        assert!(!json.contains("the-real-key"), "{json}");
        assert!(json.contains(&crate::auth::hash("the-real-key")));
    }

    #[test]
    fn an_empty_seed_still_marks_the_volume_administered() {
        // Otherwise the first boot *with* something configured would seed a
        // volume somebody had already administered — and "I removed the last
        // one and it came back" is the same bug with more steps.
        let mut roster = Roster::default();
        assert!(roster.seed(&[], &[], &[], 1));
        assert!(roster.seeded);

        let configured = vec![("jamez667".to_string(), vec!["intake".to_string()])];
        assert!(
            !roster.seed(&configured, &["intake".to_string()], &[], 2),
            "already administered"
        );
        assert!(roster.owner_for("jamez667").is_none());
        assert!(roster.enabled().is_empty());
    }

    #[test]
    fn a_disabled_repository_is_kept_and_collects_nothing() {
        let mut roster = Roster::default();
        roster.enable("intake", Some("laptop".into()), 1);
        roster.enable("other", None, 1);
        assert_eq!(roster.enabled(), ["intake", "other"]);

        assert!(roster.disable("intake"));
        assert_eq!(roster.enabled(), ["other"]);
        // Kept, so the page can say a name *was* collected for.
        assert_eq!(roster.repos.len(), 2);
        // And disabling twice is not a second event.
        assert!(!roster.disable("intake"));

        // Re-enabling is the same call, and records who confirmed it this time.
        roster.enable("intake", None, 2);
        assert_eq!(roster.enabled(), ["other", "intake"]);
        assert_eq!(roster.repos.len(), 2, "replaced, not appended");
    }

    #[test]
    fn how_a_repository_was_enabled_survives_the_round_trip() {
        // "A machine said it serves this" and "I asserted it" are different
        // claims, and a page that could not tell them apart could not explain
        // why nothing is being drafted.
        let mut roster = Roster::default();
        roster.enable("confirmed", Some("laptop".into()), 1);
        roster.enable("asserted", None, 1);

        let json = serde_json::to_string(&roster).unwrap();
        let back: Roster = serde_json::from_str(&json).unwrap();
        assert_eq!(back.repos[0].served_by.as_deref(), Some("laptop"));
        assert_eq!(back.repos[1].served_by, None);
    }

    #[test]
    fn a_roster_written_before_repositories_existed_still_loads() {
        // Commit 4 wrote owners with no `repos` key at all. A volume that
        // upgrades must not fail to parse — that is the one file the developer
        // cannot lose.
        let old = r#"{"owners":[{"login":"jamez667","repos":["intake"],
                      "added_ms":1,"revoked":false}],"seeded":true}"#;
        let roster: Roster = serde_json::from_str(old).unwrap();
        assert!(roster.owner_for("jamez667").is_some());
        assert!(roster.enabled().is_empty());
        assert!(roster.seeded, "and it is not seeded a second time");
    }

    #[test]
    fn nothing_that_grants_access_is_serialized() {
        // The roster names people and repositories. It holds no credential, so a
        // copy of the volume grants nobody anything — the same rule the account
        // and credential stores keep.
        let mut roster = Roster::default();
        roster.set_owner("jamez667", &["alpha".into()], 1);
        let json = serde_json::to_string(&roster).unwrap();
        for hazard in ["token", "secret", "key", "hash"] {
            assert!(!json.contains(hazard), "{hazard} in {json}");
        }
    }
}
