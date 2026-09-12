//! **The plugin host** (spec 25).
//!
//! Plugins are child processes speaking line-delimited JSON. They are discovered and
//! started at startup; enabling or disabling one takes a restart. That is what VS Code
//! does, and it is what makes this tractable: the plugin set is fixed before
//! `App::default()` runs, so the app is built once for that configuration and never
//! changes shape mid-render.
//!
//! # The pieces
//!
//! * [`discover`] — what is on disk, and why a directory was rejected.
//! * [`host`] — one running process: spawn, read, write, stop.
//! * [`registry`] — interned panel handles, so `PanelKind` stays `Copy`.
//! * [`view`] — flattening a plugin's content into drawable rows. Pure, like
//!   `markdown.rs`: the app builds the widgets.
//! * [`Plugins`] — this module's own type, owning the set and the startup sequence.
//!
//! # What the host guarantees
//!
//! **No plugin failure takes the editor with it.** Not a missing program, not a crash,
//! not a hang at handshake, not a stream of unparseable lines. Each is recorded and
//! shown in the Plugins panel; none is fatal, and none blocks startup beyond
//! [`host::HANDSHAKE_TIMEOUT`].
//!
//! That is only useful if it is visible, which is why the Plugins panel is part of this
//! work rather than a follow-up. A plugin that silently does not appear is
//! indistinguishable from one that was never installed.
//!
//! # What the host does not guarantee
//!
//! **Nothing about security.** A plugin is a subprocess with the user's full
//! privileges: it can read every file on the machine and make network calls. There is
//! no sandbox and no permission prompt. Plugins are trusted code, installed
//! deliberately, exactly like a shell script. The capability negotiation in the
//! handshake decides what the *host will answer*, not what the plugin can do.
//!
//! Spec 25 is direct about this for the same reason spec 22 declines to offer
//! `bypassPermissions`: a one-click path to unsupervised action is not a considered
//! decision.

pub mod discover;
pub mod host;
pub mod registry;
pub mod view;

pub use discover::{plugins_dir, Discovered, Scan};
pub use host::{Plugin, PluginEvent};
pub use registry::{PanelRegistry, PluginPanelId, RegisteredPanel};
pub use view::{flatten, Row};

use sc_plugin_proto::{Capability, HostMessage, PROTOCOL_VERSION};

/// What this host implements, advertised in the handshake.
///
/// A plugin reads this and degrades deliberately rather than discovering the gaps one
/// failed request at a time. v1 is six capabilities; decorations and language features
/// are absent by design (spec 25), and their absence here is how a plugin finds out.
pub const HOST_CAPABILITIES: &[Capability] = &[
    Capability::BufferRead,
    Capability::BufferEdit,
    Capability::FileRead,
    Capability::Diagnostics,
    Capability::EditorOpen,
    Capability::RunCommand,
];

/// Why a plugin is not running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The directory was unusable — bad `plugin.json`.
    Rejected(String),
    /// The process would not start.
    NotStarted(String),
    /// Started, but never answered the handshake.
    HandshakeTimeout,
    /// Speaks a protocol this build does not know.
    ///
    /// Refused **by name** rather than discovered as a stream of unparseable messages,
    /// so the Plugins panel can say "speaks protocol 3; this build understands 1".
    ProtocolMismatch { theirs: u32, ours: u32 },
    /// The user disabled it.
    Disabled,
    /// It exited or crashed.
    Stopped(String),
}

impl Failure {
    /// A sentence for the Plugins panel.
    pub fn describe(&self) -> String {
        match self {
            Failure::Rejected(why) => why.clone(),
            Failure::NotStarted(why) => why.clone(),
            Failure::HandshakeTimeout => format!(
                "did not answer the handshake within {}s",
                host::HANDSHAKE_TIMEOUT.as_secs()
            ),
            Failure::ProtocolMismatch { theirs, ours } => {
                format!("speaks protocol {theirs}; this build understands {ours}")
            }
            Failure::Disabled => "disabled".to_string(),
            Failure::Stopped(why) => why.clone(),
        }
    }
}

/// The set of plugins, running and failed.
#[derive(Default)]
pub struct Plugins {
    /// Running plugins, in load order.
    pub running: Vec<Plugin>,
    /// Plugins that are not running, and why. Shown in the Plugins panel so a plugin
    /// that failed is visible rather than merely absent.
    pub failed: Vec<(String, Failure)>,
    /// Command ids refused because another plugin claimed them first.
    ///
    /// Recorded rather than silently dropped: which plugin wins would otherwise depend
    /// on directory order, and a command that quietly does something else is worse than
    /// one that is missing.
    pub command_collisions: Vec<String>,
    /// Plugin panels dropped from the saved layout because their plugin is not loaded.
    ///
    /// A panel vanishing without explanation is the "silently halves the feed" failure
    /// the Claude driver counts skipped lines to avoid. The count turns a mystery into
    /// a sentence.
    pub dropped_layout_panels: usize,
}

impl Plugins {
    /// Discover, start, and handshake every plugin. Called once, at startup, before any
    /// layout is read.
    ///
    /// Returns the registry to install — deliberately returned rather than installed
    /// here, so the caller controls the ordering and so this is testable without
    /// touching a process-global.
    pub fn start(scan: Scan, workspace: Option<&std::path::Path>) -> (Self, PanelRegistry) {
        let mut plugins = Plugins {
            failed: scan
                .rejected
                .into_iter()
                .map(|r| (r.dir_name, Failure::Rejected(r.reason)))
                .collect(),
            ..Default::default()
        };
        let mut registry = PanelRegistry::default();
        let mut claimed_commands: Vec<String> = Vec::new();

        for found in scan.found {
            if !found.enabled {
                plugins.failed.push((found.dir_name, Failure::Disabled));
                continue;
            }
            let mut plugin = match Plugin::spawn(&found) {
                Ok(p) => p,
                Err(why) => {
                    plugins
                        .failed
                        .push((found.dir_name, Failure::NotStarted(why)));
                    continue;
                }
            };

            plugin.send(&HostMessage::Initialize {
                protocol_version: PROTOCOL_VERSION,
                workspace: workspace.map(|w| w.to_string_lossy().to_string()),
                host_capabilities: HOST_CAPABILITIES.to_vec(),
            });

            match await_handshake(&mut plugin) {
                Ok(()) => {}
                Err(f) => {
                    // `shutdown` rather than a bare drop: a plugin that started but did
                    // not handshake is still a live process, and leaving it orphaned is
                    // how a user ends up finding it in Task Manager.
                    plugin.shutdown();
                    plugins.failed.push((found.dir_name, f));
                    continue;
                }
            }

            if let Some(manifest) = plugin.manifest.clone() {
                for panel in &manifest.panels {
                    registry.register(&manifest.id, &panel.id, &panel.title);
                }
                for cmd in &manifest.commands {
                    if claimed_commands.contains(&cmd.id) {
                        plugins.command_collisions.push(cmd.id.clone());
                    } else {
                        claimed_commands.push(cmd.id.clone());
                    }
                }
            }
            plugins.running.push(plugin);
        }
        (plugins, registry)
    }

    /// Stop every plugin, politely then firmly.
    pub fn shutdown(&mut self) {
        for p in &mut self.running {
            p.shutdown();
        }
        self.running.clear();
    }
}

/// Wait for a plugin's handshake reply, or give up.
///
/// Blocking, deliberately, and bounded by [`host::HANDSHAKE_TIMEOUT`]. Startup is the
/// one place a synchronous wait is correct: the panel registry must be complete before
/// the layout is read, because a layout leaf naming a panel that has not been
/// registered yet cannot be resolved.
///
/// The cost is that N plugins serialize into N handshakes. That is a known issue rather
/// than an oversight — spec 25 flags Windows spawn cost as the thing to measure, and if
/// it is bad the answer is lazy activation, not a racier startup.
fn await_handshake(plugin: &mut Plugin) -> Result<(), Failure> {
    let started = std::time::Instant::now();
    while started.elapsed() < host::HANDSHAKE_TIMEOUT {
        for ev in plugin.drain() {
            match ev {
                PluginEvent::Ready(manifest) => {
                    if manifest.protocol_version != PROTOCOL_VERSION {
                        return Err(Failure::ProtocolMismatch {
                            theirs: manifest.protocol_version,
                            ours: PROTOCOL_VERSION,
                        });
                    }
                    plugin.handshake_ms = Some(started.elapsed().as_millis());
                    return Ok(());
                }
                PluginEvent::Stopped(r) => {
                    return Err(Failure::Stopped(match r {
                        Ok(()) => "exited during the handshake".to_string(),
                        Err(e) => e,
                    }));
                }
                // Anything else before the handshake is out of order. Kept in the log
                // rather than refused: a plugin that logs before declaring itself is
                // being eager, not broken.
                PluginEvent::Message(_) => {}
            }
        }
        if !plugin.is_running() {
            return Err(Failure::Stopped("exited during the handshake".to_string()));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    Err(Failure::HandshakeTimeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every failure explains itself in a sentence a user can act on. "Plugin failed to
    /// load" is the message this exists to prevent.
    #[test]
    fn every_failure_describes_itself() {
        let cases = [
            Failure::Rejected("plugin.json is not valid JSON".to_string()),
            Failure::NotStarted("blame.exe was not found".to_string()),
            Failure::HandshakeTimeout,
            Failure::ProtocolMismatch { theirs: 3, ours: 1 },
            Failure::Disabled,
            Failure::Stopped("exited with code 1".to_string()),
        ];
        for f in cases {
            let d = f.describe();
            assert!(!d.is_empty(), "{f:?} must describe itself");
            assert!(
                !d.contains("failed to load"),
                "{d:?} must say what happened"
            );
        }
    }

    /// A protocol mismatch names both versions, so the user knows whether to update the
    /// plugin or the editor.
    #[test]
    fn a_protocol_mismatch_names_both_versions() {
        let d = Failure::ProtocolMismatch { theirs: 3, ours: 1 }.describe();
        assert!(d.contains('3') && d.contains('1'), "{d}");
    }

    /// A disabled plugin is not started, and is listed rather than forgotten — the
    /// Plugins panel has to be able to offer it back.
    #[test]
    fn a_disabled_plugin_is_listed_but_not_started() {
        let scan = Scan {
            found: vec![Discovered {
                dir_name: "off".to_string(),
                dir: std::env::temp_dir(),
                command: std::path::PathBuf::from("does-not-matter"),
                args: Vec::new(),
                enabled: false,
            }],
            rejected: Vec::new(),
        };
        let (plugins, registry) = Plugins::start(scan, None);
        assert!(plugins.running.is_empty(), "nothing was spawned");
        assert_eq!(plugins.failed, vec![("off".to_string(), Failure::Disabled)]);
        assert!(registry.is_empty());
    }

    /// A plugin whose program is missing is recorded, and the others still load. One
    /// broken plugin must not hide every other.
    #[test]
    fn a_plugin_that_cannot_start_does_not_stop_the_others() {
        let scan = Scan {
            found: vec![Discovered {
                dir_name: "ghost".to_string(),
                dir: std::env::temp_dir(),
                command: std::path::PathBuf::from("sc-definitely-not-a-real-program"),
                args: Vec::new(),
                enabled: true,
            }],
            rejected: vec![discover::Rejected {
                dir_name: "broken".to_string(),
                reason: "plugin.json is not valid JSON".to_string(),
            }],
        };
        let (plugins, _) = Plugins::start(scan, None);
        assert!(plugins.running.is_empty());
        assert_eq!(plugins.failed.len(), 2, "both are reported, neither panics");
        let names: Vec<&str> = plugins.failed.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"ghost") && names.contains(&"broken"));
    }

    /// The six v1 capabilities, pinned. Adding one is a protocol change, and this test
    /// is what makes that deliberate rather than incidental.
    #[test]
    fn the_host_advertises_exactly_the_v1_capabilities() {
        assert_eq!(HOST_CAPABILITIES.len(), 6);
        for c in [
            Capability::BufferRead,
            Capability::BufferEdit,
            Capability::FileRead,
            Capability::Diagnostics,
            Capability::EditorOpen,
            Capability::RunCommand,
        ] {
            assert!(HOST_CAPABILITIES.contains(&c), "{c:?} must be advertised");
        }
        assert!(
            !HOST_CAPABILITIES.contains(&Capability::Other),
            "Other is a parse artefact, never advertised"
        );
    }
}
