//! The query string on a daemon poll.
//!
//! ## Why the declaration rides in the query string
//!
//! A daemon tells the server which repositories it has a working tree for, so
//! the server can hand it only work it can actually do. That has to reach a
//! `GET` with no body, and the choice was between a header, a new route, and
//! this.
//!
//! The query string wins on **backward compatibility**, and not by luck: the
//! router splits the path on `?` before matching
//! (<!--@ crates/sc-server/src/routes.rs -->), and so does the long-poll check
//! in [`crate::serve`]. So a server that predates this ignores the declaration
//! and answers normally, where a new route would 404 and a header would be
//! silently dropped without the daemon ever learning it was talking to an old
//! server. A new daemon against an old server therefore degrades to "gets
//! everything", which is exactly what it used to get.
//!
//! Parsing lives here rather than on [`Req`](crate::routes::Req), whose doc
//! comment argues — correctly — that it should hold only what the routes
//! actually use. One route needs this, so one route parses it.

/// What a daemon declared when it polled.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PollQuery {
    /// The repositories it says it serves. Empty means it declared nothing,
    /// which is what an older daemon sends — see
    /// [`Serves`](crate::store::Serves) for why that is not the same as
    /// serving nothing.
    pub repos: Vec<String>,
    /// The protocol it claims. **Advisory**: logged, never refused. Refusing a
    /// poll on version grounds turns a skew into a daemon that cannot fetch
    /// work at all, which is worse than one that fetches work and hands it
    /// back.
    pub protocol: Option<u32>,
}

/// The most repositories one poll may declare.
///
/// A daemon holding a key is not an attacker in this threat model, but the poll
/// is the one route re-dispatched every 250ms for the length of a hold, so an
/// unbounded `Vec<String>` rebuilt four times a second is not worth leaving
/// open. Anything past this is dropped rather than refused: a daemon serving
/// more repositories than this has a configuration problem, not a claim to make.
const MAX_REPOS: usize = 64;

/// The longest repository name accepted.
///
/// Longer than any name a person types, short enough that the bound above is a
/// real bound on memory rather than a nominal one.
const MAX_NAME: usize = 128;

impl PollQuery {
    /// Parse the raw request path — `/api/v1/work?repo=a&repo=b`.
    ///
    /// Takes the **whole** path rather than a pre-split query, because the
    /// router hands routes their path already split on `?` and this needs the
    /// part that was cut off.
    pub fn parse(raw_path: &str) -> PollQuery {
        let mut out = PollQuery::default();
        let Some((_, query)) = raw_path.split_once('?') else {
            return out;
        };

        for pair in query.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            match key {
                "repo" if out.repos.len() < MAX_REPOS => {
                    let name = decode(value);
                    // Silently skipped rather than truncated: a truncated name
                    // is a *different* name, and would match a repository the
                    // daemon never claimed to serve.
                    if !name.is_empty() && name.len() <= MAX_NAME {
                        out.repos.push(name);
                    }
                }
                "protocol" => out.protocol = decode(value).parse().ok(),
                _ => {}
            }
        }
        out
    }
}

/// Percent-decode one query value.
///
/// `+` is a space, per form encoding. A malformed `%` escape is kept **as
/// written** rather than dropped, so a name containing a stray percent fails to
/// match a configured repository instead of quietly matching a different one.
fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(&value[i + 1..i + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    // Not an escape after all. Kept verbatim.
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    // Lossy, because a repository name is compared against a configured one and
    // invalid UTF-8 cannot equal any of those — so the only question is whether
    // it fails to match loudly or panics.
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_repo_params_become_the_served_set() {
        let q = PollQuery::parse("/api/v1/work?repo=alpha&repo=beta");
        assert_eq!(q.repos, ["alpha", "beta"]);
    }

    #[test]
    fn a_poll_with_no_query_declares_nothing() {
        // What an older daemon sends. It must parse to an empty declaration
        // rather than anything surprising — the router turns this into
        // `Serves::Anything`.
        let q = PollQuery::parse("/api/v1/work");
        assert!(q.repos.is_empty());
        assert_eq!(q.protocol, None);
    }

    #[test]
    fn a_percent_encoded_name_decodes_to_the_configured_one() {
        // Repository names are free text on the daemon's side, so anything a
        // person can type has to survive the wire.
        let q = PollQuery::parse("/api/v1/work?repo=my%20repo&repo=a%2Fb&repo=c+d");
        assert_eq!(q.repos, ["my repo", "a/b", "c d"]);
    }

    #[test]
    fn the_protocol_is_read_when_present_and_absent_otherwise() {
        assert_eq!(
            PollQuery::parse("/api/v1/work?protocol=1").protocol,
            Some(1)
        );
        assert_eq!(PollQuery::parse("/api/v1/work?repo=a").protocol, None);
        // Unparseable is absent, not an error: this field never refuses a poll.
        assert_eq!(PollQuery::parse("/api/v1/work?protocol=x").protocol, None);
    }

    #[test]
    fn an_absurd_number_of_declarations_is_bounded() {
        let many = (0..500)
            .map(|i| format!("repo=r{i}"))
            .collect::<Vec<_>>()
            .join("&");
        let q = PollQuery::parse(&format!("/api/v1/work?{many}"));
        assert_eq!(q.repos.len(), MAX_REPOS);
    }

    #[test]
    fn an_over_long_name_is_dropped_rather_than_truncated() {
        // A truncated name is a *different* name, and would match a repository
        // the daemon never declared.
        let long = "a".repeat(MAX_NAME + 1);
        let q = PollQuery::parse(&format!("/api/v1/work?repo={long}&repo=ok"));
        assert_eq!(q.repos, ["ok"]);
    }

    #[test]
    fn a_malformed_escape_is_kept_verbatim() {
        // It then fails to match any configured repository, which is the safe
        // direction — the unsafe one is quietly becoming some *other* name.
        let q = PollQuery::parse("/api/v1/work?repo=100%&repo=%zz");
        assert_eq!(q.repos, ["100%", "%zz"]);
    }

    #[test]
    fn junk_in_the_query_is_ignored_rather_than_fatal() {
        // The query is caller-controlled, and a poll that cannot be parsed is a
        // daemon that cannot fetch work.
        let q = PollQuery::parse("/api/v1/work?&&=&repo&repo=alpha&x=1&=2");
        assert_eq!(q.repos, ["alpha"]);
    }

    #[test]
    fn what_the_daemon_builds_is_what_this_parses() {
        // The two halves live in different crates and were written apart, so the
        // round trip is asserted rather than assumed — an encoder and a decoder
        // that disagree produce a daemon that silently gets no work.
        let names = ["alpha", "my repo", "a/b", "100%", "dash-and_dot.v2"];
        let url = sc_proto::wire::route::work_for(&names);
        let parsed = PollQuery::parse(&url);
        assert_eq!(parsed.repos, names);
        assert_eq!(parsed.protocol, Some(sc_proto::wire::PROTOCOL_VERSION));
    }

    #[test]
    fn an_empty_name_is_not_a_declaration() {
        assert!(PollQuery::parse("/api/v1/work?repo=&repo=")
            .repos
            .is_empty());
    }
}
