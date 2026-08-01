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
}

/// The environment variables, named once so the error messages and the
/// documentation cannot disagree.
pub mod env {
    pub const BIND: &str = "SC_SERVER_BIND";
    pub const PORT: &str = "SC_SERVER_PORT";
    pub const DATA_DIR: &str = "SC_SERVER_DATA";
    pub const DAEMON_KEY: &str = "SC_SERVER_DAEMON_KEY";
    pub const ENROL_CODE: &str = "SC_SERVER_ENROL_CODE";
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
        if daemon_key.len() < 32 {
            return Err(format!(
                "{} is only {} characters. Use at least 32 — a short key looks \
                 configured while being guessable.",
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
        })
    }

    /// The address to bind.
    pub fn addr(&self) -> String {
        format!("{}:{}", self.bind, self.port)
    }
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
