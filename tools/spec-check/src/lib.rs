//! Contract-spec (ABI) compatibility checker.
//!
//! Extracts the `contractspecv0` entries embedded in a built wasm artifact and
//! compares them against a committed baseline, classifying every change as
//! either additive or breaking. Breaking changes must be explicitly allowlisted
//! (naming the tracking issue) or the check fails.
//!
//! The checker is exercised by `cargo test -p spec-check`.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A single entry in a contract spec, normalized so that it can be diffed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpecEntry {
    /// A contract function, keyed by name.
    Function {
        name: String,
        inputs: Vec<SpecParam>,
        outputs: Vec<SpecParam>,
    },
    /// A user-defined type (struct, enum, union, ...).
    Udt {
        name: String,
        /// Struct fields are map-encoded and therefore sorted by name; enum
        /// variants are positional and must keep their order.
        fields: Vec<SpecParam>,
        variants: Vec<String>,
    },
    /// A contract error, keyed by numeric code.
    Error { code: u32, name: String },
    /// A contract event, keyed by name.
    Event { name: String, inputs: Vec<SpecParam> },
}

/// A named, typed parameter (function argument, struct field, ...).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecParam {
    pub name: String,
    pub type_name: String,
}

/// The normalized spec of a single contract.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractSpec {
    pub functions: BTreeMap<String, SpecEntry>,
    pub udts: BTreeMap<String, SpecEntry>,
    pub errors: BTreeMap<u32, SpecEntry>,
    pub events: BTreeMap<String, SpecEntry>,
}

/// How a single spec change should be treated during an upgrade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    /// Safe to ship without review (new function, new error, ...).
    Additive,
    /// Requires an explicit allowlist entry naming the tracking issue.
    Breaking,
}

/// A single detected difference between baseline and current spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecChange {
    pub kind: ChangeKind,
    pub path: String,
    pub detail: String,
}

impl fmt::Display for SpecChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self.kind {
            ChangeKind::Additive => "additive",
            ChangeKind::Breaking => "BREAKING",
        };
        write!(f, "[{label}] {}: {}", self.path, self.detail)
    }
}

/// An explicit acknowledgement of a breaking change, naming the issue that
/// tracks it. Without a matching entry the checker fails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowlistEntry {
    /// Dotted path of the change, e.g. `functions.transfer.inputs`.
    pub path: String,
    /// The GitHub issue that authorizes the breaking change.
    pub issue: String,
}

/// Compare a committed baseline against the freshly extracted spec.
///
/// Every difference is classified as additive or breaking. The returned vector
/// contains only the changes; callers decide whether the breaking ones are
/// allowlisted.
pub fn diff_specs(baseline: &ContractSpec, current: &ContractSpec) -> Vec<SpecChange> {
    let mut changes = Vec::new();

    diff_functions(baseline, current, &mut changes);
    diff_udts(baseline, current, &mut changes);
    diff_errors(baseline, current, &mut changes);
    diff_events(baseline, current, &mut changes);

    changes
}

fn diff_functions(baseline: &ContractSpec, current: &ContractSpec, out: &mut Vec<SpecChange>) {
    for (name, base) in &baseline.functions {
        match current.functions.get(name) {
            None => out.push(SpecChange {
                kind: ChangeKind::Breaking,
                path: format!("functions.{name}"),
                detail: "function removed or renamed".into(),
            }),
            Some(cur) => {
                if let (SpecEntry::Function { inputs: bi, outputs: bo, .. }, SpecEntry::Function { inputs: ci, outputs: co, .. }) = (base, cur) {
                    if bi != ci {
                        out.push(SpecChange {
                            kind: ChangeKind::Breaking,
                            path: format!("functions.{name}.inputs"),
                            detail: "argument types changed".into(),
                        });
                    }
                    if bo != co {
                        out.push(SpecChange {
                            kind: ChangeKind::Breaking,
                            path: format!("functions.{name}.outputs"),
                            detail: "return types changed".into(),
                        });
                    }
                }
            }
        }
    }
    for name in current.functions.keys() {
        if !baseline.functions.contains_key(name) {
            out.push(SpecChange {
                kind: ChangeKind::Additive,
                path: format!("functions.{name}"),
                detail: "function added".into(),
            });
        }
    }
}

fn diff_udts(baseline: &ContractSpec, current: &ContractSpec, out: &mut Vec<SpecChange>) {
    for (name, base) in &baseline.udts {
        match current.udts.get(name) {
            None => out.push(SpecChange {
                kind: ChangeKind::Breaking,
                path: format!("udts.{name}"),
                detail: "type removed or renamed".into(),
            }),
            Some(cur) => {
                if let (SpecEntry::Udt { fields: bf, variants: bv, .. }, SpecEntry::Udt { fields: cf, variants: cv, .. }) = (base, cur) {
                    // Struct fields are map-encoded (sorted by name), so a
                    // removed field is breaking but a reordering is not.
                    for field in bf {
                        if !cf.iter().any(|f| f.name == field.name) {
                            out.push(SpecChange {
                                kind: ChangeKind::Breaking,
                                path: format!("udts.{name}.fields.{}", field.name),
                                detail: "struct field removed".into(),
                            });
                        }
                    }
                    for field in cf {
                        if !bf.iter().any(|f| f.name == field.name) {
                            out.push(SpecChange {
                                kind: ChangeKind::Additive,
                                path: format!("udts.{name}.fields.{}", field.name),
                                detail: "struct field added".into(),
                            });
                        }
                    }
                    // Enum variants are positional: any reordering is breaking.
                    if bv != cv {
                        out.push(SpecChange {
                            kind: ChangeKind::Breaking,
                            path: format!("udts.{name}.variants"),
                            detail: "enum variants reordered or changed".into(),
                        });
                    }
                }
            }
        }
    }
    for name in current.udts.keys() {
        if !baseline.udts.contains_key(name) {
            out.push(SpecChange {
                kind: ChangeKind::Additive,
                path: format!("udts.{name}"),
                detail: "type added".into(),
            });
        }
    }
}

fn diff_errors(baseline: &ContractSpec, current: &ContractSpec, out: &mut Vec<SpecChange>) {
    for (code, base) in &baseline.errors {
        match current.errors.get(code) {
            None => out.push(SpecChange {
                kind: ChangeKind::Breaking,
                path: format!("errors.{code}"),
                detail: "error code removed".into(),
            }),
            Some(cur) => {
                if let (SpecEntry::Error { name: bn, .. }, SpecEntry::Error { name: cn, .. }) = (base, cur) {
                    if bn != cn {
                        out.push(SpecChange {
                            kind: ChangeKind::Breaking,
                            path: format!("errors.{code}"),
                            detail: format!("error code reassigned from `{bn}` to `{cn}`"),
                        });
                    }
                }
            }
        }
    }
    for code in current.errors.keys() {
        if !baseline.errors.contains_key(code) {
            out.push(SpecChange {
                kind: ChangeKind::Additive,
                path: format!("errors.{code}"),
                detail: "error code added".into(),
            });
        }
    }
}

fn diff_events(baseline: &ContractSpec, current: &ContractSpec, out: &mut Vec<SpecChange>) {
    for (name, base) in &baseline.events {
        match current.events.get(name) {
            None => out.push(SpecChange {
                kind: ChangeKind::Breaking,
                path: format!("events.{name}"),
                detail: "event removed or renamed".into(),
            }),
            Some(cur) => {
                if let (SpecEntry::Event { inputs: bi, .. }, SpecEntry::Event { inputs: ci, .. }) = (base, cur) {
                    if bi != ci {
                        out.push(SpecChange {
                            kind: ChangeKind::Breaking,
                            path: format!("events.{name}.inputs"),
                            detail: "event payload changed".into(),
                        });
                    }
                }
            }
        }
    }
    for name in current.events.keys() {
        if !baseline.events.contains_key(name) {
            out.push(SpecChange {
                kind: ChangeKind::Additive,
                path: format!("events.{name}"),
                detail: "event added".into(),
            });
        }
    }
}

/// Filter out breaking changes that are explicitly allowlisted.
///
/// Returns the breaking changes that still need an allowlist entry.
pub fn unallowlisted_breaking<'a>(
    changes: &'a [SpecChange],
    allowlist: &[AllowlistEntry],
) -> Vec<&'a SpecChange> {
    changes
        .iter()
        .filter(|c| c.kind == ChangeKind::Breaking)
        .filter(|c| {
            !allowlist
                .iter()
                .any(|a| a.path == c.path && !a.issue.trim().is_empty())
        })
        .collect()
}

/// Load a committed baseline spec from `specs/<crate>.json`.
pub fn load_baseline(specs_dir: &Path, crate_name: &str) -> Result<ContractSpec, String> {
    let path = specs_dir.join(format!("{crate_name}.json"));
    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("failed to read baseline {}: {e}", path.display()))?;
    serde_json::from_str(&raw)
        .map_err(|e| format!("failed to parse baseline {}: {e}", path.display()))
}

/// Serialize a spec to the canonical baseline format.
pub fn to_baseline_json(spec: &ContractSpec) -> Result<String, String> {
    serde_json::to_string_pretty(spec).map_err(|e| format!("failed to serialize spec: {e}"))
}

/// Extract the contract spec from a built wasm artifact.
///
/// The `contractspecv0` custom section is parsed with `soroban-spec` and
/// normalized into a [`ContractSpec`] suitable for diffing.
pub fn extract_spec(wasm: &[u8]) -> Result<ContractSpec, String> {
    let entries = soroban_spec::read::parse_raw(wasm)
        .map_err(|e| format!("failed to parse contractspecv0: {e}"))?;
    Ok(normalize_entries(entries))
}

/// Normalize raw `soroban-spec` entries into a diffable [`ContractSpec`].
fn normalize_entries(entries: Vec<ScSpecEntry>) -> ContractSpec {
    let mut spec = ContractSpec::default();
    for entry in entries {
        match entry {
            ScSpecEntry::FunctionV0(f) => {
                let name = f.name.to_string();
                spec.functions.insert(
                    name.clone(),
                    SpecEntry::Function {
                        name,
                        inputs: f.inputs.iter().map(normalize_param).collect(),
                        outputs: f.outputs.iter().map(normalize_param).collect(),
                    },
                );
            }
            ScSpecEntry::UdtStructV0(s) => {
                let name = s.name.to_string();
                let mut fields: Vec<SpecParam> = s
                    .fields
                    .iter()
                    .map(|f| SpecParam {
                        name: f.name.to_string(),
                        type_name: f.type_.to_string(),
                    })
                    .collect();
                // Struct fields are map-encoded: sort by name so that field
                // order in the source does not register as a change.
                fields.sort_by(|a, b| a.name.cmp(&b.name));
                spec.udts.insert(
                    name.clone(),
                    SpecEntry::Udt { name, fields, variants: Vec::new() },
                );
            }
            ScSpecEntry::UdtEnumV0(e) => {
                let name = e.name.to_string();
                // Enum variants are positional: preserve declaration order.
                let variants = e.cases.iter().map(|c| c.name.to_string()).collect();
                spec.udts.insert(
                    name.clone(),
                    SpecEntry::Udt { name, fields: Vec::new(), variants },
                );
            }
            ScSpecEntry::UdtUnionV0(u) => {
                let name = u.name.to_string();
                let variants = u.cases.iter().map(|c| c.to_string()).collect();
                spec.udts.insert(
                    name.clone(),
                    SpecEntry::Udt { name, fields: Vec::new(), variants },
                );
            }
            ScSpecEntry::ErrorV0(e) => {
                spec.errors.insert(
                    e.code,
                    SpecEntry::Error { code: e.code, name: e.name.to_string() },
                );
            }
            ScSpecEntry::EventV0(e) => {
                let name = e.name.to_string();
                spec.events.insert(
                    name.clone(),
                    SpecEntry::Event {
                        name,
                        inputs: e.params.iter().map(normalize_param).collect(),
                    },
                );
            }
        }
    }
    spec
}

fn normalize_param(p: &ScSpecParam) -> SpecParam {
    SpecParam { name: p.name.to_string(), type_name: p.type_.to_string() }
}

/// Resolve the workspace `specs/` directory relative to the crate manifest.
pub fn specs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("specs")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn function(name: &str, inputs: Vec<SpecParam>) -> SpecEntry {
        SpecEntry::Function { name: name.into(), inputs, outputs: Vec::new() }
    }

    fn param(name: &str, ty: &str) -> SpecParam {
        SpecParam { name: name.into(), type_name: ty.into() }
    }

    #[test]
    fn added_function_is_additive() {
        let baseline = ContractSpec::default();
        let mut current = ContractSpec::default();
        current.functions.insert("pause".into(), function("pause", vec![]));

        let changes = diff_specs(&baseline, &current);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Additive);
    }

    #[test]
    fn removed_function_is_breaking() {
        let mut baseline = ContractSpec::default();
        baseline.functions.insert("pause".into(), function("pause", vec![]));
        let current = ContractSpec::default();

        let changes = diff_specs(&baseline, &current);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
    }

    #[test]
    fn changed_argument_type_is_breaking() {
        let mut baseline = ContractSpec::default();
        baseline
            .functions
            .insert("transfer".into(), function("transfer", vec![param("amount", "i128")]));
        let mut current = ContractSpec::default();
        current
            .functions
            .insert("transfer".into(), function("transfer", vec![param("amount", "u64")]));

        let changes = diff_specs(&baseline, &current);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
    }

    #[test]
    fn reordered_enum_variants_is_breaking() {
        let mut baseline = ContractSpec::default();
        baseline.udts.insert(
            "Status".into(),
            SpecEntry::Udt {
                name: "Status".into(),
                fields: Vec::new(),
                variants: vec!["Active".into(), "Paused".into()],
            },
        );
        let mut current = ContractSpec::default();
        current.udts.insert(
            "Status".into(),
            SpecEntry::Udt {
                name: "Status".into(),
                fields: Vec::new(),
                variants: vec!["Paused".into(), "Active".into()],
            },
        );

        let changes = diff_specs(&baseline, &current);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
    }

    #[test]
    fn removed_struct_field_is_breaking() {
        let mut baseline = ContractSpec::default();
        baseline.udts.insert(
            "Config".into(),
            SpecEntry::Udt {
                name: "Config".into(),
                fields: vec![param("admin", "Address"), param("fee", "i128")],
                variants: Vec::new(),
            },
        );
        let mut current = ContractSpec::default();
        current.udts.insert(
            "Config".into(),
            SpecEntry::Udt {
                name: "Config".into(),
                fields: vec![param("admin", "Address")],
                variants: Vec::new(),
            },
        );

        let changes = diff_specs(&baseline, &current);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
    }

    #[test]
    fn reassigned_error_code_is_breaking() {
        let mut baseline = ContractSpec::default();
        baseline
            .errors
            .insert(1, SpecEntry::Error { code: 1, name: "NotAuthorized".into() });
        let mut current = ContractSpec::default();
        current
            .errors
            .insert(1, SpecEntry::Error { code: 1, name: "Paused".into() });

        let changes = diff_specs(&baseline, &current);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Breaking);
    }

    #[test]
    fn breaking_change_requires_allowlist_entry() {
        let mut baseline = ContractSpec::default();
        baseline.functions.insert("pause".into(), function("pause", vec![]));
        let current = ContractSpec::default();
        let changes = diff_specs(&baseline, &current);

        assert_eq!(unallowlisted_breaking(&changes, &[]).len(), 1);

        let allowlist = vec![AllowlistEntry {
            path: "functions.pause".into(),
            issue: "#401".into(),
        }];
        assert!(unallowlisted_breaking(&changes, &allowlist).is_empty());
    }
}
