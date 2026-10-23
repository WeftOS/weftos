//! The `indicator` state machine (design section 3.7), sans-IO.
//!
//! ```text
//! idle -> listening -> captured -> thinking -> done_ok | done_error -> idle
//! ```
//! The glasses own the fast edges (`listening`, `captured`, `thinking`); the phone or
//! workflow sends `done_*` transitions only, never cue audio. The caller supplies a
//! monotonic `now_ms` and performs the returned actions (play a cached cue, drive the
//! LED, report the state). `next_deadline` says when to call `Tick`.

use crate::led::{LedColor, LedPattern};
use serde::{Deserialize, Serialize};

pub const CAPTURED_MS: u64 = 400;
pub const DONE_MS: u64 = 1200;
pub const THINKING_TIMEOUT_MS: u64 = 20_000;
pub const THINKING_MAX_MS: u64 = 60_000;
pub const THINKING_LOOP_PERIOD_MS: u32 = 1800;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum IndicatorState {
    Idle,
    Listening,
    Captured,
    Thinking,
    DoneOk,
    DoneError,
}

/// Pre-rendered cue assets that live in the glasses APK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum Cue {
    /// soft rising two-note chirp (E5 to A5)
    ListeningStart,
    /// soft falling two-note chirp (A5 to E5)
    Captured,
    /// one gentle chime
    DoneOk,
    /// soft low double tone
    DoneError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum IndicatorInput {
    /// The glasses' gate opened.
    GateOpen,
    /// Push-to-talk pressed.
    PttStart,
    /// The gate closed.
    GateClose,
    /// First reply audio arrived: stop the thinking loop and the timeout.
    ReplyAudioStarted,
    /// The link to the parent dropped.
    LinkLost,
    /// `indicator.set` from the phone or workflow; stale `seq` within a turn is ignored.
    Set {
        state: IndicatorState,
        turn_id: u64,
        seq: u64,
    },
    /// `indicator.hold`: extend the thinking timeout, capped at 60 s total.
    Hold { ms: u32 },
    /// Time passed; call at `next_deadline`.
    Tick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum IndicatorAction {
    PlayCue {
        cue: Cue,
    },
    /// Repeat the quiet tick every `period_ms` until `StopThinkingLoop`.
    StartThinkingLoop {
        period_ms: u32,
    },
    StopThinkingLoop,
    SetLed {
        pattern: LedPattern,
    },
    /// Release LED authority back to the BES firmware.
    ReleaseLed,
    /// Report `evt{indicator.state}` upward so the ledger knows what was shown.
    Report {
        state: IndicatorState,
        turn_id: u64,
    },
}

pub struct Indicator {
    state: IndicatorState,
    turn_id: u64,
    seq_turn: Option<u64>,
    last_seq: u64,
    /// When the current state expires on `Tick` (captured, thinking, done_*).
    deadline: Option<u64>,
    thinking_since: u64,
    loop_on: bool,
}

impl Default for Indicator {
    fn default() -> Self {
        Self::new()
    }
}

impl Indicator {
    pub fn new() -> Self {
        Self {
            state: IndicatorState::Idle,
            turn_id: 0,
            seq_turn: None,
            last_seq: 0,
            deadline: None,
            thinking_since: 0,
            loop_on: false,
        }
    }

    pub fn state(&self) -> IndicatorState {
        self.state
    }

    pub fn turn_id(&self) -> u64 {
        self.turn_id
    }

    pub fn next_deadline(&self) -> Option<u64> {
        self.deadline
    }

    pub fn handle(&mut self, now: u64, input: IndicatorInput) -> Vec<IndicatorAction> {
        use IndicatorInput as I;
        use IndicatorState as S;
        match input {
            I::GateOpen | I::PttStart => match self.state {
                // Already listening, or still showing the previous turn's end: only a
                // fresh open from a non-listening state starts a new listen.
                S::Listening => vec![],
                _ => self.enter(now, S::Listening),
            },
            I::GateClose => match self.state {
                S::Listening => self.enter(now, S::Captured),
                _ => vec![],
            },
            I::ReplyAudioStarted => {
                self.deadline = None;
                if self.state == S::Thinking {
                    self.stop_loop()
                } else {
                    vec![]
                }
            }
            I::LinkLost => match self.state {
                S::Captured | S::Thinking => self.enter(now, S::DoneError),
                _ => vec![],
            },
            I::Hold { ms } => {
                if self.state == S::Thinking && self.deadline.is_some() {
                    let cap = self.thinking_since + THINKING_MAX_MS;
                    let want = (now + u64::from(ms)).min(cap);
                    if let Some(d) = self.deadline.as_mut() {
                        *d = (*d).max(want);
                    }
                }
                vec![]
            }
            I::Set {
                state,
                turn_id,
                seq,
            } => {
                if self.seq_turn == Some(turn_id) {
                    if seq <= self.last_seq {
                        return vec![];
                    }
                } else {
                    self.seq_turn = Some(turn_id);
                }
                self.last_seq = seq;
                self.turn_id = turn_id;
                match state {
                    // The glasses own listening/captured; the workflow may confirm thinking
                    // (turn id assignment) but cannot fake the fast edges.
                    S::Listening | S::Captured => vec![],
                    S::Thinking if self.state == S::Thinking => vec![self.report()],
                    S::Thinking => self.enter(now, S::Thinking),
                    s if s == self.state => vec![],
                    s => self.enter(now, s),
                }
            }
            I::Tick => match (self.state, self.deadline) {
                (S::Captured, Some(d)) if now >= d => self.enter(now, S::Thinking),
                (S::Thinking, Some(d)) if now >= d => self.enter(now, S::DoneError),
                (S::DoneOk | S::DoneError, Some(d)) if now >= d => self.enter(now, S::Idle),
                _ => vec![],
            },
        }
    }

    fn report(&self) -> IndicatorAction {
        IndicatorAction::Report {
            state: self.state,
            turn_id: self.turn_id,
        }
    }

    fn stop_loop(&mut self) -> Vec<IndicatorAction> {
        if std::mem::replace(&mut self.loop_on, false) {
            vec![IndicatorAction::StopThinkingLoop]
        } else {
            vec![]
        }
    }

    fn enter(&mut self, now: u64, next: IndicatorState) -> Vec<IndicatorAction> {
        use IndicatorAction as A;
        use IndicatorState as S;
        let mut out = self.stop_loop();
        self.state = next;
        self.deadline = None;
        match next {
            S::Idle => out.push(A::ReleaseLed),
            S::Listening => {
                out.push(A::PlayCue {
                    cue: Cue::ListeningStart,
                });
                out.push(A::SetLed {
                    pattern: LedPattern::steady(LedColor::Blue, 160),
                });
            }
            S::Captured => {
                self.deadline = Some(now + CAPTURED_MS);
                out.push(A::PlayCue { cue: Cue::Captured });
                // blue fading out: one short cycle at lower brightness
                out.push(A::SetLed {
                    pattern: LedPattern {
                        color: LedColor::Blue,
                        on_ms: CAPTURED_MS as u32,
                        off_ms: 0,
                        cycles: 1,
                        brightness: 60,
                    },
                });
            }
            S::Thinking => {
                self.thinking_since = now;
                self.deadline = Some(now + THINKING_TIMEOUT_MS);
                self.loop_on = true;
                out.push(A::StartThinkingLoop {
                    period_ms: THINKING_LOOP_PERIOD_MS,
                });
                // slow breathing amber; enough cycles to outlast the 60 s cap
                let cycles = (THINKING_MAX_MS as u32).div_ceil(THINKING_LOOP_PERIOD_MS);
                out.push(A::SetLed {
                    pattern: LedPattern {
                        color: LedColor::Orange,
                        on_ms: 900,
                        off_ms: 900,
                        cycles,
                        brightness: 90,
                    },
                });
            }
            S::DoneOk => {
                self.deadline = Some(now + DONE_MS);
                out.push(A::PlayCue { cue: Cue::DoneOk });
                out.push(A::SetLed {
                    pattern: LedPattern {
                        color: LedColor::Green,
                        on_ms: 300,
                        off_ms: 200,
                        cycles: 2,
                        brightness: 180,
                    },
                });
            }
            S::DoneError => {
                self.deadline = Some(now + DONE_MS);
                out.push(A::PlayCue {
                    cue: Cue::DoneError,
                });
                out.push(A::SetLed {
                    pattern: LedPattern {
                        color: LedColor::Red,
                        on_ms: 300,
                        off_ms: 200,
                        cycles: 2,
                        brightness: 180,
                    },
                });
            }
        }
        out.push(self.report());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::IndicatorAction as A;
    use super::IndicatorInput as I;
    use super::IndicatorState as S;
    use super::*;

    fn has(v: &[IndicatorAction], a: IndicatorAction) -> bool {
        v.contains(&a)
    }

    /// Drive to thinking at t=1000+: returns (indicator, time thinking started).
    fn to_thinking() -> (Indicator, u64) {
        let mut i = Indicator::new();
        i.handle(0, I::GateOpen);
        i.handle(1000, I::GateClose);
        assert_eq!(i.state(), S::Captured);
        let t = 1000 + CAPTURED_MS;
        i.handle(t, I::Tick);
        assert_eq!(i.state(), S::Thinking);
        (i, t)
    }

    #[test]
    fn happy_path_with_cues_and_led() {
        let mut i = Indicator::new();
        let a = i.handle(0, I::GateOpen);
        assert_eq!(i.state(), S::Listening);
        assert!(has(
            &a,
            A::PlayCue {
                cue: Cue::ListeningStart
            }
        ));
        assert!(
            a.iter()
                .any(|x| matches!(x, A::SetLed { pattern } if pattern.color == LedColor::Blue))
        );
        assert!(has(
            &a,
            A::Report {
                state: S::Listening,
                turn_id: 0
            }
        ));

        let a = i.handle(1000, I::GateClose);
        assert!(has(&a, A::PlayCue { cue: Cue::Captured }));
        assert_eq!(i.next_deadline(), Some(1000 + CAPTURED_MS));

        // too early: no change
        assert!(i.handle(1100, I::Tick).is_empty());
        let a = i.handle(1400, I::Tick);
        assert_eq!(i.state(), S::Thinking);
        assert!(has(
            &a,
            A::StartThinkingLoop {
                period_ms: THINKING_LOOP_PERIOD_MS
            }
        ));
        assert!(
            a.iter()
                .any(|x| matches!(x, A::SetLed { pattern } if pattern.color == LedColor::Orange))
        );

        let a = i.handle(
            5000,
            I::Set {
                state: S::DoneOk,
                turn_id: 5,
                seq: 1,
            },
        );
        assert_eq!(i.state(), S::DoneOk);
        assert!(has(&a, A::StopThinkingLoop));
        assert!(has(&a, A::PlayCue { cue: Cue::DoneOk }));
        assert!(has(
            &a,
            A::Report {
                state: S::DoneOk,
                turn_id: 5
            }
        ));

        let a = i.handle(5000 + DONE_MS, I::Tick);
        assert_eq!(i.state(), S::Idle);
        assert!(has(&a, A::ReleaseLed));
        assert_eq!(i.next_deadline(), None);
    }

    #[test]
    fn thinking_times_out_to_done_error_after_20s() {
        let (mut i, t) = to_thinking();
        assert_eq!(i.next_deadline(), Some(t + THINKING_TIMEOUT_MS));
        assert!(i.handle(t + THINKING_TIMEOUT_MS - 1, I::Tick).is_empty());
        assert_eq!(i.state(), S::Thinking);
        let a = i.handle(t + THINKING_TIMEOUT_MS, I::Tick);
        assert_eq!(i.state(), S::DoneError);
        assert!(has(&a, A::StopThinkingLoop));
        assert!(has(
            &a,
            A::PlayCue {
                cue: Cue::DoneError
            }
        ));
        assert!(
            a.iter()
                .any(|x| matches!(x, A::SetLed { pattern } if pattern.color == LedColor::Red))
        );
    }

    #[test]
    fn hold_extends_but_caps_at_60s() {
        let (mut i, t) = to_thinking();
        i.handle(t + 10_000, I::Hold { ms: 30_000 });
        assert_eq!(i.next_deadline(), Some(t + 40_000));
        i.handle(t + 10_000, I::Hold { ms: 600_000 });
        assert_eq!(i.next_deadline(), Some(t + THINKING_MAX_MS));
        // a short hold never shortens
        i.handle(t + 11_000, I::Hold { ms: 100 });
        assert_eq!(i.next_deadline(), Some(t + THINKING_MAX_MS));
        i.handle(t + THINKING_MAX_MS, I::Tick);
        assert_eq!(i.state(), S::DoneError);
    }

    #[test]
    fn link_loss_while_thinking_is_immediate_done_error() {
        let (mut i, t) = to_thinking();
        let a = i.handle(t + 50, I::LinkLost);
        assert_eq!(i.state(), S::DoneError);
        assert!(has(
            &a,
            A::PlayCue {
                cue: Cue::DoneError
            }
        ));
        // idle link loss does nothing
        let mut idle = Indicator::new();
        assert!(idle.handle(0, I::LinkLost).is_empty());
    }

    #[test]
    fn reply_audio_stops_loop_and_timeout_but_stays_thinking() {
        let (mut i, t) = to_thinking();
        let a = i.handle(t + 3000, I::ReplyAudioStarted);
        assert_eq!(a, vec![A::StopThinkingLoop]);
        assert_eq!(i.state(), S::Thinking);
        assert_eq!(i.next_deadline(), None);
        i.handle(t + 60_000, I::Tick);
        assert_eq!(i.state(), S::Thinking);
        i.handle(
            t + 61_000,
            I::Set {
                state: S::DoneOk,
                turn_id: 1,
                seq: 1,
            },
        );
        assert_eq!(i.state(), S::DoneOk);
    }

    #[test]
    fn stale_and_duplicate_set_ignored_within_a_turn() {
        let (mut i, t) = to_thinking();
        i.handle(
            t,
            I::Set {
                state: S::Thinking,
                turn_id: 7,
                seq: 5,
            },
        );
        // stale seq: ignored even though it names done_ok
        assert!(
            i.handle(
                t + 1,
                I::Set {
                    state: S::DoneOk,
                    turn_id: 7,
                    seq: 5
                }
            )
            .is_empty()
        );
        assert!(
            i.handle(
                t + 1,
                I::Set {
                    state: S::DoneOk,
                    turn_id: 7,
                    seq: 4
                }
            )
            .is_empty()
        );
        assert_eq!(i.state(), S::Thinking);
        i.handle(
            t + 2,
            I::Set {
                state: S::DoneOk,
                turn_id: 7,
                seq: 6,
            },
        );
        assert_eq!(i.state(), S::DoneOk);
        // idempotent: same state again is a no-op
        assert!(
            i.handle(
                t + 3,
                I::Set {
                    state: S::DoneOk,
                    turn_id: 7,
                    seq: 7
                }
            )
            .is_empty()
        );
        // a new turn restarts the seq floor
        i.handle(
            t + 4,
            I::Set {
                state: S::DoneError,
                turn_id: 8,
                seq: 1,
            },
        );
        assert_eq!(i.state(), S::DoneError);
    }

    #[test]
    fn workflow_cannot_fake_the_fast_edges() {
        let mut i = Indicator::new();
        assert!(
            i.handle(
                0,
                I::Set {
                    state: S::Listening,
                    turn_id: 1,
                    seq: 1
                }
            )
            .is_empty()
        );
        assert_eq!(i.state(), S::Idle);
    }

    #[test]
    fn barge_in_from_thinking_restarts_listening() {
        let (mut i, t) = to_thinking();
        let a = i.handle(t + 500, I::GateOpen);
        assert_eq!(i.state(), S::Listening);
        assert!(has(&a, A::StopThinkingLoop));
        assert!(has(
            &a,
            A::PlayCue {
                cue: Cue::ListeningStart
            }
        ));
        // duplicate open while listening is a no-op
        assert!(i.handle(t + 600, I::GateOpen).is_empty());
    }

    #[test]
    fn gate_close_outside_listening_is_ignored() {
        let mut i = Indicator::new();
        assert!(i.handle(0, I::GateClose).is_empty());
        assert_eq!(i.state(), S::Idle);
    }
}
