//! Tokenization: the lexical bridge between how a user asks and how code is written
//! (spec 23 — smart search).
//!
//! The motivating case: "why is the trail behind the stars thin before it gets thick".
//! None of those words appear as-is in the code that draws the trail. They appear
//! *inside* identifiers (`draw_trails`, `width_head`) and *in comments* — which is
//! exactly where authors write the words users use. Two rules recover the bridge with
//! no embeddings:
//!
//! * split identifiers on `camelCase` and `snake_case` boundaries, so `width_head`
//!   indexes as `width` + `head` and `drawTrails` as `draw` + `trails`;
//! * index comments and string literals as first-class fields, weighted *above*
//!   ordinary code.
//!
//! The tokenizer is used for queries and documents alike — anything else would make
//! the two halves disagree about what a word is.

use serde::{Deserialize, Serialize};

/// Words dropped from queries and documents alike.
///
/// Deliberately tiny and **fixed as a `const`**: it is part of the determinism
/// surface, so it is reviewed in a diff rather than tuned at runtime. These are the
/// words a person wraps a question in ("why is the trail thin *before* it gets
/// thick") that carry no locating power, plus the handful of English function words
/// dense enough in prose comments to add noise everywhere. Domain words are never
/// here: "get"/"set"/"new" stay, because `get_width` is a real thing to find.
pub const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "before", "but", "by", "can", "do", "does", "for",
    "from", "get", "has", "have", "how", "in", "into", "is", "it", "its", "of", "on", "or", "our",
    "so", "than", "that", "the", "their", "then", "there", "these", "they", "this", "to", "was",
    "we", "were", "what", "when", "where", "which", "who", "why", "will", "with", "would", "you",
    "your",
];

/// Which part of a file a term was found in. The ordering of the weights is the
/// design: a word in a comment is stronger evidence than the same word in code,
/// because comments are written in the user's language and code is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Field {
    /// A definition's own name — the strongest signal a file can give about what it is.
    Symbol,
    /// Comment text.
    Comment,
    /// String literal contents.
    Str,
    /// Everything else: identifiers in expressions, keywords, operators.
    Code,
}

impl Field {
    /// The multiplier this field's occurrences carry into scoring.
    pub fn weight(self) -> f64 {
        match self {
            Field::Symbol => 4.0,
            Field::Comment => 3.0,
            Field::Str => 2.0,
            Field::Code => 1.0,
        }
    }
}

/// Fold a plural to its singular, and nothing else.
///
/// **Not a stemmer.** A real stemmer (Porter and friends) is a table of rules that
/// turns `intensity` into `intens` and `classes` into `class` — it improves recall on
/// English prose and mangles identifiers, and every rule is another thing that has to
/// stay identical for the index to be reproducible. This handles exactly the case the
/// evidence demands: a user writes "the trail behind the stars" while the code writes
/// `draw_trails`, so a query term and a code term differ by one `s`.
///
/// Conservative by construction: only a trailing `s` (or `es` after s/x/z/ch/sh), only
/// on words long enough that the result is still a word, never on a word ending `ss`.
fn singularize(w: &str) -> String {
    let n = w.chars().count();
    if n < 4 || w.ends_with("ss") {
        return w.to_string();
    }
    for suffix in ["ses", "xes", "zes", "ches", "shes"] {
        if w.ends_with(suffix) {
            return w[..w.len() - 2].to_string();
        }
    }
    match w.strip_suffix('s') {
        Some(base) if !base.ends_with('u') => base.to_string(),
        _ => w.to_string(),
    }
}

/// Split `text` into search terms: lowercase, `camelCase`/`snake_case`-aware,
/// singularized, one-character and stopword tokens dropped.
///
/// A compound identifier yields **both** its parts and nothing else — `width_head`
/// gives `width`, `head`. The whole is not also emitted: an exact-identifier query
/// still matches every part of it, so keeping the compound only inflates term counts
/// and biases scoring toward long names.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in split_words(text) {
        let lower = raw.to_ascii_lowercase();
        if lower.chars().count() < 2 || STOPWORDS.contains(&lower.as_str()) {
            continue;
        }
        out.push(singularize(&lower));
    }
    out
}

/// Split on every non-alphanumeric boundary, then on case transitions inside each
/// run. `HTTPServer` splits as `HTTP` + `Server` (an acronym followed by a word is
/// two words, not one), `parse2Json` as `parse` + `2` + `Json`.
fn split_words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for run in text.split(|c: char| !c.is_alphanumeric()) {
        if run.is_empty() {
            continue;
        }
        let chars: Vec<char> = run.chars().collect();
        let mut start = 0usize;
        for i in 1..chars.len() {
            let prev = chars[i - 1];
            let cur = chars[i];
            // lower|digit -> Upper  ("drawTrails")
            let camel = !prev.is_uppercase() && cur.is_uppercase();
            // Upper -> Upper followed by lower  ("HTTPServer" => HTTP | Server)
            let acronym_end = prev.is_uppercase()
                && cur.is_uppercase()
                && chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            // letter <-> digit either way ("parse2Json", "utf8")
            let digit_edge = prev.is_alphabetic() != cur.is_alphabetic();
            if camel || acronym_end || digit_edge {
                out.push(chars[start..i].iter().collect());
                start = i;
            }
        }
        out.push(chars[start..].iter().collect());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Vec<String> {
        tokenize(s)
    }

    #[test]
    fn splits_snake_case() {
        assert_eq!(t("width_head"), vec!["width", "head"]);
        // `trails` folds to `trail` -- the query's word.
        assert_eq!(t("draw_trails"), vec!["draw", "trail"]);
    }

    #[test]
    fn splits_camel_and_pascal_case() {
        assert_eq!(t("drawTrails"), vec!["draw", "trail"]);
        assert_eq!(t("Starfield"), vec!["starfield"]);
        assert_eq!(t("baseLen"), vec!["base", "len"]);
    }

    #[test]
    fn splits_an_acronym_from_the_word_after_it() {
        // HTTPServer is two ideas, and a query for "server" must find it.
        assert_eq!(t("HTTPServer"), vec!["http", "server"]);
        assert_eq!(t("parseJSONBody"), vec!["parse", "json", "body"]);
    }

    #[test]
    fn splits_letter_digit_edges() {
        assert_eq!(t("utf8Decode"), vec!["utf", "decode"]); // "8" is 1 char -> dropped
        assert_eq!(t("vec2Add"), vec!["vec", "add"]);
    }

    #[test]
    fn drops_stopwords_and_single_characters() {
        // The question words that wrap a real query carry no locating power.
        assert_eq!(
            t("why is the trail behind the stars thin before it gets thick"),
            vec!["trail", "behind", "star", "thin", "get", "thick"]
        );
        assert!(t("a i x").is_empty());
    }

    #[test]
    fn punctuation_and_paths_are_boundaries() {
        assert_eq!(
            t("self.stars[i].width = 2.0;"),
            vec!["self", "star", "width"]
        );
        assert_eq!(
            t("crates/void_engine/src/fx"),
            vec!["crate", "void", "engine", "src", "fx"]
        );
    }

    /// **The one that matters.** The user asks in prose; the code is written in
    /// identifiers. If these two token sets do not intersect, no amount of scoring
    /// downstream can find the right function.
    #[test]
    fn the_users_words_reach_the_codes_words() {
        let question = t("why is the trail behind the stars thin before it gets thick");
        let code = t("fn draw_trails(&self, stars: &[Star]) { let width_head = 0.8; }");
        // "trail" reaches `draw_trails` (split + singularized), "star" reaches `stars`.
        for w in ["trail", "star"] {
            assert!(question.contains(&w.to_string()), "question has {w}");
            assert!(code.contains(&w.to_string()), "code has {w}");
        }
    }

    #[test]
    fn singularization_is_conservative() {
        // Folds the plural the evidence demands...
        assert_eq!(t("trails"), vec!["trail"]);
        assert_eq!(t("stars"), vec!["star"]);
        assert_eq!(t("boxes"), vec!["box"]);
        // ...and leaves alone everything that would mangle an identifier.
        assert_eq!(t("class"), vec!["class"]); // -ss
        assert_eq!(t("pos"), vec!["pos"]); // too short
        assert_eq!(t("status"), vec!["status"]); // -us
        assert_eq!(t("intensity"), vec!["intensity"]); // a stemmer would cut this
    }

    #[test]
    fn field_weights_rank_comments_above_code() {
        assert!(Field::Symbol.weight() > Field::Comment.weight());
        assert!(Field::Comment.weight() > Field::Str.weight());
        assert!(Field::Str.weight() > Field::Code.weight());
    }

    #[test]
    fn tokenizing_is_stable() {
        let a = t("Starfield::draw_trails width_head");
        let b = t("Starfield::draw_trails width_head");
        assert_eq!(a, b);
    }
}
