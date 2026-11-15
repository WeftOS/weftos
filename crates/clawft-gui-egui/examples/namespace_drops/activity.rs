//! Files the agents are touching.
//!
//! A hook can append one JSON object per line to
//! `namespace-drops-activity.jsonl`, or write a JSON array. The shape is
//! actor, event, tool, paths, and ts_ms. The text field is ignored. A secret
//! in the actor, the tool, or a path drops the whole event. Heat halves
//! every 60 seconds. Edits start hotter than reads.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use super::feed::{self, Contribution, Signal};

const HALF_MS: f32 = 60_000.0;
const CAP: usize = 80;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `intensity` at `hot_ms`, cooled to `now_ms`. A zero stamp stays cold.
pub fn live_heat(intensity: f32, hot_ms: u64, now_ms: u64) -> f32 {
    if hot_ms == 0 || intensity <= 0.0 {
        return 0.0;
    }
    let dt = now_ms.saturating_sub(hot_ms) as f32;
    intensity * 0.5f32.powf(dt / HALF_MS)
}

pub fn pull(dir: &Path, now_ms: u64) -> Contribution {
    let path = dir.join("namespace-drops-activity.jsonl");
    let Ok(text) = std::fs::read_to_string(path) else {
        return Contribution::empty("activity");
    };
    contribution_from(&text, now_ms)
}

pub fn contribution_from(text: &str, now_ms: u64) -> Contribution {
    let mut events = Vec::new();
    let trimmed = text.trim();
    if trimmed.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<Value>(trimmed)
            && let Some(rows) = v.as_array()
        {
            for row in rows {
                if let Some(ev) = event_from(row) {
                    events.push(ev);
                }
            }
        }
    } else {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if let Some(ev) = event_from(&v) {
                events.push(ev);
            }
        }
    }
    events.sort_by_key(|e| e.ts_ms);
    let mut by_actor: HashMap<String, String> = HashMap::new();
    let mut files: HashMap<(String, String), Touch> = HashMap::new();
    for ev in events {
        let Some((kind, heat)) = tool_heat(&ev.tool, &ev.event) else {
            continue;
        };
        if ev.paths.is_empty() {
            let Some(project) = by_actor.get(&ev.agent).cloned() else {
                continue;
            };
            let ns = format!("{}/here", ev.agent);
            fold_touch(&mut files, &ev, project, ns, kind, heat);
            continue;
        }
        for path in &ev.paths {
            let Some(project) = project_of_path(path) else {
                continue;
            };
            let Some(ns) = ns_of(path) else { continue };
            by_actor.insert(ev.agent.clone(), project.clone());
            fold_touch(&mut files, &ev, project, ns, kind, heat);
        }
    }
    let mut rows: Vec<Touch> = files.into_values().collect();
    rows.sort_by_key(|t| t.ts_ms);
    if rows.len() > CAP {
        let drop_n = rows.len() - CAP;
        rows.drain(0..drop_n);
    }
    let mut out = Contribution::empty("activity");
    let warm = rows
        .iter()
        .filter(|t| live_heat(t.heat, t.ts_ms, now_ms) >= 0.03)
        .count();
    for touch in rows {
        let id = format!("activity/{}/{}", touch.project, touch.ns);
        out.signals.push(Signal {
            id,
            group: 4,
            project: touch.project,
            agent: touch.agent,
            ns: touch.ns,
            kind: touch.kind,
            mass: 48 + touch.hits as u32 * 16,
            hits: touch.hits,
            mag: touch.heat.clamp(0.15, 1.0),
            order: Some(order_stamp(touch.ts_ms)),
            pace: 0.0,
            quality: 0.0,
            cadence: 0.0,
            coherence: 0.0,
            failing: false,
            label: String::new(),
            heat: touch.heat,
            hot_ms: touch.ts_ms,
        });
    }
    if !out.signals.is_empty() {
        out.note = if warm == 0 {
            "the files have gone quiet.".into()
        } else {
            format!("{warm} files still warm.")
        };
    }
    out
}

struct RawEvent {
    agent: String,
    tool: String,
    event: String,
    paths: Vec<String>,
    ts_ms: u64,
}

struct Touch {
    agent: String,
    project: String,
    ns: String,
    kind: u8,
    heat: f32,
    ts_ms: u64,
    hits: u16,
}

fn fold_touch(
    files: &mut HashMap<(String, String), Touch>,
    ev: &RawEvent,
    project: String,
    ns: String,
    kind: u8,
    heat: f32,
) {
    let key = (project.clone(), ns.clone());
    if let Some(row) = files.get_mut(&key) {
        row.hits = row.hits.saturating_add(1);
        if ev.ts_ms >= row.ts_ms {
            row.agent = ev.agent.clone();
            row.kind = kind;
            row.heat = heat;
            row.ts_ms = ev.ts_ms;
        }
        return;
    }
    files.insert(
        key,
        Touch {
            agent: ev.agent.clone(),
            project,
            ns,
            kind,
            heat,
            ts_ms: ev.ts_ms,
            hits: 1,
        },
    );
}

fn event_from(v: &Value) -> Option<RawEvent> {
    let actor = v.get("actor").and_then(|a| a.as_str()).unwrap_or("");
    if actor.is_empty() || tainted(actor) {
        return None;
    }
    let tool = v.get("tool").and_then(|t| t.as_str()).unwrap_or("");
    if tainted(tool) {
        return None;
    }
    let event = v
        .get("event")
        .and_then(|e| e.as_str())
        .unwrap_or("post_tool_use");
    if tainted(event) {
        return None;
    }
    let mut paths = Vec::new();
    if let Some(rows) = v.get("paths").and_then(|p| p.as_array()) {
        for row in rows {
            let Some(path) = row.as_str() else { continue };
            if tainted(path) {
                return None;
            }
            paths.push(path.to_string());
        }
    } else if let Some(path) = v.get("path").and_then(|p| p.as_str()) {
        if tainted(path) {
            return None;
        }
        paths.push(path.to_string());
    }
    let ts_ms = v
        .get("ts_ms")
        .and_then(|n| n.as_u64())
        .or_else(|| v.get("ts").and_then(|n| n.as_u64()))
        .unwrap_or(0);
    Some(RawEvent {
        agent: agent_of(actor),
        tool: tool.to_string(),
        event: event.to_string(),
        paths,
        ts_ms,
    })
}

fn tainted(s: &str) -> bool {
    let low = s.to_ascii_lowercase();
    low.contains("sk-")
        || low.contains("bearer")
        || low.contains("api_key")
        || low.contains("password")
        || low.contains("token=")
}

fn tool_heat(tool: &str, event: &str) -> Option<(u8, f32)> {
    let event = event.to_ascii_lowercase();
    if event == "user_prompt" {
        return Some((3, 0.3));
    }
    if event == "stop" {
        return Some((4, 0.2));
    }
    match tool {
        "Read" | "Grep" | "Glob" | "ToolSearch" => Some((0, 0.4)),
        "Edit" | "MultiEdit" | "Write" | "Delete" | "StrReplace" | "NotebookEdit" => Some((1, 1.0)),
        "Bash" | "Shell" | "shell" => Some((2, 0.55)),
        _ => {
            let t = tool.to_ascii_lowercase();
            if t.contains("edit") || t.contains("write") || t.contains("delete") {
                Some((1, 1.0))
            } else if t.contains("read") || t.contains("grep") || t.contains("glob") {
                Some((0, 0.4))
            } else if t.contains("bash") || t.contains("shell") {
                Some((2, 0.55))
            } else if tool.is_empty() {
                None
            } else {
                Some((4, 0.2))
            }
        }
    }
}

pub fn project_of_path(path: &str) -> Option<String> {
    let low = path.replace('\\', "/").to_ascii_lowercase();
    if low.contains("other-project") {
        return Some("other-project".into());
    }
    if low.contains("/example-project") || low.contains("example-project/") {
        return Some("example-project".into());
    }
    if low.contains("weftos") {
        return Some("weftos".into());
    }
    const SKIP: &[&str] = &[
        "users", "volumes", "home", "tmp", "var", "private", "src", "crates", "examples", "dev",
    ];
    let mut last = None;
    for seg in low.split('/') {
        let Some(tok) = feed::keep_token(seg) else {
            continue;
        };
        let fold = tok.to_ascii_lowercase();
        if SKIP.contains(&fold.as_str()) {
            continue;
        }
        last = Some(tok);
    }
    last
}

fn ns_of(path: &str) -> Option<String> {
    let parts: Vec<String> = path
        .split(['/', '\\'])
        .filter_map(feed::keep_token)
        .collect();
    match parts.len() {
        0 => None,
        1 => Some(parts[0].clone()),
        n => Some(format!("{}/{}", parts[n - 2], parts[n - 1])),
    }
}

fn agent_of(actor: &str) -> String {
    let head = actor.split([':', '/']).next().unwrap_or(actor);
    feed::keep_token(head).unwrap_or_else(|| "agent".into())
}

fn order_stamp(ts_ms: u64) -> String {
    let secs = ts_ms / 1000;
    let ms = ts_ms % 1000;
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    let hh = tod / 3600;
    let mm = (tod % 3600) / 60;
    let ss = tod % 60;
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{ms:03}Z")
}

/// Days since 1970-01-01 to a civil date. Howard Hinnant, public domain.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldAsk {
    Empty,
    Happening,
    Doing(String),
    Follow(String),
    Stop,
    Overview,
    Unknown,
}

pub struct Sight {
    pub agent: String,
    pub project: String,
    pub ns: String,
    pub kind: u8,
    pub heat: f32,
    pub standing: bool,
}

pub fn parse_ask(raw: &str) -> FieldAsk {
    let s = raw.trim().to_ascii_lowercase();
    let s = s.trim_end_matches(['?', '.']).trim();
    if s.is_empty() {
        return FieldAsk::Empty;
    }
    if s == "what is happening" || s == "who is here" {
        return FieldAsk::Happening;
    }
    if s == "stop" || s == "stop following" {
        return FieldAsk::Stop;
    }
    if s == "overview" || s == "frame" {
        return FieldAsk::Overview;
    }
    if let Some(rest) = s.strip_prefix("follow ") {
        let name = rest.trim();
        if name.is_empty() {
            return FieldAsk::Unknown;
        }
        return FieldAsk::Follow(name.to_string());
    }
    if let Some(rest) = s.strip_prefix("what is ") {
        let name = rest.strip_suffix(" doing").unwrap_or(rest).trim();
        if name.is_empty() || name == "happening" {
            return FieldAsk::Happening;
        }
        return FieldAsk::Doing(name.to_string());
    }
    FieldAsk::Unknown
}

pub fn answer(ask: &FieldAsk, sights: &[Sight]) -> String {
    match ask {
        FieldAsk::Empty => String::new(),
        FieldAsk::Happening => happening(sights),
        FieldAsk::Doing(name) => doing(name, sights),
        FieldAsk::Follow(name) => format!("following {name}."),
        FieldAsk::Stop => "stopped.".into(),
        FieldAsk::Overview => "the whole field.".into(),
        FieldAsk::Unknown => {
            "ask what is happening, what an agent is doing, follow, stop, or overview.".into()
        }
    }
}

fn happening(sights: &[Sight]) -> String {
    let mut best: Vec<&Sight> = Vec::new();
    for sight in sights.iter().filter(|s| !s.standing && s.heat >= 0.03) {
        if let Some(row) = best.iter_mut().find(|e| e.agent == sight.agent) {
            if sight.heat > row.heat {
                *row = sight;
            }
        } else {
            best.push(sight);
        }
    }
    best.sort_by(|a, b| {
        b.heat
            .partial_cmp(&a.heat)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    best.truncate(3);
    if best.is_empty() {
        return "the room holds and waits.".into();
    }
    let lines: Vec<String> = best
        .iter()
        .map(|s| super::stream::narrate(&s.agent, s.kind, &s.ns, &s.project))
        .collect();
    super::stream::join_stream(&lines)
}

fn doing(name: &str, sights: &[Sight]) -> String {
    let mut best: Option<&Sight> = None;
    for sight in sights {
        if sight.standing || !sight.agent.eq_ignore_ascii_case(name) {
            continue;
        }
        if best.is_none_or(|b| sight.heat > b.heat) {
            best = Some(sight);
        }
    }
    match best {
        Some(s) if s.heat >= 0.03 => super::stream::join_stream(&[super::stream::narrate(
            &s.agent, s.kind, &s.ns, &s.project,
        )]),
        _ => format!("{name} holds and waits."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit_line() -> String {
        r#"{"actor":"claude:tab0","event":"post_tool_use","tool":"Edit","paths":["/work/example-project/src/foo.rs"],"text":"sk-secret-should-stay-out","ts_ms":1000000}"#.into()
    }

    #[test]
    fn an_edit_is_hotter_than_a_read_and_text_is_dropped() {
        let read = r#"{"actor":"claude:tab0","event":"post_tool_use","tool":"Read","paths":["/work/example-project/src/foo.rs"],"text":"the secret sk-abc stays in this field","ts_ms":1000000}"#;
        let now = 1_000_000u64;
        let edited = contribution_from(&edit_line(), now);
        let reading = contribution_from(read, now);
        assert_eq!(edited.signals.len(), 1);
        assert_eq!(reading.signals.len(), 1);
        assert!(edited.signals[0].heat > reading.signals[0].heat);
        assert_eq!(edited.signals[0].kind, 1);
        assert_eq!(reading.signals[0].kind, 0);
        assert_eq!(edited.signals[0].agent, "claude");
        assert_eq!(edited.signals[0].project, "example-project");
        assert_eq!(edited.signals[0].ns, "src/foo.rs");
        let blob = format!(
            "{} {} {}",
            edited.signals[0].ns, edited.signals[0].label, edited.note
        );
        assert!(!blob.contains("sk-"));
        assert!(edited.bpm.is_none());
    }

    #[test]
    fn a_secret_path_drops_the_event() {
        let line = r#"{"actor":"claude","event":"post_tool_use","tool":"Read","paths":["/tmp/token=abc"],"ts_ms":10}"#;
        assert!(contribution_from(line, 10).signals.is_empty());
    }

    #[test]
    fn heat_halves_in_a_minute_and_a_bash_is_a_command() {
        assert!((live_heat(1.0, 1_000, 61_000) - 0.5).abs() < 0.01);
        let line = r#"{"actor":"codex/main","event":"post_tool_use","tool":"Bash","paths":["/work/other-project/crates/a.rs"],"ts_ms":50}"#;
        let got = contribution_from(line, 50);
        assert_eq!(got.signals[0].kind, 2);
        assert_eq!(got.signals[0].agent, "codex");
        assert_eq!(got.signals[0].project, "other-project");
    }

    #[test]
    fn a_prompt_without_a_file_does_not_open_a_cell() {
        let line = r#"{"actor":"claude","event":"user_prompt","tool":"","ts_ms":10}"#;
        assert!(contribution_from(line, 10).signals.is_empty());
    }

    #[test]
    fn the_field_answers_from_the_hottest_touch() {
        assert_eq!(
            parse_ask("what is claude doing?"),
            FieldAsk::Doing("claude".into())
        );
        assert_eq!(
            parse_ask("follow claude"),
            FieldAsk::Follow("claude".into())
        );
        assert_eq!(parse_ask("stop following"), FieldAsk::Stop);
        assert_eq!(parse_ask("overview"), FieldAsk::Overview);
        let sights = vec![Sight {
            agent: "claude".into(),
            project: "example-project".into(),
            ns: "src/foo.rs".into(),
            kind: 1,
            heat: 0.8,
            standing: false,
        }];
        assert_eq!(
            answer(&FieldAsk::Doing("claude".into()), &sights),
            "claude rewrites src/foo.rs and changes example-project."
        );
        assert_eq!(
            answer(&FieldAsk::Doing("codex".into()), &sights),
            "codex holds and waits."
        );
        assert_eq!(
            answer(&FieldAsk::Happening, &sights),
            "claude rewrites src/foo.rs and changes example-project."
        );
    }

    #[test]
    fn an_array_and_a_quiet_room() {
        let body = r#"[{"actor":"claude","event":"post_tool_use","tool":"Edit","paths":["/work/weftos/src/a.rs"],"ts_ms":1000}]"#;
        let got = contribution_from(body, 1_000);
        assert_eq!(got.signals[0].project, "weftos");
        assert!(order_stamp(0).starts_with("1970-01-01"));
        let old = contribution_from(body, 1_000 + 6 * 60_000);
        assert!(old.note.contains("quiet"));
    }
}
