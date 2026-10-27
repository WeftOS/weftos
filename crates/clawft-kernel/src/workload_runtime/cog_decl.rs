//! The declarative interface parts of `cog.toml` (Ventuno cog plan 4.2, 4.6):
//! `[cog].interfaces` and the `[[outputs]]` / `[[inputs]]` topic tables.
//!
//! `interfaces` names what the cog's native half opens, never a board; it
//! maps to the capability a node must advertise (see [`interface_need`]). The
//! legacy `hardware_requirement` board list is not read here.

use std::collections::BTreeSet;

use clawft_types::cog_topics::{
    InputPolicy, MISSING_MESH_LIMITS, MeshLimits, TopicClass, is_registered, valid_topic_kind,
};
use serde::Deserialize;

use super::types::RuntimeError;

/// Most `[[outputs]]` and, separately, `[[inputs]]` tables accepted.
pub const MAX_TOPICS: usize = 16;
/// Longest `rate` text.
pub const MAX_RATE_LEN: usize = 64;

/// What a node must advertise for one interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterfaceNeed {
    /// Exactly this capability id.
    Exact(&'static str),
    /// This id or anything under it.
    Prefix(&'static str),
}

/// Interface names and their requirement; `none` is handled apart.
const INTERFACES: &[(&str, InterfaceNeed)] = &[
    ("i2c", InterfaceNeed::Exact("bus.i2c")),
    ("spi", InterfaceNeed::Exact("bus.spi")),
    ("uart", InterfaceNeed::Exact("bus.uart")),
    ("gpio", InterfaceNeed::Exact("bus.gpio")),
    ("can", InterfaceNeed::Exact("bus.can")),
    ("mcu", InterfaceNeed::Exact("bus.mcu")),
    ("camera", InterfaceNeed::Prefix("media.camera")),
    ("audio-in", InterfaceNeed::Exact("media.audio.capture")),
    ("audio-out", InterfaceNeed::Exact("media.audio.playback")),
    ("video-encode", InterfaceNeed::Exact("media.video.codec")),
    ("sdr", InterfaceNeed::Prefix("sdr")),
    ("npu", InterfaceNeed::Prefix("accel.npu")),
];

/// The requirement for `name`: `Ok(None)` for `none`, an error if unknown.
pub fn interface_need(name: &str) -> Result<Option<InterfaceNeed>, RuntimeError> {
    if name == "none" {
        return Ok(None);
    }
    INTERFACES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, need)| Some(*need))
        .ok_or_else(|| {
            let known: Vec<&str> = INTERFACES.iter().map(|(n, _)| *n).collect();
            bad(format!(
                "unknown interface {name:?} (allowed: {}, none)",
                known.join(", ")
            ))
        })
}

/// One `[[outputs]]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputDecl {
    /// Topic kind (`audio.pcm`); the live topic adds the source id.
    pub topic: String,
    /// Registered schema id.
    pub schema: String,
    /// Size class.
    pub class: TopicClass,
    /// Free-text rate (`16000 Hz, 20 ms chunks`).
    pub rate: Option<String>,
}

/// One `[[inputs]]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDecl {
    /// Topic kind.
    pub topic: String,
    /// Registered schema id.
    pub schema: String,
    /// What a slow reader does.
    pub policy: InputPolicy,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawOutput {
    topic: String,
    schema: String,
    class: String,
    rate: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawInput {
    topic: String,
    schema: String,
    policy: Option<String>,
}

fn bad(msg: impl Into<String>) -> RuntimeError {
    RuntimeError::InvalidConfig(msg.into())
}

fn check_topic(table: &str, i: usize, topic: &str, schema: &str) -> Result<(), RuntimeError> {
    if !valid_topic_kind(topic) {
        return Err(bad(format!(
            "{table}[{i}]: bad topic {topic:?} (lowercase [a-z][a-z0-9-]*(.[a-z0-9-]+)*, at most 64 chars)"
        )));
    }
    if !is_registered(schema) {
        return Err(bad(format!(
            "{table}[{i}]: unregistered schema {schema:?} for topic {topic:?}"
        )));
    }
    Ok(())
}

/// Validate `[cog].interfaces`: known names, no repeats, `none` alone.
pub(super) fn parse_interfaces(raw: Option<Vec<String>>) -> Result<Vec<String>, RuntimeError> {
    let names = raw.unwrap_or_default();
    let mut seen = BTreeSet::new();
    for n in &names {
        interface_need(n)?;
        if !seen.insert(n.as_str()) {
            return Err(bad(format!("interfaces: {n:?} listed twice")));
        }
    }
    if names.len() > 1 && seen.contains("none") {
        return Err(bad("interfaces: \"none\" cannot be combined with others"));
    }
    Ok(names)
}

/// Validate `[[outputs]]`.
pub(super) fn parse_outputs(raw: Vec<RawOutput>) -> Result<Vec<OutputDecl>, RuntimeError> {
    if raw.len() > MAX_TOPICS {
        return Err(bad(format!("more than {MAX_TOPICS} [[outputs]]")));
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(raw.len());
    for (i, o) in raw.into_iter().enumerate() {
        check_topic("outputs", i, &o.topic, &o.schema)?;
        let class = TopicClass::parse(&o.class).ok_or_else(|| {
            bad(format!(
                "outputs[{i}]: class {:?} must be reading, stream or blob",
                o.class
            ))
        })?;
        if o.rate.as_ref().is_some_and(|r| r.len() > MAX_RATE_LEN) {
            return Err(bad(format!(
                "outputs[{i}]: rate longer than {MAX_RATE_LEN} chars"
            )));
        }
        if !seen.insert(o.topic.clone()) {
            return Err(bad(format!("outputs[{i}]: duplicate topic {:?}", o.topic)));
        }
        out.push(OutputDecl {
            topic: o.topic,
            schema: o.schema,
            class,
            rate: o.rate,
        });
    }
    Ok(out)
}

/// Validate `[[inputs]]`.
pub(super) fn parse_inputs(raw: Vec<RawInput>) -> Result<Vec<InputDecl>, RuntimeError> {
    if raw.len() > MAX_TOPICS {
        return Err(bad(format!("more than {MAX_TOPICS} [[inputs]]")));
    }
    let mut out = Vec::with_capacity(raw.len());
    for (i, r) in raw.into_iter().enumerate() {
        check_topic("inputs", i, &r.topic, &r.schema)?;
        let policy = match r.policy.as_deref() {
            None => InputPolicy::default(),
            Some(p) => InputPolicy::parse(p).ok_or_else(|| {
                bad(format!(
                    "inputs[{i}]: policy {p:?} must be drop-oldest, block or sample"
                ))
            })?,
        };
        out.push(InputDecl {
            topic: r.topic,
            schema: r.schema,
            policy,
        });
    }
    Ok(out)
}

/// Validate `[mesh_limits]` when present: every value in range. A missing
/// table is accepted here (released cogs predate the rule) and refused at
/// pack ([`require_mesh_limits`]) and at `weaver expose on`.
pub(super) fn parse_mesh_limits(raw: Option<MeshLimits>) -> Result<Option<MeshLimits>, RuntimeError> {
    match raw {
        None => Ok(None),
        Some(l) => {
            l.validate().map_err(|e| bad(e.0))?;
            Ok(Some(l))
        }
    }
}

/// The pack-time rule (ADR-122 section 14, owner rule 2026-10-27): a cog
/// with outputs declares its own `[mesh_limits]`.
pub fn require_mesh_limits(outputs: &[OutputDecl], limits: Option<&MeshLimits>) -> Result<(), RuntimeError> {
    if !outputs.is_empty() && limits.is_none() {
        return Err(bad(MISSING_MESH_LIMITS));
    }
    Ok(())
}
