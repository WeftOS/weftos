//! Keep the console alive across transient failures.
//!
//! The daemon restarts, the leaf drops for a moment, the governance gate
//! rate-limits a publish: none of these are reasons for the host to exit.
//! [`supervise`] runs the host loop and, on any display error, drops the
//! session, backs off (capped exponential), opens a fresh session and
//! repaints the whole scene, because the pad may have rebooted or lost
//! it. The agent link is rebuilt with the host and subscribes again.
//!
//! One log line per state change (down with the reason and the first
//! retry delay, up again), never one per retry.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use weftos_display_client::{DisplayError, DisplaySession};

use super::{Host, HostConfig};

#[derive(Debug, Clone)]
pub struct SuperviseConfig {
    pub host: HostConfig,
    /// Retry backoff bounds for opening the session and for a failed frame.
    pub retry_min: Duration,
    pub retry_max: Duration,
    /// Frame pace of the loop.
    pub frame: Duration,
    /// A closed session is a transient drop (a leaf) and not the end (a
    /// window the user closed).
    pub reopen_on_close: bool,
    /// Stop after this long (smoke runs).
    pub limit: Option<Duration>,
}

impl SuperviseConfig {
    pub fn new(host: HostConfig) -> Self {
        Self {
            host,
            retry_min: Duration::from_millis(250),
            retry_max: Duration::from_secs(10),
            frame: Duration::from_millis(33),
            reopen_on_close: false,
            limit: None,
        }
    }
}

/// Why [`supervise`] returned. It never returns for a display error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    Stopped,
    TimeLimit,
    /// The session closed and `reopen_on_close` is off.
    Closed,
}

/// Run the console until `stop`, the time limit, or (without
/// `reopen_on_close`) the session closing. `open` is called for every
/// (re)connect; `clock` gives the `HH:MM` for the status strip; `log`
/// receives the state-change lines.
pub fn supervise(
    mut open: impl FnMut() -> Result<DisplaySession, DisplayError>,
    mut clock: impl FnMut() -> String,
    cfg: &SuperviseConfig,
    stop: &AtomicBool,
    mut log: impl FnMut(&str),
) -> Ended {
    let start = Instant::now();
    let mut backoff = cfg.retry_min;
    let mut down = false;
    let expired = |start: &Instant| cfg.limit.is_some_and(|l| start.elapsed() >= l);

    while !stop.load(Ordering::SeqCst) {
        if expired(&start) {
            return Ended::TimeLimit;
        }
        let failure = match run_connection(&mut open, &mut clock, cfg, stop, &start, &mut down, &mut log) {
            Conn::Done(end) => return end,
            Conn::Failed { reason, was_up } => {
                if was_up {
                    backoff = cfg.retry_min;
                }
                reason
            }
        };
        if !down {
            down = true;
            log(&format!("console host: disconnected: {failure}; retrying in {} ms", backoff.as_millis()));
        }
        sleep_unless_stopped(backoff, stop);
        backoff = (backoff * 2).min(cfg.retry_max);
    }
    Ended::Stopped
}

enum Conn {
    Done(Ended),
    Failed { reason: String, was_up: bool },
}

fn run_connection(
    open: &mut impl FnMut() -> Result<DisplaySession, DisplayError>,
    clock: &mut impl FnMut() -> String,
    cfg: &SuperviseConfig,
    stop: &AtomicBool,
    start: &Instant,
    down: &mut bool,
    log: &mut impl FnMut(&str),
) -> Conn {
    let fail = |e: DisplayError, was_up| Conn::Failed { reason: e.to_string(), was_up };
    let mut session = match open() {
        Ok(s) => s,
        Err(e) => return fail(e, false),
    };
    // Opening the host paints the full scene.
    let mut host = match Host::open(&mut session, cfg.host.clone()) {
        Ok(h) => h,
        Err(e) => return fail(e, false),
    };
    if std::mem::take(down) {
        log("console host: reconnected");
    }
    loop {
        let frame_start = Instant::now();
        let now_ms = start.elapsed().as_millis() as u32;
        if let Err(e) = host.set_clock(&clock()) {
            return fail(e, true);
        }
        match host.step(now_ms) {
            Err(e) => return fail(e, true),
            Ok(false) if cfg.reopen_on_close => return fail(DisplayError::Closed, true),
            Ok(false) => return Conn::Done(Ended::Closed),
            Ok(true) => {}
        }
        if stop.load(Ordering::SeqCst) {
            return Conn::Done(Ended::Stopped);
        }
        if cfg.limit.is_some_and(|l| start.elapsed() >= l) {
            return Conn::Done(Ended::TimeLimit);
        }
        if let Some(rest) = cfg.frame.checked_sub(frame_start.elapsed()) {
            thread::sleep(rest);
        }
    }
}

fn sleep_unless_stopped(total: Duration, stop: &AtomicBool) {
    let slice = Duration::from_millis(25);
    let mut left = total;
    while !left.is_zero() && !stop.load(Ordering::SeqCst) {
        let d = left.min(slice);
        thread::sleep(d);
        left -= d;
    }
}
