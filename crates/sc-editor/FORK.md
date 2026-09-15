# sc-editor — a fork of `iced-code-editor` 0.3.11

Upstream: <https://github.com/LuDog71FR/iced-code-editor> (MIT, by LuDog71).
Forked at **0.3.11**, the version this project had pinned exactly.

## Why a fork and not a dependency

The editor was a pinned dependency, and the pin comment said why: pre-1.0,
single-maintainer, so "a patch release must never arrive unreviewed".

It became a fork because the plugin API needs to apply an edit to an open buffer
(spec 25), and 0.3.11 has no way to do that. Everything needed is public —
`TextBuffer` with `replace_range`, `CommandHistory` with `push` and
`begin_group`/`end_group` — but `CodeEditor`'s own `buffer` and `history` fields
are `pub(crate)`, so nothing joins them. `content()` reads and nothing writes.

The alternatives were worse. Driving the widget with synthetic `Paste` messages
would replace the whole buffer on every plugin edit, losing cursor position and
making each edit a whole-file change. Rebuilding the `CodeEditor` from new text
would discard the undo stack, and spec 25 committed to plugin edits being
undoable — "a plugin edit the user cannot Ctrl+Z is a plugin edit the user will
not forgive".

The fork is also forward-looking: decorations and inline hints are deferred
parts of the plugin API (spec 25 Tier 3), and both need changes here.

## What we changed

Everything in this list is a deliberate divergence. **Keep it current** — it is
the diff someone has to re-apply when pulling upstream changes.

- `Message::ApplyEdit` and its handler — apply a range edit from outside the
  widget, through `CommandHistory` so it joins the undo stack as one entry.
- `test_measure_single_char_width` — upstream asserted that `汉` measures wider
  than `a` (and at least 1.5× as wide), which is a claim about the host's
  installed fonts rather than about this crate. It passes on a desktop and fails
  in a bare `rust:bookworm` CI container, where the glyph has no coverage and
  falls back to a notdef box. Now asserts what the measurement code owns: every
  glyph measures positive, and more characters measure wider. **Do not restore
  upstream's version on a merge** — it will go red in CI again.

## Pulling upstream changes

There is no automatic merge. Diff the new upstream release against `0.3.11`,
apply what matters by hand, and re-apply the list above. Upstream's own tests
came with the fork and are the check that a merge did not break something.
