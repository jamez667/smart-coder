//! The protocol's contract, proven against literal JSON.
//!
//! Every test here asserts against a **hand-written string**, not a round trip through
//! our own serializer. A round trip proves the types agree with themselves, which they
//! will whatever we rename; these assert the bytes on the wire, which is what a plugin
//! written in another language against the spec will actually send.

use super::*;

// ---------------------------------------------------------------------------
// The forward-compatibility rule
// ---------------------------------------------------------------------------

/// A message type this host has never heard of is ignored, not fatal.
///
/// **This is the rule the whole protocol rests on.** Without it every new message type
/// is a breaking change, and a plugin one version ahead kills its own session. Lifted
/// from `claudecode::parse_line`, whose comment gives the reason: the format belongs to
/// another project and will gain fields.
#[test]
fn an_unknown_message_type_is_ignored_not_fatal() {
    let from_the_future = r#"{"type":"decorations-set","path":"a.rs","ranges":[]}"#;
    assert_eq!(parse_plugin_line(from_the_future), Incoming::Unknown);
    assert_eq!(parse_host_line(from_the_future), Outgoing::Unknown);
}

/// Malformed JSON is ignored on the same terms. A plugin writing a stray `println!`
/// must not take down its own session.
#[test]
fn garbage_is_ignored_not_fatal() {
    for junk in [
        "",
        "   ",
        "not json",
        "{",
        r#"{"type":}"#,
        "Compiling sc-plugin-proto v0.0.0",
    ] {
        assert_eq!(parse_plugin_line(junk), Incoming::Unknown, "junk: {junk:?}");
        assert_eq!(parse_host_line(junk), Outgoing::Unknown, "junk: {junk:?}");
    }
}

/// An unknown *capability* leaves the rest of the manifest intact.
///
/// The difference between this and an unknown message matters: a manifest arrives once,
/// so failing it costs the whole plugin. A v2 plugin asking for `decorations` on a v1
/// host must still get its panels.
#[test]
fn an_unknown_capability_does_not_reject_the_manifest() {
    let line = r#"{"type":"initialized","manifest":{
        "id":"blame","name":"Git Blame","protocol_version":1,
        "capabilities":["buffer-read","decorations","telepathy"],
        "panels":[{"id":"blame","title":"Blame"}]}}"#;
    let Incoming::Message(m) = parse_plugin_line(line) else {
        panic!("must parse");
    };
    let PluginMessage::Initialized { manifest } = *m else {
        panic!("wrong variant");
    };
    assert_eq!(manifest.panels.len(), 1, "the panel survives");
    assert_eq!(
        manifest.capabilities,
        vec![Capability::BufferRead, Capability::Other, Capability::Other],
        "unknown capabilities land in Other rather than failing"
    );
}

/// An unknown content kind renders a placeholder rather than emptying the panel.
#[test]
fn an_unknown_content_kind_is_unsupported_not_an_error() {
    let line = r#"{"type":"panel-content","panel":"x","content":{"kind":"flame-graph"}}"#;
    let Incoming::Message(m) = parse_plugin_line(line) else {
        panic!("must parse");
    };
    let PluginMessage::PanelContent { content, .. } = *m else {
        panic!("wrong variant");
    };
    assert_eq!(content, Content::Unsupported);
}

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

/// The handshake, as a plugin in any language would write it.
#[test]
fn the_handshake_parses_from_literal_json() {
    let line = r#"{"type":"initialize","protocol_version":1,
        "workspace":"C:/proj","host_capabilities":["buffer-read","diagnostics"]}"#;
    let Outgoing::Message(m) = parse_host_line(line) else {
        panic!("must parse");
    };
    assert_eq!(
        *m,
        HostMessage::Initialize {
            protocol_version: 1,
            workspace: Some("C:/proj".to_string()),
            host_capabilities: vec![Capability::BufferRead, Capability::Diagnostics],
        }
    );
}

/// No project open is a real state, not an error — `workspace` is `null`, and the
/// plugin is expected to cope rather than to treat it as a failed handshake.
#[test]
fn a_handshake_with_no_workspace_is_valid() {
    let line =
        r#"{"type":"initialize","protocol_version":1,"workspace":null,"host_capabilities":[]}"#;
    let Outgoing::Message(m) = parse_host_line(line) else {
        panic!("must parse");
    };
    let HostMessage::Initialize { workspace, .. } = *m else {
        panic!("wrong variant");
    };
    assert_eq!(workspace, None);
}

/// A manifest with only the required fields works: every contribution list defaults to
/// empty. A plugin that contributes nothing is legal, and is what a plugin under
/// development looks like on its first run.
#[test]
fn a_minimal_manifest_needs_only_id_name_and_version() {
    let line = r#"{"type":"initialized","manifest":{"id":"x","name":"X","protocol_version":1}}"#;
    let Incoming::Message(m) = parse_plugin_line(line) else {
        panic!("must parse");
    };
    let PluginMessage::Initialized { manifest } = *m else {
        panic!("wrong variant");
    };
    assert!(manifest.panels.is_empty());
    assert!(manifest.commands.is_empty());
    assert!(manifest.capabilities.is_empty());
    assert!(manifest.subscriptions.is_empty());
}

/// A list row with just text serializes without the optional keys, so the common case
/// stays small on the wire and legible in a log.
#[test]
fn an_optional_field_is_omitted_not_nulled() {
    let item = ListItem::text("src/main.rs");
    let json = serde_json::to_string(&item).unwrap();
    assert_eq!(json, r#"{"text":"src/main.rs"}"#);
}

/// The full row shape, as the Blame plugin will send it.
#[test]
fn a_clickable_row_carries_its_command_and_args() {
    let item = ListItem::text("fix the parser")
        .with_detail("3 days ago")
        .with_command("blame.open-commit", vec!["a1b2c3".to_string()]);
    let json = serde_json::to_string(&item).unwrap();
    assert_eq!(
        json,
        r#"{"text":"fix the parser","detail":"3 days ago","command":"blame.open-commit","args":["a1b2c3"]}"#
    );
}

/// An edit's positions are 0-based text coordinates while a diagnostic's are 1-based
/// display coordinates. The inconsistency is deliberate (an off-by-one in an edit
/// corrupts a file; in a diagnostic it points at the wrong line), so it is pinned here
/// rather than left to be "fixed" by someone tidying.
#[test]
fn edits_are_zero_based_and_diagnostics_are_one_based() {
    let line = r#"{"type":"buffer-edit","id":1,"path":"a.rs","version":4,
        "edits":[{"start":{"line":0,"column":0},"end":{"line":0,"column":0},"text":"// hi\n"}]}"#;
    let Incoming::Message(m) = parse_plugin_line(line) else {
        panic!("must parse");
    };
    let PluginMessage::BufferEdit { edits, version, .. } = *m else {
        panic!("wrong variant");
    };
    assert_eq!(version, 4);
    assert_eq!(edits[0].start, Position { line: 0, column: 0 });

    let d = Diagnostic {
        line: 1,
        column: 1,
        severity: Severity::Error,
        code: Some("E0433".to_string()),
        message: "unresolved import".to_string(),
    };
    let json = serde_json::to_string(&d).unwrap();
    assert!(json.contains(r#""line":1"#), "1-based on the wire: {json}");
}

/// Error codes are a closed set a plugin can match on, and they survive the wire by
/// their kebab-case names.
#[test]
fn error_codes_round_trip_by_name() {
    let line =
        r#"{"type":"error-response","id":7,"code":"version-conflict","message":"buffer moved on"}"#;
    let Outgoing::Message(m) = parse_host_line(line) else {
        panic!("must parse");
    };
    let HostMessage::ErrorResponse { code, id, .. } = *m else {
        panic!("wrong variant");
    };
    assert_eq!(id, 7);
    assert_eq!(code, ErrorCode::VersionConflict);
}

/// `to_line` ends with exactly one newline — the framing the whole transport depends
/// on. A missing newline blocks the reader forever; two would produce a blank line the
/// far side counts as Unknown.
#[test]
fn to_line_frames_with_exactly_one_newline() {
    let s = to_line(&PluginMessage::Log {
        message: "hello".to_string(),
    });
    assert!(s.ends_with('\n'));
    assert!(!s.ends_with("\n\n"));
    assert_eq!(s.matches('\n').count(), 1);
}

/// A message with an embedded newline must not break the framing.
///
/// The failure this prevents is nasty and silent: a log line containing `\n` would
/// otherwise split into two lines, the first unparseable and the second arbitrary.
/// JSON escapes it, and this test is what stops someone "optimising" to a bare writer.
#[test]
fn an_embedded_newline_does_not_break_the_framing() {
    let s = to_line(&PluginMessage::Log {
        message: "line one\nline two".to_string(),
    });
    assert_eq!(
        s.matches('\n').count(),
        1,
        "only the framing newline: {s:?}"
    );
    assert_eq!(parse_plugin_line(&s), {
        Incoming::Message(Box::new(PluginMessage::Log {
            message: "line one\nline two".to_string(),
        }))
    });
}

// ---------------------------------------------------------------------------
// Identifiers and slugs
// ---------------------------------------------------------------------------

/// The panel slug shape, pinned.
///
/// This string is the spelling in `layout.json` AND the seed for the divider keys in
/// `splits.json`. The layout module already has a test asserting a single-pane layout
/// "generates the split ids it always did", because changing an id silently resets
/// every user's dividers. This is that test's mirror for plugin panels.
#[test]
fn the_panel_slug_shape_is_pinned() {
    let panel = PanelDecl {
        id: "blame".to_string(),
        title: "Blame".to_string(),
    };
    assert_eq!(panel.slug("git-blame"), "plugin:git-blame:blame");
}

/// The `plugin:` prefix is what keeps a plugin panel from colliding with the host's
/// own split ids, so nothing may be allowed to produce a bare slug.
#[test]
fn a_plugin_slug_cannot_collide_with_a_host_split_id() {
    for host_id in [
        "chat|code",
        "explorer:git|files",
        "explorer|body",
        "body|bottom",
    ] {
        let panel = PanelDecl {
            id: "x".to_string(),
            title: "X".to_string(),
        };
        assert_ne!(panel.slug("p"), host_id);
    }
    assert!(
        PanelDecl {
            id: "x".to_string(),
            title: "X".to_string()
        }
        .slug("p")
        .starts_with("plugin:"),
        "the prefix is the collision guarantee"
    );
}

#[test]
fn valid_ids_are_lowercase_alphanumeric_and_hyphens() {
    for ok in ["blame", "git-blame", "todo2", "a"] {
        assert!(manifest::is_valid_id(ok), "{ok} should be valid");
    }
}

/// Rejected characters, each for a concrete reason: a colon splits a slug into the
/// wrong parts, a space makes a `splits.json` key unreadable, and the rest are
/// inconsistency waiting to happen in a file people hand-edit.
#[test]
fn invalid_ids_are_rejected() {
    for bad in [
        "",
        "-lead",
        "trail-",
        "Upper",
        "with space",
        "with:colon",
        "wi/th",
    ] {
        assert!(!manifest::is_valid_id(bad), "{bad:?} should be invalid");
    }
}

// ---------------------------------------------------------------------------
// Content
// ---------------------------------------------------------------------------

/// The host counts elements before rendering, so a plugin cannot freeze the UI thread
/// by pushing 200,000 rows. Counting is cheap; building that widget tree is not, and a
/// frozen editor is indistinguishable from a crashed one.
#[test]
fn element_count_sees_through_nesting() {
    let c = Content::Stack {
        children: vec![
            Content::Text {
                markdown: "heading".to_string(),
            },
            Content::List {
                items: vec![ListItem::text("a"), ListItem::text("b")],
            },
        ],
    };
    assert_eq!(c.element_count(), 4, "1 stack + 1 text + 2 rows");
}

#[test]
fn empty_content_is_an_empty_list() {
    assert_eq!(Content::empty().element_count(), 0);
}

/// The four kinds, from literal JSON, as a plugin author reading the spec would write
/// them.
#[test]
fn every_content_kind_parses_from_literal_json() {
    let cases = [
        r#"{"kind":"list","items":[{"text":"a"}]}"#,
        r##"{"kind":"text","markdown":"# Title"}"##,
        r#"{"kind":"form","fields":[{"id":"q","label":"Query"}],"submit":"Go"}"#,
        r#"{"kind":"stack","children":[{"kind":"text","markdown":"x"}]}"#,
    ];
    for json in cases {
        let parsed: Content = serde_json::from_str(json).unwrap_or(Content::Unsupported);
        assert_ne!(parsed, Content::Unsupported, "should parse: {json}");
    }
}

/// A form field defaults to a visible, empty, non-secret input — so the minimal
/// declaration is two keys.
#[test]
fn a_form_field_needs_only_id_and_label() {
    let f: FormField = serde_json::from_str(r#"{"id":"q","label":"Query"}"#).unwrap();
    assert_eq!(f.value, "");
    assert_eq!(f.placeholder, None);
    assert!(!f.secret);
}

// ---------------------------------------------------------------------------
// v2, and the promise that v1 still works
// ---------------------------------------------------------------------------

/// **A v1 plugin runs unchanged on a v2 host.** Every v2 addition defaults to the v1
/// behaviour, which is what makes this additive rather than breaking — and is why a v1
/// message with none of the new keys still parses into exactly the old meaning.
#[test]
fn a_v1_message_still_parses_with_v1_meaning() {
    let v1 = r#"{"type":"panel-content","panel":"feed","content":{"kind":"list","items":[{"text":"a"}]}}"#;
    let Incoming::Message(m) = parse_plugin_line(v1) else {
        panic!("must parse");
    };
    let PluginMessage::PanelContent {
        scroll, content, ..
    } = *m
    else {
        panic!("wrong variant");
    };
    assert_eq!(scroll, None, "no scroll hint means leave it alone");
    let Content::List { items } = content else {
        panic!("wrong kind");
    };
    assert_eq!(items[0].severity, None, "no severity means ordinary");
}

/// A v1 form field is not submit-on-enter, so a v1 plugin's forms behave as they did.
#[test]
fn a_v1_form_field_does_not_submit_on_enter() {
    let f: FormField = serde_json::from_str(r#"{"id":"q","label":"Query"}"#).unwrap();
    assert!(!f.submit_on_enter);
}

/// The composer the Claude panel needs: Enter sends.
#[test]
fn submit_on_enter_survives_the_wire() {
    let json = r#"{"id":"task","label":"Task","submit_on_enter":true}"#;
    let f: FormField = serde_json::from_str(json).unwrap();
    assert!(f.submit_on_enter);
}

/// A streaming feed asks to stay pinned to its tail.
#[test]
fn a_scroll_hint_survives_the_wire() {
    let line = r#"{"type":"panel-content","panel":"feed",
        "content":{"kind":"list","items":[]},"scroll":"bottom"}"#;
    let Incoming::Message(m) = parse_plugin_line(line) else {
        panic!("must parse");
    };
    let PluginMessage::PanelContent { scroll, .. } = *m else {
        panic!("wrong variant");
    };
    assert_eq!(scroll, Some(Scroll::Bottom));
}

/// A scroll hint from a future version is ignored rather than fatal — the same
/// forward-compatibility rule the rest of the protocol follows.
#[test]
fn an_unknown_scroll_hint_is_ignored_not_fatal() {
    let line = r#"{"type":"panel-content","panel":"f",
        "content":{"kind":"list","items":[]},"scroll":"centre-on-cursor"}"#;
    let Incoming::Message(m) = parse_plugin_line(line) else {
        panic!("must parse rather than fail");
    };
    let PluginMessage::PanelContent { scroll, .. } = *m else {
        panic!("wrong variant");
    };
    assert_eq!(scroll, Some(Scroll::Other));
}

/// After a send, the plugin tells the host to empty the box. Without this the user
/// presses Enter on text that has already gone.
#[test]
fn clear_fields_parses() {
    let line = r#"{"type":"clear-fields","panel":"composer"}"#;
    let Incoming::Message(m) = parse_plugin_line(line) else {
        panic!("must parse");
    };
    assert_eq!(
        *m,
        PluginMessage::ClearFields {
            panel: "composer".to_string()
        }
    );
}

/// Severity reuses the diagnostics vocabulary rather than inventing a second one.
#[test]
fn a_row_severity_survives_the_wire() {
    let item = ListItem::text("write_file failed").with_severity(Severity::Error);
    let json = serde_json::to_string(&item).unwrap();
    assert!(json.contains(r#""severity":"error""#), "{json}");
    let back: ListItem = serde_json::from_str(&json).unwrap();
    assert_eq!(back.severity, Some(Severity::Error));
}

/// An ordinary row writes no severity key, so the common case stays small on the wire.
#[test]
fn an_ordinary_row_writes_no_severity() {
    let json = serde_json::to_string(&ListItem::text("ok")).unwrap();
    assert_eq!(json, r#"{"text":"ok"}"#);
}
