//! The pre-write tripwire for an edit that SWAPS one function for another instead of
//! adding the new one alongside it.
//!
//! Sibling of [`crate::builtin::guards`] and written to the same rules: cheap, approximate,
//! and a regression check rather than a correctness proof — it compares only the two strings
//! the model sent, so a function missing from the file for any other reason is never blamed
//! on this edit.

/// Every `fn` name declared anywhere in `src`, in first-seen order.
///
/// Unlike [`crate::builtin::guards::top_level_defs`], INDENTED lines are the point: the target
/// case is a method inside an `impl` block, which that scanner skips by construction (it
/// `continue`s on any leading whitespace). We therefore trim every line first and take `fn` at
/// any depth. Visibility/`async`/`unsafe`/`default`/`const` prefixes are stripped the same way,
/// so `pub async unsafe fn foo` keys on `foo`. Both a body (`fn name(..) {`) and a bare trait
/// signature (`fn name(..);`) are matched — the name simply runs up to the `(`.
///
/// Names repeat in the output when a name is declared more than once (the three `impl` blocks of
/// one trait each declare `fn len`); callers that only ask "is this name present" don't care.
fn fn_names(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in src.lines() {
        let mut t = line.trim();
        // Strip leading visibility / modifiers, repeatedly — `pub(crate) async fn` needs two.
        loop {
            let before = t;
            for kw in [
                "pub(crate)",
                "pub",
                "async",
                "unsafe",
                "default",
                "const",
                "extern \"C\"",
            ] {
                if let Some(rest) = t.strip_prefix(kw) {
                    if rest.starts_with([' ', '\t']) || rest.is_empty() {
                        t = rest.trim_start();
                    }
                }
            }
            if t == before {
                break;
            }
        }
        let Some(rest) = t.strip_prefix("fn") else {
            continue;
        };
        if !rest.starts_with([' ', '\t']) {
            continue; // `fnord(..)` is not a declaration
        }
        // The name runs up to the first `(` or `<` (a generic fn) or whitespace.
        if let Some(name) = rest
            .trim_start()
            .split(|c: char| c == '(' || c == '<' || c.is_whitespace())
            .next()
            .filter(|s| !s.is_empty())
        {
            out.push(name.to_string());
        }
    }
    out
}

/// The non-declaration remainder of `src`: every line that does NOT declare a `fn`, trimmed and
/// rejoined. Used only by the pure-rename carve-out, to ask whether everything AROUND the single
/// signature survived the edit untouched.
fn non_fn_remainder(src: &str) -> String {
    src.lines()
        .filter(|l| fn_names(l).is_empty())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// If this edit DELETES a function — a `fn` declared in `old_str` that is absent from `new_str` —
/// return a message naming it. `None` for every edit that keeps (or adds to) what it replaced.
///
/// THE BUG THIS EXISTS FOR. Forensics on the `rust-trait-impl` rung. The task: "Add
/// `evict(&mut self, key: &str) -> bool` to the trait and implement it for all three stores. Leave
/// the existing methods behaving exactly as they do now." The model instead REPLACED `fn len` with
/// `fn evict` four times — once in the trait, once in each of `impl Store for MemStore` /
/// `RingStore` / `PrefixStore`. One of the four edits verbatim:
///
/// ```text
/// old_str: "    /// How many keys are live.\n    fn len(&self) -> usize;"
/// new_str: "    /// Remove the key, returning whether it was present.\n    fn evict(&mut self, key: &str) -> bool;"
/// ```
///
/// The three `evict` bodies were all CORRECT and all genuinely different (a `HashMap::remove`, a
/// `retain`, a delegation through `self.full(key)`). The model understood the task; it just used
/// "replace" where it meant "add". Result: 11 errors of the form `no method named len found for
/// struct MemStore`, and a failed run.
///
/// No existing guard can see this. [`crate::builtin::guards::destructive_replacement`] returns
/// `None` the moment `new_str` holds an alphanumeric, and a fn-for-fn swap is letters throughout.
/// [`crate::builtin::guards::duplicate_definition`] fires only when a definition count goes UP;
/// here it went down. `delimiter_regression` sees braces balanced in and balanced out.
/// [`crate::builtin::guards::top_level_defs`] skips indented lines by construction, so a method
/// inside an `impl` block is invisible to it.
///
/// The predicate — ALL of these must hold:
/// 1. `new_str` is non-empty after trimming. An EMPTY `new_str` is a deletion, a real operation,
///    and the model has said plainly that it wants the text gone.
/// 2. `old_str` declares at least one `fn`. Editing a statement, a comment or a struct field is
///    none of this guard's business.
/// 3. Some `fn` name declared in `old_str` is declared nowhere in `new_str`.
/// 4. It is not a [pure rename](#renames) (below).
///
/// # Renames
///
/// A rename genuinely drops a name, so condition 3 cannot tell one from the `rust-trait-impl`
/// defect on names alone — and the four real edits above ARE, on their face, "rename `len` to
/// `evict`". The distinction we can draw cheaply and safely is what happened to everything
/// AROUND the signature. A true rename leaves it byte-identical; the live defect rewrote the doc
/// comment above the signature to describe the new method, and rewrote the body below it. So the
/// carve-out is deliberately tight: exempt only when `old_str` and `new_str` declare exactly ONE
/// `fn` each and their [non-declaration remainder](non_fn_remainder) is IDENTICAL. Every one of
/// the four transcript edits fails that test (each swapped the doc comment too) and still fires.
///
/// Anything looser — exempting any 1-fn-for-1-fn swap — would exempt the exact bug this guard
/// exists for, so the tightness is the whole point rather than a rough edge.
pub fn dropped_definition(old_str: &str, new_str: &str) -> Option<String> {
    if new_str.trim().is_empty() {
        return None; // a deletion is a real operation
    }
    let old_fns = fn_names(old_str);
    if old_fns.is_empty() {
        return None; // this edit isn't about functions
    }
    let new_fns = fn_names(new_str);
    let dropped = old_fns.iter().find(|n| !new_fns.contains(n))?;

    // A pure rename: exactly one fn each way, and everything around the signature untouched.
    if old_fns.len() == 1
        && new_fns.len() == 1
        && non_fn_remainder(old_str) == non_fn_remainder(new_str)
    {
        return None;
    }

    let added = new_fns
        .iter()
        .find(|n| !old_fns.contains(n))
        .cloned()
        .unwrap_or_default();
    let advice = if added.is_empty() {
        format!(
            "If you meant to CHANGE `fn {dropped}`, keep its declaration line in new_str and \
             edit only the part you want different."
        )
    } else {
        format!(
            "If you meant to ADD `fn {added}`, include BOTH in new_str: paste `fn {dropped}` \
             unchanged, then your new `fn {added}` after it."
        )
    };
    Some(format!(
        "this edit DELETES `fn {dropped}` and does not put it back. Nothing was written. \
         new_str REPLACES old_str entirely — it is not appended to it, so every function you \
         leave out of new_str is removed from the file, and every caller of `fn {dropped}` \
         stops compiling. {advice} To DELETE `fn {dropped}` on purpose, send an empty new_str \
         (\"\")."
    ))
}

#[cfg(test)]
mod tests {
    use super::{dropped_definition, fn_names};

    /// The trait edit, verbatim from the `rust-trait-impl` transcript.
    const TRAIT_OLD: &str = "    /// How many keys are live.\n    fn len(&self) -> usize;";
    const TRAIT_NEW: &str = "    /// Remove the key, returning whether it was present.\n    fn evict(&mut self, key: &str) -> bool;";

    #[test]
    fn fires_on_the_trait_signature_swap() {
        let msg = dropped_definition(TRAIT_OLD, TRAIT_NEW).expect("trait `len` -> `evict` fires");
        assert!(msg.contains("`fn len`"), "names the dropped fn: {msg}");
        assert!(msg.contains("`fn evict`"), "names the added fn: {msg}");
        assert!(msg.contains("BOTH"), "tells the model to keep both: {msg}");
    }

    #[test]
    fn fires_on_each_impl_body_swap() {
        // MemStore: HashMap::remove.
        let mem_old = "    fn len(&self) -> usize {\n        self.map.len()\n    }";
        let mem_new = "    fn evict(&mut self, key: &str) -> bool {\n        self.map.remove(key).is_some()\n    }";
        assert!(dropped_definition(mem_old, mem_new).is_some(), "MemStore");

        // RingStore: retain.
        let ring_old = "    fn len(&self) -> usize {\n        self.buf.len()\n    }";
        let ring_new = "    fn evict(&mut self, key: &str) -> bool {\n        let n = self.buf.len();\n        self.buf.retain(|(k, _)| k != key);\n        self.buf.len() != n\n    }";
        assert!(
            dropped_definition(ring_old, ring_new).is_some(),
            "RingStore"
        );

        // PrefixStore: delegate through self.full(key).
        let pre_old = "    fn len(&self) -> usize {\n        self.inner.len()\n    }";
        let pre_new = "    fn evict(&mut self, key: &str) -> bool {\n        let k = self.full(key);\n        self.inner.evict(&k)\n    }";
        assert!(
            dropped_definition(pre_old, pre_new).is_some(),
            "PrefixStore"
        );
    }

    #[test]
    fn a_pure_add_does_not_fire() {
        let old = "    fn len(&self) -> usize {\n        self.map.len()\n    }";
        let new = format!(
            "{old}\n\n    fn evict(&mut self, key: &str) -> bool {{\n        self.map.remove(key).is_some()\n    }}"
        );
        assert!(
            dropped_definition(old, &new).is_none(),
            "keeping `len` and adding `evict` is the correct edit"
        );
    }

    #[test]
    fn an_empty_new_str_is_a_real_deletion() {
        assert!(dropped_definition(TRAIT_OLD, "").is_none());
        assert!(dropped_definition(TRAIT_OLD, "   \n\t ").is_none());
    }

    #[test]
    fn no_fn_in_old_str_is_not_our_business() {
        assert!(dropped_definition("        here = here.max(v);", "        here = v;").is_none());
        assert!(dropped_definition("struct S { a: u8 }", "struct S { b: u8 }").is_none());
        assert!(dropped_definition("", "fn a() {}").is_none());
    }

    #[test]
    fn a_pure_rename_is_exempt_but_a_rewritten_neighbour_is_not() {
        // Body and surroundings byte-identical — a real rename, let it through.
        let old = "    fn len(&self) -> usize {\n        self.map.len()\n    }";
        let new = "    fn size(&self) -> usize {\n        self.map.len()\n    }";
        assert!(dropped_definition(old, new).is_none(), "pure rename exempt");

        // The live defect looks like a rename but rewrites the doc comment too — must still fire.
        assert!(
            dropped_definition(TRAIT_OLD, TRAIT_NEW).is_some(),
            "the transcript edit must NOT be exempted as a rename"
        );
        // Same shape with a rewritten body.
        let body_old = "    fn len(&self) -> usize {\n        self.map.len()\n    }";
        let body_new = "    fn evict(&mut self, key: &str) -> bool {\n        self.map.remove(key).is_some()\n    }";
        assert!(dropped_definition(body_old, body_new).is_some());
    }

    #[test]
    fn dropping_one_of_several_fns_fires() {
        let old = "    fn a(&self) {}\n    fn b(&self) {}\n    fn c(&self) {}";
        let new = "    fn a(&self) {}\n    fn c(&self) {}";
        let msg = dropped_definition(old, new).expect("`b` was dropped");
        assert!(msg.contains("`fn b`"), "{msg}");
        // Nothing was added, so the advice is the CHANGE wording, not the ADD wording.
        assert!(msg.contains("CHANGE"), "{msg}");
    }

    #[test]
    fn fn_names_sees_indented_and_prefixed_declarations() {
        let src = "\
pub fn top() {}
    fn method(&self) {}
        pub(crate) async fn deep(&self) {}
    pub async unsafe fn both() {}
    const fn konst() -> u8 { 0 }
    fn generic<T: Copy>(t: T) -> T { t }
    fn sig(&self) -> usize;
";
        let names = fn_names(src);
        for want in ["top", "method", "deep", "both", "konst", "generic", "sig"] {
            assert!(
                names.iter().any(|n| n == want),
                "missing {want} in {names:?}"
            );
        }
    }

    #[test]
    fn fn_names_ignores_non_declarations() {
        // A call, a word starting with `fn`, and a string mentioning fn are not declarations.
        assert!(fn_names("    self.len();").is_empty());
        assert!(fn_names("    fnord(3);").is_empty());
        assert!(fn_names("    let s = \"len\";").is_empty());
    }
}
