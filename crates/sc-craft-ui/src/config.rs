//! The Crafter's settings, and the state directory **both** products resolve through.
//!
//! # Why this is not `sc_win::config::UiConfig`
//!
//! `UiConfig` is ~35 fields and about thirty of them configure a model: endpoints, API
//! keys, per-stage provider routing, swarm worker counts, permission posture, the verify
//! sandbox. The Crafter has no use for any of it, and could not honestly carry it — the
//! fields' types come from crates that are not in its dependency tree at all (spec 21).
//!
//! So the two products have two config types, and share the *plumbing*: [`state_dir`]
//! decides where settings live, and `sc_craft_ui::persist` reads and writes them. That
//! split is deliberate. A single struct with the agent half behind `cfg` attributes would
//! reintroduce exactly the compile-time weave the crate split exists to remove.
//!
//! # Two products, two directories
//!
//! [`Product`] picks the directory. `%APPDATA%\smart-coder\` for the agent,
//! `%APPDATA%\smart-coder-crafter\` for the Crafter. They do not share, and that is a
//! feature rather than an oversight:
//!
//! * The agent's `config.json` holds API keys. The Crafter never reads a file containing
//!   one — not "reads it and ignores the keys", but never opens it. The guarantee is
//!   about what the process touches, so sharing the file would weaken it for no gain.
//! * Both write `layout.json` and a recents list. Sharing those means two running apps
//!   fight over one file and the last writer wins.
//!
//! The cost is that installing the Crafter alongside the agent starts it with a fresh
//! window layout and no recent projects. That is the correct trade: they are different
//! applications that happen to share an editor.

use std::path::PathBuf;

/// Which product is running. Set once at startup by the binary's `main`, then read
/// through [`state_dir`].
///
/// This is the *only* runtime difference between the two builds, and it exists solely to
/// pick a directory. Nothing about capability is decided here — what the Crafter can do
/// is decided by what is linked into it, which is why there is no `is_craft()` predicate
/// for feature code to branch on. If you find yourself wanting one, the feature belongs
/// in one crate or the other, not behind a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Product {
    /// Smart Coder — the agent app. `%APPDATA%\smart-coder\`.
    SmartCoder,
    /// Smart Coder Crafter — the editor. `%APPDATA%\smart-coder-crafter\`.
    Crafter,
}

impl Product {
    /// The directory name under `%APPDATA%` (or the temp-dir fallback).
    pub fn dir_name(self) -> &'static str {
        match self {
            Product::SmartCoder => "smart-coder",
            Product::Crafter => "smart-coder-crafter",
        }
    }

    /// The product's display name, for the window title and the About box.
    pub fn display_name(self) -> &'static str {
        match self {
            Product::SmartCoder => "Smart Coder",
            Product::Crafter => "Smart Coder Crafter",
        }
    }
}

/// The running product. Defaults to [`Product::SmartCoder`] so anything that reads it
/// before `main` sets it — a test, a doc example — behaves as the agent build always did.
static PRODUCT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Declare which product this process is. Called once, first thing in `main`, before any
/// state is read.
///
/// Idempotent and thread-safe, but **not** meant to be called twice with different values:
/// paths resolved before the change would point at the other product's directory.
pub fn set_product(p: Product) {
    let v = match p {
        Product::SmartCoder => 0,
        Product::Crafter => 1,
    };
    PRODUCT.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// The running product.
pub fn product() -> Product {
    match PRODUCT.load(std::sync::atomic::Ordering::Relaxed) {
        1 => Product::Crafter,
        _ => Product::SmartCoder,
    }
}

/// The directory holding this product's `config.json`, `layout.json`, recents and logs:
/// `%APPDATA%\<product>\` on Windows, falling back to the system temp dir so there is
/// always *somewhere* to look.
///
/// `SC_STATE_DIR` overrides it, for tests. Without that override, anything calling a save
/// writes the DEVELOPER'S REAL state — the exact file whose corruption ("it switched back
/// to 8080 again") took a long time to track down. Overriding `APPDATA` instead is
/// process-wide, so it races other tests reading it.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SC_STATE_DIR") {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(product().dir_name())
}

/// The Crafter's settings.
///
/// Small on purpose. Every field here is about *editing* — there is nothing to configure
/// about a model, because there is no model. It grows only as the editor grows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CraftConfig {
    /// The command that builds/checks the open project (`cargo check`, `npm run build`).
    ///
    /// The Crafter has no agent to verify anything, so this is the ONLY way to find out
    /// whether the code compiles — which makes it the single most load-bearing setting in
    /// the product, not an afterthought. Blank ⇒ detect from the project kind.
    pub compile_command: Option<String>,

    /// The Unity editor path override (Settings ▸ General). Blank ⇒ find it via the Hub
    /// convention, which works on most machines; this is for the one where it doesn't.
    pub unity_path: Option<String>,

    /// The directory the editor opens in. Unlike the agent's workspace, this is a real
    /// project the user chose — nothing here generates files, so there is no scratch dir
    /// to isolate and no reason to avoid the user's own tree.
    pub workspace: Option<PathBuf>,
}

impl CraftConfig {
    /// Load from `<state_dir>/config.json`, falling back to defaults for anything absent,
    /// blank, or malformed. A bad file degrades to compiled defaults rather than failing
    /// the launch — the same rule the agent build uses.
    pub fn load() -> Self {
        let path = state_dir().join("config.json");
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Self::default();
        };
        let field = |k: &str| {
            v.get(k)
                .and_then(|x| x.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        Self {
            compile_command: field("compile_command"),
            unity_path: field("unity_path"),
            workspace: field("workspace").map(PathBuf::from),
        }
    }

    /// Persist to `<state_dir>/config.json`. Best-effort: a read-only directory loses the
    /// setting rather than crashing the editor.
    ///
    /// Unset fields are omitted rather than written as empty strings, so an untouched
    /// option reads back as "absent" (keep the default) instead of "set to nothing".
    pub fn save(&self) {
        let mut obj = serde_json::Map::new();
        let mut put = |k: &str, v: &Option<String>| {
            if let Some(s) = v.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                obj.insert(k.to_string(), serde_json::Value::String(s.to_string()));
            }
        };
        put("compile_command", &self.compile_command);
        put("unity_path", &self.unity_path);
        put(
            "workspace",
            &self
                .workspace
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
        );
        let dir = state_dir();
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(
            dir.join("config.json"),
            serde_json::Value::Object(obj).to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two products resolve to different directories — the whole point of [`Product`].
    #[test]
    fn each_product_has_its_own_directory() {
        assert_eq!(Product::SmartCoder.dir_name(), "smart-coder");
        assert_eq!(Product::Crafter.dir_name(), "smart-coder-crafter");
        assert_ne!(Product::SmartCoder.dir_name(), Product::Crafter.dir_name());
    }

    /// A config with nothing set writes no keys at all, rather than a file full of empty
    /// strings that would read back as "explicitly blank".
    #[test]
    fn an_empty_config_writes_no_keys() {
        let dir = std::env::temp_dir().join(format!("sc-craft-cfg-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("SC_STATE_DIR", &dir);
        CraftConfig::default().save();
        let text = std::fs::read_to_string(dir.join("config.json")).unwrap();
        assert_eq!(text, "{}", "no key should be written: {text}");
        std::env::remove_var("SC_STATE_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A malformed file degrades to defaults rather than failing the launch.
    #[test]
    fn a_corrupt_config_loads_as_default() {
        let dir = std::env::temp_dir().join(format!("sc-craft-bad-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("config.json"), "{not json").unwrap();
        std::env::set_var("SC_STATE_DIR", &dir);
        assert_eq!(CraftConfig::load(), CraftConfig::default());
        std::env::remove_var("SC_STATE_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
