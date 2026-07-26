//! The shipped pack collection, embedded at compile time.
//!
//! Packs are `include_str!`'d rather than read from disk so `smart-coder comply
//! --pack soc2` works from any directory against any workspace. A path is still
//! accepted for packs a user has authored themselves.
//!
//! # Why each pack is several files
//!
//! A pack lives in `packs/<name>/` as a `framework.toml` plus one file per
//! [`Section`]. Two reasons, one practical and one structural:
//!
//! - A framework's controls run to hundreds. Kept in one file, ISO 27001 was
//!   already 661 lines with its evidence domains separated by nothing but a
//!   banner comment, and completing the framework would have quadrupled that.
//! - **The filename is the section.** [`Pack::from_dir`] takes the section from
//!   the file a control sits in, so a control cannot be filed under
//!   `organizational.toml` while scoring as Code. The classification is visible
//!   in a directory listing instead of buried in a key on line 400.
//!
//! Adding a pack means adding one entry here. The
//! `every_shipped_pack_loads_and_has_no_blocking_findings` test in
//! `sc-comply-author` enumerates the packs *directory*, so a pack added to disk
//! but forgotten here is caught by [`shipped_registry_covers_the_packs_dir`].

use sc_proto::{DcError, Result};

use crate::pack::Pack;
use crate::section::Section;

/// A framework pack shipped with the tool.
pub struct ShippedPack {
    /// Short name for `--pack <name>`.
    pub name: &'static str,
    /// One-line description for the pack list.
    pub summary: &'static str,
    /// The directory name under `packs/`.
    pub dir: &'static str,
    /// `[framework]` — identity and scope note.
    pub framework: &'static str,
    /// `(section, TOML source)` for each section file present.
    ///
    /// The section is stated here rather than inferred, because `include_str!`
    /// needs a literal path and there is no compile-time directory listing.
    /// [`shipped_sections_match_the_files_on_disk`] holds the two in agreement.
    pub sections: &'static [(Section, &'static str)],
}

impl ShippedPack {
    /// Assemble and validate this pack.
    fn load(&self) -> Result<Pack> {
        let mut pack: Pack = toml::from_str(self.framework)
            .map_err(|e| DcError::Comply(format!("parsing {}/framework.toml: {e}", self.dir)))?;
        for (section, src) in self.sections {
            let mut frag = Pack::controls_from_toml_str(src).map_err(|e| {
                DcError::Comply(format!("parsing {}/{}.toml: {e}", self.dir, section.slug()))
            })?;
            // The filename wins over any `section =` key in the file.
            for c in &mut frag {
                c.section = *section;
            }
            pack.controls.append(&mut frag);
        }
        pack.validate()?;
        Ok(pack)
    }
}

/// Every pack shipped with the tool, in rough order of how much source-visible
/// evidence each carries — the useful ones first.
pub const SHIPPED: &[ShippedPack] = &[
    ShippedPack {
        name: "soc2",
        summary: "SOC 2 Trust Services Criteria (AICPA) — the enterprise SaaS baseline",
        dir: "soc2-tsc",
        framework: include_str!("../packs/soc2-tsc/framework.toml"),
        sections: &[
            (Section::Code, include_str!("../packs/soc2-tsc/code.toml")),
            (
                Section::Infrastructure,
                include_str!("../packs/soc2-tsc/infrastructure.toml"),
            ),
            (
                Section::Organizational,
                include_str!("../packs/soc2-tsc/organizational.toml"),
            ),
        ],
    },
    ShippedPack {
        name: "iso27001",
        summary: "ISO/IEC 27001:2022 Annex A — the international ISMS standard",
        dir: "iso27001-annexa",
        framework: include_str!("../packs/iso27001-annexa/framework.toml"),
        sections: &[
            (
                Section::Code,
                include_str!("../packs/iso27001-annexa/code.toml"),
            ),
            (
                Section::Infrastructure,
                include_str!("../packs/iso27001-annexa/infrastructure.toml"),
            ),
            (
                Section::Documentation,
                include_str!("../packs/iso27001-annexa/documentation.toml"),
            ),
            (
                Section::Organizational,
                include_str!("../packs/iso27001-annexa/organizational.toml"),
            ),
        ],
    },
    ShippedPack {
        name: "ssdf",
        summary: "NIST SSDF (SP 800-218) — secure software development; US federal attestation",
        dir: "nist-ssdf",
        framework: include_str!("../packs/nist-ssdf/framework.toml"),
        sections: &[
            (Section::Code, include_str!("../packs/nist-ssdf/code.toml")),
            (
                Section::Infrastructure,
                include_str!("../packs/nist-ssdf/infrastructure.toml"),
            ),
            (
                Section::Organizational,
                include_str!("../packs/nist-ssdf/organizational.toml"),
            ),
        ],
    },
    ShippedPack {
        name: "slsa",
        summary: "SLSA v1.0 + SBOM — build provenance and supply-chain integrity",
        dir: "slsa-supply-chain",
        framework: include_str!("../packs/slsa-supply-chain/framework.toml"),
        sections: &[
            (
                Section::Code,
                include_str!("../packs/slsa-supply-chain/code.toml"),
            ),
            (
                Section::Infrastructure,
                include_str!("../packs/slsa-supply-chain/infrastructure.toml"),
            ),
            (
                Section::Organizational,
                include_str!("../packs/slsa-supply-chain/organizational.toml"),
            ),
        ],
    },
    ShippedPack {
        name: "cis",
        summary: "CIS Critical Security Controls v8 — the software-development slice",
        dir: "cis-controls-v8",
        framework: include_str!("../packs/cis-controls-v8/framework.toml"),
        sections: &[
            (
                Section::Code,
                include_str!("../packs/cis-controls-v8/code.toml"),
            ),
            (
                Section::Infrastructure,
                include_str!("../packs/cis-controls-v8/infrastructure.toml"),
            ),
            (
                Section::Documentation,
                include_str!("../packs/cis-controls-v8/documentation.toml"),
            ),
            (
                Section::Organizational,
                include_str!("../packs/cis-controls-v8/organizational.toml"),
            ),
        ],
    },
    ShippedPack {
        name: "pci",
        summary: "PCI DSS v4.0 — Requirement 6 and the code-visible parts of 3 and 8",
        dir: "pci-dss-v4",
        framework: include_str!("../packs/pci-dss-v4/framework.toml"),
        sections: &[
            (Section::Code, include_str!("../packs/pci-dss-v4/code.toml")),
            (
                Section::Infrastructure,
                include_str!("../packs/pci-dss-v4/infrastructure.toml"),
            ),
            (
                Section::Organizational,
                include_str!("../packs/pci-dss-v4/organizational.toml"),
            ),
        ],
    },
    ShippedPack {
        name: "nist-800-53",
        summary: "NIST SP 800-53 Rev. 5 moderate baseline — thin, source-visible subset",
        dir: "nist-800-53-moderate",
        framework: include_str!("../packs/nist-800-53-moderate/framework.toml"),
        sections: &[
            (
                Section::Code,
                include_str!("../packs/nist-800-53-moderate/code.toml"),
            ),
            (
                Section::Infrastructure,
                include_str!("../packs/nist-800-53-moderate/infrastructure.toml"),
            ),
            (
                Section::Organizational,
                include_str!("../packs/nist-800-53-moderate/organizational.toml"),
            ),
        ],
    },
    ShippedPack {
        name: "hipaa",
        summary: "HIPAA Security Rule — thin; technical safeguards only",
        dir: "hipaa-security-rule",
        framework: include_str!("../packs/hipaa-security-rule/framework.toml"),
        sections: &[
            (
                Section::Code,
                include_str!("../packs/hipaa-security-rule/code.toml"),
            ),
            (
                Section::Organizational,
                include_str!("../packs/hipaa-security-rule/organizational.toml"),
            ),
        ],
    },
    ShippedPack {
        name: "gdpr",
        summary: "GDPR — thin; Article 32 security of processing only",
        dir: "gdpr",
        framework: include_str!("../packs/gdpr/framework.toml"),
        sections: &[
            (Section::Code, include_str!("../packs/gdpr/code.toml")),
            (
                Section::Infrastructure,
                include_str!("../packs/gdpr/infrastructure.toml"),
            ),
            (
                Section::Organizational,
                include_str!("../packs/gdpr/organizational.toml"),
            ),
        ],
    },
    ShippedPack {
        name: "eu-regulatory",
        summary: "NIS2, DORA and the EU AI Act — thin; governance regimes",
        dir: "eu-nis2-dora-aiact",
        framework: include_str!("../packs/eu-nis2-dora-aiact/framework.toml"),
        sections: &[
            (
                Section::Code,
                include_str!("../packs/eu-nis2-dora-aiact/code.toml"),
            ),
            (
                Section::Infrastructure,
                include_str!("../packs/eu-nis2-dora-aiact/infrastructure.toml"),
            ),
            (
                Section::Organizational,
                include_str!("../packs/eu-nis2-dora-aiact/organizational.toml"),
            ),
        ],
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
    entry.load()
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
            p.load()
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
            let pack = p.load().expect("parses");
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
            .filter(|p| p.is_dir())
            .map(|p| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();

        assert_eq!(
            on_disk.len(),
            SHIPPED.len(),
            "{} pack dir(s) on disk but {} registered: {on_disk:?}",
            on_disk.len(),
            SHIPPED.len()
        );
        for p in SHIPPED {
            assert!(
                on_disk.iter().any(|d| d == p.dir),
                "registered pack {:?} has no directory {:?}",
                p.name,
                p.dir
            );
        }
    }

    /// The `sections` list must match the section files actually on disk.
    ///
    /// `include_str!` needs a literal path, so the registry states its sections
    /// by hand. A section file added to a pack directory but not listed here
    /// would compile fine and silently drop every control in it — the report
    /// would simply not mention them, which is the worst failure this crate has.
    #[test]
    fn shipped_sections_match_the_files_on_disk() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("packs");
        for p in SHIPPED {
            let dir = root.join(p.dir);
            let mut on_disk: Vec<Section> = std::fs::read_dir(&dir)
                .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|f| f.extension().is_some_and(|x| x == "toml"))
                .filter_map(|f| {
                    Section::from_slug(&f.file_stem().unwrap_or_default().to_string_lossy())
                })
                .collect();
            on_disk.sort();

            let registered: Vec<Section> = p.sections.iter().map(|(s, _)| *s).collect();
            assert_eq!(
                registered, on_disk,
                "pack {:?}: registry lists {registered:?} but the directory holds {on_disk:?}",
                p.name
            );
        }
    }

    /// Every control lands in the section its file name declares.
    #[test]
    fn a_controls_section_comes_from_its_file() {
        for p in SHIPPED {
            let pack = p.load().expect("loads");
            for (section, src) in p.sections {
                let ids: Vec<String> = Pack::controls_from_toml_str(src)
                    .expect("fragment parses")
                    .iter()
                    .map(|c| c.id.clone())
                    .collect();
                for id in ids {
                    let c = pack
                        .controls
                        .iter()
                        .find(|c| c.id == id)
                        .unwrap_or_else(|| panic!("{id} missing from assembled pack"));
                    assert_eq!(
                        c.section,
                        *section,
                        "{}: control {id} lives in {}.toml but scored as {:?}",
                        p.name,
                        section.slug(),
                        c.section
                    );
                }
            }
        }
    }

    /// No control may be declared in two section files.
    #[test]
    fn a_control_cannot_appear_in_two_sections() {
        // Duplicate ids would double-count in every score. `validate()` rejects
        // them, but only if assembly actually concatenates rather than
        // overwrites — this asserts the pack that reaches a report is whole.
        for p in SHIPPED {
            let pack = p.load().expect("loads");
            let declared: usize = p
                .sections
                .iter()
                .map(|(_, src)| Pack::controls_from_toml_str(src).expect("parses").len())
                .sum();
            assert_eq!(
                pack.controls.len(),
                declared,
                "pack {:?} assembled {} controls from {declared} declared",
                p.name,
                pack.controls.len()
            );
        }
    }
}
