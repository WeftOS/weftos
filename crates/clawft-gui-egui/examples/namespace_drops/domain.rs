//! One project, one cell. The square grows when the current one is full.
//!
//! A [`Seat`] is an agent saying how it wants to sit in a project: which
//! cell, which tint, which mixer stem, which instrument. Parsers fill seats
//! in a [`super::Contribution`]. An external registrar — a hook, or later an
//! MCP tool — can write the same claim as JSON. The picture never learns
//! who wrote the file. It only sees the seat.
//!
//! ```json
//! { "seats": [{ "project": "weftos", "agent": "planner", "stem": 3, "instrument": 4, "degree": 11 }] }
//! ```
//!
//! The file is `namespace-drops-seats.json` beside the spike. A missing file
//! is no claim. Tokens that look like secrets are dropped.

use serde_json::Value;

/// How an agent wants to take part in one project.
#[derive(Clone, Debug)]
pub struct Seat {
    pub project: String,
    pub agent: String,
    /// Mixer channel 0..7. 255 means this seat does not add a tone.
    pub stem: u8,
    /// 0 vibe, 1 sine, 2 pad, 3 marimba, 4 bell.
    pub instrument: u8,
    /// Scale degree for a held tone. `None` is visual only.
    pub degree: Option<i32>,
    /// Live tint. `[0, 0, 0]` lets the picture pick from the agent name.
    pub tint: [u8; 3],
}

impl Seat {
    pub fn new(project: impl Into<String>, agent: impl Into<String>) -> Self {
        Self {
            project: project.into(),
            agent: agent.into(),
            stem: 255,
            instrument: 0,
            degree: None,
            tint: [0, 0, 0],
        }
    }
}

/// A claimed cell. `col` and `row` are inside the current square.
#[derive(Clone, Debug)]
pub struct Domain {
    pub project: String,
    pub col: u32,
    pub row: u32,
    pub seats: Vec<Seat>,
}

/// Smallest square that can hold `n` projects. Zero and one stay 1×1.
/// The square stops at 16 so a flood of names cannot tile the window away.
pub fn grid_side(n: usize) -> u32 {
    let mut side = 1u32;
    while (side as usize) * (side as usize) < n.max(1) {
        side += 1;
        if side >= 16 {
            break;
        }
    }
    side
}

/// Normalized cell inside the unit square. `side` is the current grid.
pub fn cell_at(col: u32, row: u32, side: u32) -> (f32, f32, f32, f32) {
    let side = side.max(1) as f32;
    let gap = 0.02;
    let cell = (1.0 - gap * (side + 1.0)) / side;
    let x = gap + col as f32 * (cell + gap);
    let y = gap + row as f32 * (cell + gap);
    (x, y, x + cell, y + cell)
}

/// Keep a short project or agent token. Anything that looks like a secret,
/// a path, or a sentence is refused.
pub fn keep_token(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || t.len() > 48 {
        return None;
    }
    let low = t.to_ascii_lowercase();
    if low.contains("sk-")
        || low.contains("bearer")
        || low.contains("api_key")
        || low.contains("password")
        || low.contains("token=")
    {
        return None;
    }
    if !t
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return None;
    }
    Some(t.to_string())
}

pub fn project_key(explicit: &str, id: &str) -> String {
    if let Some(p) = keep_token(explicit) {
        return p;
    }
    let head = id.split(['/', ':']).next().unwrap_or("project");
    keep_token(head).unwrap_or_else(|| "project".into())
}

pub fn agent_key(explicit: &str) -> String {
    keep_token(explicit).unwrap_or_else(|| "agent".into())
}

pub fn tint_for(agent: &str, chosen: [u8; 3]) -> [u8; 3] {
    if chosen != [0, 0, 0] {
        return chosen;
    }
    match agent {
        "claude" => [214, 164, 72],
        "codex" => [88, 186, 128],
        "history" => [150, 156, 168],
        _ => [96, 150, 214],
    }
}

/// Seats from a registrar file. Bad rows are skipped. The rest still apply.
pub fn seats_from_json(text: &str) -> Vec<Seat> {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let Some(rows) = v.get("seats").and_then(|s| s.as_array()) else {
        return Vec::new();
    };
    rows.iter().filter_map(seat_from_value).collect()
}

fn seat_from_value(v: &Value) -> Option<Seat> {
    let project = keep_token(v.get("project").and_then(|p| p.as_str()).unwrap_or(""))?;
    let agent = keep_token(v.get("agent").and_then(|p| p.as_str()).unwrap_or(""))?;
    let mut seat = Seat::new(project, agent);
    if let Some(stem) = v.get("stem").and_then(|n| n.as_u64()) {
        seat.stem = u8::try_from(stem).unwrap_or(255);
        if seat.stem > 7 && seat.stem != 255 {
            seat.stem = 255;
        }
    }
    if let Some(inst) = v.get("instrument").and_then(|n| n.as_u64()) {
        seat.instrument = u8::try_from(inst).unwrap_or(0).min(4);
    }
    if let Some(deg) = v.get("degree").and_then(|n| n.as_i64()) {
        let deg = deg.clamp(-24, 36) as i32;
        seat.degree = Some(deg);
    }
    if let Some(rgb) = v.get("tint").and_then(|t| t.as_array())
        && rgb.len() >= 3
    {
        let c = |i: usize| rgb[i].as_u64().unwrap_or(0).min(255) as u8;
        seat.tint = [c(0), c(1), c(2)];
    }
    Some(seat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_square_grows_only_when_the_current_one_is_full() {
        assert_eq!(grid_side(0), 1);
        assert_eq!(grid_side(1), 1);
        assert_eq!(grid_side(2), 2);
        assert_eq!(grid_side(4), 2);
        assert_eq!(grid_side(5), 3);
        assert_eq!(grid_side(9), 3);
        assert_eq!(grid_side(10), 4);
        let (_x0, y0, x1, _) = cell_at(0, 0, 2);
        let (a0, b0, _, _) = cell_at(1, 0, 2);
        let (_, c0, _, _) = cell_at(0, 1, 2);
        assert!(x1 <= a0 + 1.0e-4);
        assert!((y0 - b0).abs() < 1.0e-4);
        assert!(c0 > y0);
    }

    #[test]
    fn a_seat_file_keeps_a_claim_and_drops_a_secret() {
        let seats = seats_from_json(
            r#"{"seats":[
                {"project":"weftos","agent":"planner","stem":3,"instrument":4,"degree":11,"tint":[9,8,7]},
                {"project":"sk-secret","agent":"claude"},
                {"project":"has space","agent":"claude"}
            ]}"#,
        );
        assert_eq!(seats.len(), 1);
        assert_eq!(seats[0].project, "weftos");
        assert_eq!(seats[0].agent, "planner");
        assert_eq!(seats[0].stem, 3);
        assert_eq!(seats[0].instrument, 4);
        assert_eq!(seats[0].degree, Some(11));
        assert_eq!(seats[0].tint, [9, 8, 7]);
    }
}
