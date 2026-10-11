//! `work.kickoff`: start a team or lane on a card. Gated: the agent asks the
//! owner first (a confirmation turn), every time.
//!
//! The ADR-112 §4 run surface (`weaver team up <team> --project <ULID>`, phase
//! TM5) is not built at the time of writing. When `team.runtime_command` is
//! set in config the tool runs that argv with `--card <card>` (and `--lane`,
//! `--host` when given) and reports its exit; otherwise it records a staged
//! kickoff request next to the steward's manifests and says so plainly in the
//! result (`stubbed: true`). It never claims a lane is running when it isn't.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::board::write_staged;
use super::{Gate, Tool, ToolOutcome, ToolSpec, arg_str, need_str, tool_err};
use crate::CosError;

pub struct WorkKickoff {
    pub staging_dir: PathBuf,
    /// The run surface argv, when one is configured.
    pub runtime_command: Option<Vec<String>>,
}

const KICKOFF: &str = "work.kickoff";

#[async_trait]
impl Tool for WorkKickoff {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: KICKOFF,
            description: "Start a team or lane on a card. Always confirmed by the owner first.",
            parameters: json!({
                "type": "object",
                "required": ["card"],
                "properties": {
                    "card": {"type": "string", "description": "WEFT-N or ticket id."},
                    "team": {"type": "string", "default": "weftos-core"},
                    "lane": {"type": "string", "description": "A member id such as developer; whole team when omitted."},
                    "host": {"type": "string", "enum": ["claude", "codex", "grok"]}
                }
            }),
            gate: Gate::Confirm,
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome, CosError> {
        let card = need_str(KICKOFF, &args, "card")?;
        if !card.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(tool_err(KICKOFF, "card must be WEFT-N or a ticket id"));
        }
        let team = arg_str(&args, "team").unwrap_or_else(|| "weftos-core".into());
        let lane = arg_str(&args, "lane");
        let host = arg_str(&args, "host");
        let request = json!({
            "kind": "kickoff",
            "card": card, "team": team, "lane": lane, "host": host,
            "requested_by": "chief-of-staff",
            "confirmed_by_owner": true,
            "at": chrono::Utc::now().to_rfc3339(),
        });

        let Some(argv) = self.runtime_command.as_ref().filter(|v| !v.is_empty()) else {
            let yaml = serde_yaml::to_string(&request).map_err(|e| tool_err(KICKOFF, e))?;
            let path = write_staged(&self.staging_dir, "kickoff", &card, &yaml)?;
            return Ok(ToolOutcome {
                summary: format!("kickoff of {card} recorded as a staged request; ADR-112 team runtime (TM5) is not built"),
                result: json!({
                    "started": false, "stubbed": true,
                    "reason": "ADR-112 §4 run surface (weaver team up) is not built; request staged",
                    "request_path": path,
                }),
            });
        };

        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]).arg(&team).arg("--card").arg(&card);
        if let Some(l) = &lane {
            cmd.arg("--lane").arg(l);
        }
        if let Some(h) = &host {
            cmd.arg("--host").arg(h);
        }
        let out = cmd.output().await.map_err(|e| tool_err(KICKOFF, format!("spawn {}: {e}", argv[0])))?;
        let started = out.status.success();
        let tail = |b: &[u8]| String::from_utf8_lossy(b).chars().rev().take(400).collect::<String>().chars().rev().collect::<String>();
        Ok(ToolOutcome {
            summary: format!("kickoff of {card} on {team}{}: {}", lane.as_deref().map(|l| format!("/{l}")).unwrap_or_default(), if started { "started" } else { "failed" }),
            result: json!({
                "started": started, "stubbed": false,
                "exit": out.status.code(),
                "stdout_tail": tail(&out.stdout), "stderr_tail": tail(&out.stderr),
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn without_a_runtime_the_kickoff_is_an_honest_stub() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WorkKickoff { staging_dir: dir.path().to_path_buf(), runtime_command: None };
        let out = tool.call(json!({"card": "WEFT-700", "lane": "developer"})).await.unwrap();
        assert_eq!(out.result["started"], false);
        assert_eq!(out.result["stubbed"], true);
        assert!(out.summary.contains("not built"));
        assert!(PathBuf::from(out.result["request_path"].as_str().unwrap()).exists());
    }

    #[tokio::test]
    async fn rejects_a_shell_shaped_card() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WorkKickoff { staging_dir: dir.path().to_path_buf(), runtime_command: None };
        assert!(tool.call(json!({"card": "WEFT-1; rm -rf /"})).await.is_err());
    }
}
