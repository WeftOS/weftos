//! Recording a generated `agents/catalog.json` into the index.

use serde_json::Value;

use crate::{DepRef, Recorded, VersionRecord, VersionsError, VersionsIndex};

/// What an import did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImportReport {
    pub appended: usize,
    pub already_recorded: usize,
}

fn bad(what: &'static str, v: impl ToString) -> VersionsError {
    VersionsError::Invalid { what, value: v.to_string() }
}

fn entry_record(e: &Value, now: u64) -> Result<VersionRecord, VersionsError> {
    let s = |k: &'static str| e.get(k).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| bad("catalog entry field", k));
    let dependencies = match e.get("dependencies") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|d| {
                Ok(DepRef {
                    kind: d.get("kind").and_then(Value::as_str).ok_or_else(|| bad("dependency", d))?.to_owned(),
                    name: d.get("name").and_then(Value::as_str).ok_or_else(|| bad("dependency", d))?.to_owned(),
                    digest: d.get("digest").and_then(Value::as_str).map(str::to_owned),
                })
            })
            .collect::<Result<_, VersionsError>>()?,
        Some(other) => return Err(bad("dependencies", other)),
    };
    Ok(VersionRecord {
        object: s("id")?,
        version: s("version")?,
        digest: s("digest")?,
        parent: e.get("parent").and_then(Value::as_str).map(str::to_owned),
        dependencies,
        recorded_at: now,
    })
}

impl VersionsIndex {
    /// Record every package of a catalog under `kind`. A package with no
    /// catalog `parent` whose object already has versions takes the object's
    /// latest version as its parent, so a changed package extends its history.
    /// Parents are recorded before their children; a parent that is neither
    /// recorded nor in the catalog is refused with nothing partially hidden:
    /// everything recorded before the failure stays recorded (and idempotent).
    pub fn import_catalog(&self, kind: &str, catalog: &Value, now: u64) -> Result<ImportReport, VersionsError> {
        let entries = catalog.get("agents").and_then(Value::as_array).ok_or_else(|| bad("catalog", "no agents array"))?;
        let mut pending: Vec<VersionRecord> = entries.iter().map(|e| entry_record(e, now)).collect::<Result<_, _>>()?;
        let mut report = ImportReport::default();
        while !pending.is_empty() {
            let known = self.list(kind)?;
            let before = pending.len();
            let mut next = Vec::new();
            for mut rec in pending {
                let has_digest = |d: &str| known.iter().any(|r| r.digest == d);
                if rec.parent.is_none() {
                    rec.parent = known.iter().rev().find(|r| r.object == rec.object && r.digest != rec.digest).map(|r| r.digest.clone());
                }
                match &rec.parent {
                    Some(p) if !has_digest(p) => next.push(rec),
                    _ => match self.record(kind, &rec)? {
                        Recorded::Appended => report.appended += 1,
                        Recorded::AlreadyRecorded => report.already_recorded += 1,
                    },
                }
            }
            if next.len() == before {
                let r = &next[0];
                return Err(VersionsError::UnknownParent { kind: kind.to_owned(), object: r.object.clone(), parent: r.parent.clone().unwrap_or_default() });
            }
            pending = next;
        }
        Ok(report)
    }
}
