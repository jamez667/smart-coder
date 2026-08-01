//! Configuration, from the environment.
//!
//! **Every setting is an environment variable**, because this ships as a Docker
//! image installed in Portainer and a stack editor is where a user configures it.
//! A config file baked into an image is the wrong shape: it cannot be edited
//! without rebuilding, and mounting one to override it makes two sources of truth.
//!
//! **All state lives under one directory** ([`Config::data_dir`]), so a Portainer
//! user has exactly one volume to mount and one thing to back up. State scattered
//! across several paths is a footgun — the backup that misses one of them looks
//! like it worked.

use std::path::PathBuf;

use crate::mail::Provider;

/// How short a secret may be before it is refused.
///
/// A short key is worse than no key: it looks configured while being guessable,
/// which is the failure mode nobody notices.
pub const MIN_SECRET_LEN: usize = 32;

/// How the server is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// What to bind. Defaults to all interfaces, because inside a container
    /// loopback would be unreachable from outside it — the isolation is the
    /// container's, not the bind's.
    pub bind: String,
    pub port: u16,
    /// The one volume: requests, drafted specs, and credentials.
    pub data_dir: PathBuf,
    /// The API key a daemon must present. **Required** — the server refuses to
    /// start without one rather than running open, because an unauthenticated
    /// intake surface on the public internet is the failure this whole design
    /// exists to prevent.
    pub daemon_key: String,
    /// The one-time code that enrols a browser. Generated and printed at startup
    /// when unset, so a fresh container is usable without pre-configuration but
    /// is never *open*.
    pub enrol_code: Option<String>,
    /// The public, self-serve filing surface — **absent unless asked for**.
    ///
    /// `Option` rather than a bool plus loose fields, so a route cannot read a
    /// half-configured setup: either every part needed to run a public surface is
    /// present, or the surface does not exist. The same reasoning that makes
    /// `daemon_key` required — the unsafe configuration should be impossible to
    /// express, not merely discouraged.
    pub public: Option<PublicConfig>,
}

/// What the public surface needs before it may exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicConfig {
    /// The repository public filings are attributed to.
    ///
    /// A name from *this* configuration, never from the request body — a
    /// stranger must not be able to aim work at a repository the operator did not
    /// nominate for public intake.
    pub repo: String,
    /// The absolute base URL sign-in links are built from.
    ///
    /// **Required, and not inferred from the `Host` header**, which is
    /// attacker-controlled: a link built from it would send the filer to whatever
    /// host the request claimed to be. Getting this wrong the other way emails
    /// somebody a link to `localhost`, which is merely useless.
    pub base_url: String,
    /// How to send the sign-in link.
    pub mail: MailConfig,
    /// The spam screener. `None` means file straight to `Queued` — a server that
    /// pretends to screen is worse than one that plainly does not.
    pub screen: Option<ScreenConfig>,
    /// How many unspent sign-in links may be outstanding at once.
    ///
    /// **The real ceiling on mail spend.** The rate limiter shapes traffic, but
    /// thirty a minute sustained is tens of thousands a day; this refuses before
    /// the mailer is called.
    pub max_outstanding_links: usize,
    /// Does a filer get to read the spec drafted from their request?
    ///
    /// Defaults **on**, which is a deliberate choice worth understanding: the
    /// drafted spec is model output produced by reading the developer's
    /// repository, and the filer wrote the prompt that produced it. Turn it off
    /// for a repository whose contents should not be described to strangers.
    pub show_spec: bool,
}

/// How the sign-in link is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailConfig {
    pub provider: Provider,
    pub api_key: String,
    pub from: String,
    pub from_name: String,
}

/// How filings are screened before they may be claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenConfig {
    pub api_key: String,
    pub url: String,
    pub model: String,
}

/// Where Gemini's OpenAI-compatible endpoint lives.
///
/// The default because it is what the rest of this project already points at for
/// a hosted planner, and its cheapest model is more than enough to sort spam
/// from a bug report.
pub const DEFAULT_SCREEN_URL: &str = "https://generativelanguage.googleapis.com/v1beta/openai";

/// Cheapest and fastest of the Gemini family.
pub const DEFAULT_SCREEN_MODEL: &str = "gemini-2.5-flash-lite";

/// The default ceiling on unspent sign-in links.
pub const DEFAULT_MAX_OUTSTANDING_LINKS: usize = 200;

/// The environment variables, named once so the error messages and the
/// documentation cannot disagree.
pub mod env {
    pub const BIND: &str = "SC_SERVER_BIND";
    pub const PORT: &str = "SC_SERVER_PORT";
    pub const DATA_DIR: &str = "SC_SERVER_DATA";
    pub const DAEMON_KEY: &str = "SC_SERVER_DAEMON_KEY";
    pub const ENROL_CODE: &str = "SC_SERVER_ENROL_CODE";

    /// Set to turn the public surface on. Everything below is then required.
    pub const PUBLIC_REPO: &str = "SC_SERVER_PUBLIC_REPO";
    pub const PUBLIC_BASE_URL: &str = "SC_SERVER_PUBLIC_BASE_URL";
    pub const PUBLIC_MAX_LINKS: &str = "SC_SERVER_PUBLIC_MAX_LINKS";
    pub const PUBLIC_SHOW_SPEC: &str = "SC_SERVER_PUBLIC_SHOW_SPEC";

    pub const MAIL_PROVIDER: &str = "SC_SERVER_MAIL_PROVIDER";
    pub const MAIL_KEY: &str = "SC_SERVER_MAIL_KEY";
    pub const MAIL_FROM: &str = "SC_SERVER_MAIL_FROM";
    pub const MAIL_FROM_NAME: &str = "SC_SERVER_MAIL_FROM_NAME";

    /// Optional. Absent means filings are not screened at all.
    pub const SCREEN_KEY: &str = "SC_SERVER_SCREEN_KEY";
    pub const SCREEN_URL: &str = "SC_SERVER_SCREEN_URL";
    pub const SCREEN_MODEL: &str = "SC_SERVER_SCREEN_MODEL";
}

impl Config {
    /// Read the configuration from the process environment.
    pub fn from_env() -> std::result::Result<Config, String> {
        Config::from_vars(|k| std::env::var(k).ok())
    }

    /// Read from an arbitrary lookup — the seam every test uses, so no test has
    /// to mutate the process environment and race every other test.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> std::result::Result<Config, String> {
        let daemon_key = get(env::DAEMON_KEY)
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .ok_or_else(|| {
                format!(
                    "{} is required. Generate one (32+ random characters) and set it on \
                     both this container and the daemon — without it the server would \
                     accept work from anyone.",
                    env::DAEMON_KEY
                )
            })?;

        // A short key is worse than no key: it looks configured while being
        // guessable, which is the failure mode nobody notices.
        if daemon_key.len() < MIN_SECRET_LEN {
            return Err(format!(
                "{} is only {} characters. Use at least {MIN_SECRET_LEN} — a short \
                 key looks configured while being guessable.",
                env::DAEMON_KEY,
                daemon_key.len()
            ));
        }

        let port = match get(env::PORT) {
            Some(p) => p
                .trim()
                .parse::<u16>()
                .map_err(|_| format!("{} must be a port number 1-65535, got {p:?}", env::PORT))?,
            None => 8420,
        };
        if port == 0 {
            return Err(format!("{} must not be 0", env::PORT));
        }

        Ok(Config {
            bind: get(env::BIND).unwrap_or_else(|| "0.0.0.0".to_string()),
            port,
            data_dir: get(env::DATA_DIR)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/data")),
            daemon_key,
            enrol_code: get(env::ENROL_CODE)
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty()),
            public: public_from(&get)?,
        })
    }

    /// The address to bind.
    pub fn addr(&self) -> String {
        format!("{}:{}", self.bind, self.port)
    }
}

/// Read a trimmed, non-blank setting. Blank is *absent*, not present-and-empty.
fn opt(get: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    get(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Build the public surface's configuration, or `None` if it was not asked for.
///
/// **Refuses to start half-public.** `SC_SERVER_PUBLIC_REPO` is the switch; once
/// it is set, everything the surface needs must be present. A server that
/// accepted filings but could not send a sign-in link would look configured
/// while being broken, which is the failure nobody notices until a stranger
/// reports that nothing arrived.
fn public_from(
    get: &impl Fn(&str) -> Option<String>,
) -> std::result::Result<Option<PublicConfig>, String> {
    let Some(repo) = opt(get, env::PUBLIC_REPO) else {
        // Not asked for. Every other public setting is ignored rather than
        // half-applied.
        return Ok(None);
    };

    let base_url = opt(get, env::PUBLIC_BASE_URL).ok_or_else(|| {
        format!(
            "{} is required when {} is set: a sign-in link needs an absolute URL, \
             and it must not be inferred from the Host header — that is \
             attacker-controlled, so a link built from it would send the filer to \
             whatever host the request claimed to be.",
            env::PUBLIC_BASE_URL,
            env::PUBLIC_REPO
        )
    })?;
    if !base_url.starts_with("https://") && !is_loopback(&base_url) {
        return Err(format!(
            "{} must be https:// — a sign-in link is a credential in a URL, and \
             plain HTTP puts it in the clear. (http://localhost is allowed for \
             trying it locally.)",
            env::PUBLIC_BASE_URL
        ));
    }

    Ok(Some(PublicConfig {
        repo,
        base_url: base_url.trim_end_matches('/').to_string(),
        mail: mail_from(get)?,
        screen: screen_from(get)?,
        max_outstanding_links: match opt(get, env::PUBLIC_MAX_LINKS) {
            Some(v) => v
                .parse()
                .map_err(|_| format!("{} must be a number, got {v:?}", env::PUBLIC_MAX_LINKS))?,
            None => DEFAULT_MAX_OUTSTANDING_LINKS,
        },
        // On unless explicitly turned off.
        show_spec: opt(get, env::PUBLIC_SHOW_SPEC)
            .map(|v| {
                !matches!(
                    v.to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
            })
            .unwrap_or(true),
    }))
}

fn mail_from(get: &impl Fn(&str) -> Option<String>) -> std::result::Result<MailConfig, String> {
    let raw = opt(get, env::MAIL_PROVIDER).ok_or_else(|| {
        format!(
            "{} is required when the public surface is on: signing in needs an \
             email. One of: {}.",
            env::MAIL_PROVIDER,
            Provider::ALL
                .iter()
                .map(|p| p.slug())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    let provider = Provider::parse(&raw).ok_or_else(|| {
        format!(
            "unknown mail provider {raw:?}. One of: {}.",
            Provider::ALL
                .iter()
                .map(|p| p.slug())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;

    let api_key = opt(get, env::MAIL_KEY)
        .ok_or_else(|| format!("{} is required to send sign-in links", env::MAIL_KEY))?;
    let from = opt(get, env::MAIL_FROM).ok_or_else(|| {
        format!(
            "{} is required: mail needs a sender address",
            env::MAIL_FROM
        )
    })?;

    Ok(MailConfig {
        provider,
        api_key,
        from,
        from_name: opt(get, env::MAIL_FROM_NAME).unwrap_or_else(|| "Smart Coder".to_string()),
    })
}

fn screen_from(
    get: &impl Fn(&str) -> Option<String>,
) -> std::result::Result<Option<ScreenConfig>, String> {
    // Absent is a legitimate choice: file straight to the queue and rely on the
    // account gate. Pretending to screen would be worse.
    let Some(api_key) = opt(get, env::SCREEN_KEY) else {
        return Ok(None);
    };
    if api_key.len() < MIN_SECRET_LEN {
        return Err(format!(
            "{} is only {} characters. Use at least {MIN_SECRET_LEN} — a short key \
             looks configured while being guessable.",
            env::SCREEN_KEY,
            api_key.len()
        ));
    }
    Ok(Some(ScreenConfig {
        api_key,
        url: opt(get, env::SCREEN_URL)
            .unwrap_or_else(|| DEFAULT_SCREEN_URL.to_string())
            .trim_end_matches('/')
            .to_string(),
        model: opt(get, env::SCREEN_MODEL).unwrap_or_else(|| DEFAULT_SCREEN_MODEL.to_string()),
    }))
}

/// Is this URL pointing at the machine it is running on?
fn is_loopback(url: &str) -> bool {
    let authority = url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or("");
    let host = match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn load(pairs: &[(&str, &str)]) -> std::result::Result<Config, String> {
        let map = vars(pairs);
        Config::from_vars(|k| map.get(k).cloned())
    }

    const GOOD_KEY: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn the_defaults_are_what_a_container_needs() {
        let cfg = load(&[(env::DAEMON_KEY, GOOD_KEY)]).unwrap();
        // All interfaces: inside a container, loopback is unreachable from
        // outside it, so binding loopback would make the image useless.
        assert_eq!(cfg.bind, "0.0.0.0");
        assert_eq!(cfg.port, 8420);
        // One directory, which is the one volume a Portainer user mounts.
        assert_eq!(cfg.data_dir, PathBuf::from("/data"));
        assert_eq!(cfg.addr(), "0.0.0.0:8420");
    }

    #[test]
    fn the_server_refuses_to_start_without_a_daemon_key() {
        // Running open is not a degraded mode, it is the failure this design
        // exists to prevent. `sc-web`'s `--no-token` has no equivalent here.
        let err = load(&[]).unwrap_err();
        assert!(err.contains(env::DAEMON_KEY), "{err}");
        assert!(err.contains("accept work from anyone"), "{err}");

        // Blank and whitespace are absent, not present-but-empty.
        assert!(load(&[(env::DAEMON_KEY, "")]).is_err());
        assert!(load(&[(env::DAEMON_KEY, "   ")]).is_err());
    }

    #[test]
    fn a_short_key_is_refused_rather_than_accepted_quietly() {
        // Worse than no key: it looks configured while being guessable, and
        // nobody notices until it matters.
        let err = load(&[(env::DAEMON_KEY, "hunter2")]).unwrap_err();
        assert!(err.contains("at least 32"), "{err}");
    }

    #[test]
    fn every_setting_is_overridable_from_the_environment() {
        // A Portainer stack editor sets environment variables; anything not
        // settable that way is not configurable in practice.
        let cfg = load(&[
            (env::DAEMON_KEY, GOOD_KEY),
            (env::BIND, "127.0.0.1"),
            (env::PORT, "9000"),
            (env::DATA_DIR, "/srv/state"),
            (env::ENROL_CODE, "let-me-in"),
        ])
        .unwrap();
        assert_eq!(cfg.addr(), "127.0.0.1:9000");
        assert_eq!(cfg.data_dir, PathBuf::from("/srv/state"));
        assert_eq!(cfg.enrol_code.as_deref(), Some("let-me-in"));
    }

    #[test]
    fn a_bad_port_is_a_clear_error_not_a_silent_default() {
        // Falling back to the default would leave the container listening
        // somewhere the user did not ask for, which they discover by the service
        // being unreachable.
        for bad in ["", "http", "70000", "-1"] {
            let err = load(&[(env::DAEMON_KEY, GOOD_KEY), (env::PORT, bad)]).unwrap_err();
            assert!(err.contains(env::PORT), "{bad:?}: {err}");
        }
    }

    /// The minimum a working public surface needs.
    fn public_vars() -> Vec<(&'static str, &'static str)> {
        vec![
            (env::DAEMON_KEY, GOOD_KEY),
            (env::PUBLIC_REPO, "intake"),
            (env::PUBLIC_BASE_URL, "https://specs.example.com"),
            (env::MAIL_PROVIDER, "brevo"),
            (env::MAIL_KEY, GOOD_KEY),
            (env::MAIL_FROM, "noreply@example.com"),
        ]
    }

    #[test]
    fn the_public_surface_is_off_unless_asked_for() {
        // A fresh container must not be an open intake form by accident.
        let cfg = load(&[(env::DAEMON_KEY, GOOD_KEY)]).unwrap();
        assert!(cfg.public.is_none());
    }

    #[test]
    fn the_server_refuses_to_start_half_public() {
        // Accepting filings while unable to send a sign-in link looks configured
        // while being broken — the failure nobody notices until a stranger
        // reports that nothing arrived.
        for missing in [
            env::PUBLIC_BASE_URL,
            env::MAIL_PROVIDER,
            env::MAIL_KEY,
            env::MAIL_FROM,
        ] {
            let vars: Vec<_> = public_vars()
                .into_iter()
                .filter(|(k, _)| *k != missing)
                .collect();
            let err = load(&vars).unwrap_err();
            assert!(err.contains(missing), "missing {missing}: {err}");
        }
    }

    #[test]
    fn a_fully_configured_public_surface_loads() {
        let cfg = load(&public_vars()).unwrap();
        let p = cfg.public.expect("configured");
        assert_eq!(p.repo, "intake");
        assert_eq!(p.base_url, "https://specs.example.com");
        assert_eq!(p.mail.provider, crate::mail::Provider::Brevo);
        assert_eq!(p.mail.from_name, "Smart Coder", "a sensible default");
        assert_eq!(p.max_outstanding_links, DEFAULT_MAX_OUTSTANDING_LINKS);
        // Screening is absent unless a key is given, and that is a legitimate
        // choice rather than a broken one.
        assert!(p.screen.is_none());
    }

    #[test]
    fn a_base_url_over_plain_http_is_refused_because_the_link_is_a_credential() {
        let mut vars = public_vars();
        vars.retain(|(k, _)| *k != env::PUBLIC_BASE_URL);
        vars.push((env::PUBLIC_BASE_URL, "http://specs.example.com"));
        let err = load(&vars).unwrap_err();
        assert!(err.contains("https"), "{err}");
        assert!(err.contains("in the clear"), "{err}");
    }

    #[test]
    fn a_loopback_base_url_over_http_is_allowed_for_trying_it_locally() {
        for url in ["http://localhost:8420", "http://127.0.0.1:8420"] {
            let mut vars = public_vars();
            vars.retain(|(k, _)| *k != env::PUBLIC_BASE_URL);
            vars.push((env::PUBLIC_BASE_URL, url));
            assert!(load(&vars).is_ok(), "{url}");
        }
    }

    #[test]
    fn an_unknown_mail_provider_names_the_ones_that_exist() {
        // "unknown provider" alone leaves the operator guessing at spellings.
        let mut vars = public_vars();
        vars.retain(|(k, _)| *k != env::MAIL_PROVIDER);
        vars.push((env::MAIL_PROVIDER, "sendgrid"));
        let err = load(&vars).unwrap_err();
        assert!(err.contains("brevo"), "{err}");
        assert!(err.contains("resend"), "{err}");
    }

    #[test]
    fn screening_is_configured_or_absent_but_never_half_present() {
        let mut vars = public_vars();
        vars.push((env::SCREEN_KEY, GOOD_KEY));
        let p = load(&vars).unwrap().public.unwrap();
        let s = p.screen.expect("configured");
        // Sensible defaults, so only the key is mandatory.
        assert_eq!(s.url, DEFAULT_SCREEN_URL);
        assert_eq!(s.model, DEFAULT_SCREEN_MODEL);

        // And a short key is refused rather than quietly accepted.
        let mut short = public_vars();
        short.push((env::SCREEN_KEY, "hunter2"));
        assert!(load(&short).unwrap_err().contains("at least"));
    }

    #[test]
    fn showing_the_spec_defaults_on_and_can_be_turned_off() {
        // On by default is a deliberate choice: the drafted spec describes the
        // developer's repository, and a filer wrote the prompt that produced it.
        assert!(load(&public_vars()).unwrap().public.unwrap().show_spec);

        for off in ["0", "false", "no", "OFF"] {
            let mut vars = public_vars();
            vars.push((env::PUBLIC_SHOW_SPEC, off));
            assert!(
                !load(&vars).unwrap().public.unwrap().show_spec,
                "{off} should turn it off"
            );
        }
    }

    #[test]
    fn public_settings_are_ignored_entirely_when_the_surface_is_off() {
        // Half-applied public settings on a private server would be a surprise;
        // the switch is one variable and everything else hangs off it.
        let cfg = load(&[
            (env::DAEMON_KEY, GOOD_KEY),
            (env::MAIL_PROVIDER, "nonsense"),
            (env::PUBLIC_BASE_URL, "http://not-https.example.com"),
        ])
        .expect("no public repo means no public surface, and no validation of it");
        assert!(cfg.public.is_none());
    }

    #[test]
    fn an_absent_enrol_code_is_none_so_one_can_be_generated() {
        // A fresh container should be usable without pre-configuration, but
        // never open — the caller generates and prints one when this is None.
        let cfg = load(&[(env::DAEMON_KEY, GOOD_KEY)]).unwrap();
        assert!(cfg.enrol_code.is_none());
        let blank = load(&[(env::DAEMON_KEY, GOOD_KEY), (env::ENROL_CODE, "  ")]).unwrap();
        assert!(blank.enrol_code.is_none(), "blank is absent, not empty");
    }
}
