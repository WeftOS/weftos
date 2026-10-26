//! What the agent knows about the glance panel: the live `glance.card` card
//! and the latest idle art, both kept under the run directory (owner-only)
//! so a restart of the agent or the bridge restores them, plus a mirror of
//! the panel's five fields for `GET /v1/cos/glance`.
//!
//! The bridge owns the art: it writes `glance.art.png` and `glance.art.json`
//! beside the status file, and the agent only reads them. No upload route.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::CosError;
use crate::events::{CardKind, Event};
use crate::glance::Glance;

pub const CARD_FILE: &str = "glance.card.json";
pub const ART_PNG_FILE: &str = "glance.art.png";
pub const ART_META_FILE: &str = "glance.art.json";

/// A card as the agent holds and persists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCard {
    pub id: String,
    pub kind: CardKind,
    pub title: String,
    pub detail: String,
    pub alert: String,
    pub options: Vec<String>,
    pub reason: String,
    pub expires_at: DateTime<Utc>,
}

impl StoredCard {
    /// The `card` event that shows it, with the time it has left.
    pub fn to_event(&self, now: DateTime<Utc>) -> Event {
        let left = (self.expires_at - now).num_seconds().max(1);
        Event::Card {
            card: self.kind,
            id: self.id.clone(),
            title: self.title.clone(),
            detail: self.detail.clone(),
            alert: self.alert.clone(),
            options: self.options.clone(),
            ttl_minutes: ((left + 59) / 60) as u32,
            reason: self.reason.clone(),
        }
    }

    fn to_json(&self) -> Value {
        json!({"id": self.id, "kind": self.kind, "title": self.title, "detail": self.detail, "alert": self.alert,
            "options": self.options, "reason": self.reason, "expires_at": self.expires_at.to_rfc3339_opts(SecondsFormat::Secs, true)})
    }
}

#[derive(Default)]
struct Inner {
    card: Option<StoredCard>,
    mirror: Glance,
    dir: Option<PathBuf>,
}

#[derive(Clone, Default)]
pub struct GlanceStore(Arc<Mutex<Inner>>);

impl GlanceStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Persist under `dir` from now on, and restore a card that is still live.
    pub fn set_dir(&self, dir: PathBuf) {
        let _ = std::fs::create_dir_all(&dir);
        let restored: Option<StoredCard> = std::fs::read_to_string(dir.join(CARD_FILE)).ok().and_then(|t| serde_json::from_str(&t).ok());
        let now = Utc::now();
        let mut s = self.lock();
        s.dir = Some(dir);
        if let Some(c) = restored.filter(|c| c.expires_at > now) {
            s.mirror.apply_at(&c.to_event(now), now);
            s.card = Some(c);
        }
    }

    /// The card, unless it has expired.
    pub fn live(&self, now: DateTime<Utc>) -> Option<StoredCard> {
        self.lock().card.clone().filter(|c| c.expires_at > now)
    }

    pub fn put(&self, card: StoredCard) {
        let mut s = self.lock();
        persist_card(&s.dir, Some(&card));
        s.card = Some(card);
    }

    /// Take the card down; returns it when one was up.
    pub fn clear(&self) -> Option<StoredCard> {
        let mut s = self.lock();
        persist_card(&s.dir, None);
        s.card.take()
    }

    /// Feed the panel mirror (the agent calls this for every event it emits).
    pub fn observe(&self, ev: &Event) {
        self.lock().mirror.apply(ev);
    }

    /// `GET /v1/cos/glance`.
    pub fn snapshot(&self, now: DateTime<Utc>) -> Value {
        let mut s = self.lock();
        s.mirror.tick(now);
        let line = s.mirror.to_line(&chrono::Local::now().format("%H:%M").to_string());
        let all: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        let fields: Value = ["state", "task", "next", "alert", "model", "ts"].iter().map(|k| (k.to_string(), all[*k].clone())).collect::<serde_json::Map<_, _>>().into();
        let card = s.card.clone().filter(|c| c.expires_at > now).map(|c| c.to_json());
        let art = s.dir.as_deref().and_then(read_art);
        json!({"card": card, "fields": fields, "art": art})
    }

    /// The latest art PNG, if the bridge has written one.
    pub fn art_png(&self) -> Option<Vec<u8>> {
        let dir = self.lock().dir.clone()?;
        std::fs::read(dir.join(ART_PNG_FILE)).ok()
    }

    /// Resolve the live card `id` with the owner's answer: the chosen option
    /// text. Clears the card. Unknown or expired ids are a client error.
    pub fn answer(&self, id: &str, value: &Value, now: DateTime<Utc>) -> Result<(StoredCard, String), CosError> {
        let card = self.live(now).filter(|c| c.id == id).ok_or_else(|| CosError::Config(format!("no live card {id}")))?;
        let choice = pick_option(&card, value)?;
        self.clear();
        Ok((card, choice))
    }
}

fn pick_option(card: &StoredCard, value: &Value) -> Result<String, CosError> {
    let bad = |m: String| CosError::Config(m);
    let text = match value {
        Value::Number(n) => n.as_u64().map(|n| n.to_string()),
        Value::String(s) => Some(s.trim().to_string()),
        _ => None,
    }
    .filter(|t| !t.is_empty())
    .ok_or_else(|| bad("answer.value must be an option number or its text".into()))?;
    if card.options.is_empty() {
        return if text.chars().count() <= 200 { Ok(text) } else { Err(bad("answer longer than 200 characters".into())) };
    }
    if let Ok(n) = text.parse::<usize>() {
        return card.options.get(n.wrapping_sub(1)).cloned().ok_or_else(|| bad(format!("choice {n} is out of range 1..={}", card.options.len())));
    }
    card.options.iter().find(|o| o.eq_ignore_ascii_case(&text)).cloned().ok_or_else(|| bad(format!("{text:?} is not one of the card's options")))
}

fn read_art(dir: &Path) -> Option<Value> {
    if !dir.join(ART_PNG_FILE).is_file() {
        return None;
    }
    let meta: Value = std::fs::read_to_string(dir.join(ART_META_FILE)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    let at = meta["last_art_at"].as_i64().and_then(|t| DateTime::from_timestamp(t, 0)).map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true));
    Some(json!({"caption": meta["caption"].as_str().unwrap_or(""), "at": at, "png_url": "/v1/cos/glance/art.png"}))
}

fn persist_card(dir: &Option<PathBuf>, card: Option<&StoredCard>) {
    let Some(dir) = dir else { return };
    match card {
        Some(c) => {
            let _ = write_private(&dir.join(CARD_FILE), serde_json::to_string(c).unwrap_or_default().as_bytes());
        }
        None => {
            let _ = std::fs::remove_file(dir.join(CARD_FILE));
        }
    }
}

/// Write a file only the owner can read, atomically.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(&tmp)?.write_all(bytes)?;
    std::fs::rename(&tmp, path)
}

/// A fresh short card id: 8 random hex characters.
pub fn new_card_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}
