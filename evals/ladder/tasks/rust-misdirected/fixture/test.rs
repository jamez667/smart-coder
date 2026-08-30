// Contract test for config::parse. FROZEN: a solver must not modify this file.
#[path = "config.rs"]
mod config;
use config::parse;

fn pairs(text: &str) -> Vec<(String, String)> {
    parse(text)
}

#[test]
fn a_simple_pair() {
    assert_eq!(pairs("host = localhost"), vec![("host".into(), "localhost".into())]);
}

#[test]
fn blank_and_comment_lines_are_skipped() {
    let text = "\n# just a comment\nport = 8080\n";
    assert_eq!(pairs(text), vec![("port".into(), "8080".into())]);
}

#[test]
fn a_trailing_comment_is_stripped() {
    assert_eq!(pairs("port = 8080 # the port"), vec![("port".into(), "8080".into())]);
}

#[test]
fn a_value_may_contain_an_equals_sign() {
    // Only the FIRST `=` separates key from value.
    assert_eq!(pairs("expr = a=b"), vec![("expr".into(), "a=b".into())]);
}

#[test]
fn a_url_value_survives_intact() {
    // No comment on this line: the `#` is part of the URL fragment.
    assert_eq!(
        pairs("docs = http://example.com/page#section"),
        vec![("docs".into(), "http://example.com/page#section".into())]
    );
}
