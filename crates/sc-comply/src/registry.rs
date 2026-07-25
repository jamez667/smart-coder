//! The shipped pack collection, embedded at compile time.
//!
//! Packs are `include_str!`'d rather than read from disk so `smart-coder comply
//! --pack soc2` works from any directory against any workspace. A path is still
//! accepted for packs a user has authored themselves.
//!
//! Adding a pack means adding one entry here. The
//! `every_shipped_pack_loads_and_has_no_blocking_findings` test in
//! `sc-comply-author` enumerates the packs *directory*, so a pack added to disk
//! but forgotten here is caught by [`shipped_registry_covers_the_packs_dir`].

use sc_proto::{DcError, Result};

use crate::pack::Pack;

/// A framework pack shipped with the tool.
pub struct ShippedPack {
    /// Short name for `--pack <name>`.
    pub name: &'static str,
    /// One-line description for the pack list.
    pub summary: &'static str,
    /// The TOML source, embedded.
    pub source: &'static str,
}

/// Every pack shipped with the tool, in rough order of how much source-visible
/// evidence each carries — the useful ones first.
pub const SHIPPED: &[ShippedPack] = &[
    ShippedPack {
        name: "soc2",
        summary: "SOC 2 Trust Services Criteria (AICPA) — the enterprise SaaS baseline",
        source: include_str!("../packs/soc2-tsc.toml"),
    },
    ShippedPack {
        name: "iso27001",
        summary: "ISO/IEC 27001:2022 Annex A — the international ISMS standard",
        source: include_str!("../packs/iso27001-annexa.toml"),
    },
    ShippedPack {
        name: "ssdf",
        summary: "NIST SSDF (SP 800-218) — secure software development; US federal attestation",
        source: include_str!("../packs/nist-ssdf.toml"),
    },
    ShippedPack {
        name: "slsa",
        summary: "SLSA v1.0 + SBOM — build provenance and supply-chain integrity",
        source: include_str!("../packs/slsa-supply-chain.toml"),
    },
    ShippedPack {
        name: "cis",
        summary: "CIS Critical Security Controls v8 — the software-development slice",
        source: include_str!("../packs/cis-controls-v8.toml"),
    },
    ShippedPack {
        name: "pci",
        summary: "PCI DSS v4.0 — Requirement 6 and the code-visible parts of 3 and 8",
        source: include_str!("../packs/pci-dss-v4.toml"),
    },
    ShippedPack {
        name: "nist-800-53",
        summary: "NIST SP 800-53 Rev. 5 moderate baseline — thin, source-visible subset",
        source: include_str!("../packs/nist-800-53-moderate.toml"),
    },
    ShippedPack {
        name: "hipaa",
        summary: "HIPAA Security Rule — thin; technical safeguards only",
        source: include_str!("../packs/hipaa-security-rule.toml"),
    },
    ShippedPack {
        name: "gdpr",
        summary: "GDPR — thin; Article 32 security of processing only",
        source: include_str!("../packs/gdpr.toml"),
    },
    ShippedPack {
        name: "eu-regulatory",
        summary: "NIS2, DORA and the EU AI Act — thin; governance regimes",
        source: include_str!("../packs/eu-nis2-dora-aiact.toml"),
    },
];

/// Look up a shipped pack by name.
pub fn find(name: &str) -> Option<&'static ShippedPack> {
    let want = name.trim().to_lowercase();
    SHIPPED.iter().find(|p| p.name == want)
}

/// Load a shipped pack by name, parsed and validated.
pub fn load_shipped(name: &str) -> Result<Pack> {
    let entry = find(name).ok_or_else(|| {
        DcError::Comply(format!(
            "unknown pack {name:?}. Available: {}",
            names().join(", ")
        ))
    })?;
    Pack::from_toml_str(entry.source)
}

/// Every shipped pack name.
pub fn names() -> Vec<&'static str> {
    SHIPPED.iter().map(|p| p.name).collect()
}

/// A human-readable listing for `--list-packs`.
pub fn listing() -> String {
    let width = SHIPPED.iter().map(|p| p.name.len()).max().unwrap_or(8);
    let mut s = String::from("Shipped compliance packs:\n\n");
    for p in SHIPPED {
        s.push_str(&format!("  {:width$}  {}\n", p.name, p.summary));
    }
    s.push_str(
        "\nUse with: smart-coder comply --pack <name>\n\
         A filesystem path is also accepted for your own packs.\n\
         Every pack states what it CANNOT evidence in its scope note — read it.\n",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shipped_pack_parses_and_validates() {
        // include_str! means a malformed pack is a COMPILE-time inclusion but a
        // runtime parse failure, so this is the guard.
        for p in SHIPPED {
            Pack::from_toml_str(p.source)
                .unwrap_or_else(|e| panic!("shipped pack {:?} failed to parse: {e}", p.name));
        }
    }

    #[test]
    fn names_are_unique_and_lowercase() {
        let mut seen = std::collections::HashSet::new();
        for p in SHIPPED {
            assert!(seen.insert(p.name), "duplicate pack name {:?}", p.name);
            assert_eq!(
                p.name,
                p.name.to_lowercase(),
                "{:?} must be lowercase",
                p.name
            );
            assert!(
                !p.name.contains(' '),
                "{:?} must not contain spaces",
                p.name
            );
        }
    }

    #[test]
    fn lookup_is_case_insensitive_and_trims() {
        assert!(find("soc2").is_some());
        assert!(find("SOC2").is_some());
        assert!(find("  iso27001  ").is_some());
        assert!(find("nope").is_none());
    }

    #[test]
    fn an_unknown_name_lists_the_alternatives() {
        // A bare "unknown pack" error would leave a user guessing.
        let err = load_shipped("iso27002").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("iso27002"), "{msg}");
        assert!(
            msg.contains("soc2"),
            "the error must list what IS available: {msg}"
        );
    }

    #[test]
    fn load_shipped_returns_a_usable_pack() {
        let pack = load_shipped("soc2").expect("soc2 loads");
        assert!(!pack.controls.is_empty());
        assert!(!pack.framework.scope_note.trim().is_empty());
    }

    #[test]
    fn every_pack_states_its_scope() {
        // The scope note is where a pack says what it CANNOT see. A pack without
        // one implies complete coverage.
        for p in SHIPPED {
            let pack = Pack::from_toml_str(p.source).expect("parses");
            assert!(
                pack.framework.scope_note.trim().len() > 100,
                "pack {:?} needs a substantive scope note",
                p.name
            );
        }
    }

    #[test]
    fn listing_covers_every_pack() {
        let out = listing();
        for p in SHIPPED {
            assert!(out.contains(p.name), "listing omits {:?}", p.name);
        }
        assert!(
            out.contains("scope note"),
            "the listing must point at the scope notes"
        );
    }

    #[test]
    fn shipped_registry_covers_the_packs_dir() {
        // A pack added to disk but forgotten here would be silently unreachable
        // by name. This catches that.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("packs");
        let on_disk: Vec<String> = std::fs::read_dir(&dir)
            .expect("packs dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .map(|p| {
                p.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();

        assert_eq!(
            on_disk.len(),
            SHIPPED.len(),
            "{} pack file(s) on disk but {} registered: {on_disk:?}",
            on_disk.len(),
            SHIPPED.len()
        );
    }
}
