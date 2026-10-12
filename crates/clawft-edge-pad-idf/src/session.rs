//! The pad's `display` service (ADR-117): display sessions over the
//! certified-leaf channel.
//!
//! The session rules, bounds and ownership live in
//! `weftos_leaf_scene::session::DisplayService`, shared with the host-side
//! emulator so the wire behaviour is tested once. This module is the glue:
//! it feeds frames from the push path into the service, applies what the
//! service decides to the retained scene, and queues the frames to send
//! back (`Opened`, `Refused`, `Ended`, `Rejected`, and the touch and
//! push-to-talk events) in an outbox the mesh loop journals.

use std::collections::VecDeque;
use std::time::Instant;

use log::{info, warn};
use weftos_leaf_scene::session::{Action, DisplayService, LeafInfo, ServiceConfig, SessionFrame};
use weftos_leaf_scene::{InputEnvelope, NodeId};

use crate::board::{SCREEN_HEIGHT, SCREEN_WIDTH};
use crate::display::DpiDisplay;
use crate::scene::{LeafScene, BOOT_LINE_ID, BOOT_TITLE_ID, DISPLAY_ID};

/// Frames waiting to be journaled for the parent. Bounded: the oldest
/// diagnostics go first if the link stays down.
const OUTBOX_MAX: usize = 16;

pub struct PadSession {
    service: DisplayService,
    started: Instant,
    outbox: VecDeque<SessionFrame>,
}

impl PadSession {
    pub fn new() -> Self {
        let service = DisplayService::new(ServiceConfig {
            display: DISPLAY_ID,
            info: LeafInfo {
                width: SCREEN_WIDTH as u16,
                height: SCREEN_HEIGHT as u16,
                // The DPI surface reports the v1 baseline mask.
                render_caps: 0,
                touch: true,
                ptt: true,
            },
            max_nodes: 4096,
            max_ops_per_sec: 1000,
            image_budget_bytes: 512 * 1024,
            hide_on_open: vec![
                NodeId::from_parts(DISPLAY_ID, BOOT_TITLE_ID),
                NodeId::from_parts(DISPLAY_ID, BOOT_LINE_ID),
            ],
        });
        Self { service, started: Instant::now(), outbox: VecDeque::new() }
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// One frame the parent pushed.
    pub fn on_frame(&mut self, frame: SessionFrame, scene: &mut LeafScene, surface: &mut DpiDisplay) {
        let actions = self.service.handle(frame, self.now_ms());
        self.run(actions, scene, surface);
    }

    /// Housekeeping: a host that went quiet loses the screen.
    pub fn tick(&mut self, scene: &mut LeafScene, surface: &mut DpiDisplay) {
        let actions = self.service.tick(self.now_ms());
        self.run(actions, scene, surface);
    }

    /// A hit-tested touch event as the session frame to send, if a session
    /// holds the screen.
    pub fn input_frame(&self, envelope: InputEnvelope) -> Option<SessionFrame> {
        self.service.input(envelope)
    }

    /// A push-to-talk edge as the session frame to send, if a session holds
    /// the screen. Otherwise the edge belongs to nobody and is dropped.
    pub fn ptt_frame(&self, pressed: bool) -> Option<SessionFrame> {
        self.service.sensor(if pressed { "ptt.down" } else { "ptt.up" }, pressed)
    }

    pub fn outbox_front(&self) -> Option<&SessionFrame> {
        self.outbox.front()
    }

    pub fn outbox_pop(&mut self) {
        self.outbox.pop_front();
    }

    fn run(&mut self, actions: Vec<Action>, scene: &mut LeafScene, surface: &mut DpiDisplay) {
        for action in actions {
            match action {
                Action::Send(frame) => {
                    if self.outbox.len() >= OUTBOX_MAX {
                        warn!("[session] outbox full; dropping the oldest frame");
                        self.outbox.pop_front();
                    }
                    self.outbox.push_back(frame);
                }
                Action::Apply { envelope, render } => scene.apply_session_ops(surface, &envelope, render),
                Action::SessionEnded => {
                    info!("[session] ended; back to the idle screen");
                    scene.boot_screen(surface, "subscribed -- waiting for pushes");
                }
            }
        }
    }
}
