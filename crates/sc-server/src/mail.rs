//! Sending the one email this system sends: a sign-in link.
//!
//! ## The body is fixed, and that is the important part
//!
//! An unauthenticated form that emails an address someone types is, structurally,
//! an open relay. What decides whether that is a nuisance or a weapon is whether
//! the *content* is attacker-controlled.
//!
//! Here it is not. [`sign_in_body`] is a constant with one URL interpolated: no
//! request text, no name, no note, nothing the sender chose. So the worst use of
//! this endpoint is causing someone to receive a sign-in link they did not ask
//! for — annoying, and nothing like a channel for delivering a message.
//!
//! The rate limiter shapes the flow; the **outstanding-link cap** is the real
//! ceiling, refusing before the mailer is ever called.
//!
//! ## A small enum of providers, not templated configuration
//!
//! It is tempting to make the header name and body shape configurable and call
//! that "provider-agnostic". It would be a mistake: authentication differs
//! *structurally* between providers — Brevo uses `api-key`, Resend a bearer,
//! Postmark its own header — and the JSON bodies differ in nesting, so a
//! configurable body is a templating language living in an environment variable.
//!
//! Worse, a configurable **endpoint URL is a credential-exfiltration primitive**:
//! point it at a host you control and the API key is POSTed there. So the URL is
//! a per-variant constant. Adding a provider is a dozen lines in one `match`,
//! reviewable and testable — which free-form configuration is not.

use sc_proto::{DcError, Result};

/// Who sends the mail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// api.brevo.com — authenticates with an `api-key` header.
    Brevo,
    /// api.resend.com — authenticates with a bearer token.
    Resend,
    /// api.postmarkapp.com — authenticates with `X-Postmark-Server-Token`.
    Postmark,
}

impl Provider {
    /// Parse what the operator configured.
    ///
    /// `None` for anything unknown, so the caller can refuse to start rather
    /// than defaulting — a server that silently picked a provider the operator
    /// did not name would fail at the first signup, hours later.
    pub fn parse(s: &str) -> Option<Provider> {
        match s.trim().to_ascii_lowercase().as_str() {
            "brevo" => Some(Provider::Brevo),
            "resend" => Some(Provider::Resend),
            "postmark" => Some(Provider::Postmark),
            _ => None,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Provider::Brevo => "brevo",
            Provider::Resend => "resend",
            Provider::Postmark => "postmark",
        }
    }

    /// Every provider, so an error message can list what is on offer.
    pub const ALL: [Provider; 3] = [Provider::Brevo, Provider::Resend, Provider::Postmark];

    /// Where the send request goes.
    ///
    /// **A constant, never configuration.** A settable endpoint would let anyone
    /// with environment access redirect the API key to a host they own.
    pub fn endpoint(self) -> &'static str {
        match self {
            Provider::Brevo => "https://api.brevo.com/v3/smtp/email",
            Provider::Resend => "https://api.resend.com/emails",
            Provider::Postmark => "https://api.postmarkapp.com/email",
        }
    }

    /// The header carrying the key. Structurally different per provider, which
    /// is the reason this is an enum rather than a template.
    pub fn auth_header(self, key: &str) -> (&'static str, String) {
        match self {
            Provider::Brevo => ("api-key", key.to_string()),
            Provider::Resend => ("Authorization", format!("Bearer {key}")),
            Provider::Postmark => ("X-Postmark-Server-Token", key.to_string()),
        }
    }

    /// The request body.
    pub fn body(
        self,
        from: &str,
        from_name: &str,
        to: &str,
        subject: &str,
        text: &str,
    ) -> serde_json::Value {
        match self {
            Provider::Brevo => serde_json::json!({
                "sender": { "email": from, "name": from_name },
                "to": [{ "email": to }],
                "subject": subject,
                "textContent": text,
            }),
            Provider::Resend => serde_json::json!({
                "from": format!("{from_name} <{from}>"),
                "to": [to],
                "subject": subject,
                "text": text,
            }),
            Provider::Postmark => serde_json::json!({
                "From": format!("{from_name} <{from}>"),
                "To": to,
                "Subject": subject,
                "TextBody": text,
            }),
        }
    }
}

/// The subject line. A constant: nothing the sender chose reaches it.
pub const SUBJECT: &str = "Your sign-in link";

/// The message body.
///
/// **Fixed text with one URL interpolated.** No request text, no name, no
/// anything a stranger typed. This is what keeps an unauthenticated send
/// endpoint from being a channel for delivering messages, and it costs nothing:
/// the person who asked for the link knows why they asked.
///
/// Plain text rather than HTML: the server renders no HTML anywhere else, and
/// HTML mail would be one more escaping surface for no gain.
pub fn sign_in_body(link_url: &str) -> String {
    format!(
        "Someone asked to sign in to Smart Coder with this address.\n\n\
         Open this link within 15 minutes:\n\n  {link_url}\n\n\
         The link works once. If this was not you, ignore this message — \
         nothing has been created and no further mail will be sent.\n"
    )
}

/// How the server sends mail.
///
/// A trait so the decisions around sending — what is sent, when it is refused,
/// what the caller does when it fails — are testable without a network or an
/// account.
pub trait Mailer: Send + Sync {
    fn send(&self, to: &str, subject: &str, text: &str) -> Result<()>;
}

/// The real mailer.
pub struct HttpMailer {
    provider: Provider,
    api_key: String,
    from: String,
    from_name: String,
    agent: ureq::Agent,
}

impl HttpMailer {
    pub fn new(
        provider: Provider,
        api_key: impl Into<String>,
        from: impl Into<String>,
        from_name: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            api_key: api_key.into(),
            from: from.into(),
            from_name: from_name.into(),
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(std::time::Duration::from_secs(10)))
                // A redirect could carry the API key to wherever the response
                // points. The endpoints here never legitimately redirect.
                .max_redirects(0)
                .build()
                .into(),
        }
    }
}

impl Mailer for HttpMailer {
    fn send(&self, to: &str, subject: &str, text: &str) -> Result<()> {
        let (header, value) = self.provider.auth_header(&self.api_key);
        let body = self
            .provider
            .body(&self.from, &self.from_name, to, subject, text);

        self.agent
            .post(self.provider.endpoint())
            .header(header, &value)
            .send_json(&body)
            .map(|_| ())
            // The address is deliberately absent from the error: it is somebody's
            // personal data and this string lands in the container log.
            .map_err(|e| DcError::Backend(format!("{}: {e}", self.provider.slug())))
    }
}

/// A mailer that sends nothing and says so.
///
/// What runs when mail is not configured. Explicit rather than silently
/// succeeding, so a misconfigured server surfaces at the first signup instead of
/// leaving people waiting for a link that was never sent.
pub struct Unconfigured;

impl Mailer for Unconfigured {
    fn send(&self, _to: &str, _subject: &str, _text: &str) -> Result<()> {
        Err(DcError::Eval(
            "no mail provider is configured, so no sign-in link can be sent".to_string(),
        ))
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::sync::Mutex;

    /// Records what would have been sent.
    #[derive(Default)]
    pub struct Recording {
        pub sent: Mutex<Vec<(String, String, String)>>,
        /// When set, every send fails — a provider that is down.
        pub failing: bool,
    }

    impl Recording {
        pub fn failing() -> Self {
            Self {
                failing: true,
                ..Default::default()
            }
        }

        pub fn count(&self) -> usize {
            self.sent.lock().unwrap().len()
        }

        pub fn last_body(&self) -> Option<String> {
            self.sent.lock().unwrap().last().map(|s| s.2.clone())
        }
    }

    impl Mailer for Recording {
        fn send(&self, to: &str, subject: &str, text: &str) -> Result<()> {
            if self.failing {
                return Err(DcError::Backend("provider unreachable".into()));
            }
            self.sent
                .lock()
                .unwrap()
                .push((to.to_string(), subject.to_string(), text.to_string()));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_contains_nothing_the_sender_chose() {
        // The property that separates a bounded notification mailer from a
        // usable spam relay: a stranger cannot deliver a message through it,
        // because there is nowhere for their words to go.
        let body = sign_in_body("https://example.com/public/signin/abc123");
        assert!(body.contains("https://example.com/public/signin/abc123"));

        for injected in [
            "buy cheap watches",
            "click here to claim",
            "<script>",
            "Bcc:",
        ] {
            assert!(!body.contains(injected), "{injected}");
        }
        // And it says how to ignore it, because the recipient may not have asked.
        assert!(body.contains("was not you"), "{body}");
    }

    #[test]
    fn each_provider_authenticates_the_way_it_actually_does() {
        // The reason this is an enum rather than a configurable header name:
        // these are structurally different, not parameterised.
        let (h, v) = Provider::Brevo.auth_header("K");
        assert_eq!((h, v.as_str()), ("api-key", "K"));

        let (h, v) = Provider::Resend.auth_header("K");
        assert_eq!((h, v.as_str()), ("Authorization", "Bearer K"));

        let (h, v) = Provider::Postmark.auth_header("K");
        assert_eq!((h, v.as_str()), ("X-Postmark-Server-Token", "K"));
    }

    #[test]
    fn the_endpoint_is_not_configurable() {
        // A settable URL would let anyone with environment access redirect the
        // API key to a host they own.
        for p in Provider::ALL {
            let url = p.endpoint();
            assert!(url.starts_with("https://"), "{url}");
            assert!(!url.contains("localhost"), "{url}");
        }
        assert_eq!(
            Provider::Brevo.endpoint(),
            "https://api.brevo.com/v3/smtp/email"
        );
    }

    #[test]
    fn each_provider_gets_the_body_shape_it_expects() {
        let brevo = Provider::Brevo.body("f@x.com", "Smart Coder", "t@y.com", "S", "T");
        assert_eq!(brevo["sender"]["email"], "f@x.com");
        assert_eq!(brevo["to"][0]["email"], "t@y.com");
        assert_eq!(brevo["textContent"], "T");

        let resend = Provider::Resend.body("f@x.com", "Smart Coder", "t@y.com", "S", "T");
        assert_eq!(resend["from"], "Smart Coder <f@x.com>");
        assert_eq!(resend["to"][0], "t@y.com");

        let postmark = Provider::Postmark.body("f@x.com", "Smart Coder", "t@y.com", "S", "T");
        assert_eq!(postmark["To"], "t@y.com");
        assert_eq!(postmark["TextBody"], "T");
    }

    #[test]
    fn a_recipient_cannot_smuggle_a_header_through_the_json_encoder() {
        // Belt and braces with the structural validation in `account::valid_email`:
        // the encoder is the guarantee, the validation is the part a reader can
        // see.
        let hostile = "victim@x.com\r\nBcc: everyone@y.com";
        let body = Provider::Brevo.body("f@x.com", "SC", hostile, "S", "T");
        let encoded = serde_json::to_string(&body).unwrap();
        assert!(!encoded.contains("\r\n"), "{encoded}");
        assert!(encoded.contains("\\r\\n"), "escaped, not raw: {encoded}");
    }

    #[test]
    fn an_unknown_provider_is_refused_rather_than_defaulted() {
        // Defaulting would fail at the first signup, hours after the operator
        // stopped watching.
        assert_eq!(Provider::parse("brevo"), Some(Provider::Brevo));
        assert_eq!(Provider::parse("  BREVO "), Some(Provider::Brevo));
        assert_eq!(Provider::parse("sendgrid"), None);
        assert_eq!(Provider::parse(""), None);
    }

    #[test]
    fn an_unconfigured_mailer_fails_loudly_rather_than_pretending() {
        // Silently succeeding would leave people waiting for a link that was
        // never sent, with nothing in the log to say so.
        let err = Unconfigured
            .send("a@x.com", "S", "T")
            .expect_err("must not pretend");
        assert!(err.to_string().contains("no mail provider"), "{err}");
    }

    #[test]
    fn the_subject_is_a_constant() {
        assert_eq!(SUBJECT, "Your sign-in link");
    }

    #[test]
    fn the_recording_mailer_captures_what_would_have_gone_out() {
        // The seam the route tests use to assert that mail was — or crucially was
        // *not* — sent, without an account or a network.
        use testing::Recording;
        let m = Recording::default();
        assert_eq!(m.count(), 0);

        m.send("a@x.com", SUBJECT, &sign_in_body("https://x.test/l/abc"))
            .unwrap();
        assert_eq!(m.count(), 1);
        assert!(m.last_body().unwrap().contains("https://x.test/l/abc"));

        // And a provider that is down surfaces as an error rather than silently
        // counting as sent.
        let down = Recording::failing();
        assert!(down.send("a@x.com", SUBJECT, "T").is_err());
        assert_eq!(down.count(), 0);
    }
}
