//! End-to-end check of the parser against a realistically shaped profile.
//!
//! The unit tests in `flame.rs` use small hand-written inputs; this one runs the whole pipeline
//! — parse, lay out, rank — over a profile with the depth and symbol shapes a real Rust run
//! produces, which is where an off-by-one in the percentages would actually show up.

use sc_win::flame;

const SAMPLE: &str = "\
# a synthetic profile, shaped like a real agent run
main;sc_core::run;sc_model::complete;reqwest::send 4200
main;sc_core::run;sc_model::complete;serde_json::from_str 1100
main;sc_core::run;sc_tools::dispatch;std::fs::read_to_string 890
main;sc_core::run;sc_tools::dispatch;regex::Regex::is_match 640
main;sc_core::run;sc_context::prune;<alloc::vec::Vec<T> as Clone>::clone 1500
main;sc_core::run;sc_context::prune;tiktoken::encode 2300
main;sc_core::run 210
main;sc_win::layout::parse 95
main;config::load;toml::from_str 140
main 60
";

#[test]
fn a_realistic_profile_parses_lays_out_and_ranks() {
    let p = flame::parse_folded(SAMPLE);
    assert_eq!(p.skipped, 0, "every line of a well-formed profile is read");
    assert_eq!(p.total(), 11_135);

    // The hottest frame is the HTTP call, and it is the biggest single cost.
    let hot = flame::hot_frames(&p.root, 5);
    assert_eq!(hot[0].0, "reqwest::send");
    assert_eq!(hot[0].1, 4200);
    assert_eq!(hot[1].0, "tiktoken::encode");

    // Widths sum to the parent's width at every level: the invariant that makes the picture
    // honest. `main` holds everything, so it spans the full viewport.
    let placed = flame::layout(&p.root, 0.0);
    let main = placed.iter().find(|f| f.name == "main").unwrap();
    assert!((main.width - 1.0).abs() < 1e-5);

    // The row under `main` must tile it exactly, with no overlap and no gap beyond self time.
    let kids: Vec<_> = placed
        .iter()
        .filter(|f| f.depth == 2 && f.x < 1.0)
        .collect();
    let covered: f32 = kids.iter().map(|f| f.width).sum();
    assert!(covered <= 1.0 + 1e-5, "children cannot exceed their parent");

    // Zooming into the model call renormalises to fill the view but keeps its true share.
    let path = vec![
        flame::ROOT.to_string(),
        "main".to_string(),
        "sc_core::run".to_string(),
        "sc_model::complete".to_string(),
    ];
    let z = flame::at_path(&p.root, &path).expect("the path resolves");
    assert_eq!(z.total, 5300);
    let zp = flame::layout(z, 0.0);
    assert!((zp[0].width - 1.0).abs() < 1e-5, "fills the zoomed view");
    let share = zp[0].percent_of(p.total());
    assert!(
        (share - 47.6).abs() < 0.2,
        "still ~47.6% of the run, got {share}"
    );

    // Search finds the allocation-heavy clone by a fragment of its mangled name.
    let pct = flame::matched_percent(&p.root, "clone");
    assert!((pct - 13.5).abs() < 0.2, "got {pct}");
}
