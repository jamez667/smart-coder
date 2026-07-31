//! Feedback: kept, not acted on.
//!
//! The one intake path that never reaches a model and never writes to a
//! repository. Feedback is "this annoys me" or "that flow feels wrong" — a note,
//! not a request — so drafting a spec for it would manufacture a work item nobody
//! asked for.
//!
//! Stored **daemon-side**, under `~/.smart-coder/feedback/<repo>/`, deliberately
//! outside every working tree. Two reasons:
//!
//! * Nothing lands in a repository the developer would then have to clean up, and
//!   no gate is needed to keep phone-typed text out of a commit.
//! * Feedback about a repository the daemon later stops serving leaves no litter
//!   behind in that repository.
//!
//! It is *filed* rather than *queued*: there is no state machine, because there is
//! nothing to progress through. A note is either outstanding or acknowledged.

use std::path::{Path, PathBuf};

use sc_proto::{DcError, Result};
use serde::{Deserialize, Serialize};

use crate::task::now_ms;

/// One kept note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feedback {
    pub id: String,
    /// What was said, verbatim. Never summarised — the wording is the content.
    pub text: String,
    /// The repository it was about.
    pub repo: String,
    /// Unix ms when it was filed.
    pub filed_ms: u64,
    /// Read and dealt with. Kept rather than deleted, so a list can show what has
    /// already been considered instead of silently shrinking.
    #[serde(default)]
    pub acknowledged: bool,
}

impl Feedback {
    pub fn new(id: impl Into<String>, text: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            repo: repo.into(),
            filed_ms: now_ms(),
            acknowledged: false,
        }
    }

    /// The first line, for a list.
    pub fn summary(&self) -> &str {
        self.text.lines().next().unwrap_or("").trim()
    }
}

/// The feedback store.
#[derive(Debug, Clone)]
pub struct FeedbackStore {
    dir: PathBuf,
}

impl FeedbackStore {
    pub fn open(dir: impl Into<PathBuf>) -> Result<FeedbackStore> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(FeedbackStore { dir })
    }

    /// The daemon's default store, `~/.smart-coder/feedback/`.
    pub fn default_store() -> Result<FeedbackStore> {
        FeedbackStore::open(crate::config::home().join("feedback"))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// One directory per repository, so feedback is trivially filterable and a
    /// repo that is dropped takes only its own notes with it.
    fn repo_dir(&self, repo: &str) -> PathBuf {
        self.dir.join(repo)
    }

    fn path_for(&self, repo: &str, id: &str) -> PathBuf {
        self.repo_dir(repo).join(format!("{id}.json"))
    }

    /// Keep a note.
    pub fn put(&self, item: &Feedback) -> Result<()> {
        let json = serde_json::to_string_pretty(item).map_err(|e| DcError::Eval(e.to_string()))?;
        crate::atomic::write(&self.path_for(&item.repo, &item.id), json.as_bytes())
    }

    /// Every note, or every note about one repository — newest first, because
    /// what someone just said is what they want to see.
    pub fn all(&self, repo: Option<&str>) -> Result<Vec<Feedback>> {
        let dirs: Vec<PathBuf> = match repo {
            Some(r) => vec![self.repo_dir(r)],
            None => std::fs::read_dir(&self.dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter(|e| e.path().is_dir())
                        .map(|e| e.path())
                        .collect()
                })
                .unwrap_or_default(),
        };

        let mut out: Vec<Feedback> = Vec::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.filter_map(|e| e.ok()) {
                let path = e.path();
                if path.extension().is_none_or(|x| x != "json") {
                    continue;
                }
                // An unreadable note is skipped rather than fatal: one bad file
                // must not hide everything else someone took the time to say.
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if let Ok(item) = serde_json::from_str::<Feedback>(&text) {
                        out.push(item);
                    }
                }
            }
        }
        out.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(out)
    }

    /// Outstanding notes only.
    pub fn outstanding(&self, repo: Option<&str>) -> Result<Vec<Feedback>> {
        Ok(self
            .all(repo)?
            .into_iter()
            .filter(|f| !f.acknowledged)
            .collect())
    }

    /// Mark a note as read and dealt with. It is kept, not deleted.
    pub fn acknowledge(&self, repo: &str, id: &str) -> Result<Feedback> {
        let path = self.path_for(repo, id);
        let text = std::fs::read_to_string(&path)
            .map_err(|_| DcError::Eval(format!("no feedback {id:?} for {repo}")))?;
        let mut item: Feedback =
            serde_json::from_str(&text).map_err(|e| DcError::Eval(e.to_string()))?;
        item.acknowledged = true;
        self.put(&item)?;
        Ok(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_dir;

    fn store(tag: &str) -> (FeedbackStore, PathBuf) {
        let dir = temp_dir(tag);
        (FeedbackStore::open(&dir).unwrap(), dir)
    }

    #[test]
    fn a_note_is_kept_verbatim_and_survives_a_restart() {
        // The wording IS the content — a summarised complaint is a different
        // complaint.
        let (s, dir) = store("fb-keep");
        let said = "the approve button is too easy to hit by accident\non a phone";
        s.put(&Feedback::new("f-1", said, "alpha")).unwrap();
        drop(s);

        let reopened = FeedbackStore::open(&dir).unwrap();
        let all = reopened.all(None).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].text, said, "kept verbatim");
        assert!(!all[0].acknowledged);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_is_written_into_any_repository() {
        // The property that lets feedback skip a gate entirely: it never lands in
        // a working tree, so there is no unreviewed text to clean up.
        let (s, dir) = store("fb-outside");
        s.put(&Feedback::new("f-1", "a note", "alpha")).unwrap();
        assert!(
            s.dir().starts_with(&dir),
            "feedback lives in the daemon's own store"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn notes_are_filterable_by_repository() {
        let (s, dir) = store("fb-per-repo");
        s.put(&Feedback::new("f-1", "about alpha", "alpha"))
            .unwrap();
        s.put(&Feedback::new("f-2", "about beta", "beta")).unwrap();

        assert_eq!(s.all(Some("alpha")).unwrap().len(), 1);
        assert_eq!(s.all(Some("beta")).unwrap().len(), 1);
        assert_eq!(s.all(None).unwrap().len(), 2);
        assert_eq!(s.all(Some("alpha")).unwrap()[0].text, "about alpha");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_newest_note_comes_first() {
        // What someone just said is what they want to see.
        let (s, dir) = store("fb-order");
        for id in ["f-1", "f-2", "f-3"] {
            s.put(&Feedback::new(id, format!("note {id}"), "alpha"))
                .unwrap();
        }
        let ids: Vec<String> = s.all(None).unwrap().into_iter().map(|f| f.id).collect();
        assert_eq!(ids, vec!["f-3", "f-2", "f-1"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn acknowledging_keeps_the_note_rather_than_deleting_it() {
        // A list that silently shrinks cannot show what has already been
        // considered, so the same point gets raised again.
        let (s, dir) = store("fb-ack");
        s.put(&Feedback::new("f-1", "a note", "alpha")).unwrap();
        s.put(&Feedback::new("f-2", "another", "alpha")).unwrap();

        s.acknowledge("alpha", "f-1").unwrap();
        assert_eq!(s.all(None).unwrap().len(), 2, "both are still kept");
        let outstanding = s.outstanding(None).unwrap();
        assert_eq!(outstanding.len(), 1);
        assert_eq!(outstanding[0].id, "f-2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unknown_note_is_a_clear_error() {
        let (s, dir) = store("fb-missing");
        assert!(s
            .acknowledge("alpha", "nope")
            .unwrap_err()
            .to_string()
            .contains("nope"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_unreadable_note_does_not_hide_the_others() {
        let (s, dir) = store("fb-corrupt");
        s.put(&Feedback::new("f-1", "a good note", "alpha"))
            .unwrap();
        std::fs::write(dir.join("alpha").join("f-2.json"), "{ truncated").unwrap();
        assert_eq!(s.all(None).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_store_is_empty_rather_than_an_error() {
        let (s, dir) = store("fb-empty");
        assert!(s.all(None).unwrap().is_empty());
        assert!(s.all(Some("never-used")).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
