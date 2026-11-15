//! What the picture is allowed to know.
//!
//! A parser returns a [`Contribution`]. The picture never reads a session,
//! a board, or a git ref itself. [`merge`] folds any number of contributions
//! into one [`World`]: live signals share a replay clock, standing signals
//! are the field around them, and a later parser does not have to know the
//! earlier ones exist.

/// Seconds in one visual replay. The soundtrack loops on the same clock.
pub const REPLAY_SECS: f32 = 36.0;

#[path = "domain.rs"]
mod domain;

pub use domain::{Domain, Seat, cell_at, keep_token, seats_from_json};

/// One stalk, or one landing on a stalk.
///
/// `order: Some` is a live event. Merge sorts those and lays them on the
/// replay. `order: None` is a standing stalk (a branch, a card, anything
/// that is already there).
#[derive(Clone)]
pub struct Signal {
    pub id: String,
    pub group: u8,
    /// Project domain. Empty falls back to the first segment of `id`.
    pub project: String,
    /// Who produced it. "claude", "codex", "history", or a registered agent.
    pub agent: String,
    pub ns: String,
    pub kind: u8,
    pub mass: u32,
    pub hits: u16,
    pub mag: f32,
    pub order: Option<String>,
    pub pace: f32,
    pub quality: f32,
    pub cadence: f32,
    pub coherence: f32,
    pub failing: bool,
    /// Short hover text. Not a command line and not a log.
    pub label: String,
    /// Intensity at `hot_ms`. Zero does not cool the stalk.
    pub heat: f32,
    /// Unix milliseconds of the touch. Zero is a history stalk.
    pub hot_ms: u64,
}

#[derive(Clone)]
pub struct Phrase {
    pub order: String,
    /// Replay position, assigned by [`merge`].
    pub at: f32,
    pub clause: String,
}

#[derive(Clone, Copy)]
pub struct Resident {
    /// 0 vibe, 1 sine, 2 pad, 3 marimba, 4 bell.
    pub instrument: u8,
    /// Mixer channel this tone follows.
    pub stem: u8,
    pub degree: i32,
}

#[derive(Clone)]
pub struct Contribution {
    pub source: &'static str,
    pub signals: Vec<Signal>,
    pub phrases: Vec<Phrase>,
    pub residents: Vec<Resident>,
    /// Claims on a project. A seat with no signals still takes a cell.
    pub seats: Vec<Seat>,
    pub failing: Vec<String>,
    pub bpm: Option<f32>,
    /// 0 on an ordinary hour. Rises while a release is in motion.
    pub release_approach: f32,
    pub release_landed: bool,
    pub note: String,
}

impl Contribution {
    pub fn empty(source: &'static str) -> Self {
        Self {
            source,
            signals: Vec::new(),
            phrases: Vec::new(),
            residents: Vec::new(),
            seats: Vec::new(),
            failing: Vec::new(),
            bpm: None,
            release_approach: 0.0,
            release_landed: false,
            note: String::new(),
        }
    }
}

/// Some fields are carried for the tests and for later parsers.
#[derive(Clone)]
#[allow(dead_code)]
pub struct Placed {
    pub id: String,
    pub group: u8,
    pub project: String,
    pub agent: String,
    pub standing: bool,
    pub domain: u16,
    pub hue: [u8; 3],
    pub ns: String,
    pub kind: u8,
    pub mass: u32,
    pub hits: u16,
    pub pace: f32,
    pub quality: f32,
    pub cadence: f32,
    pub coherence: f32,
    pub failing: bool,
    pub label: String,
    pub ink: [u8; 3],
    pub heat: f32,
    pub hot_ms: u64,
}

#[derive(Clone, Copy)]
pub struct Landing {
    pub node: u32,
    pub at: f32,
    pub kind: u8,
    pub mag: f32,
}

pub struct World {
    pub placed: Vec<Placed>,
    pub landings: Vec<Landing>,
    pub phrases: Vec<Phrase>,
    pub residents: Vec<Resident>,
    pub failing: Vec<String>,
    pub domains: Vec<Domain>,
    /// Side of the square that holds [`World::domains`]. One open cell is
    /// the next project.
    pub side: u32,
    pub sources: Vec<&'static str>,
    pub bpm: f32,
    pub release_approach: f32,
    pub release_landed: bool,
    pub pace: f32,
    pub cadence: f32,
    pub coherence: f32,
    pub note: String,
}

/// Repositories the example reads. `NAMESPACE_DROPS_REPOS` is a comma list of
/// `name=path`; with it unset the example reads the current directory only.
pub fn repos() -> &'static [(String, String)] {
    static REPOS: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();
    REPOS.get_or_init(|| {
        let from_env = std::env::var("NAMESPACE_DROPS_REPOS").unwrap_or_default();
        let mut out: Vec<(String, String)> = from_env
            .split(',')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .filter(|(k, v)| !k.is_empty() && !v.is_empty())
            .collect();
        if out.is_empty() {
            out.push(("example-project".to_string(), ".".to_string()));
        }
        out
    })
}

/// Hue follows quality (red toward green). Cadence is brightness.
/// Coherence is saturation. A failing stalk is held near red.
pub fn ink(pace: f32, quality: f32, cadence: f32, coherence: f32, failing: bool) -> [u8; 3] {
    let _ = pace;
    let q = if failing {
        0.1
    } else {
        quality.clamp(0.0, 1.0)
    };
    let hue = q * 120.0;
    let sat = 0.25 + 0.75 * coherence.clamp(0.0, 1.0);
    let val = 0.30 + 0.62 * cadence.clamp(0.0, 1.0);
    hsv_to_rgb(hue, sat.clamp(0.0, 1.0), val.clamp(0.0, 1.0))
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [u8; 3] {
    let c = v * s;
    let h6 = (h / 60.0).rem_euclid(6.0);
    let x = c * (1.0 - (h6.rem_euclid(2.0) - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h6 as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    ]
}

/// Fold parsers into the world the picture draws.
///
/// Live signals, from every parser, share one clock in `order` sequence.
/// Standing signals are appended after them, so a landing index never points
/// at the field. Ink is applied here, so a parser describes a vector and
/// does not pick a color.
pub fn merge(parts: Vec<Contribution>) -> World {
    let mut bpm = 96.0f32;
    let mut release_approach = 0.0f32;
    let mut release_landed = false;
    let mut note = String::new();
    let mut sources = Vec::new();
    let mut failing = Vec::new();
    let mut residents = Vec::new();
    let mut phrases = Vec::new();
    let mut live = Vec::new();
    let mut standing = Vec::new();
    let mut projects: Vec<String> = Vec::new();
    let mut claimed: Vec<Seat> = Vec::new();

    for part in parts {
        sources.push(part.source);
        if let Some(b) = part.bpm {
            bpm = b.clamp(60.0, 140.0);
        }
        release_approach = release_approach.max(part.release_approach.clamp(0.0, 1.0));
        release_landed |= part.release_landed;
        if !part.note.is_empty() {
            if !note.is_empty() {
                note.push(' ');
            }
            note.push_str(&part.note);
        }
        for line in part.failing {
            if !line.is_empty() && !failing.iter().any(|e| e == &line) {
                failing.push(line);
            }
        }
        for seat in part.seats {
            let Some(project) = domain::keep_token(&seat.project) else {
                continue;
            };
            let Some(agent) = domain::keep_token(&seat.agent) else {
                continue;
            };
            if !projects.iter().any(|p| p == &project) {
                projects.push(project.clone());
            }
            if let Some(degree) = seat.degree {
                let stem = if seat.stem < 8 { seat.stem } else { 0 };
                let instrument = seat.instrument.min(4);
                if !residents
                    .iter()
                    .any(|e: &Resident| e.degree == degree && e.stem == stem)
                {
                    residents.push(Resident {
                        instrument,
                        stem,
                        degree,
                    });
                }
            }
            claimed.push(Seat {
                project,
                agent,
                ..seat
            });
        }
        for r in part.residents {
            if !residents
                .iter()
                .any(|e| e.degree == r.degree && e.stem == r.stem)
            {
                residents.push(r);
            }
        }
        phrases.extend(part.phrases);
        for mut signal in part.signals {
            signal.project = domain::project_key(&signal.project, &signal.id);
            signal.agent = domain::agent_key(&signal.agent);
            if !projects.iter().any(|p| p == &signal.project) {
                projects.push(signal.project.clone());
            }
            if signal.order.is_some() {
                live.push(signal);
            } else {
                standing.push(signal);
            }
        }
    }

    let side = domain::grid_side(projects.len());
    let domains: Vec<Domain> = projects
        .iter()
        .enumerate()
        .map(|(i, project)| Domain {
            project: project.clone(),
            col: (i as u32) % side,
            row: (i as u32) / side,
            seats: claimed
                .iter()
                .filter(|s| s.project == *project)
                .cloned()
                .collect(),
        })
        .collect();

    live.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.id.cmp(&b.id)));
    phrases.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.clause.cmp(&b.clause)));
    let n_phr = phrases.len();
    for (i, phrase) in phrases.iter_mut().enumerate() {
        phrase.at = if n_phr < 2 {
            0.0
        } else {
            i as f32 / (n_phr - 1) as f32
        };
    }

    let mut placed: Vec<Placed> = Vec::new();
    let mut index: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();
    let mut landings = Vec::new();
    let n_live = live.len();
    for (i, signal) in live.into_iter().enumerate() {
        let at = if n_live < 2 {
            0.0
        } else {
            i as f32 / (n_live - 1) as f32
        };
        let key = (signal.project.clone(), signal.id.clone());
        let node = if let Some(&idx) = index.get(&key) {
            let row = &mut placed[idx];
            row.hits = row.hits.saturating_add(1);
            row.mass = row.mass.saturating_add(signal.mass);
            row.kind = signal.kind;
            row.failing |= signal.failing;
            if signal.hot_ms >= row.hot_ms {
                row.heat = signal.heat;
                row.hot_ms = signal.hot_ms;
                row.agent = signal.agent.clone();
            }
            idx
        } else {
            let idx = placed.len();
            placed.push(place_of(&signal, &domains, &claimed));
            index.insert(key, idx);
            idx
        };
        landings.push(Landing {
            node: node as u32,
            at,
            kind: signal.kind,
            mag: signal.mag,
        });
    }

    for signal in standing {
        let key = (signal.project.clone(), signal.id.clone());
        if index.contains_key(&key) {
            continue;
        }
        index.insert(key, placed.len());
        placed.push(place_of(&signal, &domains, &claimed));
    }

    let field: Vec<&Placed> = placed.iter().filter(|p| p.standing).collect();
    let (pace, cadence, coherence) = if field.is_empty() {
        (0.5, 0.5, 0.5)
    } else {
        let n = field.len() as f32;
        (
            field.iter().map(|p| p.pace).sum::<f32>() / n,
            field.iter().map(|p| p.cadence).sum::<f32>() / n,
            field.iter().map(|p| p.coherence).sum::<f32>() / n,
        )
    };

    World {
        placed,
        landings,
        phrases,
        residents,
        failing,
        domains,
        side,
        sources,
        bpm,
        release_approach,
        release_landed,
        pace,
        cadence,
        coherence,
        note,
    }
}

fn place_of(signal: &Signal, domains: &[Domain], seats: &[Seat]) -> Placed {
    let standing = signal.order.is_none();
    let ink = if standing {
        ink(
            signal.pace,
            signal.quality,
            signal.cadence,
            signal.coherence,
            signal.failing,
        )
    } else {
        [0, 0, 0]
    };
    let domain = domains
        .iter()
        .position(|d| d.project == signal.project)
        .unwrap_or(0) as u16;
    let chosen = seats
        .iter()
        .find(|s| s.project == signal.project && s.agent == signal.agent)
        .map(|s| s.tint)
        .unwrap_or([0, 0, 0]);
    Placed {
        id: signal.id.clone(),
        group: signal.group,
        project: signal.project.clone(),
        agent: signal.agent.clone(),
        standing,
        domain,
        hue: domain::tint_for(&signal.agent, chosen),
        ns: signal.ns.clone(),
        kind: signal.kind,
        mass: signal.mass.max(1),
        hits: signal.hits.max(1),
        pace: signal.pace,
        quality: signal.quality,
        cadence: signal.cadence,
        coherence: signal.coherence,
        failing: signal.failing,
        label: signal.label.clone(),
        ink,
        heat: signal.heat,
        hot_ms: signal.hot_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(id: &str, group: u8, order: &str) -> Signal {
        Signal {
            id: id.into(),
            group,
            project: "sessions".into(),
            agent: "claude".into(),
            ns: id.into(),
            kind: 1,
            mass: 10,
            hits: 1,
            mag: 0.4,
            order: Some(order.into()),
            pace: 0.0,
            quality: 0.0,
            cadence: 0.0,
            coherence: 0.0,
            failing: false,
            label: String::new(),
            heat: 0.0,
            hot_ms: 0,
        }
    }

    fn standing(id: &str) -> Signal {
        Signal {
            id: id.into(),
            group: 3,
            project: id.split('/').next().unwrap_or(id).into(),
            agent: "history".into(),
            ns: id.into(),
            kind: 4,
            mass: 40,
            hits: 2,
            mag: 0.0,
            order: None,
            pace: 0.2,
            quality: 0.9,
            cadence: 0.3,
            coherence: 0.8,
            failing: false,
            label: "done".into(),
            heat: 0.0,
            hot_ms: 0,
        }
    }

    #[test]
    fn two_parsers_share_one_clock_and_a_third_can_join() {
        let sessions = Contribution {
            source: "sessions",
            signals: vec![
                live("b", 0, "200"),
                live("a", 1, "100"),
                live("b", 0, "300"),
            ],
            phrases: vec![Phrase {
                order: "2".into(),
                at: 0.0,
                clause: "later".into(),
            }],
            residents: vec![Resident {
                instrument: 0,
                stem: 0,
                degree: 2,
            }],
            seats: vec![],
            failing: vec!["weftos/wt — gate".into()],
            bpm: None,
            release_approach: 0.0,
            release_landed: false,
            note: "live".into(),
        };
        let history = Contribution {
            source: "history",
            signals: vec![standing("example-project/EX-1")],
            phrases: vec![],
            residents: vec![],
            seats: vec![],
            failing: vec!["example-project/EX-1 — card".into()],
            bpm: Some(104.0),
            release_approach: 0.6,
            release_landed: true,
            note: "field".into(),
        };
        let extra = Contribution {
            source: "another-project",
            signals: vec![standing("other/branch")],
            phrases: vec![Phrase {
                order: "1".into(),
                at: 0.0,
                clause: "sooner".into(),
            }],
            residents: vec![],
            seats: vec![],
            failing: vec![],
            bpm: None,
            release_approach: 0.0,
            release_landed: false,
            note: String::new(),
        };
        let world = merge(vec![sessions, history, extra]);
        assert_eq!(
            world.sources,
            vec!["sessions", "history", "another-project"]
        );
        assert_eq!(world.bpm, 104.0);
        assert!((world.release_approach - 0.6).abs() < 0.01);
        assert!(world.release_landed);
        assert_eq!(world.landings.len(), 3);
        assert!(world.landings[0].at < world.landings[1].at);
        assert_eq!(
            world.landings[0].node,
            world.placed.iter().position(|p| p.id == "a").unwrap() as u32
        );
        let field: Vec<_> = world
            .placed
            .iter()
            .filter(|p| p.group == 3)
            .map(|p| p.id.as_str())
            .collect();
        assert_eq!(field, vec!["example-project/EX-1", "other/branch"]);
        assert!(
            world
                .placed
                .iter()
                .filter(|p| p.group == 3)
                .all(|p| p.ink[1] > p.ink[0])
        );
        assert_eq!(world.failing.len(), 2);
        assert_eq!(world.phrases[0].clause, "sooner");
        assert_eq!(world.phrases[1].clause, "later");
        assert!(world.note.contains("live"));
        assert!(world.note.contains("field"));
        assert_eq!(world.side, domain::grid_side(3));
        assert_eq!(world.side, 2);
        assert_eq!(
            world
                .domains
                .iter()
                .map(|d| d.project.as_str())
                .collect::<Vec<_>>(),
            vec!["sessions", "example-project", "other"]
        );
        assert_eq!(world.domains[2].col, 0);
        assert_eq!(world.domains[2].row, 1);
    }

    #[test]
    fn a_seat_claims_a_cell_before_any_stalk_lands() {
        let mut claim = Seat::new("other-project", "planner");
        claim.stem = 3;
        claim.instrument = 4;
        claim.degree = Some(11);
        claim.tint = [1, 2, 3];
        let mut seats = Contribution::empty("seats");
        seats.seats.push(claim);
        let mut sessions = Contribution::empty("sessions");
        sessions.signals.push(live("a", 0, "1"));
        let world = merge(vec![seats, sessions]);
        assert_eq!(world.side, 2);
        assert_eq!(world.domains[0].project, "other-project");
        assert!(world.placed.iter().all(|p| p.project != "other-project"));
        assert_eq!(world.domains[0].seats[0].stem, 3);
        assert!(
            world
                .residents
                .iter()
                .any(|r| r.degree == 11 && r.instrument == 4 && r.stem == 3)
        );
        assert_eq!(world.placed[0].hue, [214, 164, 72]);
    }

    #[test]
    fn low_quality_inks_redder_than_high_quality() {
        let bad = ink(0.5, 0.05, 0.6, 0.7, false);
        let good = ink(0.5, 0.95, 0.6, 0.7, false);
        assert!(bad[0] > bad[1]);
        assert!(good[1] > good[0]);
        let failed = ink(0.9, 0.95, 0.8, 0.8, true);
        assert!(failed[0] > failed[1]);
    }
}
