//! Editable-buffer rules: what may be opened for editing, and when a save is safe.
//!
//! Pure and host-testable — no iced, no `App`, no widget. Everything here is the part of the
//! editor that can *lose your work*, kept where it can be tested without a GUI.
//!
//! The read-only code view ([`crate::codeview`]) can be relaxed about its input: it truncates
//! long files, replaces invalid UTF-8 with U+FFFD, and normalises line endings, because none of
//! that ever goes back to disk. **The moment a save path exists, all three become silent
//! corruption** — so the edit path uses this module instead and refuses what it cannot round-trip
//! faithfully.
//!
//! The refusals are deliberate. A file that opens read-only with a stated reason is a small
//! annoyance; a file that opens, looks fine, and writes back mangled bytes is data loss the user
//! discovers later in a diff. Spec 21.

/// The line ending a file uses, detected on load so a save can restore it.
///
/// Windows-first client, git repo: silently rewriting every line ending turns a one-line edit
/// into a whole-file diff. (Note the editor widget tracks endings per line, so this is mostly an
/// assertion — it matters for a buffer that went *mixed* mid-edit.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ending {
    /// `\n` — POSIX.
    #[default]
    Lf,
    /// `\r\n` — Windows.
    CrLf,
}

impl Ending {
    /// The bytes this ending writes.
    pub fn as_str(self) -> &'static str {
        match self {
            Ending::Lf => "\n",
            Ending::CrLf => "\r\n",
        }
    }
    /// Short label for the editor status line.
    pub fn label(self) -> &'static str {
        match self {
            Ending::Lf => "LF",
            Ending::CrLf => "CRLF",
        }
    }
}

/// Why a file cannot be edited. Each is shown to the user in place of the caret, so the reason
/// is always visible rather than the pane merely refusing to take keystrokes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoEdit {
    /// Too big to lay out responsively. Editing is refused rather than risking a buffer we
    /// cannot hold whole — a partially-loaded file that saves would delete the rest.
    TooLarge { lines: usize },
    /// Not valid UTF-8. Opening it would mean lossy decoding, and saving would write U+FFFD
    /// over the user's bytes.
    NotUtf8,
    /// Contains NUL — a binary file. Nothing sensible to show, nothing safe to write.
    Binary,
    /// Could not be read at all (missing, permissions).
    Missing,
}

impl NoEdit {
    /// The sentence shown where the editor would be. Always says *why*, never just "cannot edit".
    pub fn reason(&self) -> String {
        match self {
            NoEdit::TooLarge { lines } => format!(
                "{lines} lines — too large to edit here. Showing read-only so nothing is lost."
            ),
            NoEdit::NotUtf8 => {
                "Not valid UTF-8 — read-only, so saving can't corrupt the original bytes."
                    .to_string()
            }
            NoEdit::Binary => "Binary file — not editable.".to_string(),
            NoEdit::Missing => "File could not be read.".to_string(),
        }
    }
}

/// The ceiling for the *edit* view, in lines.
///
/// Deliberately far above [`crate::codeview::MAX_LINES`] (which caps the read-only view at 4000):
/// truncating a view is fine, truncating a buffer that can be saved is not, so this limit exists
/// only to keep layout responsive — not to bound memory. A file past it opens read-only.
pub const MAX_EDIT_LINES: usize = 20_000;

/// The ceiling for the edit view, in bytes. A file can be under the line limit and still be
/// enormous (one very long line), so both apply.
pub const MAX_EDIT_BYTES: usize = 2 * 1024 * 1024;

/// The outcome of inspecting a file's bytes: editable text, or a stated reason it isn't.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classified {
    /// Safe to edit. `text` is the whole file, losslessly decoded; `ending` is what to write back.
    Editable {
        text: String,
        ending: Ending,
        /// The file began with a UTF-8 BOM, stripped from `text` and restored on save. Windows
        /// tooling writes these, and dropping one is a spurious whole-file diff.
        bom: bool,
    },
    /// Not safe to edit, with the reason to show.
    Refused(NoEdit),
}

/// The UTF-8 byte-order mark.
const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// Decide whether `bytes` can be edited, and decode them if so.
///
/// Order matters: binary before UTF-8 (a NUL is a clearer explanation than "invalid UTF-8"), and
/// size last so a small binary still reports as binary. Uses strict [`std::str::from_utf8`] —
/// never `from_utf8_lossy`, which is exactly the silent corruption this module exists to prevent.
pub fn classify(bytes: &[u8]) -> Classified {
    if bytes.contains(&0) {
        return Classified::Refused(NoEdit::Binary);
    }
    let bom = bytes.starts_with(BOM);
    let body = if bom { &bytes[BOM.len()..] } else { bytes };

    let Ok(text) = std::str::from_utf8(body) else {
        return Classified::Refused(NoEdit::NotUtf8);
    };
    if body.len() > MAX_EDIT_BYTES {
        return Classified::Refused(NoEdit::TooLarge {
            lines: text.lines().count(),
        });
    }
    let lines = text.lines().count();
    if lines > MAX_EDIT_LINES {
        return Classified::Refused(NoEdit::TooLarge { lines });
    }
    Classified::Editable {
        ending: dominant_ending(text),
        text: text.to_string(),
        bom,
    }
}

/// The line ending most of `text` uses.
///
/// Counts CRLF against *bare* LF (an LF that isn't part of a CRLF). Ties and empty input go to
/// [`Ending::Lf`] — a file with no newline at all has no ending to preserve, so the platform
/// default is as good an answer as any.
pub fn dominant_ending(text: &str) -> Ending {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf; // total LF minus those inside a CRLF
    if crlf > lf {
        Ending::CrLf
    } else {
        Ending::Lf
    }
}

/// Whether `text` mixes line endings — the only case where a save has to rewrite them.
///
/// A buffer that stays consistent is written back byte-for-byte; one that went mixed during
/// editing gets normalised to the file's original ending rather than committing a diff that
/// touches lines the user never looked at.
pub fn is_mixed(text: &str) -> bool {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    crlf > 0 && lf > 0
}

/// Rewrite every line ending in `text` to `ending`, preserving whether the file ends with a
/// trailing newline.
pub fn normalize_to(text: &str, ending: Ending) -> String {
    let trailing = text.ends_with('\n');
    let mut out = text.lines().collect::<Vec<_>>().join(ending.as_str());
    if trailing {
        out.push_str(ending.as_str());
    }
    out
}

/// Restore what [`classify`] stripped: the BOM, if the file had one.
pub fn to_bytes(text: &str, bom: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + if bom { BOM.len() } else { 0 });
    if bom {
        out.extend_from_slice(BOM);
    }
    out.extend_from_slice(text.as_bytes());
    out
}

/// A cheap fingerprint of the file on disk, taken when it was opened and after each save.
///
/// Modified-time plus length, deliberately: hashing the file on every Ctrl+S would be a
/// synchronous read on the UI thread, and this pair catches every realistic case — above all the
/// agent writing a file you have open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiskStamp {
    /// `None` when the platform/filesystem didn't supply one; two `None`s compare equal, so
    /// detection falls back to length alone rather than crying conflict on every save.
    pub mtime: Option<std::time::SystemTime>,
    pub len: u64,
}

impl DiskStamp {
    /// Stamp a file from its metadata.
    pub fn of(meta: &std::fs::Metadata) -> Self {
        Self {
            mtime: meta.modified().ok(),
            len: meta.len(),
        }
    }
    /// Stamp the path, or `None` if it can't be read (deleted, permissions).
    pub fn read(path: &std::path::Path) -> Option<Self> {
        std::fs::metadata(path).ok().map(|m| Self::of(&m))
    }
}

/// What a save should do, given the stamp at open and the stamp now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveVerdict {
    /// Nothing changed underneath — write the buffer.
    Write,
    /// The file changed on disk but the buffer is clean, so there is nothing to lose: reload.
    ReloadClean,
    /// The file changed on disk AND the buffer has unsaved edits. **Refuse.**
    Conflict,
}

/// Decide whether it is safe to write.
///
/// The rule follows the precedent already set by [`crate::linecomment::locate_range`], which
/// re-anchors by content and refuses when ambiguous rather than guessing: when two versions
/// disagree and both hold real work, the editor does not pick a winner. Clobbering the agent's
/// write is as much data loss as the agent clobbering yours.
///
/// A file that vanished (`current` is `None`) counts as changed — writing it back would
/// resurrect something the user may have deleted deliberately.
pub fn verdict(opened: &DiskStamp, current: Option<&DiskStamp>, dirty: bool) -> SaveVerdict {
    let unchanged = current.is_some_and(|c| c == opened);
    match (unchanged, dirty) {
        (true, _) => SaveVerdict::Write,
        (false, true) => SaveVerdict::Conflict,
        (false, false) => SaveVerdict::ReloadClean,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editable(c: Classified) -> (String, Ending, bool) {
        match c {
            Classified::Editable { text, ending, bom } => (text, ending, bom),
            Classified::Refused(r) => panic!("expected editable, got {r:?}"),
        }
    }

    #[test]
    fn crlf_and_lf_files_round_trip_byte_for_byte() {
        // The headline guarantee: opening and saving without typing must not change one byte.
        // On Windows, in a git repo, a silent CRLF→LF rewrite shows up as EVERY line changed.
        for original in ["a\r\nb\r\nc\r\n", "a\nb\nc\n", "no trailing newline"] {
            let (text, ending, bom) = editable(classify(original.as_bytes()));
            // Untouched buffer ⇒ not mixed ⇒ written back verbatim.
            assert!(!is_mixed(&text) || original.contains("\r\n"));
            let written = String::from_utf8(to_bytes(&text, bom)).unwrap();
            assert_eq!(written, original, "round trip changed the bytes");
            let _ = ending;
        }
    }

    #[test]
    fn dominant_ending_picks_the_majority() {
        assert_eq!(dominant_ending("a\r\nb\r\n"), Ending::CrLf);
        assert_eq!(dominant_ending("a\nb\n"), Ending::Lf);
        // Mixed, CRLF in the majority.
        assert_eq!(dominant_ending("a\r\nb\r\nc\n"), Ending::CrLf);
        // No newline at all ⇒ nothing to preserve ⇒ the default.
        assert_eq!(dominant_ending("one line"), Ending::Lf);
        assert_eq!(dominant_ending(""), Ending::Lf);
    }

    #[test]
    fn only_a_mixed_buffer_gets_normalised() {
        assert!(!is_mixed("a\r\nb\r\n"), "consistent CRLF is not mixed");
        assert!(!is_mixed("a\nb\n"), "consistent LF is not mixed");
        assert!(is_mixed("a\r\nb\n"), "one of each IS mixed");
        // Normalising a mixed buffer settles on the file's original ending.
        assert_eq!(normalize_to("a\r\nb\n", Ending::CrLf), "a\r\nb\r\n");
        assert_eq!(normalize_to("a\r\nb\n", Ending::Lf), "a\nb\n");
        // A file with no trailing newline keeps not having one.
        assert_eq!(normalize_to("a\r\nb", Ending::Lf), "a\nb");
    }

    #[test]
    fn invalid_utf8_is_refused_not_mangled() {
        // `from_utf8_lossy` would turn this into U+FFFD and a save would write that REPLACEMENT
        // over the user's bytes. Refusing is the whole point.
        let bytes = b"valid \xFF\xFE invalid";
        assert_eq!(classify(bytes), Classified::Refused(NoEdit::NotUtf8));
    }

    #[test]
    fn binary_is_refused_before_anything_else() {
        // A NUL is a clearer explanation than "invalid UTF-8", so it's checked first — even
        // though such a file would usually fail the UTF-8 check too.
        assert_eq!(
            classify(b"\x7fELF\0\0\0"),
            Classified::Refused(NoEdit::Binary)
        );
    }

    #[test]
    fn a_bom_survives_the_round_trip() {
        // Windows tooling writes BOMs. Dropping one is a spurious whole-file diff.
        let src = b"\xEF\xBB\xBFfn main() {}\n";
        let (text, _, bom) = editable(classify(src));
        assert!(bom, "BOM detected");
        assert!(text.starts_with("fn main"), "and stripped from the text");
        assert_eq!(to_bytes(&text, bom), src, "and restored on write");
    }

    #[test]
    fn oversized_files_are_refused_rather_than_truncated() {
        // THE data-loss case. `codeview` truncates at 4000 lines for display, which is fine
        // because it never writes back. Saving a truncated buffer would delete everything past
        // the cut — so the edit path refuses instead.
        let huge = "x\n".repeat(MAX_EDIT_LINES + 1);
        match classify(huge.as_bytes()) {
            Classified::Refused(NoEdit::TooLarge { lines }) => {
                assert!(lines > MAX_EDIT_LINES, "reports the real size: {lines}");
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
        // A very long single line trips the byte ceiling instead.
        let wide = "x".repeat(MAX_EDIT_BYTES + 1);
        assert!(matches!(
            classify(wide.as_bytes()),
            Classified::Refused(NoEdit::TooLarge { .. })
        ));
        // And a normal file is still fine.
        assert!(matches!(
            classify(b"fn main() {}\n"),
            Classified::Editable { .. }
        ));
    }

    #[test]
    fn a_dirty_buffer_over_a_changed_file_refuses_to_save() {
        let opened = DiskStamp {
            mtime: Some(std::time::UNIX_EPOCH),
            len: 100,
        };
        let same = opened;
        let changed = DiskStamp {
            mtime: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(60)),
            len: 120,
        };

        // Nothing moved → write, dirty or not.
        assert_eq!(verdict(&opened, Some(&same), true), SaveVerdict::Write);
        assert_eq!(verdict(&opened, Some(&same), false), SaveVerdict::Write);
        // Changed underneath, nothing of ours to lose → take theirs.
        assert_eq!(
            verdict(&opened, Some(&changed), false),
            SaveVerdict::ReloadClean
        );
        // Changed underneath AND we have edits → refuse. Never pick a winner: clobbering the
        // agent's write is as much data loss as it clobbering ours.
        assert_eq!(
            verdict(&opened, Some(&changed), true),
            SaveVerdict::Conflict
        );
        // Deleted underneath while dirty → also a conflict, not a silent resurrection.
        assert_eq!(verdict(&opened, None, true), SaveVerdict::Conflict);
    }

    #[test]
    fn a_same_length_rewrite_is_still_caught_by_mtime() {
        // The subtle case: an agent rewrites a file to the SAME length. Length alone would miss
        // it; the mtime is what catches it.
        let opened = DiskStamp {
            mtime: Some(std::time::UNIX_EPOCH),
            len: 100,
        };
        let touched = DiskStamp {
            mtime: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1)),
            len: 100,
        };
        assert_eq!(
            verdict(&opened, Some(&touched), true),
            SaveVerdict::Conflict
        );
    }
}
