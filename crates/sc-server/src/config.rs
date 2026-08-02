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
    /// Print sign-in links to the log instead of emailing them.
    ///
    /// **For looking at the surface locally.** A sign-in link is a credential, so
    /// this hands an account to anyone who can read the log.
    ///
    /// Guarded by the *base URL* rather than by the bind address: inside a
    /// container the bind is `0.0.0.0` whether or not anyone outside can reach
    /// it, so a loopback-bind check would reject exactly the case this exists
    /// for. The base URL is the address links are actually built from — if that
    /// is `localhost`, the links only work for somebody already on the machine,
    /// which is the same person who can read the log.
    pub mail_to_console: bool,
}

/// What the public surface needs before it may exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicConfig {
    /// The repository public filings are attributed to.
    ///
    /// A name from *this* configuration, never from the request body — a
    /// stranger must not be able to aim work at a repository the operator did not
    /// nominate for public intake.
    ///
    /// **A routing key, not a label.** The daemon matches it exactly against its
    /// own `queue add-repo` names, so it is not free text — see
    /// [`site_name`](Self::site_name) for what the masthead shows.
    pub repo: String,
    /// What the masthead calls this site. Defaults to [`repo`](Self::repo).
    ///
    /// Separate from the routing key because the two answer different questions.
    /// `repo` has to equal what the daemon was told, exactly; a heading wants
    /// `jamez667/smart-coder` — the name a person recognises. Folding them into
    /// one field would mean renaming the heading forces a matching
    /// `queue add-repo`, and a mismatch there is a queue that silently never
    /// drains.
    pub site_name: String,
    /// The absolute base URL sign-in links are built from.
    ///
    /// **Required, and not inferred from the `Host` header**, which is
    /// attacker-controlled: a link built from it would send the filer to whatever
    /// host the request claimed to be. Getting this wrong the other way emails
    /// somebody a link to `localhost`, which is merely useless.
    pub base_url: String,
    /// How to send the sign-in link.
    ///
    /// `None` only when [`Config::mail_to_console`] is on, which is refused
    /// unless the base URL is loopback. An `Option` rather than a placeholder
    /// `MailConfig`: a placeholder holding a real provider name and an empty key
    /// is one refactor away from silently constructing an `HttpMailer` that
    /// authenticates with nothing, and this way that does not compile.
    pub mail: Option<MailConfig>,
    /// The spam screener. `None` means file straight to `Queued` — a server that
    /// pretends to screen is worse than one that plainly does not.
    pub screen: Option<ScreenConfig>,
    /// How many unspent sign-in links may be outstanding at once.
    ///
    /// **The real ceiling on mail spend.** The rate limiter shapes traffic, but
    /// thirty a minute sustained is tens of thousands a day; this refuses before
    /// the mailer is called.
    pub max_outstanding_links: usize,
    /// How many requests one account may file per rolling 24 hours.
    ///
    /// **The ceiling on model spend**, and the only thing standing between a
    /// hostile account and the developer's bill. Every filing that clears the
    /// screener costs a full drafting run on the developer's machine, and the
    /// per-credential rate limit — 240 a minute — is no defence against
    /// something that expensive.
    ///
    /// Generous for a person and tight against a script: a real filer reporting
    /// a morning's worth of bugs does not reach it, and a loop hits it in
    /// seconds.
    pub max_daily_filings: usize,
    /// How many accounts may exist at all.
    ///
    /// Without this the per-account cap is weaker than it looks: an id the
    /// attacker cannot *vary* is one they can **re-mint**, and a script with a
    /// hundred disposable addresses holds a hundred budgets. This is the bound
    /// the filing cap is built on.
    ///
    /// Reached, signup stops and the lever is **raising this** — revoking does
    /// *not* make room, because revoked accounts still occupy their slot. A
    /// revoked address can never be re-created, so a freed slot could only be
    /// taken by a different one, and counting only live accounts would let an
    /// attacker's burned identities be swapped one for one under a wall that
    /// looks intact.
    ///
    /// A public form that quietly stops working is bad; one that silently scales
    /// to any number of budgets is worse.
    pub max_accounts: usize,
    /// Should cookies carry the `Secure` attribute?
    ///
    /// **On everywhere except a loopback base URL**, and derived rather than
    /// configured: a browser silently *discards* a `Secure` cookie sent over
    /// plain HTTP, so on `http://localhost` sign-in and the language switcher
    /// would both appear to do nothing at all — the request succeeds, the cookie
    /// vanishes, and the next page has forgotten. The symptom looks like a bug
    /// in the feature rather than a property of the cookie.
    ///
    /// Derived from the same check that already governs `http://` in
    /// [`PublicConfig::base_url`], so there is no setting to get wrong: a
    /// deployed server cannot be talked into dropping `Secure`, because its base
    /// URL must be `https://` to start at all.
    pub secure_cookies: bool,
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

/// The default per-account daily filing cap.
///
/// Twenty is a lot of genuine bug reports from one person in a day and nothing
/// at all to a script.
pub const DEFAULT_MAX_DAILY_FILINGS: usize = 20;

/// The default ceiling on how many accounts may exist.
///
/// High enough that a real audience never notices, low enough that a signup
/// flood stops somewhere the developer can see rather than growing until the
/// volume fills.
pub const DEFAULT_MAX_ACCOUNTS: usize = 1_000;

/// The window the filing cap is measured over.
///
/// Rolling rather than calendar: "resets at midnight" invites waiting for
/// midnight, and midnight in whose timezone has no good answer on a server that
/// holds no locale.
pub const FILING_WINDOW_MS: u64 = 24 * 60 * 60 * 1000;

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
    /// What the masthead calls this site. Defaults to `PUBLIC_REPO`.
    ///
    /// Separate from it because that one is a **routing key** the daemon matches
    /// exactly — a heading wants `owner/repo`, and renaming a heading should not
    /// force a matching `queue add-repo`.
    pub const PUBLIC_SITE_NAME: &str = "SC_SERVER_PUBLIC_SITE_NAME";
    pub const PUBLIC_MAX_LINKS: &str = "SC_SERVER_PUBLIC_MAX_LINKS";
    pub const PUBLIC_SHOW_SPEC: &str = "SC_SERVER_PUBLIC_SHOW_SPEC";
    /// Requests one account may file per rolling 24h — the model-spend ceiling.
    pub const PUBLIC_MAX_DAILY: &str = "SC_SERVER_PUBLIC_MAX_DAILY";
    /// How many accounts may exist — what the per-account cap rests on.
    pub const PUBLIC_MAX_ACCOUNTS: &str = "SC_SERVER_PUBLIC_MAX_ACCOUNTS";

    pub const MAIL_PROVIDER: &str = "SC_SERVER_MAIL_PROVIDER";
    pub const MAIL_KEY: &str = "SC_SERVER_MAIL_KEY";
    pub const MAIL_FROM: &str = "SC_SERVER_MAIL_FROM";
    pub const MAIL_FROM_NAME: &str = "SC_SERVER_MAIL_FROM_NAME";

    /// Print sign-in links to the log instead of sending them — **local only**.
    ///
    /// Honoured solely when `PUBLIC_BASE_URL` is a loopback address, so setting
    /// it on a deployed server is a startup error rather than a quiet downgrade
    /// to "anyone reading the log can sign in as anyone".
    pub const MAIL_TO_CONSOLE: &str = "SC_SERVER_MAIL_TO_CONSOLE";

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

        // `opt` for the same reason as `bind` and `data_dir` below: a stack
        // editor passes unconfigured settings through as empty strings, and an
        // empty port is "not set", not a parse error.
        let port = match opt(&get, env::PORT) {
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
            // `opt`, not `get`: **an empty value must mean "unset"**, not
            // "this is the value". A Compose file written for a stack editor
            // passes every setting through as `${NAME:-}`, so unconfigured ones
            // arrive as empty strings rather than as absent — and an empty
            // `SC_SERVER_DATA` overrides the image's own `/data` default, after
            // which the server tries to write to `/` and dies with a bare
            // "Permission denied" that names nothing.
            bind: opt(&get, env::BIND).unwrap_or_else(|| "0.0.0.0".to_string()),
            port,
            data_dir: opt(&get, env::DATA_DIR)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/data")),
            daemon_key,
            enrol_code: get(env::ENROL_CODE)
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty()),
            // Read again here rather than threaded out of `public_from`, which
            // returns `None` when the public surface is off — and the switch is
            // meaningless in that case anyway, since nothing sends mail.
            mail_to_console: flag(&get, env::MAIL_TO_CONSOLE),
            public: public_from(&get)?,
        })
    }

    /// The address to bind.
    pub fn addr(&self) -> String {
        format!("{}:{}", self.bind, self.port)
    }
}

/// Read a numeric limit, or its default.
///
/// **Zero is refused.** A cap of nought is a public surface that silently
/// accepts nothing, which reads to the operator as a broken feature rather than
/// a setting — and "turn it off" is expressed by not setting `PUBLIC_REPO` at
/// all, which turns the whole surface off honestly.
fn count(
    get: &impl Fn(&str) -> Option<String>,
    key: &str,
    default: usize,
) -> std::result::Result<usize, String> {
    let Some(raw) = opt(get, key) else {
        return Ok(default);
    };
    let n: usize = raw
        .parse()
        .map_err(|_| format!("{key} must be a whole number, got {raw:?}"))?;
    if n == 0 {
        return Err(format!(
            "{key} must not be 0 — that would accept nothing while looking \
             configured. To turn the public surface off, leave {} unset.",
            env::PUBLIC_REPO
        ));
    }
    Ok(n)
}

/// Read a boolean switch. **Off unless plainly turned on.**
///
/// Only the affirmative spellings count, so `SC_SERVER_MAIL_TO_CONSOLE=false`
/// means false rather than "a non-empty string, therefore true" — which is the
/// classic way a safety switch ends up on.
fn flag(get: &impl Fn(&str) -> Option<String>, key: &str) -> bool {
    matches!(
        opt(get, key).map(|v| v.to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
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
    if !base_url.starts_with("https://") && !is_private_host(&base_url) {
        return Err(format!(
            "{} must be https:// — a sign-in link is a credential in a URL, and \
             plain HTTP puts it in the clear. (http://localhost is allowed for \
             trying it locally.)",
            env::PUBLIC_BASE_URL
        ));
    }

    // Console mail waives the provider settings, because supplying a Brevo key
    // to a server that will not call Brevo is a hurdle with no purpose. The
    // placeholder below is never read: `build_mailer` picks `Console` first.
    //
    // Validated here rather than in `Config::from_vars` so that the base URL —
    // which is what the guard tests — is already parsed and checked.
    let to_console = flag(get, env::MAIL_TO_CONSOLE);
    if to_console && !is_private_host(&base_url) {
        return Err(format!(
            "{} prints sign-in links to the log, which hands an account to anyone \
             who can read it. It is honoured only when {} is a loopback address, \
             and this one is {base_url:?}. Configure a real mail provider.",
            env::MAIL_TO_CONSOLE,
            env::PUBLIC_BASE_URL
        ));
    }

    Ok(Some(PublicConfig {
        // Defaults to the routing key, so an operator who does not care sees a
        // sensible heading without configuring a second thing.
        site_name: opt(get, env::PUBLIC_SITE_NAME).unwrap_or_else(|| repo.clone()),
        repo,
        // Computed before `base_url` is moved, and from the same value the
        // `https://` check above ran on.
        secure_cookies: !is_private_host(&base_url),
        base_url: base_url.trim_end_matches('/').to_string(),
        mail: if to_console {
            None
        } else {
            Some(mail_from(get)?)
        },
        screen: screen_from(get)?,
        max_outstanding_links: count(get, env::PUBLIC_MAX_LINKS, DEFAULT_MAX_OUTSTANDING_LINKS)?,
        max_daily_filings: count(get, env::PUBLIC_MAX_DAILY, DEFAULT_MAX_DAILY_FILINGS)?,
        max_accounts: count(get, env::PUBLIC_MAX_ACCOUNTS, DEFAULT_MAX_ACCOUNTS)?,
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

/// The host part of a URL, without scheme, port or path.
///
/// Handles the bracketed form (`http://[::1]:8420`), because splitting an IPv6
/// authority on `:` otherwise returns an empty host and quietly fails open.
fn host_of(url: &str) -> &str {
    let authority = url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or("");
    match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    }
}

/// Is this URL somewhere a plain-HTTP sign-in link cannot leak from?
///
/// **Loopback, or a private network address.** The rule this relaxes is that a
/// sign-in link is a credential in a URL, so plain HTTP puts it in the clear —
/// true on the internet, and not true on a link that cannot be routed off the
/// network it was issued on. Somebody running this on their own LAN to try it
/// is not exposing anything to anyone who was not already on that LAN.
///
/// The ranges are the private ones from RFC 1918 and RFC 4193, plus link-local:
/// `10/8`, `172.16/12`, `192.168/16`, `169.254/16`, and IPv6 `fc00::/7` and
/// `fe80::/10`. Anything else — a public IP, a hostname, a DNS name that happens
/// to resolve privately — is treated as the internet, because this function only
/// sees a string and a name is not a promise about where it points.
fn is_private_host(url: &str) -> bool {
    let host = host_of(url);
    if matches!(host, "localhost" | "127.0.0.1" | "::1") {
        return true;
    }

    // IPv4 private ranges. Parsed rather than prefix-matched: "10.0.0.1" is
    // private and "100.0.0.1" is not, and `starts_with("10.")` gets that right
    // only by accident of the dot.
    let octets: Vec<u8> = host
        .split('.')
        .filter_map(|o| o.parse::<u8>().ok())
        .collect();
    if octets.len() == 4 && host.split('.').count() == 4 {
        return match (octets[0], octets[1]) {
            (10, _) => true,
            (172, b) => (16..=31).contains(&b),
            (192, 168) => true,
            // Link-local, which is what a machine gives itself with no DHCP.
            (169, 254) => true,
            _ => false,
        };
    }

    // IPv6 unique-local (fc00::/7) and link-local (fe80::/10).
    let lower = host.to_ascii_lowercase();
    lower.starts_with("fc") || lower.starts_with("fd") || lower.starts_with("fe80:")
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
        //
        // **Blank is not in this list**, and the distinction is the point: a
        // *typo* is a value that was meant to be a port and is not, while an
        // empty string is an unfilled box in a stack editor. Compose passes
        // every unconfigured setting through as `${NAME:-}`, so refusing blanks
        // here would make a bare deploy fail to start. See
        // `an_empty_value_means_unset_for_every_setting`.
        for bad in ["http", "70000", "-1", "8420 8421", "84.20"] {
            let err = load(&[(env::DAEMON_KEY, GOOD_KEY), (env::PORT, bad)]).unwrap_err();
            assert!(err.contains(env::PORT), "{bad:?}: {err}");
        }
        // Whitespace is blank too — a box somebody typed a space into.
        assert_eq!(
            load(&[(env::DAEMON_KEY, GOOD_KEY), (env::PORT, "  ")])
                .expect("whitespace is unset")
                .port,
            8420
        );
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
    fn the_portainer_stack_file_documents_every_setting() {
        // The deployment file is how a Portainer user learns this server exists
        // at all — it is the only documentation most installs will ever get.
        // Nothing else notices when it falls behind: the server starts fine
        // without a setting being *mentioned*, so the drift is silent and the
        // symptom is somebody not knowing a cap exists until it bites.
        //
        // Checked **both ways**. A missing setting is undocumented; a setting
        // named in the file but unknown to the server is worse, because a user
        // will paste it, see no error, and believe it took effect.
        let stack = include_str!("../../../deploy/sc-server.stack.yml");
        let source = include_str!("config.rs");
        let block = source
            .split("pub mod env {")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("the env module is declared in this file");

        let declared: Vec<&str> = block
            .split("pub const ")
            .skip(1)
            .filter_map(|s| s.split('"').nth(1))
            .collect();
        assert!(
            declared.len() > 10,
            "the env names did not parse: {declared:?}"
        );

        for name in &declared {
            assert!(
                stack.contains(name),
                "{name} is not mentioned in deploy/sc-server.stack.yml"
            );
        }

        // Consumed by **Compose**, not by the server: it substitutes the image
        // tag before the container exists. Named individually rather than
        // matched by a pattern, so the next addition has to be a deliberate
        // entry here instead of quietly slipping through a prefix rule.
        const NOT_THE_SERVERS: [&str; 1] = ["SC_SERVER_TAG"];

        // The reverse direction, by scanning the file for anything that looks
        // like one of ours.
        for word in stack.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            if word.starts_with("SC_SERVER_") && !NOT_THE_SERVERS.contains(&word) {
                assert!(
                    declared.contains(&word),
                    "{word} is in the stack file but the server never reads it"
                );
            }
        }

        // And the exception list itself must stay honest: an entry that *is* a
        // real setting would silently exempt it from the check above.
        for name in NOT_THE_SERVERS {
            assert!(
                !declared.contains(&name),
                "{name} is a real setting, so it does not belong in NOT_THE_SERVERS"
            );
        }
    }

    // -- console mail, the local-only escape hatch ---------------------------

    #[test]
    fn an_empty_value_means_unset_for_every_setting() {
        // **How a stack editor actually passes configuration.** A Compose file
        // written for Portainer lists every setting as `${NAME:-}` so the box
        // for it exists in the UI, which means unconfigured settings arrive as
        // empty strings rather than as absent.
        //
        // Getting this wrong was not theoretical: an empty SC_SERVER_DATA
        // overrode the image's own `/data`, and the server tried to write to `/`
        // and died with a bare "Permission denied" naming nothing. The others
        // fail more loudly but just as wrongly — an empty port is a parse error.
        let empties: Vec<(&str, &str)> = vec![
            (env::DAEMON_KEY, GOOD_KEY),
            (env::BIND, ""),
            (env::PORT, ""),
            (env::DATA_DIR, ""),
            (env::ENROL_CODE, ""),
            (env::PUBLIC_REPO, ""),
            (env::PUBLIC_BASE_URL, ""),
            (env::MAIL_PROVIDER, ""),
            (env::MAIL_KEY, ""),
            (env::MAIL_FROM, ""),
            (env::MAIL_TO_CONSOLE, ""),
            (env::SCREEN_KEY, ""),
            (env::PUBLIC_MAX_DAILY, ""),
            (env::PUBLIC_MAX_ACCOUNTS, ""),
            (env::PUBLIC_MAX_LINKS, ""),
            (env::PUBLIC_SHOW_SPEC, ""),
        ];
        let cfg = load(&empties).expect("every blank is treated as unset");

        // Each falls back to what it would have been with the variable absent.
        assert_eq!(cfg.bind, "0.0.0.0");
        assert_eq!(cfg.port, 8420);
        assert_eq!(cfg.data_dir, PathBuf::from("/data"));
        assert_eq!(cfg.enrol_code, None);
        assert!(!cfg.mail_to_console);
        // And a blank PUBLIC_REPO leaves the public surface off rather than
        // turning it on with an empty repository name.
        assert!(cfg.public.is_none());
    }

    #[test]
    fn a_private_network_address_is_told_apart_from_the_internet() {
        // The rule being relaxed is "plain HTTP puts a sign-in link in the
        // clear", which is true on the internet and not true of a link that
        // cannot be routed off the network it was issued on. So the boundary
        // has to be exact: an address one digit outside a private range is the
        // internet, and treating it as private would leak credentials.
        for private in [
            "http://localhost:8420",
            "http://127.0.0.1",
            "http://[::1]:8420",
            "http://10.0.0.1:8420",
            "http://10.255.255.255",
            "http://172.16.0.1",
            "http://172.31.255.1",
            "http://192.168.0.100:8420",
            "http://169.254.1.1",
            "http://[fd00::1]:8420",
            "http://[fe80::1]",
        ] {
            assert!(is_private_host(private), "{private} should be private");
        }

        for public in [
            "https://specs.example.com",
            "http://8.8.8.8",
            // Just outside 172.16/12 on both sides — the range a prefix match
            // gets wrong.
            "http://172.15.0.1",
            "http://172.32.0.1",
            // `starts_with("10.")` says private; parsing says otherwise.
            "http://100.0.0.1",
            // Not 192.168.
            "http://192.169.0.1",
            // A name is not a promise about where it resolves.
            "http://internal.example.com",
            // Craftable lookalikes.
            "http://10.0.0.1.example.com",
            "http://192.168.0.1.attacker.test",
        ] {
            assert!(!is_private_host(public), "{public} should NOT be private");
        }
    }

    #[test]
    fn a_lan_deployment_may_serve_the_public_surface_over_plain_http() {
        // Running it on your own LAN to try it out is a real case, and neither
        // localhost nor the internet. Refusing it forced a choice between not
        // trying the feature and putting a self-signed certificate in front.
        let cfg = load(&[
            (env::DAEMON_KEY, GOOD_KEY),
            (env::PUBLIC_REPO, "intake"),
            (env::PUBLIC_BASE_URL, "http://192.168.0.100:8420"),
            (env::MAIL_TO_CONSOLE, "1"),
        ])
        .expect("a private address may serve plain HTTP");
        let public = cfg.public.unwrap();
        // And cookies drop `Secure` there for the same reason they do on
        // localhost: a browser discards a `Secure` cookie sent over plain HTTP,
        // so keeping it would make sign-in appear to silently do nothing.
        assert!(!public.secure_cookies);
    }

    #[test]
    fn plain_http_on_a_public_address_is_still_refused() {
        // The guard that must not have been weakened by the above.
        let err = load(&[
            (env::DAEMON_KEY, GOOD_KEY),
            (env::PUBLIC_REPO, "intake"),
            (env::PUBLIC_BASE_URL, "http://specs.example.com"),
        ])
        .unwrap_err();
        assert!(err.contains("https://"), "{err}");
        assert!(err.contains("in the clear"), "{err}");
    }

    #[test]
    fn console_mail_is_refused_on_a_deployed_base_url() {
        // The guard that keeps this out of production. It prints sign-in links —
        // which are credentials — so on a reachable host it would hand an
        // account to anyone who can read the log.
        let err = load(&[
            (env::DAEMON_KEY, GOOD_KEY),
            (env::PUBLIC_REPO, "intake"),
            (env::PUBLIC_BASE_URL, "https://specs.example.com"),
            (env::MAIL_TO_CONSOLE, "1"),
        ])
        .unwrap_err();
        assert!(err.contains(env::MAIL_TO_CONSOLE), "{err}");
        assert!(err.contains("read it"), "{err}");
    }

    #[test]
    fn console_mail_waives_the_provider_settings_on_loopback() {
        // The point of the switch: looking at the surface locally must not
        // require an API key for a third party.
        let cfg = load(&[
            (env::DAEMON_KEY, GOOD_KEY),
            (env::PUBLIC_REPO, "intake"),
            (env::PUBLIC_BASE_URL, "http://localhost:8420"),
            (env::MAIL_TO_CONSOLE, "true"),
        ])
        .unwrap();
        assert!(cfg.mail_to_console);
        // `None`, not a placeholder: there is no provider to fall back to, so no
        // later branch can quietly construct an `HttpMailer` with an empty key.
        assert!(cfg.public.unwrap().mail.is_none());
    }

    #[test]
    fn console_mail_is_off_unless_plainly_turned_on() {
        // The classic way a safety switch ends up on is "non-empty means true",
        // which reads `MAIL_TO_CONSOLE=false` as yes.
        //
        // Asserted on a **loopback** base URL deliberately. With a deployed one
        // the guard would reject a wrong reading with an error, and this test
        // would pass on the panic rather than on the property — reporting the
        // right result for the wrong reason.
        for value in ["false", "0", "no", "off", "", "  ", "maybe"] {
            let cfg = load(&[
                (env::DAEMON_KEY, GOOD_KEY),
                (env::PUBLIC_REPO, "intake"),
                (env::PUBLIC_BASE_URL, "http://localhost:8420"),
                (env::MAIL_PROVIDER, "brevo"),
                (env::MAIL_KEY, GOOD_KEY),
                (env::MAIL_FROM, "noreply@example.com"),
                (env::MAIL_TO_CONSOLE, value),
            ])
            .unwrap_or_else(|e| panic!("{value:?} was read as on: {e}"));
            assert!(!cfg.mail_to_console, "{value:?} turned it on");
            // And the real provider is still required and read.
            assert!(cfg.public.unwrap().mail.is_some(), "{value:?}");
        }
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
        let mail = p.mail.as_ref().expect("a provider is configured");
        assert_eq!(mail.provider, crate::mail::Provider::Brevo);
        assert_eq!(mail.from_name, "Smart Coder", "a sensible default");
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
    fn the_spend_ceilings_have_defaults_and_are_overridable() {
        let p = load(&public_vars()).unwrap().public.unwrap();
        assert_eq!(p.max_daily_filings, DEFAULT_MAX_DAILY_FILINGS);
        assert_eq!(p.max_accounts, DEFAULT_MAX_ACCOUNTS);

        let mut vars = public_vars();
        vars.push((env::PUBLIC_MAX_DAILY, "5"));
        vars.push((env::PUBLIC_MAX_ACCOUNTS, "50"));
        let p = load(&vars).unwrap().public.unwrap();
        assert_eq!(p.max_daily_filings, 5);
        assert_eq!(p.max_accounts, 50);
    }

    #[test]
    fn a_cap_of_zero_is_refused_rather_than_silently_accepting_nothing() {
        // A public surface that accepts nothing reads as a broken feature, not a
        // setting — and "off" is expressed by leaving PUBLIC_REPO unset, which
        // turns the whole surface off honestly.
        for key in [
            env::PUBLIC_MAX_DAILY,
            env::PUBLIC_MAX_ACCOUNTS,
            env::PUBLIC_MAX_LINKS,
        ] {
            let mut vars = public_vars();
            vars.push((key, "0"));
            let err = load(&vars).unwrap_err();
            assert!(err.contains(key), "{err}");
            assert!(err.contains("accept nothing"), "{err}");
        }
    }

    #[test]
    fn a_non_numeric_cap_is_a_clear_error() {
        let mut vars = public_vars();
        vars.push((env::PUBLIC_MAX_DAILY, "lots"));
        let err = load(&vars).unwrap_err();
        assert!(err.contains("whole number"), "{err}");
        assert!(err.contains("lots"), "names what was given: {err}");
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
