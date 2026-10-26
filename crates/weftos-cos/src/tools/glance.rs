//! `glance.card`: the chief of staff puts a card on the owner's e-ink glance
//! panel on purpose (ADR-119 S4). Open-gated: it changes what the panel shows
//! and nothing else. The panel is e-paper, so posts are rate limited; the
//! call is journaled in the tool log with the reason it was shown.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};

use super::{Gate, Tool, ToolOutcome, ToolRegistry, ToolSpec, arg_str, tool_err};
use crate::CosError;
use crate::events::{CardKind, Event};
use crate::glance::ascii_clip;
use crate::glance_store::{GlanceStore, StoredCard, new_card_id};
use crate::guide::UiHandle;

/// At most one non-clear card change this often.
pub const MIN_GAP: Duration = Duration::minutes(2);
/// At most this many non-clear cards in any rolling 24 hours.
pub const MAX_PER_DAY: usize = 30;
pub const MAX_TTL_MINUTES: u32 = 1440;
const MAX_TITLE: usize = 60;
const MAX_DETAIL: usize = 60;
const MAX_ALERT: usize = 40;
const MAX_OPTIONS: usize = 2;
const MAX_OPTION: usize = 24;

pub fn register(reg: ToolRegistry, ui: &UiHandle, store: &GlanceStore) -> ToolRegistry {
    reg.with(Arc::new(GlanceCard::new(ui.clone(), store.clone())))
}

#[derive(Default)]
struct Posted {
    last_change: Option<DateTime<Utc>>,
    /// When each accepted card was posted, oldest first, last 24 h only.
    day: VecDeque<DateTime<Utc>>,
}

pub struct GlanceCard {
    ui: UiHandle,
    store: GlanceStore,
    posted: Mutex<Posted>,
}

impl GlanceCard {
    pub fn new(ui: UiHandle, store: GlanceStore) -> Self {
        Self { ui, store, posted: Mutex::new(Posted::default()) }
    }

    /// Validate, rate limit and emit one card at `now`.
    pub fn post(&self, args: &Value, now: DateTime<Utc>) -> Result<ToolOutcome, CosError> {
        const ID: &str = "glance.card";
        let kind_s = arg_str(args, "kind").ok_or_else(|| tool_err(ID, "missing argument `kind`"))?;
        let kind = CardKind::parse(&kind_s).ok_or_else(|| tool_err(ID, format!("unknown kind `{kind_s}`; use needs_you, now, waiting_on, eod or clear")))?;
        let reason = arg_str(args, "reason").ok_or_else(|| tool_err(ID, "missing argument `reason`: say why you are showing this"))?;
        let mut p = self.posted.lock().unwrap_or_else(|e| e.into_inner());

        if kind == CardKind::Clear {
            drop(p);
            let id = self.store.clear().map(|c| c.id).unwrap_or_default();
            self.ui.emit(Event::Card { card: kind, id, title: String::new(), detail: String::new(), alert: String::new(), options: Vec::new(), ttl_minutes: 0, reason: reason.clone() });
            return Ok(ToolOutcome { summary: format!("card cleared ({})", ascii_clip(&reason, 80)), result: json!({"status": "cleared"}) });
        }

        let title = arg_str(args, "title").ok_or_else(|| tool_err(ID, "missing argument `title`"))?;
        let detail = arg_str(args, "detail").unwrap_or_default();
        let alert = arg_str(args, "alert").unwrap_or_default();
        let options = options_arg(args)?;
        for (name, v, max) in [("title", &title, MAX_TITLE), ("detail", &detail, MAX_DETAIL), ("alert", &alert, MAX_ALERT)] {
            if v.chars().count() > max {
                return Err(tool_err(ID, format!("`{name}` is {} chars; the panel shows about 26, keep it under {max}", v.chars().count())));
            }
        }
        let ttl = match args.get("ttl_minutes") {
            None | Some(Value::Null) => kind.default_ttl_minutes(),
            Some(v) => match v.as_u64() {
                Some(n) if (1..=u64::from(MAX_TTL_MINUTES)).contains(&n) => n as u32,
                _ => return Err(tool_err(ID, format!("`ttl_minutes` must be a whole number from 1 to {MAX_TTL_MINUTES}"))),
            },
        };

        if let Some(c) = self.store.live(now).filter(|c| c.kind == kind && c.title == title && c.detail == detail && c.alert == alert && c.options == options) {
            return Ok(ToolOutcome { summary: format!("card {} unchanged", kind.as_str()), result: json!({"status": "unchanged", "id": c.id}) });
        }
        if let Some(last) = p.last_change
            && now - last < MIN_GAP
        {
            let wait = (MIN_GAP - (now - last)).num_seconds().max(1);
            return Err(tool_err(ID, format!("rate limited: the e-paper allows one card change per 2 minutes; try again in {wait} s, or clear")));
        }
        while p.day.front().is_some_and(|t| now - *t >= Duration::hours(24)) {
            p.day.pop_front();
        }
        if p.day.len() >= MAX_PER_DAY {
            return Err(tool_err(ID, format!("rate limited: {MAX_PER_DAY} cards in the last 24 hours is the limit; do not post more today")));
        }
        p.day.push_back(now);
        p.last_change = Some(now);
        drop(p);
        let id = new_card_id();
        self.store.put(StoredCard {
            id: id.clone(),
            kind,
            title: title.clone(),
            detail: detail.clone(),
            alert: alert.clone(),
            options: options.clone(),
            reason: reason.clone(),
            expires_at: now + Duration::minutes(i64::from(ttl)),
        });

        let summary = format!("card {}: {} (why: {}; ttl {ttl}m)", kind.as_str(), ascii_clip(&title, 60), ascii_clip(&reason, 100));
        self.ui.emit(Event::Card { card: kind, id: id.clone(), title, detail, alert, options, ttl_minutes: ttl, reason });
        Ok(ToolOutcome { summary, result: json!({"status": "shown", "id": id, "kind": kind.as_str(), "ttl_minutes": ttl}) })
    }
}

/// Up to [`MAX_OPTIONS`] short ASCII strings the owner can tap.
fn options_arg(args: &Value) -> Result<Vec<String>, CosError> {
    const ID: &str = "glance.card";
    let Some(v) = args.get("options").filter(|v| !v.is_null()) else { return Ok(Vec::new()) };
    let arr = v.as_array().ok_or_else(|| tool_err(ID, "`options` must be a list of strings"))?;
    if arr.len() > MAX_OPTIONS {
        return Err(tool_err(ID, format!("at most {MAX_OPTIONS} options")));
    }
    arr.iter()
        .map(|o| {
            let o = o.as_str().map(str::trim).filter(|o| !o.is_empty()).ok_or_else(|| tool_err(ID, "each option must be a non-empty string"))?;
            if !o.is_ascii() || o.chars().count() > MAX_OPTION {
                return Err(tool_err(ID, format!("option `{o}` must be ASCII and at most {MAX_OPTION} chars")));
            }
            Ok(o.to_string())
        })
        .collect()
}

#[async_trait]
impl Tool for GlanceCard {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: "glance.card",
            description: "Put one short card on the owner's e-ink glance panel, or clear it. The panel shows about 26 ASCII characters per line and is slow e-paper: post rarely (one change per 2 minutes, 30 a day), keep text short, clear when resolved.",
            parameters: json!({"type": "object", "required": ["kind", "reason"], "properties": {
                "kind": {"type": "string", "enum": ["needs_you", "now", "waiting_on", "eod", "clear"],
                    "description": "needs_you: a real decision blocking work (2 options max). now: the single recommended next step. waiting_on: what you are blocked on and since when. eod: end-of-day summary, once. clear: take the card down."},
                "title": {"type": "string", "maxLength": MAX_TITLE, "description": "The decision or focus item. Required unless kind is clear."},
                "detail": {"type": "string", "maxLength": MAX_DETAIL, "description": "Optional second line, e.g. \"A: ship rc.16 / B: soak 1 more day\"."},
                "alert": {"type": "string", "maxLength": MAX_ALERT, "description": "Optional short alert line."},
                "options": {"type": "array", "maxItems": MAX_OPTIONS, "items": {"type": "string", "maxLength": MAX_OPTION},
                    "description": "Up to two short answers the owner can tap on the phone; shown on the panel as \"A: x / B: y\" when detail is empty. needs_you should normally carry two. The owner's pick comes back to you as a turn: Answer to card \"<title>\": <option>."},
                "ttl_minutes": {"type": "integer", "minimum": 1, "maximum": MAX_TTL_MINUTES, "description": "How long the card stays. Default: needs_you 240, now 120, waiting_on 480, eod 720."},
                "reason": {"type": "string", "description": "Why you are showing this; kept in the tool log for the owner."}}}),
            gate: Gate::Open,
        }
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome, CosError> {
        self.post(&args, Utc::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast;

    fn tool() -> (GlanceCard, broadcast::Receiver<Event>) {
        let (tx, rx) = broadcast::channel(16);
        (GlanceCard::new(UiHandle::new(tx), GlanceStore::default()), rx)
    }

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-25T09:00:00Z").unwrap().with_timezone(&Utc)
    }

    fn card(kind: &str, title: &str) -> Value {
        json!({"kind": kind, "title": title, "reason": "because"})
    }

    #[test]
    fn validation_names_the_problem() {
        let (t, _rx) = tool();
        for (args, needle) in [
            (json!({"reason": "r"}), "kind"),
            (json!({"kind": "loud", "title": "x", "reason": "r"}), "unknown kind"),
            (json!({"kind": "now", "title": "x"}), "reason"),
            (json!({"kind": "now", "reason": "r"}), "title"),
            (json!({"kind": "now", "title": "x".repeat(61), "reason": "r"}), "under 60"),
            (json!({"kind": "now", "title": "x", "alert": "y".repeat(41), "reason": "r"}), "alert"),
            (json!({"kind": "now", "title": "x", "ttl_minutes": 1441, "reason": "r"}), "ttl_minutes"),
            (json!({"kind": "now", "title": "x", "ttl_minutes": 0, "reason": "r"}), "ttl_minutes"),
        ] {
            let e = t.post(&args, t0()).unwrap_err().to_string();
            assert!(e.contains(needle), "{args}: {e}");
        }
    }

    #[test]
    fn default_ttl_per_kind_and_event_shape() {
        for (kind, ttl) in [("needs_you", 240), ("now", 120), ("waiting_on", 480), ("eod", 720)] {
            let (t, mut rx) = tool();
            let out = t.post(&card(kind, "T"), t0()).unwrap();
            assert_eq!(out.result["ttl_minutes"], ttl);
            let v = serde_json::to_value(rx.try_recv().unwrap()).unwrap();
            assert_eq!(v["id"].as_str().unwrap(), out.result["id"].as_str().unwrap());
            assert_eq!(v["id"].as_str().unwrap().len(), 8);
            let mut v = v;
            v.as_object_mut().unwrap().remove("id");
            assert_eq!(v, json!({"kind": "card", "card": kind, "title": "T", "ttl_minutes": ttl, "reason": "because"}));
        }
        let (t, mut rx) = tool();
        t.post(&json!({"kind": "now", "title": "T", "ttl_minutes": 30, "reason": "r"}), t0()).unwrap();
        assert!(matches!(rx.try_recv().unwrap(), Event::Card { ttl_minutes: 30, .. }));
    }

    #[test]
    fn one_change_per_two_minutes_and_clear_always_allowed() {
        let (t, mut rx) = tool();
        t.post(&card("now", "A"), t0()).unwrap();
        let e = t.post(&card("now", "B"), t0() + Duration::seconds(90)).unwrap_err().to_string();
        assert!(e.contains("rate limited") && e.contains("30 s"), "{e}");
        t.post(&json!({"kind": "clear", "reason": "done"}), t0() + Duration::seconds(91)).unwrap();
        assert!(t.post(&card("now", "B"), t0() + Duration::seconds(120)).is_ok());
        let kinds: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).map(|e| matches!(e, Event::Card { card: CardKind::Clear, .. })).collect();
        assert_eq!(kinds, [false, true, false], "the refused call emitted nothing");
    }

    #[test]
    fn identical_repost_is_a_noop_until_it_expires() {
        let (t, mut rx) = tool();
        t.post(&card("now", "A"), t0()).unwrap();
        let again = t.post(&card("now", "A"), t0() + Duration::seconds(5)).unwrap();
        assert_eq!(again.result["status"], "unchanged");
        assert!(rx.try_recv().is_ok() && rx.try_recv().is_err(), "only the first post emitted");
        let later = t.post(&card("now", "A"), t0() + Duration::minutes(121)).unwrap();
        assert_eq!(later.result["status"], "shown");
    }

    #[test]
    fn thirty_a_day_then_the_window_rolls() {
        let (t, _rx) = tool();
        for i in 0..MAX_PER_DAY {
            t.post(&card("now", &format!("c{i}")), t0() + Duration::minutes(3 * i as i64)).unwrap();
        }
        let at = t0() + Duration::minutes(3 * MAX_PER_DAY as i64);
        assert!(t.post(&card("now", "over"), at).unwrap_err().to_string().contains("24 hours"));
        assert!(t.post(&card("now", "next day"), t0() + Duration::hours(24) + Duration::minutes(1)).is_ok());
    }

    #[test]
    fn journal_summary_carries_the_reason_and_the_event_is_redacted() {
        let (t, mut rx) = tool();
        let out = t.post(&json!({"kind": "needs_you", "title": "Ship?", "reason": "rc blocks on it"}), t0()).unwrap();
        assert!(out.summary.contains("why: rc blocks on it") && out.summary.contains("ttl 240m"));
        let _ = rx.try_recv();
        let (t, mut rx) = tool();
        let tok = format!("wfb_{}", "b".repeat(20));
        t.post(&json!({"kind": "now", "title": format!("use {tok}"), "reason": "r"}), t0()).unwrap();
        match rx.try_recv().unwrap() {
            Event::Card { title, .. } => assert!(!title.contains(&tok) && title.contains("[redacted]")),
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn options_are_validated_and_ride_the_event() {
        let (t, mut rx) = tool();
        for (opts, needle) in [(json!(["a", "b", "c"]), "at most 2"), (json!(["x".repeat(25)]), "24 chars"), (json!(["caf\u{e9}"]), "ASCII"), (json!([""]), "non-empty"), (json!("ship"), "list")] {
            let e = t.post(&json!({"kind": "needs_you", "title": "T", "options": opts, "reason": "r"}), t0()).unwrap_err().to_string();
            assert!(e.contains(needle), "{opts}: {e}");
        }
        t.post(&json!({"kind": "needs_you", "title": "T", "options": ["ship", "soak"], "reason": "r"}), t0()).unwrap();
        assert!(matches!(rx.try_recv().unwrap(), Event::Card { options, .. } if options == ["ship", "soak"]));
    }

    #[test]
    fn repost_with_other_options_is_a_change_and_keeps_the_id_when_identical() {
        let (t, _rx) = tool();
        let a = t.post(&json!({"kind": "needs_you", "title": "T", "options": ["x"], "reason": "r"}), t0()).unwrap();
        let same = t.post(&json!({"kind": "needs_you", "title": "T", "options": ["x"], "reason": "r"}), t0() + Duration::seconds(3)).unwrap();
        assert_eq!((same.result["status"].as_str(), &same.result["id"]), (Some("unchanged"), &a.result["id"]));
        let changed = t.post(&json!({"kind": "needs_you", "title": "T", "options": ["y"], "reason": "r"}), t0() + Duration::minutes(3)).unwrap();
        assert_ne!(changed.result["id"], a.result["id"]);
    }
}
