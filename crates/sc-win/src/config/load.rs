//! Loading and persisting [`UiConfig`]: the env → config.json → compiled-default
//! precedence chain, the pre-connections migration, and the write-back.

use super::file::{
    config_file, is_gemini_url, parse_config, serialize_config, ConfigFields,
    GEMINI_OPENAI_BASE_URL,
};
use super::types::{Mode, Provider, UiConfig};

impl UiConfig {
    /// The default config, then the machine-local endpoint/model layered on top.
    ///
    /// Precedence (highest first): env `SC_BASE_URL`/`SC_MODEL` → `config.json`
    /// (`%APPDATA%\smart-coder\config.json`) → the neutral compiled [`Default`], so the
    /// endpoint the GUI talks to is never hard-coded in the repo — swap models by editing the git-ignored
    /// JSON (or exporting an env var), with zero source churn.
    pub fn load() -> Self {
        let mut cfg = Self::default();
        let file = std::fs::read_to_string(config_file())
            .map_or_else(|_| ConfigFields::default(), |t| parse_config(&t));

        // For each field: env wins over file wins over default; each layer only overrides
        // when present and non-blank. `set` applies an `Option<String>` onto a required field
        // (base_url/model); `set_opt` onto an optional one (the URLs/models/keys that stay
        // `None` unless configured).
        let env = |k: &str| std::env::var(k).ok().filter(|s| !s.trim().is_empty());
        let set = |dst: &mut String, v: Option<String>| {
            if let Some(v) = v.filter(|s| !s.trim().is_empty()) {
                *dst = v;
            }
        };
        let set_opt = |dst: &mut Option<String>, v: Option<String>| {
            if let Some(v) = v.filter(|s| !s.trim().is_empty()) {
                *dst = Some(v);
            }
        };

        // How the app works (spec 21). `SC_MODE=craft` lets an org pin Craft mode without
        // hand-editing config.json. An unparseable value stays `None` — "never chosen" — so a
        // corrupt file asks again at startup rather than silently picking a mode.
        cfg.mode = if cfg!(feature = "craft-only") {
            // A craft-only build has one mode, and neither the env var nor a config.json carried
            // over from an ordinary build may contradict it. `craft()` already answers true
            // regardless, so this is belt-and-braces — but leaving `Some(Assistant)` in the
            // struct would be a live trap for any later code that reads `mode` directly instead
            // of going through the predicate.
            Some(Mode::Craft)
        } else {
            env("SC_MODE")
                .or(file.mode)
                .as_deref()
                .and_then(Mode::from_slug)
        };

        set(&mut cfg.base_url, env("SC_BASE_URL").or(file.base_url));
        set(&mut cfg.model, env("SC_MODEL").or(file.model));
        // Coder API key: SC_KEY (or the conventional GEMINI_API_KEY) → config.json.
        set_opt(
            &mut cfg.key,
            env("SC_KEY").or_else(|| env("GEMINI_API_KEY")).or(file.key),
        );
        // The planner (orchestrator) endpoint/model/key — the Gemini-as-planner path.
        set_opt(
            &mut cfg.orchestrator_url,
            env("SC_ORCH_URL").or(file.orchestrator_url),
        );
        set_opt(
            &mut cfg.orchestrator_model,
            env("SC_ORCH_MODEL").or(file.orchestrator_model),
        );
        // Orchestrator key falls back to GEMINI_API_KEY too, so a single env var lights up a
        // Gemini planner without also forcing the coder onto it.
        set_opt(
            &mut cfg.orchestrator_key,
            env("SC_ORCH_KEY")
                .or_else(|| env("GEMINI_API_KEY"))
                .or(file.orchestrator_key),
        );
        // The endpoint-agnostic knobs (spec 21). File-only — none has ever had an env override,
        // and inventing one here would be scope this change doesn't need. Absent leaves the
        // compiled default in place, which for the two flags means OFF: a config that lost its
        // `yolo` key must not come back with permissions loosened.
        set_opt(&mut cfg.verify_command, file.verify_command);
        set_opt(&mut cfg.unity_path, file.unity_path);
        if let Some(v) = file.yolo {
            cfg.yolo = v;
        }
        if let Some(v) = file.dry_run {
            cfg.dry_run = v;
        }
        // The sandbox image and on/off are env-overridable too, so a machine can point the
        // terminal/agent at a project-appropriate image (e.g. a rust image) without editing
        // config.json. `SC_USE_DOCKER=0/false` forces host mode.
        if let Ok(img) = std::env::var("SC_DOCKER_IMAGE") {
            if !img.trim().is_empty() {
                cfg.docker_image = img;
            }
        }
        if let Ok(v) = std::env::var("SC_USE_DOCKER") {
            let v = v.trim().to_ascii_lowercase();
            cfg.use_docker = !matches!(v.as_str(), "0" | "false" | "no" | "off");
        }

        // --- Build the connection layer, migrating a pre-connections config.json ---
        //
        // New configs carry `local_*`/`gemini_*`/`*_provider`. Older ones only have the flat
        // `base_url`/`key`/`orchestrator_*` scalars (already loaded above with env layered on).
        // We derive connections from whichever is present so an old file keeps working AND, on the
        // next save, is written in the new shape.

        // Local connection: its own field, else the (already env/file-resolved) coder endpoint.
        cfg.local_conn.base_url = file
            .local_url
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| cfg.base_url.clone());
        cfg.local_conn.key = file
            .local_key
            .clone()
            .filter(|s| !s.trim().is_empty())
            // Migration: the flat `key` was the LOCAL coder key in old configs. But if the coder
            // was actually pointed at Gemini (base_url == Gemini), that key belongs to Gemini, not
            // Local — don't copy it onto Local.
            .or_else(|| {
                if is_gemini_url(&cfg.base_url) {
                    None
                } else {
                    cfg.key.clone()
                }
            });

        // Gemini connection: its own fields, else migrate from the orchestrator_* (or the coder if
        // the coder itself was on Gemini), else the preset url + env key.
        cfg.gemini_conn.base_url = file
            .gemini_url
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| cfg.orchestrator_url.clone().filter(|u| is_gemini_url(u)))
            .or_else(|| Some(cfg.base_url.clone()).filter(|u| is_gemini_url(u)))
            .unwrap_or_else(|| GEMINI_OPENAI_BASE_URL.to_string());
        cfg.gemini_conn.key = file
            .gemini_key
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| cfg.orchestrator_key.clone())
            // If the coder was on Gemini, its `key` is the Gemini key.
            .or_else(|| {
                if is_gemini_url(&cfg.base_url) {
                    cfg.key.clone()
                } else {
                    None
                }
            })
            .or_else(|| env("GEMINI_API_KEY"));

        // Per-stage routing: explicit slugs, else migrate by looking at which endpoint each stage
        // pointed at. Coder: Gemini iff its base_url is the Gemini endpoint. Planner: Gemini iff it
        // had a Gemini orchestrator_url OR (no orchestrator override AND the coder was on Gemini).
        cfg.coder_provider = file
            .coder_provider
            .as_deref()
            .and_then(Provider::from_slug)
            .unwrap_or(if is_gemini_url(&cfg.base_url) {
                Provider::Gemini
            } else {
                Provider::Local
            });
        cfg.planner_provider = file
            .planner_provider
            .as_deref()
            .and_then(Provider::from_slug)
            .unwrap_or_else(|| match &cfg.orchestrator_url {
                Some(u) if is_gemini_url(u) => Provider::Gemini,
                Some(_) => Provider::Local,
                None => cfg.coder_provider, // no override ⇒ same as coder
            });
        cfg.advisor_provider = file
            .advisor_provider
            .as_deref()
            .and_then(Provider::from_slug)
            .unwrap_or(cfg.coder_provider);

        // Flatten connections+routing back into the scalar fields the builders read, so `load()`'s
        // result is internally consistent regardless of which shape the file was in.
        cfg.resolve_stages();
        cfg
    }

    /// Persist the connection fields to `%APPDATA%\smart-coder\config.json` (best-effort — a
    /// write failure is silently ignored, like [`crate::persist::save`]). This is what makes a
    /// Gemini-planner setup entered in the settings panel survive a restart: previously the file
    /// was read-only (hand-edited) so nothing the UI changed was ever written back.
    ///
    /// Stores the connection/routing shape AND the endpoint-agnostic knobs — the verify command,
    /// the posture flags, the Unity path. Those last four used to reset on every restart, which
    /// spec 21 called out: a setting the app forgets is one the user re-enters forever. env vars
    /// still override on the next `load()`.
    pub fn save_config(&self) {
        let fields = ConfigFields {
            // How the app works (spec 21). This MUST be persisted: an unsaved mode would reset
            // on restart, which the user would rightly read as the app ignoring their answer.
            // `None` (never chosen) writes nothing, so the first-run prompt still fires.
            //
            // A craft-only build never writes it. The guard belongs HERE rather than only at the
            // two mode-writing messages, because this function also runs on ordinary connection
            // edits — so a craft-only build saving a Gemini key would otherwise stamp a `mode`
            // into config.json that a later ordinary build would silently honour, pinning a user
            // into Craft with no record of having chosen it.
            mode: self
                .mode
                .filter(|_| self.mode_switchable())
                .map(|m| m.slug().to_string()),
            // The connection + routing shape (the authoring surface).
            local_url: Some(self.local_conn.base_url.clone()),
            local_key: self.local_conn.key.clone(),
            gemini_url: Some(self.gemini_conn.base_url.clone()),
            gemini_key: self.gemini_conn.key.clone(),
            coder_provider: Some(self.coder_provider.slug().to_string()),
            planner_provider: Some(self.planner_provider.slug().to_string()),
            advisor_provider: Some(self.advisor_provider.slug().to_string()),
            // The resolved scalars too — so an older build, the CLI, or a hand-editor still reads a
            // working endpoint/model from the same file. `orchestrator_model` is the planner model.
            base_url: Some(self.base_url.clone()),
            model: Some(self.model.clone()),
            key: self.key.clone(),
            orchestrator_url: self.orchestrator_url.clone(),
            orchestrator_model: self.orchestrator_model.clone(),
            orchestrator_key: self.orchestrator_key.clone(),
            // The endpoint-agnostic knobs. Each written only when it differs from the compiled
            // default, so the file stays small and an unset key keeps meaning "use the default"
            // rather than freezing today's default into every user's config.
            verify_command: self.verify_command.clone(),
            unity_path: self.unity_path.clone(),
            yolo: self.yolo.then_some(true),
            dry_run: self.dry_run.then_some(true),
        };
        let path = config_file();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, serialize_config(&fields));
    }
}
