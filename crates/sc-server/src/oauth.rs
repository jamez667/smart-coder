//! Signing in with GitHub, for owners.
//!
//! **This proves who somebody is and nothing about what they may see.** Whether
//! a login is an owner comes from [`PublicConfig::owners`](crate::config::PublicConfig),
//! checked on every request — so this module hands back a *name*, and the
//! configuration decides what it is worth.
//!
//! ## The shape of the exchange
//!
//! ```text
//!   GET  /public/auth/github            an interstitial with a link to GitHub
//!   ⟶ github.com/login/oauth/authorize?client_id=…&state=…
//!   GET  /public/auth/github/callback?code=…&state=…
//!        server → github.com/login/oauth/access_token   (the secret goes here)
//!        server → api.github.com/user                   (who is this?)
//!   ⟶ signed in, or a page saying why not
//! ```
//!
//! ## Why an interstitial rather than a redirect
//!
//! [`Res`](crate::routes::Res) carries no `Location` header, and giving it one
//! means changing the response writer for a single route. More to the point,
//! `set_language` records a standing objection to redirects on this surface — a
//! "return to where you were" parameter on a route anyone can reach is an open
//! redirect waiting to be found. A page with a link needs no new machinery and
//! keeps that position intact.
//!
//! It also stays inside the CSP as written: `form-action 'self'` would block a
//! form posting to github.com, but an `<a href>` is governed by nothing in
//! `default-src 'none'`.
//!
//! ## The state token
//!
//! Modelled on [`Links`](crate::account::Links) rather than reusing it: the
//! machinery is the same shape — issue a plaintext token while storing only its
//! hash, spend it once, expire it, sweep it — but a `MagicLink` carries an email
//! hash and hint that an OAuth state has no use for, and its outstanding cap is
//! wired to *mail* spend.

use serde::{Deserialize, Serialize};

use crate::auth::{hash, matches, mint_secret};

/// How long a sign-in may sit half-finished.
///
/// The window between clicking the link and GitHub sending the reader back —
/// seconds in practice. Ten minutes is generous for somebody who stopped to
/// authorise the application, and short enough that an abandoned state is not a
/// standing invitation.
pub const STATE_TTL_MS: u64 = 10 * 60 * 1000;

/// The most half-finished sign-ins that may be outstanding at once.
///
/// A bound on the file's size, since anyone reaching the start of the flow can
/// mint one. Generous against real use — these live for ten minutes — and it
/// refuses before the store grows rather than after.
pub const MAX_OUTSTANDING_STATES: usize = 200;

/// One half-finished sign-in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// SHA-256 of the state token. **Never the token**, like everything else
    /// this server keeps.
    pub token_hash: String,
    pub issued_ms: u64,
    #[serde(default)]
    pub consumed: bool,
}

impl State {
    pub fn expired(&self, now_ms: u64) -> bool {
        // `saturating_sub`, so a clock stepping backwards cannot mint a state
        // good for the next forty-nine days.
        now_ms.saturating_sub(self.issued_ms) > STATE_TTL_MS
    }

    pub fn usable(&self, now_ms: u64) -> bool {
        !self.consumed && !self.expired(now_ms)
    }
}

/// Why a callback's state was not accepted.
///
/// Two variants rather than one, for the same reason [`LinkError`] has two: a
/// second click on the same callback is a different event from a forged one,
/// and telling a person "that did not work" when their sign-in already
/// succeeded reads as a bug.
///
/// [`LinkError`]: crate::account::LinkError
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateError {
    /// Already spent — the reader came back, or GitHub called twice.
    AlreadyUsed,
    /// Never issued here, or long expired.
    Invalid,
}

/// The half-finished sign-ins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct States {
    #[serde(default)]
    pub states: Vec<State>,
}

impl States {
    /// Start a sign-in, returning the token to send to GitHub.
    ///
    /// The plaintext is returned **once** and never stored, so the file grants
    /// nobody the ability to complete somebody else's sign-in.
    pub fn issue(&mut self, now_ms: u64) -> String {
        let token = mint_secret();
        self.states.push(State {
            token_hash: hash(&token),
            issued_ms: now_ms,
            consumed: false,
        });
        token
    }

    /// How many could still be spent — the cap this store is bounded by.
    pub fn outstanding(&self, now_ms: u64) -> usize {
        self.states.iter().filter(|s| s.usable(now_ms)).count()
    }

    /// Spend a state token.
    ///
    /// **Single use.** Without that, a callback URL captured from a browser's
    /// history or a referrer log could be replayed to sign in again.
    pub fn consume(&mut self, token: &str, now_ms: u64) -> std::result::Result<(), StateError> {
        let Some(state) = self
            .states
            .iter_mut()
            .find(|s| matches(token, &s.token_hash))
        else {
            return Err(StateError::Invalid);
        };
        if state.consumed {
            return Err(StateError::AlreadyUsed);
        }
        if state.expired(now_ms) {
            return Err(StateError::Invalid);
        }
        state.consumed = true;
        Ok(())
    }

    /// Drop what can no longer be spent.
    ///
    /// Kept for a while after consumption so a second callback can say "already
    /// used" rather than "invalid" — the same reasoning as the magic links.
    pub fn sweep(&mut self, now_ms: u64) {
        self.states
            .retain(|s| now_ms.saturating_sub(s.issued_ms) <= STATE_TTL_MS * 6);
    }
}

/// Where the flow sends a reader, and what the callback exchanges with.
///
/// Held apart from the URLs so a test can point them at a local stub — the
/// alternative is a test that talks to GitHub, which is not a test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub authorize: String,
    pub token: String,
    pub user: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            authorize: "https://github.com/login/oauth/authorize".to_string(),
            token: "https://github.com/login/oauth/access_token".to_string(),
            user: "https://api.github.com/user".to_string(),
        }
    }
}

/// Ask GitHub who a code belongs to.
///
/// Two calls: the code becomes an access token, the token names a user. The
/// **client secret only ever appears in the first**, server to server, and never
/// reaches a page or a log.
pub trait Github {
    /// The login this authorisation code belongs to, lowercased.
    fn login_for(&self, code: &str) -> sc_proto::Result<String>;
}

/// The real client.
pub struct HttpGithub {
    client_id: String,
    client_secret: String,
    endpoints: Endpoints,
    agent: ureq::Agent,
}

impl HttpGithub {
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        HttpGithub {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            endpoints: Endpoints::default(),
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(std::time::Duration::from_secs(10)))
                // A redirect could carry the client secret to wherever the
                // response points. These endpoints never legitimately redirect —
                // the same reasoning as the mailer's.
                .max_redirects(0)
                .build()
                .into(),
        }
    }

    /// Point at a stub, for tests.
    pub fn with_endpoints(mut self, endpoints: Endpoints) -> Self {
        self.endpoints = endpoints;
        self
    }

    /// The URL a reader follows to authorise.
    ///
    /// No `redirect_uri`: GitHub uses the callback registered with the OAuth
    /// application, and one sent here would be a parameter an attacker could try
    /// to vary.
    pub fn authorize_url(&self, state: &str) -> String {
        format!(
            "{}?client_id={}&state={}",
            self.endpoints.authorize,
            encode(&self.client_id),
            encode(state)
        )
    }
}

impl Github for HttpGithub {
    fn login_for(&self, code: &str) -> sc_proto::Result<String> {
        use sc_proto::DcError;

        let token: serde_json::Value = self
            .agent
            .post(&self.endpoints.token)
            .header("Accept", "application/json")
            .send_json(serde_json::json!({
                "client_id": self.client_id,
                "client_secret": self.client_secret,
                "code": code,
            }))
            .map_err(|e| DcError::Backend(format!("github token exchange: {e}")))?
            .body_mut()
            .read_json()
            .map_err(|e| DcError::Backend(format!("github token exchange: unreadable: {e}")))?;

        // GitHub reports a refused code as a 200 with an `error` field, so a
        // status check alone would take a failure for a success and go on to ask
        // `/user` with no token.
        let access = token
            .get("access_token")
            .and_then(|t| t.as_str())
            .ok_or_else(|| {
                DcError::Backend(format!(
                    "github refused the code: {}",
                    token
                        .get("error_description")
                        .and_then(|e| e.as_str())
                        .unwrap_or("no access token in the response")
                ))
            })?;

        let user: serde_json::Value = self
            .agent
            .get(&self.endpoints.user)
            .header("Authorization", &format!("Bearer {access}"))
            .header("Accept", "application/vnd.github+json")
            // GitHub refuses an API request without one.
            .header("User-Agent", "sc-server")
            .call()
            .map_err(|e| DcError::Backend(format!("github user: {e}")))?
            .body_mut()
            .read_json()
            .map_err(|e| DcError::Backend(format!("github user: unreadable: {e}")))?;

        user.get("login")
            .and_then(|l| l.as_str())
            // Lowercased here, once, so every later comparison against the
            // configured allowlist is against the same shape.
            .map(|l| l.to_ascii_lowercase())
            .ok_or_else(|| DcError::Backend("github user: no login in the response".into()))
    }
}

/// Percent-encode a query value, keeping only the unambiguous.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_is_single_use() {
        // A callback URL sits in browser history and in referrer logs. Replaying
        // one must not sign anybody in twice.
        let mut states = States::default();
        let token = states.issue(1_000);

        assert_eq!(states.consume(&token, 1_000), Ok(()));
        assert_eq!(states.consume(&token, 1_000), Err(StateError::AlreadyUsed));
    }

    #[test]
    fn a_state_expires() {
        let mut states = States::default();
        let token = states.issue(1_000);
        assert_eq!(
            states.consume(&token, 1_000 + STATE_TTL_MS + 1),
            Err(StateError::Invalid)
        );
    }

    #[test]
    fn a_state_nobody_issued_is_invalid() {
        // What a forged callback presents. Distinguished from "already used",
        // because telling somebody whose sign-in worked that their link was
        // invalid reads as a bug.
        let mut states = States::default();
        states.issue(1_000);
        assert_eq!(
            states.consume("not-a-real-token", 1_000),
            Err(StateError::Invalid)
        );
    }

    #[test]
    fn nothing_that_grants_access_is_serialized() {
        // The same rule the account store keeps: the file holds hashes, so a
        // copy of the volume lets nobody finish somebody else's sign-in.
        let mut states = States::default();
        let token = states.issue(1_000);
        let json = serde_json::to_string(&states).unwrap();
        assert!(!json.contains(&token), "the token is in {json}");
    }

    #[test]
    fn a_backwards_clock_does_not_extend_a_state() {
        let mut states = States::default();
        let token = states.issue(10_000);
        // "Now" is before it was issued. It must not read as freshly minted for
        // the next forty-nine days.
        assert!(!states.states[0].expired(1));
        assert_eq!(states.consume(&token, 1), Ok(()));
    }

    #[test]
    fn spent_states_are_swept_but_not_at_once() {
        // Kept a while so a second callback says "already used" rather than
        // "invalid" — the same reasoning as the magic links.
        let mut states = States::default();
        let token = states.issue(1_000);
        states.consume(&token, 1_000).unwrap();

        states.sweep(1_000 + STATE_TTL_MS);
        assert_eq!(states.states.len(), 1, "still says what happened");

        states.sweep(1_000 + STATE_TTL_MS * 7);
        assert!(states.states.is_empty(), "eventually gone");
    }

    #[test]
    fn the_authorize_url_carries_the_state_and_no_redirect() {
        // No `redirect_uri`: GitHub uses the callback registered with the
        // application, and one sent here would be a parameter to try to vary.
        let gh = HttpGithub::new("client-id", "secret");
        let url = gh.authorize_url("state-token");
        assert!(url.contains("client_id=client-id"), "{url}");
        assert!(url.contains("state=state-token"), "{url}");
        assert!(!url.contains("redirect_uri"), "{url}");
        assert!(!url.contains("secret"), "the secret never leaves: {url}");
    }
}
