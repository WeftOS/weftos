//! `DisplaySession` — the API in docs/plans/adr-118-cos-contract.md §1.

use std::collections::VecDeque;
use std::time::Instant;

use weftos_leaf_scene::{DisplayId, SceneEnvelope, SceneOp};

use crate::backend::sim::SimBackend;
use crate::backend::{Backend, BackendEvent};
use crate::caps::{DisplayCapabilities, DisplaySize};
use crate::error::DisplayError;
use crate::event::ObjectEvent;
use crate::gesture::GestureMapper;
use crate::grant::{AdmitAll, DisplayGrant, GrantGate};
use crate::limits::{count_ops, OpRateLimiter, SceneLedger, SessionLimits};
use crate::target::DisplayTarget;

/// Knobs beyond the contract's `open(target, grant)`: the bounds and
/// the admission gate. Defaults are the D1 limits and `AdmitAll`.
pub struct SessionOptions {
    pub limits: SessionLimits,
    pub gate: Box<dyn GrantGate>,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            limits: SessionLimits::d1(),
            gate: Box::new(AdmitAll),
        }
    }
}

/// One app's hold on one display.
pub struct DisplaySession {
    backend: Box<dyn Backend>,
    display: DisplayId,
    grant: DisplayGrant,
    gate: Box<dyn GrantGate>,
    limits: SessionLimits,
    ledger: SceneLedger,
    rate: OpRateLimiter,
    mapper: GestureMapper,
    pending: VecDeque<ObjectEvent>,
    started: Instant,
    closed: bool,
}

impl DisplaySession {
    /// Open a session on `target` with `grant`, D1 limits, admit-all.
    pub fn open(target: DisplayTarget, grant: DisplayGrant) -> Result<Self, DisplayError> {
        Self::open_with(target, grant, SessionOptions::default())
    }

    /// Open with explicit limits and gate.
    pub fn open_with(
        target: DisplayTarget,
        grant: DisplayGrant,
        mut opts: SessionOptions,
    ) -> Result<Self, DisplayError> {
        opts.gate.admit(&grant)?;
        let backend = open_backend(target, &grant)?;
        let display = backend.display_id();
        Ok(Self {
            backend,
            display,
            grant,
            gate: opts.gate,
            limits: opts.limits,
            ledger: SceneLedger::default(),
            rate: OpRateLimiter::default(),
            mapper: GestureMapper::new(),
            pending: VecDeque::new(),
            started: Instant::now(),
            closed: false,
        })
    }

    /// Send a batch of scene ops. Either the whole batch is accepted
    /// and delivered or nothing is: the bounds are checked first.
    pub fn apply(&mut self, ops: &[SceneOp]) -> Result<(), DisplayError> {
        if self.closed {
            return Err(DisplayError::Closed);
        }
        if let Err(e) = self.gate.still_valid(&self.grant) {
            self.end();
            return Err(e);
        }
        let projected = self.ledger.projected(self.display, ops);
        if projected.node_count() > self.limits.max_nodes {
            return Err(DisplayError::NodeBudget {
                max: self.limits.max_nodes,
                would_be: projected.node_count(),
            });
        }
        if projected.image_bytes() > self.limits.image_budget_bytes {
            return Err(DisplayError::ImageBudget {
                max_bytes: self.limits.image_budget_bytes,
                would_be: projected.image_bytes(),
            });
        }
        let now_ms = self.started.elapsed().as_millis() as u64;
        self.rate
            .admit(now_ms, count_ops(ops), self.limits.max_ops_per_sec)
            .map_err(|attempted| DisplayError::OpRate {
                max_per_sec: self.limits.max_ops_per_sec,
                attempted,
            })?;
        let env = SceneEnvelope::new(self.display, ops.to_vec());
        match self.backend.apply(&env) {
            Ok(()) => {
                self.ledger = projected;
                Ok(())
            }
            Err(DisplayError::Closed) => {
                self.end();
                Err(DisplayError::Closed)
            }
            Err(e) => Err(e),
        }
    }

    /// Next object event, if one is ready. Never blocks; call it from
    /// the app's loop. On the window backend this also pumps the
    /// window, so call it even when idle.
    pub fn poll_event(&mut self) -> Option<ObjectEvent> {
        while self.pending.is_empty() {
            if self.closed {
                return None;
            }
            match self.backend.poll()? {
                BackendEvent::Input(env) => self.mapper.feed(&env, &mut self.pending),
                BackendEvent::Sensor { name, value } => {
                    self.pending.push_back(ObjectEvent::Sensor { name, value })
                }
                BackendEvent::Closed => self.end(),
            }
        }
        self.pending.pop_front()
    }

    pub fn size(&self) -> DisplaySize {
        self.backend.size()
    }

    pub fn capabilities(&self) -> DisplayCapabilities {
        self.backend.capabilities()
    }

    pub fn display_id(&self) -> DisplayId {
        self.display
    }

    pub fn grant(&self) -> &DisplayGrant {
        &self.grant
    }

    pub fn limits(&self) -> SessionLimits {
        self.limits
    }

    /// Nodes this session currently holds on the display.
    pub fn node_count(&self) -> usize {
        self.ledger.node_count()
    }

    /// Bitmap bytes this session currently holds on the display.
    pub fn image_bytes(&self) -> usize {
        self.ledger.image_bytes()
    }

    /// True once the session has ended, by `close` or by the backend.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Release the display: the session's objects are cleared on the
    /// leaf and no further ops are accepted. Idempotent.
    pub fn close(&mut self) -> Result<(), DisplayError> {
        if self.closed {
            return Ok(());
        }
        let env = SceneEnvelope::single(self.display, SceneOp::Clear);
        let cleared = self.backend.apply(&env);
        self.end();
        match cleared {
            Ok(()) | Err(DisplayError::Closed) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn end(&mut self) {
        if !self.closed {
            self.closed = true;
            self.backend.close();
            self.ledger = SceneLedger::default();
        }
    }
}

impl Drop for DisplaySession {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn open_backend(target: DisplayTarget, grant: &DisplayGrant) -> Result<Box<dyn Backend>, DisplayError> {
    match target {
        DisplayTarget::LeafSim(sim) => Ok(Box::new(SimBackend::new(sim))),
        #[cfg(feature = "window")]
        DisplayTarget::Window(cfg) => Ok(Box::new(crate::backend::window::WindowBackend::open(&cfg)?)),
        #[cfg(not(feature = "window"))]
        DisplayTarget::Window(_) => Err(DisplayError::NotYet(
            "built without the `window` feature; rebuild weftos-display-client with --features window",
        )),
        DisplayTarget::Leaf(ep) => crate::backend::leaf::open(&ep, grant),
    }
}
