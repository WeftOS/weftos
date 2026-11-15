//! File points laid across a project cell.
//!
//! An edge runs from a file to the card that names it, to a pull request on
//! that branch, and to a doc the same branch touched. `weave` is pure.
//! `pull` is the only place that talks to git, and the tests do not call it.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::feed::{self, Domain};
use super::parse_history::card_in_branch;

pub struct Touch {
    pub project: String,
    pub branch: String,
    pub files: Vec<String>,
    pub pr: Option<u32>,
    /// Edit heat for a file point. Git tips stay cold.
    pub heat: f32,
    pub hot_ms: u64,
}

/// Stalk crowns stop at 0.46. Files sit above that, docs and pull requests
/// on the plane between, cards on the ground with the stalks.
pub const PLANE_FILE: f32 = 0.92;
pub const PLANE_MID: f32 = 0.68;
pub const PLANE_CARD: f32 = 0.03;

pub fn plane_of(kind: u8) -> f32 {
    match kind {
        0 => PLANE_FILE,
        1 | 2 => PLANE_MID,
        _ => PLANE_CARD,
    }
}

/// A file is drawn while its edit is still warm. Cold files keep their slot.
pub fn file_visible(heat: f32, hot_ms: u64, now_ms: u64) -> bool {
    super::activity::live_heat(heat, hot_ms, now_ms) >= 0.08
}

pub struct Point {
    pub key: String,
    pub project: String,
    pub kind: u8,
    pub label: String,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub heat: f32,
    pub hot_ms: u64,
}

pub struct Edge {
    pub from: String,
    pub to: String,
}

pub struct Graph {
    pub points: Vec<Point>,
    pub edges: Vec<Edge>,
}

pub fn weave(touches: &[Touch]) -> Graph {
    let mut points: Vec<Point> = Vec::new();
    let mut index: HashMap<String, ()> = HashMap::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut seen_edge: HashSet<(String, String)> = HashSet::new();
    for touch in touches {
        if touch.project.is_empty() || secret(&touch.project) {
            continue;
        }
        let files: Vec<&str> = touch
            .files
            .iter()
            .map(|s| s.as_str())
            .filter(|p| !p.is_empty() && !secret(p))
            .take(8)
            .collect();
        let mut docs: Vec<String> = Vec::new();
        let mut codes: Vec<String> = Vec::new();
        for path in files {
            let doc = is_doc(path);
            let key = point_key(if doc { "doc" } else { "file" }, &touch.project, &two(path));
            let kind = if doc { 1 } else { 0 };
            add_point(
                &mut points,
                &mut index,
                Point {
                    key: key.clone(),
                    project: touch.project.clone(),
                    kind,
                    label: two(path),
                    x: -1.0,
                    y: -1.0,
                    z: plane_of(kind),
                    heat: if doc { 0.0 } else { touch.heat },
                    hot_ms: if doc { 0 } else { touch.hot_ms },
                },
            );
            if doc {
                if docs.len() < 4 {
                    docs.push(key);
                }
            } else {
                codes.push(key);
            }
        }
        let card = card_in_branch(&touch.branch).map(|n| format!("EX-{n}"));
        if let Some(tag) = &card {
            let key = format!("card:{tag}");
            add_point(
                &mut points,
                &mut index,
                Point {
                    key: key.clone(),
                    project: touch.project.clone(),
                    kind: 3,
                    label: tag.clone(),
                    x: -1.0,
                    y: -1.0,
                    z: plane_of(3),
                    heat: 0.0,
                    hot_ms: 0,
                },
            );
            for from in codes.iter().chain(docs.iter()) {
                add_edge(&mut edges, &mut seen_edge, from, &key);
            }
        }
        if let Some(n) = touch.pr {
            let key = format!("pr:{}:{n}", touch.project);
            add_point(
                &mut points,
                &mut index,
                Point {
                    key: key.clone(),
                    project: touch.project.clone(),
                    kind: 2,
                    label: format!("pr {n}"),
                    x: -1.0,
                    y: -1.0,
                    z: plane_of(2),
                    heat: 0.0,
                    hot_ms: 0,
                },
            );
            for from in codes.iter().chain(docs.iter()) {
                add_edge(&mut edges, &mut seen_edge, from, &key);
            }
        }
        for from in &codes {
            for to in &docs {
                add_edge(&mut edges, &mut seen_edge, from, to);
            }
        }
    }
    points.sort_by(|a, b| a.key.cmp(&b.key));
    Graph { points, edges }
}

/// Midpoint and length of each connector that is still drawn.
/// A cold file drops its strings. The tall ones run between planes.
pub fn chord_strings(graph: &Graph, now_ms: u64) -> Vec<(f32, f32, f32)> {
    let mut out = Vec::new();
    for edge in &graph.edges {
        let Some(a) = graph.points.iter().find(|p| p.key == edge.from) else {
            continue;
        };
        let Some(b) = graph.points.iter().find(|p| p.key == edge.to) else {
            continue;
        };
        if a.x < 0.0 || b.x < 0.0 {
            continue;
        }
        if a.kind == 0 && !file_visible(a.heat, a.hot_ms, now_ms) {
            continue;
        }
        if b.kind == 0 && !file_visible(b.heat, b.hot_ms, now_ms) {
            continue;
        }
        let len = ((a.x - b.x).powi(2) + (a.y - b.y).powi(2) + (a.z - b.z).powi(2)).sqrt();
        if len < 0.02 {
            continue;
        }
        out.push(((a.x + b.x) * 0.5, (a.y + b.y) * 0.5, len));
    }
    out.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(16);
    out
}

/// Spread each project's points across its cell. Missing projects stay unplaced.
pub fn place(graph: &mut Graph, domains: &[Domain], side: u32) {
    let mut by_project: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, p) in graph.points.iter().enumerate() {
        by_project.entry(p.project.clone()).or_default().push(i);
    }
    for (project, ids) in by_project {
        let Some(domain) = domains.iter().find(|d| d.project == project) else {
            continue;
        };
        let (x0, y0, x1, y1) = feed::cell_at(domain.col, domain.row, side.max(1));
        let n = ids.len().max(1);
        let cols = (n as f32).sqrt().ceil() as usize;
        let cols = cols.max(1);
        let rows = n.div_ceil(cols);
        let w = (x1 - x0).max(0.01);
        let h = (y1 - y0).max(0.01);
        for (k, id) in ids.iter().enumerate() {
            let col = k % cols;
            let row = k / cols;
            let x = x0 + (col as f32 + 0.5) / cols as f32 * w;
            let y = y0 + (row as f32 + 0.5) / rows.max(1) as f32 * h;
            graph.points[*id].x = x;
            graph.points[*id].y = y;
        }
    }
}

pub fn pull(dir: &Path) -> Graph {
    let mut touches = Vec::new();
    let mut prs: HashMap<(String, String), u32> = HashMap::new();
    for (project, path) in super::feed::repos().iter().take(2) {
        for (branch, n) in gh_prs(Path::new(path)) {
            prs.insert(((*project).to_string(), branch), n);
        }
    }
    for (project, path) in super::feed::repos() {
        let heads = git_lines(
            Path::new(path),
            &[
                "for-each-ref",
                "--sort=-committerdate",
                "--count=8",
                "--format=%(refname:short)",
                "refs/heads",
            ],
            800,
        );
        for branch in heads.into_iter().take(8) {
            let files = git_lines(
                Path::new(path),
                &["log", "-1", "--name-only", "--pretty=format:", &branch],
                800,
            );
            let pr = prs.get(&(project.to_string(), branch.clone())).copied();
            touches.push(Touch {
                project: project.clone(),
                branch,
                files,
                pr,
                heat: 0.0,
                hot_ms: 0,
            });
        }
    }
    touches.extend(activity_touches(dir));
    weave(&touches)
}

fn activity_touches(dir: &Path) -> Vec<Touch> {
    let path = dir.join("namespace-drops-activity.jsonl");
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let mut buf = vec![0u8; 256 * 1024];
    let n = file.read(&mut buf).unwrap_or(0);
    let text = String::from_utf8_lossy(&buf[..n]);
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut touches = Vec::new();
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let trimmed = text.trim();
    if trimmed.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed)
            && let Some(arr) = v.as_array()
        {
            rows.extend(arr.iter().cloned());
        }
    } else {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                rows.push(v);
            }
        }
    }
    for v in rows {
        let tool = v.get("tool").and_then(|t| t.as_str()).unwrap_or("");
        let heat = activity_heat(tool);
        if heat <= 0.0 {
            continue;
        }
        let hot_ms = v
            .get("ts_ms")
            .and_then(|n| n.as_u64())
            .or_else(|| v.get("ts").and_then(|n| n.as_u64()))
            .unwrap_or(0);
        let Some(paths) = v.get("paths").and_then(|p| p.as_array()) else {
            continue;
        };
        for p in paths {
            let Some(path) = p.as_str() else { continue };
            if secret(path) {
                continue;
            }
            let Some(project) = super::activity::project_of_path(path) else {
                continue;
            };
            let n = counts.entry(project.clone()).or_insert(0);
            if *n >= 8 {
                continue;
            }
            *n += 1;
            touches.push(Touch {
                project,
                branch: String::new(),
                files: vec![path.to_string()],
                pr: None,
                heat,
                hot_ms,
            });
        }
    }
    touches
}

fn activity_heat(tool: &str) -> f32 {
    let t = tool.to_ascii_lowercase();
    if t.contains("edit") || t.contains("write") || t.contains("delete") {
        1.0
    } else if t.contains("read") || t.contains("grep") || t.contains("glob") {
        0.4
    } else if tool.is_empty() {
        0.0
    } else {
        0.35
    }
}

fn add_point(points: &mut Vec<Point>, index: &mut HashMap<String, ()>, point: Point) {
    if let Some(existing) = points.iter_mut().find(|p| p.key == point.key) {
        if point.hot_ms >= existing.hot_ms && point.heat > 0.0 {
            existing.heat = point.heat;
            existing.hot_ms = point.hot_ms;
        }
        return;
    }
    if points.len() >= 80 {
        return;
    }
    index.insert(point.key.clone(), ());
    points.push(point);
}

fn add_edge(edges: &mut Vec<Edge>, seen: &mut HashSet<(String, String)>, from: &str, to: &str) {
    if from == to || edges.len() >= 240 {
        return;
    }
    let key = (from.to_string(), to.to_string());
    if !seen.insert(key) {
        return;
    }
    edges.push(Edge {
        from: from.to_string(),
        to: to.to_string(),
    });
}

fn point_key(kind: &str, project: &str, label: &str) -> String {
    format!("{kind}:{project}:{label}")
}

fn is_doc(path: &str) -> bool {
    let low = path.replace('\\', "/").to_ascii_lowercase();
    low.ends_with(".md") || low.contains("/docs/") || low.starts_with("docs/")
}

fn two(path: &str) -> String {
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    match parts.len() {
        0 => "file".into(),
        1 => parts[0].to_string(),
        n => format!("{}/{}", parts[n - 2], parts[n - 1]),
    }
}

fn secret(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.contains("sk-")
        || l.contains("bearer")
        || l.contains("api_key")
        || l.contains("password")
        || l.contains("token=")
}

fn gh_prs(repo: &Path) -> Vec<(String, u32)> {
    let mut cmd = Command::new("gh");
    cmd.args(["pr", "list", "--limit", "8", "--json", "number,headRefName"])
        .current_dir(repo)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1");
    let Some(text) = run_cmd(cmd, 1500) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(rows) = v.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for row in rows.iter().take(8) {
        let Some(branch) = row.get("headRefName").and_then(|s| s.as_str()) else {
            continue;
        };
        let Some(n) = row.get("number").and_then(|s| s.as_u64()) else {
            continue;
        };
        if secret(branch) {
            continue;
        }
        out.push((branch.to_string(), n as u32));
    }
    out
}

fn git_lines(repo: &Path, args: &[&str], ms: u128) -> Vec<String> {
    if !repo.exists() {
        return Vec::new();
    }
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).args(args);
    let Some(text) = run_cmd(cmd, ms) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !secret(l))
        .take(8)
        .map(str::to_string)
        .collect()
}

fn run_cmd(mut cmd: Command, ms: u128) -> Option<String> {
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
                    .recv_timeout(Duration::from_millis(300))
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
    fn a_branch_ties_a_file_to_its_card_its_doc_and_its_pr() {
        let g = weave(&[Touch {
            project: "example-project".into(),
            branch: "feature/EX-1055-panel".into(),
            files: vec!["src/foo.rs".into(), "docs/guide.md".into()],
            pr: Some(42),
            heat: 0.0,
            hot_ms: 0,
        }]);
        assert!(
            g.points
                .iter()
                .any(|p| p.kind == 1 && p.label.contains("guide.md")),
            "doc point missing"
        );
        assert!(
            g.edges.iter().any(|e| e.to.contains("EX-1055")),
            "card edge missing"
        );
        assert!(
            g.edges.iter().any(|e| e.to.contains("pr:example-project:42")),
            "pr edge missing"
        );
        assert!(
            g.edges
                .iter()
                .any(|e| e.from.contains("foo.rs") && e.to.contains("guide.md")),
            "file to doc missing"
        );
        let blob = g
            .points
            .iter()
            .map(|p| p.label.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!blob.contains("token"));
    }

    #[test]
    fn a_secret_path_never_becomes_a_point() {
        let g = weave(&[Touch {
            project: "example-project".into(),
            branch: "feature/EX-1-x".into(),
            files: vec!["src/token=abc.rs".into(), "src/ok.rs".into()],
            pr: None,
            heat: 0.0,
            hot_ms: 0,
        }]);
        let blob = g
            .points
            .iter()
            .map(|p| p.label.clone())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!blob.contains("token"));
        assert!(blob.contains("ok.rs"));
        assert!(g.points.len() <= 80);
    }

    #[test]
    fn a_file_keeps_a_slot_above_the_stalks_and_fades_when_cold() {
        let mut g = weave(&[Touch {
            project: "example-project".into(),
            branch: "feature/EX-1055-panel".into(),
            files: vec!["src/foo.rs".into(), "docs/guide.md".into()],
            pr: Some(42),
            heat: 1.0,
            hot_ms: 5_000,
        }]);
        let domains = [Domain {
            project: "example-project".into(),
            col: 0,
            row: 0,
            seats: Vec::new(),
        }];
        place(&mut g, &domains, 1);
        let file = g.points.iter().find(|p| p.kind == 0).expect("file");
        let doc = g.points.iter().find(|p| p.kind == 1).expect("doc");
        let card = g.points.iter().find(|p| p.kind == 3).expect("card");
        assert!(file.z > 0.46, "file plane {}", file.z);
        assert!(doc.z > 0.46 && doc.z < file.z, "doc plane {}", doc.z);
        assert!(card.z < 0.2, "card plane {}", card.z);
        let (x0, y0, x1, y1) = feed::cell_at(0, 0, 1);
        assert!(file.x >= x0 && file.x <= x1, "{}", file.x);
        assert!(file.y >= y0 && file.y <= y1, "{}", file.y);
        assert!(file_visible(file.heat, file.hot_ms, 5_000));
        assert!(!file_visible(0.0, 0, 5_000));
        assert!(!file_visible(1.0, 5_000, 5_000 + 600_000));
        let slot = (file.x, file.y, file.z);
        place(&mut g, &domains, 1);
        let file = g.points.iter().find(|p| p.kind == 0).expect("file");
        assert_eq!((file.x, file.y, file.z), slot);
        let warm = chord_strings(&g, 5_000);
        let cold = chord_strings(&g, 5_000 + 600_000);
        assert!(warm.len() > cold.len());
        let long = warm.iter().map(|s| s.2).fold(0.0f32, f32::max);
        let short = warm.iter().map(|s| s.2).fold(f32::MAX, f32::min);
        assert!(long > short + 0.3, "{long} {short}");
    }
}
