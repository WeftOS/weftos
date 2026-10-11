//! `board.propose`: write a staged manifest for the steward. The chief of
//! staff never writes a board row; Stew dedupes against the live board,
//! dry-runs and applies (agents/steward).

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Gate, Tool, ToolOutcome, ToolSpec, arg_str, need_str, tool_err};
use crate::CosError;

pub struct BoardPropose {
    pub staging_dir: PathBuf,
}

const PROPOSE: &str = "board.propose";

/// Lowercase ascii slug for a file name.
pub(crate) fn slug(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
        if out.len() >= max {
            break;
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Write `contents` to `<dir>/<prefix>-<timestamp>-<slug>.yaml` and return
/// the path. Never overwrites: a second write in the same second gets a suffix.
pub(crate) fn write_staged(dir: &PathBuf, prefix: &str, title: &str, contents: &str) -> Result<PathBuf, CosError> {
    std::fs::create_dir_all(dir).map_err(|e| tool_err(prefix, format!("staging dir {}: {e}", dir.display())))?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let base = format!("{prefix}-{stamp}-{}", slug(title, 40));
    for n in 0..100 {
        let name = if n == 0 { format!("{base}.yaml") } else { format!("{base}-{n}.yaml") };
        let path = dir.join(name);
        if path.exists() {
            continue;
        }
        std::fs::write(&path, contents).map_err(|e| tool_err(prefix, format!("write {}: {e}", path.display())))?;
        return Ok(path);
    }
    Err(tool_err(prefix, "could not pick a free staged file name"))
}

#[async_trait]
impl Tool for BoardPropose {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: PROPOSE,
            description: "Stage a board manifest for the steward (one card). Not a board write: the steward dedupes and applies it.",
            parameters: json!({
                "type": "object",
                "required": ["title", "outcome", "acceptance"],
                "properties": {
                    "title": {"type": "string", "maxLength": 300},
                    "outcome": {"type": "string", "description": "What is true when this card is done."},
                    "requirements": {"type": "string"},
                    "acceptance": {"type": "string", "description": "Observable completion condition."},
                    "evidence": {"type": "string", "description": "What the owner said, quoted."},
                    "refs": {"type": "array", "items": {"type": "string"}},
                    "turn_owner": {"type": "string", "description": "Role slug of who owes the next move, or 'unclear'."}
                }
            }),
            gate: Gate::Open,
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome, CosError> {
        let title = need_str(PROPOSE, &args, "title")?;
        let outcome = need_str(PROPOSE, &args, "outcome")?;
        let acceptance = need_str(PROPOSE, &args, "acceptance")?;
        let refs: Vec<String> = args
            .get("refs")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        let now = chrono::Utc::now();
        let manifest = json!({
            "batch": {
                "producer": "chief-of-staff",
                "slug": slug(&title, 40),
                "title": title,
                "date": now.format("%Y-%m-%d").to_string(),
                "source": "chief-of-staff console turn",
            },
            "tickets": [{
                "title": title,
                "outcome": outcome,
                "requirements": arg_str(&args, "requirements").unwrap_or_default(),
                "acceptance": acceptance,
                "evidence": {
                    "tier": if arg_str(&args, "evidence").is_some() { "directly stated" } else { "implied" },
                    "quote": arg_str(&args, "evidence").unwrap_or_default(),
                },
                "refs": refs,
                "asked_by": "owner",
                "turn_owner": arg_str(&args, "turn_owner").unwrap_or_else(|| "unclear".into()),
                "filed_by": "chief-of-staff",
            }]
        });
        let yaml = serde_yaml::to_string(&manifest).map_err(|e| tool_err(PROPOSE, e))?;
        let path = write_staged(&self.staging_dir, "manifest", &title, &yaml)?;
        Ok(ToolOutcome {
            summary: format!("staged manifest for the steward: {}", path.file_name().unwrap_or_default().to_string_lossy()),
            result: json!({"staged_path": path, "next_mover": "steward", "applied": false}),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stages_a_manifest_and_does_not_apply() {
        let dir = tempfile::tempdir().unwrap();
        let tool = BoardPropose { staging_dir: dir.path().to_path_buf() };
        let out = tool
            .call(json!({"title": "Wire the pad button", "outcome": "ptt events arrive", "acceptance": "sim shows ptt.down/up"}))
            .await
            .unwrap();
        assert_eq!(out.result["applied"], false);
        let path = PathBuf::from(out.result["staged_path"].as_str().unwrap());
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("filed_by: chief-of-staff"));
        assert!(text.contains("turn_owner: unclear"));
    }

    #[test]
    fn slug_is_filename_safe() {
        assert_eq!(slug("Wire the pad: button!", 40), "wire-the-pad-button");
    }
}
