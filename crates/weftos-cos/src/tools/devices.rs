//! `devices.status`: the owner's own devices, with freshness and a plain
//! verdict each. Read-only. Glasses (Mentra listen and speak loops) come from
//! the status files those loops write; the pad (CrowPanel leaf) from the
//! daemon's `display.status`; the e-ink glance from the status file
//! `glance-serial` writes. A source that cannot be read says so and why; it is
//! never reported as healthy or broken on a guess.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use super::nodes::{Dial, dial};
use super::{Gate, Tool, ToolOutcome, ToolSpec};
use crate::CosError;
use crate::config::DevicesConfig;

/// A loop that has not updated its status file for longer than this is not
/// running, whatever the file still claims.
pub const STALE_SECS: i64 = 30;

/// Read-only daemon calls the pad check needs. `Ok(None)`: no daemon answered.
#[async_trait]
pub trait LeafRpc: Send + Sync {
    async fn call(&self, method: &str, params: Value) -> Result<Option<Value>, CosError>;
}

/// The runtime source: the weaver daemon's socket, same endpoint resolver as
/// `weaver leaf status`.
pub struct DaemonLeafRpc;

#[async_trait]
impl LeafRpc for DaemonLeafRpc {
    async fn call(&self, method: &str, params: Value) -> Result<Option<Value>, CosError> {
        if method != "display.status" {
            return Err(super::tool_err("devices.status", format!("{method} is not a read-only device method")));
        }
        let mut client = match dial().await {
            Ok(c) => c,
            Err(Dial::Unreachable(_)) => return Ok(None),
            Err(Dial::Refused(why)) => return Err(super::tool_err("devices.status", why)),
        };
        let resp = client
            .call(clawft_rpc::Request::with_params(method, params))
            .await
            .map_err(|e| super::tool_err("devices.status", e))?;
        if !resp.ok {
            return Err(super::tool_err("devices.status", format!("{method}: {}", resp.error.unwrap_or_default())));
        }
        Ok(Some(resp.result.unwrap_or(Value::Null)))
    }
}

pub struct DevicesStatus {
    pub cfg: DevicesConfig,
    pub rpc: Arc<dyn LeafRpc>,
}

#[async_trait]
impl Tool for DevicesStatus {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: "devices.status",
            description: "Status of the owner's own devices with freshness and a verdict each: the Mentra glasses (microphone listen loop, speaker), the pad (CrowPanel leaf: connected, who holds the display) and the e-ink glance. Use for any question about a device, the glasses, the mic, the pad or the e-ink panel. Read-only.",
            parameters: json!({"type": "object", "properties": {}}),
            gate: Gate::Open,
        }
    }

    async fn call(&self, _args: Value) -> Result<ToolOutcome, CosError> {
        let now = Utc::now();
        let glasses = glasses_report(&self.cfg.mentra_dir, now);
        let pad = pad_report(&self.cfg, self.rpc.as_ref()).await;
        let eink = eink_report(&self.cfg.glance_status, now);
        let summary = format!(
            "glasses mic: {}; pad: {}; e-ink: {}",
            glasses["mic"]["state"].as_str().unwrap_or("unknown"),
            pad["state"].as_str().unwrap_or("unknown"),
            eink["state"].as_str().unwrap_or("unknown"),
        );
        Ok(ToolOutcome { summary, result: json!({"glasses": glasses, "pad": pad, "eink": eink}) })
    }
}

enum Read {
    Missing,
    Corrupt(String),
    Ok(Value),
}

fn read_json(path: &Path) -> Read {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Read::Missing,
        Err(e) => Read::Corrupt(e.to_string()),
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.is_object() => Read::Ok(v),
            Ok(_) => Read::Corrupt("not a JSON object".into()),
            Err(e) => Read::Corrupt(e.to_string()),
        },
    }
}

fn parse_time(v: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(v.as_str()?).ok().map(|t| t.with_timezone(&Utc))
}

fn ago(secs: i64) -> String {
    match secs.max(0) {
        s if s < 90 => format!("{s} s ago"),
        s if s < 90 * 60 => format!("{} min ago", s / 60),
        s if s < 48 * 3600 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86400),
    }
}

fn pid_alive(pid: u64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn clip(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push_str("...");
    }
    out
}

/// A status-file read that could not produce a body: the state and verdict.
fn unreadable(what: &str, path: &Path, r: &Read) -> Option<Value> {
    match r {
        Read::Missing => Some(json!({
            "state": "missing",
            "verdict": format!("No {what} status file at {}: that loop has never run on this machine, or writes elsewhere. I can't tell if it works from here.", path.display()),
        })),
        Read::Corrupt(why) => Some(json!({
            "state": "unreadable",
            "verdict": format!("The {what} status file at {} is unreadable ({why}); I can't tell its state from it.", path.display()),
        })),
        Read::Ok(_) => None,
    }
}

fn take_json(t: &Value, now: DateTime<Utc>) -> Value {
    let age = parse_time(&t["at"]).map(|at| (now - at).num_seconds());
    json!({
        "at": t["at"], "age": age.map(ago), "rms": t["rms"], "voiced_pct": t["voiced"].as_f64().map(|v| (v * 100.0).round()),
        "outcome": t["outcome"], "reason": t["reason"], "transcript": t["transcript"],
    })
}

/// The glasses: microphone listen loop, speaker watcher and the control file.
pub fn glasses_report(dir: &Path, now: DateTime<Utc>) -> Value {
    json!({
        "mic": mic_report(&dir.join("listen.status.json"), now),
        "speaker": speaker_report(&dir.join("speak.status.json"), now),
        "control": control_report(&dir.join("control.json")),
    })
}

fn mic_report(path: &Path, now: DateTime<Utc>) -> Value {
    let r = read_json(path);
    if let Some(v) = unreadable("glasses listen loop", path, &r) {
        return v;
    }
    let Read::Ok(v) = r else { unreachable!("handled above") };
    let age = parse_time(&v["updated_at"]).map(|t| (now - t).num_seconds());
    let recent: Vec<&Value> = v["recent"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    let posted: Vec<&&Value> = recent.iter().filter(|t| t["outcome"] == "posted").collect();
    let skipped = recent.iter().filter(|t| t["outcome"] == "skipped").count();
    let mut reasons: std::collections::BTreeMap<String, usize> = Default::default();
    for t in recent.iter().filter(|t| t["outcome"] == "skipped") {
        *reasons.entry(t["reason"].as_str().unwrap_or("unknown").to_string()).or_default() += 1;
    }
    let reasons_txt = reasons.iter().map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", ");
    let last_posted = posted.last().map(|t| take_json(t, now));
    let last_posted_txt = last_posted.as_ref().map(|t| {
        format!("the last posted take was {} ('{}')", t["age"].as_str().unwrap_or("at an unknown time"), clip(t["transcript"].as_str().unwrap_or(""), 80))
    });
    let (state, verdict) = match age {
        None => ("unknown", "The listen status file has no readable updated_at, so I can't tell if the loop is running.".to_string()),
        Some(a) if a > STALE_SECS => (
            "stale",
            format!(
                "The glasses mic loop is not running: its status was last updated {}.{}",
                ago(a),
                last_posted_txt.as_ref().map(|t| format!(" Before that, {t}.")).unwrap_or_default()
            ),
        ),
        Some(_) if v["listening"] == false => (
            "off",
            "The glasses mic loop is up but listening is switched off, so nothing is being captured.".to_string(),
        ),
        Some(_) if recent.is_empty() => ("running", "The glasses mic loop is running and listening; it has not captured a take yet.".to_string()),
        Some(_) if posted.is_empty() => (
            "silent",
            format!("The mic loop is running, but the last {} takes were all skipped ({reasons_txt}): it hears the room but nothing reached the floor/voice threshold.", recent.len()),
        ),
        Some(_) => (
            "working",
            format!("The glasses mic is working: {}. Of the last {} takes, {} were posted and {skipped} skipped.", last_posted_txt.unwrap_or_default(), recent.len(), posted.len()),
        ),
    };
    json!({
        "state": state, "verdict": verdict, "path": path.display().to_string(),
        "status_age": age.map(ago), "listening": v["listening"], "armed": v["armed"],
        "pid": v["pid"], "pid_alive": v["pid"].as_u64().map(pid_alive),
        "last_take": v["last_take"].is_object().then(|| take_json(&v["last_take"], now)),
        "last_posted": last_posted,
        "recent": {"takes": recent.len(), "posted": posted.len(), "skipped": skipped, "skip_reasons": reasons},
    })
}

fn speaker_report(path: &Path, now: DateTime<Utc>) -> Value {
    let r = read_json(path);
    if let Some(v) = unreadable("glasses speak watcher", path, &r) {
        return v;
    }
    let Read::Ok(v) = r else { unreachable!("handled above") };
    let age = parse_time(&v["updated_at"]).map(|t| (now - t).num_seconds());
    let play = v["last_play"].is_object().then(|| {
        let p = &v["last_play"];
        json!({"at": p["at"], "age": parse_time(&p["at"]).map(|t| ago((now - t).num_seconds())), "chars": p["chars"], "ok": p["ok"], "error": p["error"]})
    });
    let play_txt = match &play {
        Some(p) if p["ok"] == true => format!("its last play was {} and succeeded", p["age"].as_str().unwrap_or("at an unknown time")),
        Some(p) => format!("its last play ({}) failed: {}", p["age"].as_str().unwrap_or("unknown time"), p["error"].as_str().unwrap_or("no error given")),
        None => "it has not played anything yet".to_string(),
    };
    let (state, verdict) = match age {
        None => ("unknown", "The speak status file has no readable updated_at.".to_string()),
        Some(a) if a > STALE_SECS => ("stale", format!("The glasses speak watcher is not running: last updated {}; {play_txt}.", ago(a))),
        Some(_) if v["up"] == false => ("down", format!("The glasses speak watcher reports it is down; {play_txt}.")),
        Some(_) => ("up", format!("The glasses speak watcher is up; {play_txt}.")),
    };
    json!({"state": state, "verdict": verdict, "path": path.display().to_string(), "status_age": age.map(ago), "last_play": play})
}

fn control_report(path: &Path) -> Value {
    match read_json(path) {
        Read::Missing => json!({"present": false, "note": "the optional control file is absent; that is normal and says nothing about the glasses"}),
        Read::Corrupt(why) => json!({"present": true, "readable": false, "reason": why}),
        Read::Ok(v) => json!({"present": true, "readable": true, "content": v}),
    }
}

/// Leaf ids provisioned on this machine: the file stems in the registry dir.
/// `Err` when the directory cannot be listed, so the caller can say why.
fn registry_ids(dir: &Path) -> Result<Vec<String>, String> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut ids: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            let stem = p.file_stem()?.to_str()?.to_string();
            (p.extension()? == "json" && !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_hexdigit())).then_some(stem)
        })
        .collect();
    ids.sort();
    Ok(ids)
}

/// The pad: leaf ids come from the registry of provisioned leaves and the
/// config (there is no leaf-list RPC); each is asked `display.status`, the
/// call `weaver leaf status` makes.
async fn pad_report(cfg: &DevicesConfig, rpc: &dyn LeafRpc) -> Value {
    let mut ids: Vec<String> = cfg.pad_leaf_ids.clone();
    let configured = ids.len();
    match registry_ids(&cfg.leaf_registry_dir) {
        Ok(found) => {
            for id in found {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        Err(e) => {
            return json!({"state": "unknown", "verdict": format!("I can't read the leaf registry at {} ({e}), so I don't know which leaf is the pad.", cfg.leaf_registry_dir.display())});
        }
    }
    if ids.is_empty() {
        return json!({
            "state": "unknown",
            "verdict": format!("No leaf id is known: none is configured (devices.pad_leaf_ids) and {} lists no provisioned leaf, and there is no leaf-list RPC. Give me the pad's leaf id and I can ask the daemon directly.", cfg.leaf_registry_dir.display()),
        });
    }
    let mut leaves = Vec::new();
    for id in &ids {
        match rpc.call("display.status", json!({"leaf_id": id})).await {
            Ok(Some(v)) => {
                let connected = v["connected"].as_bool().unwrap_or(false);
                let holder = v["session"].as_str().map(str::to_string);
                let verdict = match (connected, &holder) {
                    (true, Some(h)) => format!("connected; the display is held by {h}"),
                    (true, None) => "connected; the display is free".into(),
                    (false, Some(h)) => format!("not connected to this daemon (a session by {h} is still recorded)"),
                    (false, None) => "not connected to this daemon".into(),
                };
                leaves.push(json!({"leaf_id": id, "connected": connected, "session_holder": holder, "verdict": verdict}));
            }
            Ok(None) => {
                return json!({
                    "state": "unknown",
                    "verdict": "I can't reach the weaver daemon, so I can't tell whether the pad is connected. That says nothing about the pad itself; `weaver leaf status --leaf-id <id>` asks the same question once the daemon is up.",
                    "leaf_ids": ids,
                });
            }
            Err(e) => leaves.push(json!({"leaf_id": id, "verdict": format!("display.status failed: {e}")})),
        }
    }
    let connected = leaves.iter().filter(|l| l["connected"] == true).count();
    let state = if connected > 0 {
        "connected"
    } else if leaves.iter().all(|l| l["connected"] == false) {
        "not_connected"
    } else {
        "unknown"
    };
    let verdict = if leaves.len() == 1 {
        format!("The pad leaf is {}.", leaves[0]["verdict"].as_str().unwrap_or("unknown"))
    } else {
        format!("{connected} of {} known leaves are connected.", leaves.len())
    };
    json!({"state": state, "verdict": verdict, "leaves": leaves, "ids_from_config": configured})
}

/// The e-ink glance: the status file `weftos-cos glance-serial` writes, plus
/// whether its pid is alive.
pub fn eink_report(path: &Path, now: DateTime<Utc>) -> Value {
    let r = read_json(path);
    if let Some(mut v) = unreadable("e-ink glance", path, &r) {
        if matches!(r, Read::Missing) {
            v["state"] = json!("not_running");
            v["verdict"] = json!(format!("No glance status file at {}: `weftos-cos glance-serial` has not run here, so the e-ink panel is not being fed.", path.display()));
        }
        return v;
    }
    let Read::Ok(v) = r else { unreachable!("handled above") };
    let age = parse_time(&v["updated_at"]).map(|t| (now - t).num_seconds());
    let alive = v["pid"].as_u64().map(pid_alive);
    let port = v["port"].as_str().unwrap_or("its serial port");
    let (state, verdict) = match (alive, age) {
        (Some(false), _) => ("not_running", format!("The glance process (pid {}) is gone; the e-ink panel is no longer being fed.", v["pid"])),
        (_, Some(a)) if a > 3 * STALE_SECS => ("stale", format!("The glance status was last written {}; the process may be hung.", ago(a))),
        (Some(true), _) if v["port_open"] == false => (
            "port_down",
            format!("The glance process is running but cannot open {port}{}: the panel is not updating.", v["last_error"].as_str().map(|e| format!(" ({e})")).unwrap_or_default()),
        ),
        (Some(true), _) => ("running", format!("The glance process holds {port} and last wrote the panel {}.", v["last_write_at"].as_str().and_then(|s| parse_time(&json!(s))).map(|t| ago((now - t).num_seconds())).unwrap_or_else(|| "(not yet)".into()))),
        (None, _) => ("unknown", "The glance status file has no pid, so I can't tell if it is running.".to_string()),
    };
    json!({"state": state, "verdict": verdict, "path": path.display().to_string(), "status_age": age.map(ago), "pid": v["pid"], "port": v["port"]})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const NOW: &str = "2026-10-13T15:36:40Z";
    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(NOW).unwrap().with_timezone(&Utc)
    }

    fn take(at: &str, outcome: &str, reason: &str, text: &str) -> Value {
        json!({"at": at, "rms": 100.0, "voiced": 0.1, "outcome": outcome, "reason": reason, "transcript": text})
    }

    fn mic_file(updated: &str, listening: bool, recent: Vec<Value>) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("listen.status.json");
        let last = recent.last().cloned().unwrap_or(Value::Null);
        std::fs::write(&p, json!({"armed": true, "listening": listening, "pid": std::process::id(), "updated_at": updated, "last_take": last, "recent": recent}).to_string()).unwrap();
        (dir, p)
    }

    #[test]
    fn fresh_with_posted_take_is_working() {
        let (_d, p) = mic_file("2026-10-13T15:36:30Z", true, vec![
            take("2026-10-13T15:32:40Z", "posted", "", "buy useful data"),
            take("2026-10-13T15:36:30Z", "skipped", "quiet", ""),
        ]);
        let v = mic_report(&p, now());
        assert_eq!(v["state"], "working");
        let verdict = v["verdict"].as_str().unwrap();
        assert!(verdict.contains("glasses mic is working") && verdict.contains("4 min ago") && verdict.contains("buy useful data"), "{verdict}");
        assert_eq!(v["recent"]["posted"], 1);
        assert_eq!(v["recent"]["skipped"], 1);
    }

    #[test]
    fn all_skipped_says_running_but_silent() {
        let takes = (0..5).map(|_| take("2026-10-13T15:36:00Z", "skipped", "quiet", "")).collect();
        let (_d, p) = mic_file("2026-10-13T15:36:30Z", true, takes);
        let v = mic_report(&p, now());
        assert_eq!(v["state"], "silent");
        let verdict = v["verdict"].as_str().unwrap();
        assert!(verdict.contains("running, but the last 5 takes were all skipped") && verdict.contains("quiet 5"), "{verdict}");
    }

    #[test]
    fn stale_status_is_not_running_and_keeps_last_posted() {
        let (_d, p) = mic_file("2026-10-13T15:30:00Z", true, vec![take("2026-10-13T15:29:00Z", "posted", "", "hello there")]);
        let v = mic_report(&p, now());
        assert_eq!(v["state"], "stale");
        let verdict = v["verdict"].as_str().unwrap();
        assert!(verdict.contains("not running") && verdict.contains("hello there"), "{verdict}");
    }

    #[test]
    fn exactly_thirty_seconds_is_still_fresh() {
        let (_d, p) = mic_file("2026-10-13T15:36:10Z", true, vec![]);
        assert_eq!(mic_report(&p, now())["state"], "running");
    }

    #[test]
    fn listening_off_is_off() {
        let (_d, p) = mic_file("2026-10-13T15:36:30Z", false, vec![take("2026-10-13T15:36:00Z", "posted", "", "x")]);
        assert_eq!(mic_report(&p, now())["state"], "off");
    }

    #[test]
    fn missing_and_corrupt_refuse_to_guess() {
        let dir = tempfile::tempdir().unwrap();
        let g = glasses_report(dir.path(), now());
        assert_eq!(g["mic"]["state"], "missing");
        assert!(g["mic"]["verdict"].as_str().unwrap().contains("can't tell"));
        assert_eq!(g["speaker"]["state"], "missing");
        assert_eq!(g["control"]["present"], false);
        std::fs::write(dir.path().join("listen.status.json"), "{not json").unwrap();
        std::fs::write(dir.path().join("speak.status.json"), "[1]").unwrap();
        std::fs::write(dir.path().join("control.json"), "nope").unwrap();
        let g = glasses_report(dir.path(), now());
        assert_eq!(g["mic"]["state"], "unreadable");
        assert_eq!(g["speaker"]["state"], "unreadable");
        assert_eq!(g["control"]["readable"], false);
    }

    #[test]
    fn speaker_up_stale_and_failed_play() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("speak.status.json");
        let write = |updated: &str, ok: bool| {
            std::fs::write(&p, json!({"up": true, "updated_at": updated, "last_play": {"at": "2026-10-13T15:36:20Z", "chars": 81, "ok": ok, "error": if ok { Value::Null } else { json!("no route") }}}).to_string()).unwrap();
        };
        write("2026-10-13T15:36:30Z", true);
        let v = speaker_report(&p, now());
        assert_eq!(v["state"], "up");
        assert!(v["verdict"].as_str().unwrap().contains("succeeded"));
        write("2026-10-13T15:36:30Z", false);
        assert!(speaker_report(&p, now())["verdict"].as_str().unwrap().contains("no route"));
        write("2026-10-13T15:00:00Z", true);
        assert_eq!(speaker_report(&p, now())["state"], "stale");
    }

    struct FakeRpc {
        up: bool,
        status: Value,
    }
    #[async_trait]
    impl LeafRpc for FakeRpc {
        async fn call(&self, _method: &str, _params: Value) -> Result<Option<Value>, CosError> {
            Ok(self.up.then(|| self.status.clone()))
        }
    }

    fn cfg(registry: &Path, ids: &[&str]) -> DevicesConfig {
        DevicesConfig { leaf_registry_dir: registry.into(), pad_leaf_ids: ids.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    fn registry_with(ids: &[&str]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for id in ids {
            std::fs::write(d.path().join(format!("{id}.json")), "{}").unwrap();
        }
        std::fs::write(d.path().join("notes.txt"), "x").unwrap();
        d
    }

    #[tokio::test]
    async fn pad_connected_with_holder_found_from_registry() {
        let reg = registry_with(&["ab12cd"]);
        let rpc = FakeRpc { up: true, status: json!({"connected": true, "session": "cos-console"}) };
        let v = pad_report(&cfg(reg.path(), &[]), &rpc).await;
        assert_eq!(v["state"], "connected");
        assert_eq!(v["leaves"][0]["leaf_id"], "ab12cd");
        assert!(v["verdict"].as_str().unwrap().contains("held by cos-console"), "{v}");
    }

    #[tokio::test]
    async fn pad_not_connected_and_configured_id_not_duplicated() {
        let reg = registry_with(&["ab12cd"]);
        let rpc = FakeRpc { up: true, status: json!({"connected": false, "session": null}) };
        let v = pad_report(&cfg(reg.path(), &["ab12cd"]), &rpc).await;
        assert_eq!(v["state"], "not_connected");
        assert_eq!(v["leaves"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn pad_unknown_when_daemon_down_or_no_ids() {
        let reg = registry_with(&["ab12cd"]);
        let down = FakeRpc { up: false, status: Value::Null };
        let v = pad_report(&cfg(reg.path(), &[]), &down).await;
        assert_eq!(v["state"], "unknown");
        assert!(v["verdict"].as_str().unwrap().contains("can't reach the weaver daemon"));
        let empty = registry_with(&[]);
        let up = FakeRpc { up: true, status: Value::Null };
        let v = pad_report(&cfg(empty.path(), &[]), &up).await;
        assert_eq!(v["state"], "unknown");
        assert!(v["verdict"].as_str().unwrap().contains("No leaf id is known"));
        let v = pad_report(&cfg(&empty.path().join("absent"), &[]), &up).await;
        assert!(v["verdict"].as_str().unwrap().contains("No leaf id is known"));
    }

    #[tokio::test]
    async fn read_only_guard_refuses_other_methods() {
        let e = DaemonLeafRpc.call("kernel.shutdown", Value::Null).await.unwrap_err();
        assert!(e.to_string().contains("not a read-only device method"), "{e}");
    }

    #[tokio::test]
    async fn tool_summarises_all_three() {
        let dir = tempfile::tempdir().unwrap();
        let tool = DevicesStatus {
            cfg: DevicesConfig { mentra_dir: dir.path().into(), glance_status: dir.path().join("glance.json"), leaf_registry_dir: dir.path().join("reg"), pad_leaf_ids: vec![] },
            rpc: Arc::new(FakeRpc { up: false, status: Value::Null }),
        };
        let out = tool.call(json!({})).await.unwrap();
        assert!(out.summary.contains("glasses mic: missing") && out.summary.contains("pad: unknown") && out.summary.contains("e-ink: not_running"), "{}", out.summary);
        assert_eq!(tool.spec().gate, Gate::Open);
    }

    #[test]
    fn eink_running_dead_and_port_down() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("glance.json");
        let write = |pid: u64, open: bool| {
            std::fs::write(&p, json!({"pid": pid, "port": "/dev/ttyX", "port_open": open, "updated_at": "2026-10-13T15:36:35Z", "last_write_at": "2026-10-13T15:36:00Z", "last_error": "busy"}).to_string()).unwrap();
        };
        write(std::process::id() as u64, true);
        assert_eq!(eink_report(&p, now())["state"], "running");
        write(std::process::id() as u64, false);
        assert_eq!(eink_report(&p, now())["state"], "port_down");
        write(4_000_000, true);
        assert_eq!(eink_report(&p, now())["state"], "not_running");
    }
}
