//! History parser. Branches, Example-project cards, and the newest CI conclusion.
//!
//! The picture does not call git or read a board. This parser returns a
//! [`Contribution`] of standing stalks. Vectors are smoothed against the
//! last run and written back, so pace, quality, cadence, and coherence move
//! with the work instead of resetting. Titles stay off the stalk unless the
//! row is the one the panel has to name.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use super::feed::{Contribution, Seat, Signal};

/// Optional JSON board export, named by `NAMESPACE_DROPS_BOARD`.
fn board_path() -> Option<String> {
    std::env::var("NAMESPACE_DROPS_BOARD").ok()
}

/// How long a release can be in motion before it is fully built.
const RELEASE_LEAD: u64 = 6 * 60 * 60;
/// A prep mark older than this is no longer moving.
const RELEASE_FRESH: u64 = 90 * 60;

pub fn pull(persist_dir: &Path) -> Contribution {
    let prev = load_prev(&persist_dir.join("namespace-drops-history.json"));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut signals = Vec::new();
    let mut titles: HashMap<String, String> = HashMap::new();
    let mut branch_n = 0usize;
    let mut ref_texts = Vec::new();
    for (repo, path) in super::feed::repos() {
        let text = git(
            Path::new(path),
            &[
                "for-each-ref",
                "--sort=-committerdate",
                "--count=60",
                "--format=%(refname:short)%09%(committerdate:unix)",
                "refs/heads",
            ],
            4_000,
        );
        let raw = text.unwrap_or_default();
        let rows = signals_from_refs(repo, &raw, now);
        branch_n += rows.len();
        signals.extend(rows);
        ref_texts.push(raw);
    }
    if let Some(text) = board_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        let (cards, card_titles) = signals_from_board(&text);
        titles = card_titles;
        signals.extend(cards);
    }
    let card_n = signals
        .iter()
        .filter(|s| s.id.starts_with("example-project/EX-"))
        .count();
    for signal in &mut signals {
        if let Some(old) = prev.vectors.get(&signal.id) {
            signal.pace = smooth_vec(signal.pace, old[0]);
            signal.quality = smooth_vec(signal.quality, old[1]);
            signal.cadence = smooth_vec(signal.cadence, old[2]);
            signal.coherence = smooth_vec(signal.coherence, old[3]);
        }
    }
    let mut failing = Vec::new();
    for (repo, path) in super::feed::repos().iter().take(2) {
        let Some(json) = gh_runs(Path::new(path)) else {
            continue;
        };
        apply_failures(&mut signals, &titles, repo, &json, &mut failing);
    }
    let arc = release_arc(&collect_release_marks(&ref_texts, now), now);
    let measured = bpm_toward_release(
        bpm_from_quarter(rolling_quarter_avg(&commit_stamps(), now)),
        arc,
    );
    let bpm = smooth_bpm(prev.bpm, measured);
    save_prev(
        &persist_dir.join("namespace-drops-history.json"),
        bpm,
        &signals,
    );
    for signal in &mut signals {
        if signal.failing {
            signal.quality = 0.1;
        }
    }
    let mut out = Contribution::empty("history");
    for (repo, _) in super::feed::repos() {
        if signals.iter().any(|s| s.project == *repo) {
            let mut seat = Seat::new(repo.as_str(), "history");
            seat.stem = 2;
            seat.instrument = 2;
            out.seats.push(seat);
        }
    }
    out.bpm = Some(bpm);
    out.release_approach = arc.approach;
    out.release_landed = arc.landed;
    out.failing = failing;
    out.signals = signals;
    out.note = format!(
        "{branch_n} branches and {card_n} cards. color is pace, quality, cadence, and coherence. \
         weftos dashboard rows are behind sign-in, so those branches take the same four stages from how recently they moved. \
         example-project cards are the 2026-10-14 board."
    );
    if arc.landed {
        out.note.push_str(" a release landed in the last hour.");
    } else if arc.approach > 0.0 {
        out.note.push_str(" a release is building.");
    }
    out
}

struct Prev {
    bpm: Option<f32>,
    vectors: HashMap<String, [f32; 4]>,
}

fn load_prev(path: &Path) -> Prev {
    let mut prev = Prev {
        bpm: None,
        vectors: HashMap::new(),
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return prev;
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return prev;
    };
    prev.bpm = v.get("bpm").and_then(|n| n.as_f64()).map(|n| n as f32);
    if let Some(map) = v.get("vectors").and_then(|n| n.as_object()) {
        for (id, row) in map {
            let Some(arr) = row.as_array() else { continue };
            if arr.len() < 4 {
                continue;
            }
            prev.vectors.insert(
                id.clone(),
                [
                    arr[0].as_f64().unwrap_or(0.5) as f32,
                    arr[1].as_f64().unwrap_or(0.5) as f32,
                    arr[2].as_f64().unwrap_or(0.5) as f32,
                    arr[3].as_f64().unwrap_or(0.5) as f32,
                ],
            );
        }
    }
    prev
}

fn save_prev(path: &Path, bpm: f32, signals: &[Signal]) {
    let mut vectors = serde_json::Map::new();
    for signal in signals {
        vectors.insert(
            signal.id.clone(),
            serde_json::json!([
                signal.pace,
                signal.quality,
                signal.cadence,
                signal.coherence
            ]),
        );
    }
    let body = serde_json::json!({ "bpm": bpm, "vectors": vectors });
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, body.to_string());
}

pub fn smooth_vec(measured: f32, prev: f32) -> f32 {
    (0.65 * measured + 0.35 * prev).clamp(0.0, 1.0)
}

/// Three quarters of the previous tempo, then a step of at most 8.
pub fn smooth_bpm(prev: Option<f32>, measured: f32) -> f32 {
    let measured = measured.clamp(60.0, 140.0);
    let Some(prev) = prev else {
        return measured;
    };
    let prev = prev.clamp(60.0, 140.0);
    let blended = 0.75 * prev + 0.25 * measured;
    blended.clamp(prev - 8.0, prev + 8.0).clamp(60.0, 140.0)
}

/// Zero commits in the 15-minute average sits at 60.
/// Four commits in that average sits at 140.
pub fn bpm_from_quarter(per_quarter: f32) -> f32 {
    (60.0 + (per_quarter / 4.0) * 80.0).clamp(60.0, 140.0)
}

/// Average commits inside a 15-minute window, sampled once a minute across
/// the last hour. Each window sits inside that hour. A commit older than
/// the hour does not count.
pub fn rolling_quarter_avg(stamps: &[u64], now: u64) -> f32 {
    const STEP: u64 = 60;
    const WIN: u64 = 15 * 60;
    const HOUR: u64 = 60 * 60;
    let earliest = now.saturating_sub(HOUR);
    let mut sum = 0.0f32;
    let mut n = 0u32;
    let mut end = now;
    loop {
        let begin = end.saturating_sub(WIN);
        if begin < earliest {
            break;
        }
        let count = stamps
            .iter()
            .filter(|&&t| t >= begin && (t < end || (end == now && t == now)))
            .count();
        sum += count as f32;
        n += 1;
        if end < earliest + WIN + STEP {
            break;
        }
        end = end.saturating_sub(STEP);
    }
    if n == 0 { 0.0 } else { sum / n as f32 }
}

/// A mark on the way to a release, or the release itself.
/// The timestamp is unix seconds. The words that produced it are not kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseMark {
    Prep(u64),
    Ship(u64),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReleaseArc {
    /// 0 is an ordinary hour. 1 is a release that has arrived, or a lead that has run its length.
    pub approach: f32,
    pub landed: bool,
}

/// Prep marks across the last six hours raise the approach while the newest
/// one is still fresh. A ship inside the last hour is the release.
/// A ship older than the hour has already happened.
pub fn release_arc(marks: &[ReleaseMark], now: u64) -> ReleaseArc {
    let lead = now.saturating_sub(RELEASE_LEAD);
    let hour = now.saturating_sub(60 * 60);
    let mut landed = false;
    let mut oldest: Option<u64> = None;
    let mut newest: Option<u64> = None;
    for mark in marks {
        match *mark {
            ReleaseMark::Ship(t) if t >= hour && t <= now => landed = true,
            ReleaseMark::Prep(t) if t >= lead && t <= now => {
                oldest = Some(oldest.map(|o| o.min(t)).unwrap_or(t));
                newest = Some(newest.map(|o| o.max(t)).unwrap_or(t));
            }
            _ => {}
        }
    }
    if landed {
        return ReleaseArc {
            approach: 1.0,
            landed: true,
        };
    }
    let (Some(first), Some(last)) = (oldest, newest) else {
        return ReleaseArc {
            approach: 0.0,
            landed: false,
        };
    };
    if now.saturating_sub(last) > RELEASE_FRESH {
        return ReleaseArc {
            approach: 0.0,
            landed: false,
        };
    }
    let span = now.saturating_sub(first) as f32 / RELEASE_LEAD as f32;
    ReleaseArc {
        approach: (0.2 + 0.8 * span).clamp(0.0, 1.0),
        landed: false,
    }
}

/// The hour's pace still moves the tempo. During a release the floor rises
/// with the approach, and the landing itself sits at least at 120.
pub fn bpm_toward_release(measured: f32, arc: ReleaseArc) -> f32 {
    let measured = measured.clamp(60.0, 140.0);
    if arc.approach <= 0.0 && !arc.landed {
        return measured;
    }
    let floor = 60.0 + arc.approach * 48.0;
    let mut out = measured.max(floor);
    if arc.landed {
        out = out.max(120.0);
    }
    out.clamp(60.0, 140.0)
}

/// `Some(true)` is the release. `Some(false)` is work moving toward one.
/// A secret subject is dropped.
pub fn release_kind(subject: &str) -> Option<bool> {
    if redacted(subject) {
        return None;
    }
    let l = subject.to_ascii_lowercase();
    if has_version(&l) && (l.contains("release") || l.contains("cut ")) {
        return Some(true);
    }
    if version_lead(&l) {
        return Some(true);
    }
    if l.contains("changelog")
        || l.contains("version bump")
        || l.contains("bump version")
        || l.contains("prepare release")
        || l.contains("release prep")
        || l.contains("cutting a release")
        || l.contains("release branch")
    {
        return Some(false);
    }
    None
}

pub fn release_branch(name: &str) -> bool {
    let n = name.trim().trim_start_matches("refs/heads/");
    n == "release" || n.starts_with("release/") || n.starts_with("rel/")
}

pub fn marks_from_refs(text: &str, now: u64) -> Vec<ReleaseMark> {
    let mut out = Vec::new();
    for line in text.lines().take(60) {
        let mut parts = line.split('\t');
        let Some(name) = parts.next() else { continue };
        let name = name.trim();
        if name.is_empty() || redacted(name) || !release_branch(name) {
            continue;
        }
        let ts = parts
            .next()
            .unwrap_or("0")
            .trim()
            .parse::<u64>()
            .unwrap_or(0);
        if ts == 0 || ts > now || now.saturating_sub(ts) > RELEASE_LEAD {
            continue;
        }
        out.push(ReleaseMark::Prep(ts));
    }
    out
}

fn has_version(l: &str) -> bool {
    let b = l.as_bytes();
    b.windows(3)
        .any(|w| w[0].is_ascii_digit() && w[1] == b'.' && w[2].is_ascii_digit())
}

fn version_lead(l: &str) -> bool {
    let token = l.split_whitespace().next().unwrap_or("");
    let token = token.strip_prefix('v').unwrap_or(token);
    let mut parts = token.split('.');
    let Some(a) = parts.next() else { return false };
    let Some(b) = parts.next() else { return false };
    let Some(c) = parts.next() else { return false };
    !a.is_empty()
        && a.bytes().all(|ch| ch.is_ascii_digit())
        && !b.is_empty()
        && b.bytes().all(|ch| ch.is_ascii_digit())
        && !c.is_empty()
        && c.bytes().all(|ch| ch.is_ascii_digit())
        && parts.next().is_none()
}

fn mark_from_subject(ts: u64, subject: &str) -> Option<ReleaseMark> {
    match release_kind(subject) {
        Some(true) => Some(ReleaseMark::Ship(ts)),
        Some(false) => Some(ReleaseMark::Prep(ts)),
        None => None,
    }
}

/// Tags in the last hour are the release. Subjects are classified and dropped.
/// Branch names on a release line inside the lead are prep.
fn collect_release_marks(ref_texts: &[String], now: u64) -> Vec<ReleaseMark> {
    let mut marks = Vec::new();
    for text in ref_texts {
        marks.extend(marks_from_refs(text, now));
    }
    for (_, path) in super::feed::repos() {
        let tags = git(
            Path::new(path),
            &[
                "for-each-ref",
                "--sort=-creatordate",
                "--count=20",
                "--format=%(creatordate:unix)",
                "refs/tags",
            ],
            4_000,
        );
        for line in tags.unwrap_or_default().lines() {
            let Ok(t) = line.trim().parse::<u64>() else {
                continue;
            };
            if t <= now && now.saturating_sub(t) <= 60 * 60 {
                marks.push(ReleaseMark::Ship(t));
            }
        }
        let log = git(
            Path::new(path),
            &["log", "--since=6.hours", "--format=%ct%x09%s"],
            6_000,
        );
        for line in log.unwrap_or_default().lines() {
            let mut parts = line.splitn(2, '\t');
            let Ok(t) = parts.next().unwrap_or("").trim().parse::<u64>() else {
                continue;
            };
            let subject = parts.next().unwrap_or("");
            if let Some(mark) = mark_from_subject(t, subject) {
                marks.push(mark);
            }
        }
    }
    marks
}

fn commit_stamps() -> Vec<u64> {
    let mut stamps = Vec::new();
    for (_, path) in super::feed::repos() {
        let text = git(
            Path::new(path),
            &["log", "--since=70.minutes", "--format=%ct"],
            6_000,
        );
        for line in text.unwrap_or_default().lines() {
            if let Ok(t) = line.trim().parse::<u64>() {
                stamps.push(t);
            }
        }
    }
    stamps
}

pub fn signals_from_refs(repo: &str, text: &str, now: u64) -> Vec<Signal> {
    let mut out = Vec::new();
    for line in text.lines().take(60) {
        let mut parts = line.split('\t');
        let Some(name) = parts.next() else { continue };
        let name = name.trim();
        if name.is_empty() || name.contains("origin/") || redacted(name) {
            continue;
        }
        let ts = parts
            .next()
            .unwrap_or("0")
            .trim()
            .parse::<u64>()
            .unwrap_or(0);
        let age = now.saturating_sub(ts);
        let (pace, quality, cadence, coherence, stage, hits) = branch_vec(age);
        let id = format!("{repo}/{name}");
        out.push(Signal {
            id: id.clone(),
            group: 3,
            project: repo.to_string(),
            agent: "history".into(),
            ns: id,
            kind: 4,
            mass: (120.0 + pace * 14_000.0) as u32,
            hits,
            mag: 0.0,
            order: None,
            pace,
            quality,
            cadence,
            coherence,
            failing: false,
            label: stage.to_string(),
            heat: 0.0,
            hot_ms: 0,
        });
    }
    out
}

fn branch_vec(age: u64) -> (f32, f32, f32, f32, &'static str, u16) {
    const DAY: u64 = 86_400;
    if age < 2 * DAY {
        (0.86, 0.55, 0.82, 0.64, "in progress", 8)
    } else if age < 14 * DAY {
        (0.46, 0.7, 0.52, 0.66, "review", 4)
    } else {
        (0.18, 0.42, 0.22, 0.4, "todo", 2)
    }
}

pub fn signals_from_board(text: &str) -> (Vec<Signal>, HashMap<String, String>) {
    let mut signals = Vec::new();
    let mut titles = HashMap::new();
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return (signals, titles);
    };
    let Some(rows) = v.get("tickets").and_then(|t| t.as_array()) else {
        return (signals, titles);
    };
    for row in rows {
        let Some(num) = row.get("num").and_then(num_of) else {
            continue;
        };
        let status = row.get("status").and_then(|s| s.as_str()).unwrap_or("todo");
        let (pace, quality, cadence, coherence, stage, hits) = card_vec(status);
        let id = format!("example-project/EX-{num}");
        let title = row
            .get("title")
            .and_then(|t| t.as_str())
            .map(clip_title)
            .unwrap_or_default();
        if !title.is_empty() {
            titles.insert(id.clone(), title);
        }
        signals.push(Signal {
            id,
            group: 3,
            project: "example-project".into(),
            agent: "history".into(),
            ns: format!("example-project/EX-{num}"),
            kind: 4,
            mass: (80.0 + pace * 9_000.0) as u32,
            hits,
            mag: 0.0,
            order: None,
            pace,
            quality,
            cadence,
            coherence,
            failing: false,
            label: stage.to_string(),
            heat: 0.0,
            hot_ms: 0,
        });
    }
    (signals, titles)
}

fn num_of(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_i64().and_then(|n| u64::try_from(n).ok()))
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

fn card_vec(status: &str) -> (f32, f32, f32, f32, &'static str, u16) {
    match status {
        "in_progress" => (0.9, 0.56, 0.86, 0.62, "in progress", 9),
        "review" => (0.5, 0.72, 0.58, 0.7, "review", 5),
        "done" => (0.12, 0.92, 0.2, 0.84, "done", 2),
        "cancelled" => (0.05, 0.3, 0.08, 0.18, "cancelled", 1),
        _ => (0.22, 0.4, 0.24, 0.42, "todo", 2),
    }
}

/// Newest conclusion per branch wins. A later success clears an older failure.
/// `ex-1055` in a failing branch names that card too.
pub fn apply_failures(
    signals: &mut [Signal],
    titles: &HashMap<String, String>,
    repo: &str,
    json: &str,
    failing: &mut Vec<String>,
) {
    let Ok(rows) = serde_json::from_str::<Vec<Value>>(json) else {
        return;
    };
    let mut best: HashMap<String, (String, String, String)> = HashMap::new();
    for row in rows {
        let Some(branch) = row.get("headBranch").and_then(|v| v.as_str()) else {
            continue;
        };
        if branch.is_empty() || redacted(branch) {
            continue;
        }
        let when = row
            .get("createdAt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let conclusion = row
            .get("conclusion")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let name = row
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("ci")
            .to_string();
        match best.get(branch) {
            Some((prev, _, _)) if prev.as_str() >= when.as_str() => {}
            _ => {
                best.insert(branch.to_string(), (when, conclusion, name));
            }
        }
    }
    for (branch, (_, conclusion, name)) in best {
        if conclusion != "failure" && conclusion != "startup_failure" {
            continue;
        }
        let workflow = clip_title(&name);
        let id = format!("{repo}/{branch}");
        if let Some(signal) = signals.iter_mut().find(|s| s.id == id) {
            signal.failing = true;
            signal.kind = 5;
        }
        let detail = if workflow.is_empty() {
            id.clone()
        } else {
            format!("{id} — {workflow}")
        };
        if !failing.iter().any(|e| e == &detail) {
            failing.push(detail);
        }
        if let Some(num) = card_in_branch(&branch) {
            let card = format!("example-project/EX-{num}");
            if let Some(signal) = signals.iter_mut().find(|s| s.id == card) {
                signal.failing = true;
                signal.kind = 5;
                if let Some(title) = titles.get(&card) {
                    signal.label = format!("{} · {title}", signal.label);
                }
            }
            let line = match titles.get(&card) {
                Some(title) if !title.is_empty() => format!("{card} — {title}"),
                _ => card.clone(),
            };
            if !failing.iter().any(|e| e == &line) {
                failing.push(line);
            }
        }
    }
}

pub fn card_in_branch(branch: &str) -> Option<u32> {
    let lower = branch.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i + 4 < bytes.len() {
        if bytes[i] == b'e' && bytes[i + 1] == b'x' && bytes[i + 2] == b'-' {
            let digits: String = lower[i + 3..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if !digits.is_empty() {
                return digits.parse().ok();
            }
        }
        i += 1;
    }
    None
}

fn clip_title(raw: &str) -> String {
    let t = raw.trim();
    if t.is_empty() || redacted(t) {
        return String::new();
    }
    t.chars().take(42).collect()
}

fn redacted(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.contains("sk-")
        || l.contains("api_key")
        || l.contains("bearer")
        || l.contains("token=")
        || l.contains("password")
}

fn gh_runs(repo: &Path) -> Option<String> {
    let mut cmd = Command::new("gh");
    cmd.args([
        "run",
        "list",
        "--limit",
        "60",
        "--json",
        "headBranch,conclusion,name,createdAt",
    ])
    .current_dir(repo)
    .env("GH_PROMPT_DISABLED", "1")
    .env("GH_NO_UPDATE_NOTIFIER", "1");
    git_cmd(cmd, 8_000)
}

fn git(repo: &Path, args: &[&str], ms: u128) -> Option<String> {
    if !repo.join(".git").exists() && !repo.join(".git").is_file() {
        // A worktree's .git is a file. A missing checkout is neither.
        if !repo.exists() {
            return None;
        }
    }
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).args(args);
    git_cmd(cmd, ms)
}

fn git_cmd(mut cmd: Command, ms: u128) -> Option<String> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let text = rx
                    .recv_timeout(Duration::from_millis(400))
                    .unwrap_or_default();
                return status.success().then_some(text);
            }
            Ok(None) => {
                if start.elapsed().as_millis() > ms {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tempo_stays_inside_the_band_and_does_not_jump() {
        assert_eq!(bpm_from_quarter(0.0), 60.0);
        assert!((bpm_from_quarter(4.0) - 140.0).abs() < 0.01);
        assert_eq!(bpm_from_quarter(8.0), 140.0);
        let now = 10_000_000u64;
        assert_eq!(rolling_quarter_avg(&[], now), 0.0);
        assert_eq!(rolling_quarter_avg(&[now - 3_600 - 5], now), 0.0);
        let burst = rolling_quarter_avg(&[now - 10, now - 20, now - 40, now - 50], now);
        assert!((burst - 4.0 / 46.0).abs() < 0.02, "{burst}");
        let mut steady = Vec::new();
        for minute in 0..60 {
            steady.push(now - minute * 60);
        }
        let avg = rolling_quarter_avg(&steady, now);
        assert!((avg - 15.0).abs() < 1.0, "{avg}");
        assert_eq!(smooth_bpm(None, 128.0), 128.0);
        let next = smooth_bpm(Some(80.0), 140.0);
        assert!(next <= 88.0, "{next}");
        assert!(next > 80.0);
    }

    #[test]
    fn a_release_builds_while_pace_still_moves_the_tempo() {
        let now = 10_000_000u64;
        let idle = release_arc(&[], now);
        assert_eq!(idle.approach, 0.0);
        assert!(!idle.landed);
        assert_eq!(bpm_toward_release(72.0, idle), 72.0);

        let stale = release_arc(&[ReleaseMark::Prep(now - 5 * 3_600)], now);
        assert_eq!(stale.approach, 0.0);
        assert!(!stale.landed);

        let building = release_arc(
            &[
                ReleaseMark::Prep(now - 3 * 3_600),
                ReleaseMark::Prep(now - 10 * 60),
            ],
            now,
        );
        assert!(!building.landed);
        assert!(
            (building.approach - 0.6).abs() < 0.02,
            "{}",
            building.approach
        );
        let quiet = bpm_toward_release(60.0, building);
        let busy = bpm_toward_release(140.0, building);
        assert!((quiet - 88.8).abs() < 0.05, "{quiet}");
        assert_eq!(busy, 140.0);

        let landed = release_arc(&[ReleaseMark::Ship(now - 120)], now);
        assert!(landed.landed);
        assert_eq!(landed.approach, 1.0);
        assert_eq!(bpm_toward_release(60.0, landed), 120.0);
        assert_eq!(bpm_toward_release(140.0, landed), 140.0);

        let past = release_arc(&[ReleaseMark::Ship(now - 3_700)], now);
        assert!(!past.landed);
        assert_eq!(past.approach, 0.0);

        assert_eq!(release_kind("fix the panel"), None);
        assert_eq!(release_kind("bump serde"), None);
        assert_eq!(release_kind("prepare the changelog"), Some(false));
        assert_eq!(release_kind("release v1.4.0"), Some(true));
        assert_eq!(release_kind("v1.4.0"), Some(true));
        assert_eq!(release_kind("release v1.2 token=abc"), None);
        assert!(release_branch("release/1.4"));
        assert!(!release_branch("wt/namespace-drops"));
        let refs = format!("release/1.4\t{}\nwt/new\t{}\n", now - 600, now - 600);
        assert_eq!(
            marks_from_refs(&refs, now),
            vec![ReleaseMark::Prep(now - 600)]
        );
        assert!(marks_from_refs("release/secret token=abc\t1\n", now).is_empty());
    }

    #[test]
    fn a_recent_branch_is_in_progress_and_an_old_one_is_not_done() {
        let now = 1_700_000_000u64;
        let rows = signals_from_refs(
            "weftos",
            &format!(
                "wt/new\t{}\nrelease/old\t{}\n",
                now - 3_600,
                now - 40 * 86_400
            ),
            now,
        );
        assert_eq!(rows[0].label, "in progress");
        assert_eq!(rows[1].label, "todo");
        assert_eq!(rows[0].ns, "weftos/wt/new");
    }

    #[test]
    fn board_rows_keep_status_and_drop_a_secret_title() {
        let text = r#"{"count":2,"tickets":[
            {"num":1055,"title":"the panel","status":"in_progress","kind":"bug"},
            {"num":"9","title":"set token=abc","status":"done","kind":"task"}
        ]}"#;
        let (signals, titles) = signals_from_board(text);
        assert_eq!(signals.len(), 2);
        assert_eq!(signals[0].label, "in progress");
        assert_eq!(signals[0].id, "example-project/EX-1055");
        assert_eq!(
            titles.get("example-project/EX-1055").map(String::as_str),
            Some("the panel")
        );
        assert!(!titles.contains_key("example-project/EX-9"));
        assert!(signals[1].quality > signals[0].quality);
    }

    #[test]
    fn the_newest_run_wins_and_a_card_in_the_branch_is_named() {
        let text =
            r#"{"tickets":[{"num":1055,"title":"the panel","status":"review","kind":"bug"}]}"#;
        let (mut signals, titles) = signals_from_board(text);
        signals.push(signals_from_refs("weftos", "ex-1055-fix\t1\n", 10)[0].clone());
        let runs = r#"[
            {"headBranch":"ex-1055-fix","conclusion":"failure","name":"Release Gate","createdAt":"2026-10-17T00:00:00Z"},
            {"headBranch":"ex-1055-fix","conclusion":"success","name":"Release Gate","createdAt":"2026-10-18T00:00:00Z"}
        ]"#;
        let mut failing = Vec::new();
        apply_failures(&mut signals, &titles, "weftos", runs, &mut failing);
        assert!(failing.is_empty());
        let runs = r#"[
            {"headBranch":"ex-1055-fix","conclusion":"success","name":"Release Gate","createdAt":"2026-10-17T00:00:00Z"},
            {"headBranch":"ex-1055-fix","conclusion":"failure","name":"Release Gate","createdAt":"2026-10-18T00:00:00Z"}
        ]"#;
        apply_failures(&mut signals, &titles, "weftos", runs, &mut failing);
        assert!(failing.iter().any(|l| l.contains("weftos/ex-1055-fix")));
        assert!(failing.iter().any(|l| l.contains("example-project/EX-1055")));
        assert!(failing.iter().all(|l| !l.contains("http")));
        assert!(
            signals
                .iter()
                .any(|s| s.id == "example-project/EX-1055" && s.failing)
        );
        assert_eq!(card_in_branch("feature/EX-1055-panel"), Some(1055));
    }
}
