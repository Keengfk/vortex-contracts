//! Contract-spec (ABI) compatibility checker.
//!
//! Extracts the `contractspecv0` entries embedded in built wasm artifacts,
//! compares them against the committed baselines in `specs/<crate>.json`, and
//! fails on breaking changes. Additive changes are reported but allowed.
//!
//! Breaking changes must be explicitly acknowledged via an allowlist entry in
//! `specs/allowlist.json` that names the tracking issue.
//!
//! Run with `cargo test -p spec-check`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use soroban_spec::read::from_wasm;

/// A single entry in the committed baseline / extracted spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SpecEntry {
    /// `function`, `struct`, `enum`, `error`, `event`, ...
    kind: String,
    name: String,
    /// Canonical, order-sensitive representation of the entry body.
    signature: String,
}

/// A committed baseline for one contract crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Baseline {
    crate_name: String,
    entries: Vec<SpecEntry>,
}

/// An explicit acknowledgement of a breaking change.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AllowlistEntry {
    crate_name: String,
    /// Stable identifier of the changed entry, e.g. `function:transfer`.
    entry: String,
    /// The tracking issue that authorises the break.
    issue: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Allowlist {
    #[serde(default)]
    entries: Vec<AllowlistEntry>,
}

/// Classification of a single detected change.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChangeKind {
    /// New function/type/error/event, or a widened signature.
    Additive,
    /// Removed/renamed entry, changed argument type, reordered enum variant,
    /// changed error code, or removed struct field.
    Breaking,
}

#[derive(Debug, Clone)]
struct Change {
    kind: ChangeKind,
    entry: String,
    detail: String,
}

fn workspace_root() -> PathBuf {
    // `tools/spec-check/` -> workspace root is two levels up.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("spec-check must live at tools/spec-check")
        .to_path_buf()
}

/// Extract the spec entries from a built wasm artifact.
fn extract_entries(wasm: &[u8]) -> Result<Vec<SpecEntry>, String> {
    let spec = from_wasm(wasm).map_err(|e| format!("failed to parse contractspecv0: {e}"))?;
    let mut entries = Vec::new();

    for func in &spec.functions {
        let args: Vec<String> = func
            .inputs
            .iter()
            .map(|i| format!("{}:{}", i.name, i.type_))
            .collect();
        entries.push(SpecEntry {
            kind: "function".into(),
            name: func.name.clone(),
            signature: format!("({})->{}", args.join(","), func.outputs.join(",")),
        });
    }

    for udt in &spec.udt {
        match &udt.body {
            soroban_spec::SpecEntryUdtBody::Struct(fields) => {
                // `contracttype` structs are map-encoded and sorted by name, so
                // field order is not part of the ABI. Sort for a stable diff.
                let mut sorted: Vec<String> = fields
                    .iter()
                    .map(|f| format!("{}:{}", f.name, f.type_))
                    .collect();
                sorted.sort();
                entries.push(SpecEntry {
                    kind: "struct".into(),
                    name: udt.name.clone(),
                    signature: sorted.join(","),
                });
            }
            soroban_spec::SpecEntryUdtBody::Enum(cases) => {
                // Enum variants are positional, so order is significant.
                let variants: Vec<String> = cases
                    .iter()
                    .map(|c| format!("{}:{}", c.name, c.value))
                    .collect();
                entries.push(SpecEntry {
                    kind: "enum".into(),
                    name: udt.name.clone(),
                    signature: variants.join(","),
                });
            }
            soroban_spec::SpecEntryUdtBody::Union(cases) => {
                let variants: Vec<String> = cases
                    .iter()
                    .map(|c| format!("{}:{}", c.name, c.value))
                    .collect();
                entries.push(SpecEntry {
                    kind: "union".into(),
                    name: udt.name.clone(),
                    signature: variants.join(","),
                });
            }
        }
    }

    for err in &spec.error_enums {
        let cases: Vec<String> = err
            .cases
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect();
        entries.push(SpecEntry {
            kind: "error".into(),
            name: err.name.clone(),
            signature: cases.join(","),
        });
    }

    for event in &spec.events {
        let fields: Vec<String> = event
            .params
            .iter()
            .map(|p| format!("{}:{}", p.name, p.type_))
            .collect();
        entries.push(SpecEntry {
            kind: "event".into(),
            name: event.name.clone(),
            signature: fields.join(","),
        });
    }

    entries.sort_by(|a, b| (&a.kind, &a.name).cmp(&(&b.kind, &b.name)));
    Ok(entries)
}

fn entry_id(entry: &SpecEntry) -> String {
    format!("{}:{}", entry.kind, entry.name)
}

/// Compare a baseline against a freshly extracted spec.
fn diff(baseline: &[SpecEntry], current: &[SpecEntry]) -> Vec<Change> {
    let base: BTreeMap<String, &SpecEntry> =
        baseline.iter().map(|e| (entry_id(e), e)).collect();
    let cur: BTreeMap<String, &SpecEntry> = current.iter().map(|e| (entry_id(e), e)).collect();

    let mut changes = Vec::new();

    for (id, old) in &base {
        match cur.get(id) {
            None => changes.push(Change {
                kind: ChangeKind::Breaking,
                entry: id.clone(),
                detail: "entry removed or renamed".into(),
            }),
            Some(new) if new.signature != old.signature => changes.push(Change {
                kind: ChangeKind::Breaking,
                entry: id.clone(),
                detail: format!(
                    "signature changed: `{}` -> `{}`",
                    old.signature, new.signature
                ),
            }),
            Some(_) => {}
        }
    }

    for id in cur.keys() {
        if !base.contains_key(id) {
            changes.push(Change {
                kind: ChangeKind::Additive,
                entry: id.clone(),
                detail: "new entry".into(),
            });
        }
    }

    changes
}

fn load_allowlist(root: &Path) -> Allowlist {
    let path = root.join("specs/allowlist.json");
    match fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("invalid {}: {e}", path.display())),
        Err(_) => Allowlist::default(),
    }
}

fn is_allowed(allowlist: &Allowlist, crate_name: &str, entry: &str) -> bool {
    allowlist.entries.iter().any(|a| {
        a.crate_name == crate_name && a.entry == entry && !a.issue.trim().is_empty()
    })
}

/// Discover every contract crate that has a committed baseline.
fn baseline_crates(root: &Path) -> Vec<(String, PathBuf)> {
    let specs_dir = root.join("specs");
    let mut crates = Vec::new();
    let Ok(read) = fs::read_dir(&specs_dir) else {
        return crates;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem == "allowlist" {
            continue;
        }
        crates.push((stem.to_string(), path));
    }
    crates.sort();
    crates
}

fn wasm_path(root: &Path, crate_name: &str) -> PathBuf {
    root.join("target/wasm32-unknown-unknown/release")
        .join(format!("{crate_name}.wasm"))
}

#[test]
fn contract_spec_is_compatible() {
    let root = workspace_root();
    let allowlist = load_allowlist(&root);
    let crates = baseline_crates(&root);

    assert!(
        !crates.is_empty(),
        "no baselines found in {}; commit specs/<crate>.json",
        root.join("specs").display()
    );

    let mut failures: Vec<String> = Vec::new();

    for (crate_name, baseline_path) in crates {
        let raw = fs::read_to_string(&baseline_path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", baseline_path.display()));
        let baseline: Baseline = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("invalid {}: {e}", baseline_path.display()));

        let wasm = wasm_path(&root, &crate_name);
        let bytes = fs::read(&wasm).unwrap_or_else(|e| {
            panic!(
                "cannot read {}: {e}; build the contract first",
                wasm.display()
            )
        });

        let current = extract_entries(&bytes)
            .unwrap_or_else(|e| panic!("{}: {e}", wasm.display()));

        for change in diff(&baseline.entries, &current) {
            match change.kind {
                ChangeKind::Additive => {
                    eprintln!(
                        "[spec-check] additive change in {crate_name}: {} ({})",
                        change.entry, change.detail
                    );
                }
                ChangeKind::Breaking => {
                    if is_allowed(&allowlist, &crate_name, &change.entry) {
                        eprintln!(
                            "[spec-check] allowed breaking change in {crate_name}: {} ({})",
                            change.entry, change.detail
                        );
                    } else {
                        failures.push(format!(
                            "{crate_name}: breaking change to {}: {} (add an allowlist entry naming the issue)",
                            change.entry, change.detail
                        ));
                    }
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "contract spec compatibility check failed:\n{}",
        failures.join("\n")
    );
}
