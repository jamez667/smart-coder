//! Reading a sampling profiler's output, and turning it into a flame graph.
//!
//! A flame graph answers one question — *where did the time go?* — from one input:
//! **folded stacks**. Every sampling profiler on every platform can emit them, and the
//! format is one line per unique stack:
//!
//! ```text
//! main;run_agent;model_call 58
//! main;run_agent;tool_dispatch 13
//! main;parse_config 12
//! ```
//!
//! A semicolon-separated call path, a space, and a sample count. That is the whole format.
//!
//! # Why the folded stack is the seam, not the profiler
//!
//! The obvious design is "run `cargo flamegraph`, show its SVG". It is wrong for the same
//! reason `sc-comply`'s model is optional: it welds the *viewer* to one *producer*.
//!
//! Nothing about drawing a flame graph needs a profiler to be installed. Folded stacks arrive
//! from `perf script | stackcollapse-perf.pl`, from `dtrace`, from `samply`, from a colleague's
//! bug report, from CI — and on Windows, where the sampling story is genuinely poor, an
//! imported file may be the *only* way a user ever sees one. Parsing the text is
//! therefore the load-bearing part, and it is pure: [`parse_folded`] takes a `&str` and returns
//! a tree. Running a profiler is one convenience layered on top (`sc_win::flame::tool`),
//! and it is allowed to be unavailable.
//!
//! That inversion is what makes this section useful on a machine with no profiler on it at all,
//! which — checked while building it — is this machine.
//!
//! # Everything here is pure
//!
//! No iced types, no file system, no process spawning. The tree, the layout rectangles, the
//! search and the zoom are all values in and values out, so the parts that can be *wrong* —
//! percentages, rectangle geometry, merge order — are tested without a GUI.

use std::collections::BTreeMap;

/// One node in the call tree: a frame, its total samples, and its callees.
///
/// "Total" is inclusive — a frame's own samples plus everything it called — because that is
/// what a flame graph's width means. Self time is [`Frame::self_samples`], derived rather than
/// stored so the two can never disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The function name, exactly as the profiler spelled it.
    pub name: String,
    /// Inclusive sample count: this frame and everything beneath it.
    pub total: u64,
    /// Callees, kept sorted by name so the same input always draws the same picture.
    ///
    /// Sorted by *name*, deliberately, not by weight: a flame graph is read by finding a
    /// function, and a frame that jumps to a different x-position between two runs of the
    /// same workload is much harder to compare. Ordering by name makes two profiles of the
    /// same program line up visually.
    pub children: Vec<Frame>,
}

impl Frame {
    /// Samples spent *in this frame itself*, not in anything it called.
    ///
    /// Saturating: a malformed input whose children outweigh their parent yields 0 rather than
    /// panicking on underflow. A profile is diagnostic data, never a reason to take the IDE down.
    pub fn self_samples(&self) -> u64 {
        self.total
            .saturating_sub(self.children.iter().map(|c| c.total).sum::<u64>())
    }

    /// Total number of frames in this subtree, including itself.
    pub fn count(&self) -> usize {
        1 + self.children.iter().map(Frame::count).sum::<usize>()
    }

    /// How deep this subtree goes. A leaf is 1.
    pub fn depth(&self) -> usize {
        1 + self.children.iter().map(Frame::depth).max().unwrap_or(0)
    }
}

/// A parsed profile: the synthetic root holding every sampled stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// The root frame. Its `total` is the sample count of the whole profile.
    pub root: Frame,
    /// Lines the parser could not make sense of, kept for the UI to report.
    ///
    /// Not an error: real profiler output carries banners, warnings and blank lines, and a
    /// profile that is 99% readable is worth showing. The count is surfaced so a *wholly*
    /// unreadable file doesn't masquerade as an empty one.
    pub skipped: usize,
}

impl Profile {
    /// Total samples across the profile.
    pub fn total(&self) -> u64 {
        self.root.total
    }

    /// Whether anything at all was parsed.
    pub fn is_empty(&self) -> bool {
        self.root.children.is_empty()
    }
}

/// The name given to the synthetic frame that holds every top-level stack.
pub const ROOT: &str = "all";

/// Parse folded-stack text into a call tree.
///
/// The format, one line per unique stack: `frame;frame;frame <count>`. The count is the last
/// whitespace-separated token; everything before it is the semicolon-separated path. This
/// matches `stackcollapse-*.pl`, `samply`'s folded export, and what `cargo flamegraph` leaves
/// behind in its `.folded` file.
///
/// # What is tolerated, and why
///
/// Real files are messy, so parsing never fails as a whole — unreadable lines are counted into
/// [`Profile::skipped`] and the rest is kept:
///
/// - blank lines and `#` comments are skipped silently (not counted as errors)
/// - a missing or non-numeric count skips the line
/// - `0`-sample stacks are skipped: they contribute nothing and would draw zero-width frames
/// - empty path segments (`a;;b`) are dropped, so a trailing `;` is harmless
///
/// Identical stacks appearing on several lines are **summed**, which is what makes the parser
/// correct on `perf script` output that has not been de-duplicated.
pub fn parse_folded(text: &str) -> Profile {
    // Build with maps keyed by name so merging repeated stacks is a lookup rather than a scan,
    // then convert to the sorted `Vec` form once at the end.
    #[derive(Default)]
    struct Node {
        total: u64,
        children: BTreeMap<String, Node>,
    }

    let mut root = Node::default();
    let mut skipped = 0usize;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // The count is the final token; the stack is everything before it. `rsplit_once` on
        // whitespace is what makes frame names containing spaces work — and they do, constantly:
        // `<core::iter::Map<I,F> as Iterator>::next` is one frame, with spaces in it.
        let Some((stack, count)) = line.rsplit_once(char::is_whitespace) else {
            skipped += 1;
            continue;
        };
        let Ok(count) = count.trim().parse::<u64>() else {
            skipped += 1;
            continue;
        };
        if count == 0 {
            continue;
        }

        let path: Vec<&str> = stack
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if path.is_empty() {
            skipped += 1;
            continue;
        }

        // Walk the path, creating as needed, adding the count to every frame on the way down.
        // The root accumulates too, so `root.total` is the profile's sample count for free.
        root.total += count;
        let mut cur = &mut root;
        for seg in path {
            cur = cur.children.entry(seg.to_string()).or_default();
            cur.total += count;
        }
    }

    fn build(name: String, n: Node) -> Frame {
        Frame {
            name,
            total: n.total,
            // `BTreeMap` iterates in key order, so children come out sorted by name with no
            // explicit sort — the stable ordering promised on `Frame::children`.
            children: n.children.into_iter().map(|(k, v)| build(k, v)).collect(),
        }
    }

    Profile {
        root: build(ROOT.to_string(), root),
        skipped,
    }
}

/// A frame placed on screen: where to draw it and what it represents.
///
/// Coordinates are **fractions of the viewport**, not pixels: `x` and `width` in `0.0..=1.0`,
/// `depth` in rows from the top of the drawn tree. The renderer multiplies by its own size, so
/// the layout is resolution-independent and testable without a window.
#[derive(Debug, Clone, PartialEq)]
pub struct Placed {
    /// The frame's name.
    pub name: String,
    /// Left edge, as a fraction of the full width.
    pub x: f32,
    /// Width, as a fraction of the full width.
    pub width: f32,
    /// Row index; 0 is the zoom root.
    pub depth: usize,
    /// Inclusive samples.
    pub total: u64,
    /// Samples in this frame alone.
    pub own: u64,
    /// The path from the profile root to this frame, used to zoom and to identify it.
    pub path: Vec<String>,
}

impl Placed {
    /// This frame's share of the *whole profile*, as a percentage.
    ///
    /// Takes the profile total rather than using the zoom root, so a zoomed-in frame still
    /// reports its true cost. A frame that is 100% of the current view but 3% of the run is
    /// a very different fact, and the tooltip must not blur them.
    pub fn percent_of(&self, profile_total: u64) -> f32 {
        if profile_total == 0 {
            return 0.0;
        }
        self.total as f32 / profile_total as f32 * 100.0
    }
}

/// Flatten a tree into drawable rectangles.
///
/// Children are laid out left to right inside their parent's span, each taking the share of the
/// width its sample count earns. Self time is simply the gap left where no child sits, which is
/// why nothing is emitted for it — the parent showing through *is* the self time.
///
/// `min_width` drops frames narrower than that fraction of the viewport, along with their
/// subtrees. A deep profile has tens of thousands of frames, almost all of them sub-pixel; at
/// 1e-4 the tree stays under a few thousand rectangles and looks identical. Pass `0.0` to keep
/// everything.
pub fn layout(root: &Frame, min_width: f32) -> Vec<Placed> {
    let mut out = Vec::new();
    let mut path = Vec::new();
    place(root, 0.0, 1.0, 0, min_width, &mut path, &mut out);
    out
}

fn place(
    f: &Frame,
    x: f32,
    width: f32,
    depth: usize,
    min_width: f32,
    path: &mut Vec<String>,
    out: &mut Vec<Placed>,
) {
    if width < min_width {
        return;
    }
    path.push(f.name.clone());
    out.push(Placed {
        name: f.name.clone(),
        x,
        width,
        depth,
        total: f.total,
        own: f.self_samples(),
        path: path.clone(),
    });

    // Children are positioned by running offset, so they sit flush against each other and the
    // self-time gap ends up on the right — the conventional flame graph arrangement.
    if f.total > 0 {
        let mut cursor = x;
        for c in &f.children {
            let w = width * (c.total as f32 / f.total as f32);
            place(c, cursor, w, depth + 1, min_width, path, out);
            cursor += w;
        }
    }
    path.pop();
}

/// Find the subtree at `path`, for zooming.
///
/// The path is the one carried on [`Placed::path`], starting with the root's own name. `None`
/// when it doesn't resolve — which happens legitimately when a new profile is loaded while a
/// zoom from the previous one is still held, so callers fall back to the root rather than
/// treating it as an error.
pub fn at_path<'a>(root: &'a Frame, path: &[String]) -> Option<&'a Frame> {
    let mut cur = root;
    // The first segment names the root itself.
    if path.first().map(String::as_str) != Some(cur.name.as_str()) {
        return None;
    }
    for seg in &path[1..] {
        cur = cur.children.iter().find(|c| &c.name == seg)?;
    }
    Some(cur)
}

/// Case-insensitive substring match, for the search box.
///
/// Case-insensitive because nobody hunting for `model_call` in a stack full of
/// `<T as Trait>::MODEL_CALL` wants to think about it.
pub fn matches(name: &str, needle: &str) -> bool {
    !needle.is_empty() && name.to_lowercase().contains(&needle.to_lowercase())
}

/// The share of the profile matched by `needle`, as a percentage.
///
/// Counts each matched frame's *own* samples, never its total, so nested matches cannot be
/// counted twice — searching `a` against a stack `a;a;a` must report `a`'s real cost, not
/// three times it.
pub fn matched_percent(root: &Frame, needle: &str) -> f32 {
    fn walk(f: &Frame, needle: &str, acc: &mut u64) {
        if matches(&f.name, needle) {
            *acc += f.self_samples();
        }
        for c in &f.children {
            walk(c, needle, acc);
        }
    }
    if root.total == 0 || needle.is_empty() {
        return 0.0;
    }
    let mut acc = 0;
    walk(root, needle, &mut acc);
    acc as f32 / root.total as f32 * 100.0
}

/// The frames costing the most *self* time, worst first.
///
/// The flame graph shows shape; this shows the answer. A wide frame near the root is usually
/// just "main called things", whereas the top of this list is where the CPU actually sat —
/// so it is the first thing worth reading, and the UI puts it beside the graph.
///
/// Frames of the same name occurring in different stacks are summed: one function's cost is
/// one number regardless of how many paths reach it.
pub fn hot_frames(root: &Frame, limit: usize) -> Vec<(String, u64)> {
    fn walk(f: &Frame, acc: &mut BTreeMap<String, u64>) {
        let own = f.self_samples();
        if own > 0 {
            *acc.entry(f.name.clone()).or_default() += own;
        }
        for c in &f.children {
            walk(c, acc);
        }
    }
    let mut acc = BTreeMap::new();
    for c in &root.children {
        walk(c, &mut acc);
    }
    let mut v: Vec<(String, u64)> = acc.into_iter().collect();
    // Descending by samples; name breaks ties so the order is deterministic.
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(limit);
    v
}

/// Shorten a frame name for display.
///
/// Rust symbols are long — `<alloc::vec::Vec<T,A> as core::ops::index::Index<I>>::index` is
/// routine — and a flame graph is mostly narrow rectangles. The generic parameters and the
/// module path are the first things to go, since the trailing segment is what identifies the
/// function to a human.
///
/// Kept pure and separate from the renderer so the elision rules can be asserted directly.
pub fn short_name(name: &str) -> &str {
    // Trailing hash that `perf` appends to Rust symbols: `foo::h3f2a1b`. Dropped by taking the
    // segment before it only when it looks like one (17 chars, starts `h`, all hex).
    let name = match name.rsplit_once("::") {
        Some((head, tail))
            if tail.len() == 17
                && tail.starts_with('h')
                && tail[1..].chars().all(|c| c.is_ascii_hexdigit()) =>
        {
            head
        }
        _ => name,
    };
    // The last `::` segment, but only when the name has no generics — inside `<...>` a `::` is
    // part of a type, and splitting there produces nonsense like `index>::index`.
    if name.contains('<') || name.contains('(') {
        return name;
    }
    name.rsplit("::").next().unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kid<'a>(f: &'a Frame, name: &str) -> &'a Frame {
        f.children.iter().find(|c| c.name == name).expect(name)
    }

    #[test]
    fn a_single_stack_becomes_a_spine() {
        let p = parse_folded("main;a;b 10");
        assert_eq!(p.total(), 10);
        assert_eq!(p.skipped, 0);
        let main = kid(&p.root, "main");
        assert_eq!(main.total, 10);
        assert_eq!(kid(kid(main, "a"), "b").total, 10);
        // Every frame on a single spine has zero self time except the leaf.
        assert_eq!(main.self_samples(), 0);
        assert_eq!(kid(kid(main, "a"), "b").self_samples(), 10);
    }

    #[test]
    fn stacks_sharing_a_prefix_merge_and_sum() {
        let p = parse_folded("main;a 3\nmain;b 7\nmain;a 5");
        assert_eq!(p.total(), 15);
        let main = kid(&p.root, "main");
        assert_eq!(main.total, 15);
        // The repeated `main;a` is summed, not duplicated.
        assert_eq!(main.children.len(), 2);
        assert_eq!(kid(main, "a").total, 8);
        assert_eq!(kid(main, "b").total, 7);
    }

    #[test]
    fn self_time_is_the_parent_minus_its_children() {
        // `main` is sampled 10 times on its own, plus 6 in a callee.
        let p = parse_folded("main 10\nmain;work 6");
        let main = kid(&p.root, "main");
        assert_eq!(main.total, 16);
        assert_eq!(main.self_samples(), 10);
    }

    #[test]
    fn frame_names_may_contain_spaces() {
        // Rust trait-impl symbols are full of spaces; splitting on the FIRST space would
        // shred them. The count is the last token.
        let p = parse_folded("main;<Vec<T> as Index<I>>::index 4");
        let main = kid(&p.root, "main");
        assert_eq!(main.children[0].name, "<Vec<T> as Index<I>>::index");
        assert_eq!(main.children[0].total, 4);
    }

    #[test]
    fn junk_is_counted_and_the_rest_survives() {
        let p = parse_folded(
            "# a comment\n\
             \n\
             main;a 10\n\
             this line has no count\n\
             main;b notanumber\n\
             main;c 5\n",
        );
        // Comment and blank are silent; the two malformed lines are counted.
        assert_eq!(p.skipped, 2);
        assert_eq!(p.total(), 15);
        assert_eq!(kid(&p.root, "main").children.len(), 2);
    }

    #[test]
    fn zero_sample_and_empty_segments_are_dropped() {
        let p = parse_folded("main;gone 0\nmain;;a; 4");
        assert_eq!(p.total(), 4);
        assert_eq!(p.skipped, 0);
        let main = kid(&p.root, "main");
        // `main;;a;` collapses to `main;a` — no empty-named frames.
        assert_eq!(main.children.len(), 1);
        assert_eq!(main.children[0].name, "a");
    }

    #[test]
    fn an_empty_input_is_an_empty_profile_not_a_failure() {
        let p = parse_folded("");
        assert!(p.is_empty());
        assert_eq!(p.total(), 0);
        // The layout of an empty profile is just the root, and must not panic on the
        // divide-by-total inside `place`.
        assert_eq!(layout(&p.root, 0.0).len(), 1);
    }

    #[test]
    fn children_are_ordered_by_name_for_a_stable_picture() {
        // Written weight-first; must come out name-sorted so two runs line up visually.
        let p = parse_folded("main;zebra 100\nmain;alpha 1\nmain;middle 50");
        let names: Vec<&str> = kid(&p.root, "main")
            .children
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "middle", "zebra"]);
    }

    #[test]
    fn layout_gives_each_child_its_share_and_leaves_self_time_as_a_gap() {
        // `main` = 100: 25 in `a`, 25 in `b`, 50 its own.
        let p = parse_folded("main 50\nmain;a 25\nmain;b 25");
        let placed = layout(&p.root, 0.0);

        let main = placed.iter().find(|p| p.name == "main").unwrap();
        assert_eq!((main.x, main.width), (0.0, 1.0));

        let a = placed.iter().find(|p| p.name == "a").unwrap();
        let b = placed.iter().find(|p| p.name == "b").unwrap();
        assert_eq!(a.depth, 2);
        assert!((a.x - 0.0).abs() < 1e-6);
        assert!((a.width - 0.25).abs() < 1e-6);
        // `b` sits flush against `a`.
        assert!((b.x - 0.25).abs() < 1e-6);
        assert!((b.width - 0.25).abs() < 1e-6);
        // The right half is `main`'s self time: nothing is drawn there.
        assert!(!placed.iter().any(|p| p.depth == 2 && p.x >= 0.5));
    }

    #[test]
    fn narrow_frames_and_their_subtrees_are_dropped() {
        let p = parse_folded("main;fat 999\nmain;thin;deep 1");
        let all = layout(&p.root, 0.0);
        assert!(all.iter().any(|p| p.name == "deep"));

        let culled = layout(&p.root, 0.01);
        assert!(culled.iter().any(|p| p.name == "fat"));
        // The thin frame goes, and takes its child with it.
        assert!(!culled.iter().any(|p| p.name == "thin"));
        assert!(!culled.iter().any(|p| p.name == "deep"));
    }

    #[test]
    fn zooming_resolves_a_path_and_rejects_a_stale_one() {
        let p = parse_folded("main;a;b 10");
        let path = vec![ROOT.to_string(), "main".to_string(), "a".to_string()];
        let z = at_path(&p.root, &path).unwrap();
        assert_eq!(z.name, "a");
        assert_eq!(z.total, 10);

        // A path from some other profile must not resolve to something arbitrary.
        let stale = vec![ROOT.to_string(), "main".to_string(), "nope".to_string()];
        assert!(at_path(&p.root, &stale).is_none());
        // A path not rooted at the root is rejected too.
        assert!(at_path(&p.root, &["main".to_string()]).is_none());
    }

    #[test]
    fn a_zoomed_frame_still_reports_its_share_of_the_whole_run() {
        // `a` is 100% of its own subtree but 10% of the profile.
        let p = parse_folded("main;a 10\nmain;b 90");
        let path = vec![ROOT.to_string(), "main".to_string(), "a".to_string()];
        let z = at_path(&p.root, &path).unwrap();
        let placed = layout(z, 0.0);
        let a = &placed[0];
        assert!((a.width - 1.0).abs() < 1e-6, "fills the zoomed viewport");
        assert!((a.percent_of(p.total()) - 10.0).abs() < 1e-3, "but is 10%");
    }

    #[test]
    fn search_counts_self_time_so_nesting_cannot_double_count() {
        // `a` calls `a` calls `a`; the cost of `a` is the 10 samples at the bottom, not 30.
        let p = parse_folded("a;a;a 10");
        assert!((matched_percent(&p.root, "a") - 100.0).abs() < 1e-3);

        let p2 = parse_folded("main;target 25\nmain;other 75");
        assert!((matched_percent(&p2.root, "TARGET") - 25.0).abs() < 1e-3);
        assert_eq!(matched_percent(&p2.root, ""), 0.0);
        assert_eq!(matched_percent(&p2.root, "absent"), 0.0);
    }

    #[test]
    fn hot_frames_sum_a_function_across_every_stack_that_reaches_it() {
        // `alloc` is called from two places: 30 + 40. `main` itself never costs anything.
        let p = parse_folded("main;a;alloc 30\nmain;b;alloc 40\nmain;a 5");
        let hot = hot_frames(&p.root, 10);
        assert_eq!(hot[0], ("alloc".to_string(), 70));
        assert_eq!(hot[1], ("a".to_string(), 5));
        // Frames with no self time never appear.
        assert!(!hot.iter().any(|(n, _)| n == "main" || n == "b"));
    }

    #[test]
    fn hot_frames_respects_the_limit() {
        let p = parse_folded("m;a 5\nm;b 4\nm;c 3\nm;d 2");
        assert_eq!(hot_frames(&p.root, 2).len(), 2);
        assert_eq!(hot_frames(&p.root, 2)[0].0, "a");
    }

    #[test]
    fn long_symbols_shorten_but_generics_are_left_alone() {
        assert_eq!(short_name("sc_core::agent::run"), "run");
        // A perf-style trailing hash is dropped.
        assert_eq!(short_name("sc_core::agent::run::h0123456789abcdef"), "run");
        // Something that merely looks like a hash but isn't hex is kept.
        assert_eq!(short_name("foo::hZZZZZZZZZZZZZZZZ"), "hZZZZZZZZZZZZZZZZ");
        // Generics would be shredded by a naive `::` split, so they're left whole.
        let g = "<Vec<T> as Index<I>>::index";
        assert_eq!(short_name(g), g);
        assert_eq!(short_name("main"), "main");
    }

    #[test]
    fn a_frame_knows_its_size_and_depth() {
        let p = parse_folded("main;a;b 1\nmain;c 1");
        // root + main + a + b + c
        assert_eq!(p.root.count(), 5);
        // all -> main -> a -> b
        assert_eq!(p.root.depth(), 4);
    }

    #[test]
    fn a_child_heavier_than_its_parent_does_not_underflow() {
        // Not producible by the parser, but a hand-built tree must not panic.
        let f = Frame {
            name: "p".into(),
            total: 1,
            children: vec![Frame {
                name: "c".into(),
                total: 99,
                children: vec![],
            }],
        };
        assert_eq!(f.self_samples(), 0);
    }
}
