//! Tracing → pending-buffer bridge for ExoChain events (WEFT-597 / BUG-3).
//!
//! Non-kernel crates emit structured events on the `chain_event` tracing
//! target. Some call sites also push into
//! [`clawft_core::chain_event::PENDING_CHAIN_EVENTS`] (via the
//! `chain_event!` macro or `push_chain_event`); others only emit tracing
//! (graphify, soul CLI, project init, …) and previously never reached
//! ExoChain.
//!
//! [`ChainEventLayer`] captures every `target == "chain_event"` event and
//! forwards it into the pending buffer **unless** the event is already
//! marked `chain_buffered = true` (the `chain_event!` macro sets that
//! flag so the Layer does not double-buffer). The daemon drain loop in
//! [`crate::daemon`] then appends drained records to
//! `ChainManager`.
//!
//! Wire the layer once at process start (see `weaver` `main.rs`).

use std::fmt;

use clawft_core::chain_event::push_chain_event;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// Tracing target string producers use for ExoChain markers.
pub const CHAIN_EVENT_TARGET: &str = "chain_event";

/// Field name the `chain_event!` macro sets when it has already pushed
/// into the pending buffer. The Layer skips those events.
pub const CHAIN_BUFFERED_FIELD: &str = "chain_buffered";

/// Tracing [`Layer`] that bridges `chain_event` target events into the
/// global pending buffer drained by the daemon.
#[derive(Debug, Default, Clone, Copy)]
pub struct ChainEventLayer;

impl ChainEventLayer {
    /// Construct the layer.
    pub fn new() -> Self {
        Self
    }
}

impl<S> Layer<S> for ChainEventLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() != CHAIN_EVENT_TARGET {
            return;
        }

        let mut visitor = ChainEventVisitor::default();
        event.record(&mut visitor);

        // Macro / explicit push already buffered this event.
        if visitor.chain_buffered {
            return;
        }

        let source = visitor
            .source
            .unwrap_or_else(|| "unknown".to_string());
        let kind = visitor.kind.unwrap_or_else(|| "unknown".to_string());
        let payload = if visitor.fields.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(visitor.fields))
        };

        push_chain_event(&source, &kind, payload);
    }
}

/// Collects structured fields from a `chain_event` tracing record.
#[derive(Default)]
struct ChainEventVisitor {
    source: Option<String>,
    kind: Option<String>,
    chain_buffered: bool,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl ChainEventVisitor {
    fn insert_string(&mut self, name: &str, value: String) {
        match name {
            "source" => self.source = Some(value),
            "kind" => self.kind = Some(value),
            "message" => {} // tracing message body — not payload
            CHAIN_BUFFERED_FIELD => {
                self.chain_buffered = matches!(
                    value.as_str(),
                    "true" | "True" | "1"
                );
            }
            other => {
                self.fields
                    .insert(other.to_string(), serde_json::Value::String(value));
            }
        }
    }
}

impl Visit for ChainEventVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert_string(field.name(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == CHAIN_BUFFERED_FIELD {
            self.chain_buffered = value;
            return;
        }
        if field.name() == "message" {
            return;
        }
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::Bool(value),
        );
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        if field.name() == "message" {
            return;
        }
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "message" {
            return;
        }
        self.fields.insert(
            field.name().to_string(),
            serde_json::json!(value),
        );
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        if field.name() == "message" {
            return;
        }
        // JSON numbers are finite f64 / integer; store as string if too large.
        if let Some(n) = serde_json::Number::from_i128(value) {
            self.fields
                .insert(field.name().to_string(), serde_json::Value::Number(n));
        } else {
            self.fields.insert(
                field.name().to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        if field.name() == "message" {
            return;
        }
        if let Some(n) = serde_json::Number::from_u128(value) {
            self.fields
                .insert(field.name().to_string(), serde_json::Value::Number(n));
        } else {
            self.fields.insert(
                field.name().to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        if field.name() == "message" {
            return;
        }
        if let Some(n) = serde_json::Number::from_f64(value) {
            self.fields
                .insert(field.name().to_string(), serde_json::Value::Number(n));
        } else {
            self.fields.insert(
                field.name().to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        // `%` / `?` field syntax lands here. Format without Debug quotes
        // where possible by using the Debug output and stripping a single
        // surrounding quote pair for plain strings.
        let raw = format!("{value:?}");
        let cleaned = strip_debug_string_quotes(&raw).to_string();
        self.insert_string(field.name(), cleaned);
    }
}

/// `format!("{:?}", "foo")` yields `"foo"` — strip the surrounding quotes
/// so payload values match the producer intent.
fn strip_debug_string_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Forward a drained pending batch onto a [`ChainManager`]-like appender.
///
/// Used by the daemon drain loop and by tests. Returns the number of
/// events appended. Events naming a reserved source (see
/// `clawft_kernel::project_identity::is_caller_reserved_source`) are dropped:
/// tracing and `push_chain_event` let any code choose its source, and
/// reserved sources belong to the daemon's own identity events.
#[cfg(feature = "exochain")]
pub fn forward_pending_to_chain(cm: &clawft_kernel::chain::ChainManager) -> usize {
    let pending = clawft_core::chain_event::drain_pending_chain_events();
    let mut n = 0;
    for evt in pending {
        if clawft_kernel::project_identity::is_caller_reserved_source(&evt.source) {
            tracing::warn!(source = %evt.source, kind = %evt.kind, "dropping pending chain event with a reserved source");
            continue;
        }
        if clawft_kernel::chain::is_caller_reserved_kind(&evt.kind) {
            tracing::warn!(source = %evt.source, kind = %evt.kind, "dropping pending chain event with a reserved kind");
            continue;
        }
        cm.append(&evt.source, &evt.kind, evt.payload);
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawft_core::chain_event::drain_pending_chain_events;
    use std::sync::{Mutex, OnceLock};
    use tracing::subscriber::with_default;
    use tracing_subscriber::layer::SubscriberExt;

    /// Global pending-buffer is process-wide; serialize tests that touch it.
    fn buffer_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn with_layer<F: FnOnce()>(f: F) {
        let subscriber = tracing_subscriber::registry().with(ChainEventLayer::new());
        with_default(subscriber, f);
    }

    #[test]
    fn layer_captures_tracing_only_chain_event() {
        let _guard = buffer_lock();
        let _ = drain_pending_chain_events();

        with_layer(|| {
            tracing::info!(
                target: "chain_event",
                source = "graphify",
                kind = "graphify.build",
                entity_count = 3u64,
                "chain"
            );
        });

        let events = drain_pending_chain_events();
        assert_eq!(events.len(), 1, "layer must push one pending event");
        assert_eq!(events[0].source, "graphify");
        assert_eq!(events[0].kind, "graphify.build");
        let payload = events[0].payload.as_ref().expect("payload present");
        assert_eq!(payload.get("entity_count").and_then(|v| v.as_u64()), Some(3));
    }

    #[test]
    fn layer_skips_already_buffered_macro_events() {
        let _guard = buffer_lock();
        let _ = drain_pending_chain_events();

        with_layer(|| {
            // Simulate the chain_event! macro: push first, then emit with
            // chain_buffered=true so the Layer does not double-buffer.
            push_chain_event(
                "session",
                "session.create",
                Some(serde_json::json!({"key": "k1"})),
            );
            tracing::info!(
                target: "chain_event",
                source = "session",
                kind = "session.create",
                chain_buffered = true,
                key = "k1",
                "chain"
            );
        });

        let events = drain_pending_chain_events();
        assert_eq!(
            events.len(),
            1,
            "already-buffered events must not be double-pushed by the layer"
        );
        assert_eq!(events[0].kind, "session.create");
    }

    #[test]
    fn layer_ignores_non_chain_targets() {
        let _guard = buffer_lock();
        let _ = drain_pending_chain_events();

        with_layer(|| {
            tracing::info!(target: "voice.audit", source = "x", kind = "y", "nope");
        });

        assert!(drain_pending_chain_events().is_empty());
    }

    #[cfg(feature = "exochain")]
    #[test]
    fn forward_pending_appends_to_chain_manager() {
        use clawft_kernel::chain::ChainManager;

        let _guard = buffer_lock();
        let _ = drain_pending_chain_events();
        let cm = ChainManager::new(0, 1000);
        let before = cm.len();

        with_layer(|| {
            // The 7 tracing-only emission kinds that used to hit stdout only.
            for (source, kind) in [
                ("graphify", "graphify.build"),
                ("graphify", "graphify.pipeline"),
                ("graphify", "graphify.ingest"),
                ("graphify", "graphify.hook"),
                ("weave", "project.init"),
                ("soul", "soul.promote"),
                ("routing", "model_override_bypass"),
            ] {
                tracing::info!(
                    target: "chain_event",
                    source = source,
                    kind = kind,
                    "chain"
                );
            }
            // Plus a subset of the buffered / direct-push kinds for completeness.
            for (source, kind) in [
                ("session", "session.create"),
                ("session", "session.destroy"),
                ("workspace", "workspace.create"),
                ("workspace", "workspace.config"),
                ("tools", "tool.register"),
            ] {
                tracing::info!(
                    target: "chain_event",
                    source = source,
                    kind = kind,
                    "chain"
                );
            }
        });

        let n = forward_pending_to_chain(&cm);
        assert_eq!(n, 12, "expected 12 bridged events, got {n}");
        assert_eq!(cm.len(), before + 12);

        let kinds: Vec<String> = cm.tail(0).into_iter().map(|e| e.kind.clone()).collect();
        for expected in [
            "graphify.build",
            "graphify.pipeline",
            "graphify.ingest",
            "graphify.hook",
            "project.init",
            "soul.promote",
            "model_override_bypass",
            "session.create",
            "session.destroy",
            "workspace.create",
            "workspace.config",
            "tool.register",
        ] {
            assert!(
                kinds.iter().any(|k| k == expected),
                "missing on-chain kind {expected}; have {kinds:?}"
            );
        }
    }

    #[cfg(feature = "exochain")]
    #[test]
    fn reserved_sources_are_not_forwarded_to_the_chain() {
        let _guard = buffer_lock();
        let _ = drain_pending_chain_events();
        let cm = clawft_kernel::chain::ChainManager::new(0, 1000);
        let before = cm.tail_from(0).len();
        clawft_core::chain_event::push_chain_event("user.projects", "project.revoke", None);
        clawft_core::chain_event::push_chain_event("graphify", "graphify.build", None);
        assert_eq!(forward_pending_to_chain(&cm), 1);
        let events = cm.tail_from(0);
        assert_eq!(events.len(), before + 1);
        assert!(events.iter().all(|e| e.source != "user.projects"));
    }

    #[cfg(feature = "exochain")]
    #[test]
    fn a_reserved_kind_is_not_forwarded_whatever_its_source() {
        let _guard = buffer_lock();
        let _ = drain_pending_chain_events();
        let cm = clawft_kernel::chain::ChainManager::new(0, 1000);
        // The instance journal's generation is recorded by the host's own code only.
        clawft_core::chain_event::push_chain_event("workload.host", "workload.journal", None);
        clawft_core::chain_event::push_chain_event("graphify", "graphify.build", None);
        assert_eq!(forward_pending_to_chain(&cm), 1);
        assert!(cm.tail_from(0).iter().all(|e| e.kind != "workload.journal"));
    }
}
