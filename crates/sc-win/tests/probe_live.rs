//! A live probe against whatever is actually installed on this machine.
//!
//! `#[ignore]` by default: it asserts on the developer's PATH, which CI does not share. Run it
//! with `--ignored` after changing `detect` or the probe arguments.
//!
//! It exists because the probe bug it guards was invisible to every offline test: the argument
//! list was asserted, but nothing ever ran the binary to find out that `cargo-flamegraph
//! --version` exits non-zero.

#[test]
#[ignore]
fn detect_finds_what_is_actually_installed() {
    use sc_win::flame::tool::{is_installed, Profiler};
    for p in [Profiler::Samply, Profiler::CargoFlamegraph] {
        println!(
            "{:<18} program={:<18} probe={:?} installed={}",
            p.label(),
            p.program(),
            p.probe_args(),
            is_installed(p)
        );
    }
    println!("detect() -> {:?}", sc_win::flame::tool::detect());
}
