//! The lock after a plan: per-file hashes, per-package version and digest,
//! the applied team records (slots, digest) and the seeded project context.

use std::collections::BTreeMap;

use super::InitError;
use super::digest::{package_digest, team_digest};
use super::lock::{
    Lock, LockAgent, LockContext, LockFile, LockSlot, LockTeam, RENDERER, SCHEMA, sha256_hex,
};
use super::package::{package_version, team_slots};
use super::plan::{Action, AgentInitOptions, CONTEXT_PATH, PlanItem};
use super::source::AgentSource;

/// Version of a team: its team.yaml `version`, else the weftos version.
fn team_version(src: &AgentSource, name: &str) -> String {
    src.get(&format!("teams/{name}/team.yaml"))
        .and_then(|b| serde_yaml::from_slice::<serde_yaml::Value>(b).ok())
        .and_then(|v| v.get("version").and_then(|x| x.as_str().map(str::to_string)))
        .unwrap_or_else(|| crate::VERSION.to_string())
}

fn team_record(
    opts: &AgentInitOptions,
    src: &AgentSource,
    name: &str,
    prior: Option<&LockTeam>,
) -> Result<Option<LockTeam>, InitError> {
    // A team the source does not hold (init falls back to every package) has no record.
    let Some(slots) = team_slots(src, name)? else { return Ok(None) };
    let slots = slots
        .into_iter()
        .map(|s| LockSlot { agent: s.agent, position: s.position, active: s.active })
        .collect();
    let mut hosts = prior.map(|p| p.hosts.clone()).unwrap_or_default();
    for h in &opts.hosts {
        if !hosts.contains(h) {
            hosts.push(*h);
        }
    }
    hosts.sort();
    Ok(Some(LockTeam {
        name: name.to_string(),
        digest: team_digest(src, name)?,
        version: team_version(src, name),
        hosts,
        slots,
    }))
}

/// The lock after this run: entries for hosts not in the run are kept; a
/// drifted file keeps its old hash so it stays reported as drifted.
pub fn next_lock(
    opts: &AgentInitOptions,
    src: &AgentSource,
    team: Option<String>,
    prior_lock: &Lock,
    items: &[PlanItem],
) -> Result<Lock, InitError> {
    let mut agents: BTreeMap<String, LockAgent> = BTreeMap::new();
    for a in &prior_lock.agents {
        let mut kept = a.clone();
        kept.files.retain(|f| !opts.hosts.contains(&f.host));
        agents.insert(a.id.clone(), kept);
    }
    let prior_files = prior_lock.files_by_path();
    for it in items {
        let Some(host) = it.host else { continue };
        let entry = match it.action {
            Action::Create | Action::Update | Action::Unchanged | Action::Overwrite => {
                Some(sha256_hex(&it.bytes))
            }
            Action::Drifted => prior_files.get(it.path.as_str()).map(|(_, f)| f.sha256.clone()),
            _ => None,
        };
        let Some(sha256) = entry else { continue };
        let agent = agents.entry(it.owner.clone()).or_insert_with(|| LockAgent {
            id: it.owner.clone(),
            ..Default::default()
        });
        if it.action != Action::Drifted {
            agent.weftos_version = crate::VERSION.to_string();
            agent.commit = src.commit.clone();
            if src.package_ids().contains(&it.owner) {
                agent.digest = package_digest(src, &it.owner)?;
                agent.version = package_version(src, &it.owner)
                    .unwrap_or_else(|| crate::VERSION.to_string());
            }
        }
        agent.files.push(LockFile { path: it.path.clone(), host, sha256, merge: it.merge });
    }

    let mut teams: Vec<LockTeam> = prior_lock
        .teams
        .iter()
        .filter(|t| !opts.drop_teams.contains(&t.name))
        .cloned()
        .collect();
    if let Some(name) = &team {
        let at = teams.iter().position(|t| &t.name == name);
        if let Some(rec) = team_record(opts, src, name, at.map(|i| &teams[i]))? {
            match at {
                Some(i) => teams[i] = rec,
                None => teams.push(rec),
            }
        }
    }

    let context = match items.iter().find(|i| i.path == CONTEXT_PATH) {
        Some(i) if i.action == Action::Seed => {
            Some(LockContext { path: i.path.clone(), sha256: sha256_hex(&i.bytes) })
        }
        Some(i) if i.action == Action::Delete => None,
        _ => prior_lock.context.clone(),
    };

    // Nothing of ours left: a context the owner has edited is theirs now.
    let removing = !opts.remove.is_empty() || !opts.drop_teams.is_empty();
    let context = if removing && agents.values().all(|a| a.files.is_empty()) { None } else { context };

    let last_team = team
        .or_else(|| prior_lock.team.clone().filter(|t| teams.iter().any(|x| &x.name == t)))
        .or_else(|| teams.first().map(|t| t.name.clone()));
    let mut lock = Lock {
        schema: SCHEMA,
        renderer: RENDERER.into(),
        weftos_version: crate::VERSION.to_string(),
        commit: src.commit.clone(),
        source: src.origin.clone(),
        team: last_team,
        agents: agents.into_values().filter(|a| !a.files.is_empty()).collect(),
        teams,
        context,
    };
    lock.normalize();
    Ok(lock)
}
