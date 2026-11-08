//! Property-style tests for the transport's own rules: the circuit breaker's state machine and the
//! retain-until check. Seeded and printed on failure, so a bad run reproduces.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::*;
use crate::backend::{LockMode, PutOpts, Retention};
use crate::sigv4::{MAX_ISO_SECS, parse_iso};
use crate::testrng::Rng;

#[derive(Debug, Clone, Copy)]
enum Op {
    Admit,
    Down,
    Up,
    Release,
}

fn state(threshold: u32, cooldown: Duration) -> BreakerState {
    BreakerState { cfg: Breaker { threshold, cooldown }, ..Default::default() }
}

/// Random calls against the breaker, checked against laws that must hold at every step, not just on
/// the scripted path: it opens at exactly the threshold, lets one probe through at a time, closes
/// fully on any answer, never makes a caller wait longer than a cooldown, and `release_probe`
/// leaves it as it was.
#[test]
fn the_breaker_keeps_its_laws_under_random_traffic() {
    let t0 = Instant::now();
    for seed in 0..300u64 {
        let mut r = Rng::new(0xB4EA_0000 + seed);
        let threshold = r.range(1, 6) as u32;
        let cooldown = Duration::from_secs(r.range(1, 20));
        let wait_cap = cooldown.max(Duration::from_secs(1));
        let mut b = state(threshold, cooldown);
        let mut now = t0;
        // Downs in a row since the last answer, and the probe we handed out (and when).
        let (mut consec, mut probe): (u32, Option<Instant>) = (0, None);
        for step in 0..400 {
            let span = if r.chance(4) { 25_000 } else { 3_000 };
            now += Duration::from_millis(r.below(span));
            let op = r.pick(&[Op::Admit, Op::Admit, Op::Down, Op::Down, Op::Up, Op::Release]);
            let ctx = format!("seed {seed} step {step} {op:?} threshold {threshold} cooldown {cooldown:?}");
            match op {
                Op::Admit => {
                    let was_open = b.open_until.is_some();
                    let got = b.admit(now);
                    if !was_open {
                        assert!(got.is_ok(), "a closed breaker admits: {ctx}");
                    }
                    if let Err(w) = got {
                        assert!(w <= wait_cap, "waits at most a cooldown, said {w:?}: {ctx}");
                        assert!(b.is_open(now), "refused but not open: {ctx}");
                    }
                    if let Some(p) = probe.filter(|p| now < *p + cooldown) {
                        assert!(got.is_err(), "a second request got through while the probe from {p:?} was out: {ctx}");
                    }
                    if was_open && got.is_ok() {
                        assert!(b.probing, "{ctx}");
                        probe = Some(now);
                    }
                }
                Op::Down => {
                    let was_open = b.open_until.is_some();
                    consec = consec.saturating_add(1);
                    b.record(true, now);
                    probe = None;
                    if !was_open {
                        assert_eq!(b.open_until.is_some(), consec >= threshold, "opens at exactly the threshold (consec {consec}): {ctx}");
                    } else {
                        assert!(b.open_until.is_some_and(|t| t > now), "a failure while open keeps it open: {ctx}");
                    }
                }
                Op::Up => {
                    b.record(false, now);
                    (consec, probe) = (0, None);
                    assert!(b.open_until.is_none() && !b.probing && b.fails == 0, "any answer closes it fully: {ctx}");
                    assert!(b.admit(now).is_ok(), "{ctx}");
                }
                Op::Release => {
                    let (open, fails) = (b.open_until, b.fails);
                    b.release_probe();
                    probe = None;
                    assert_eq!((b.open_until, b.fails, b.probing), (open, fails, false), "release_probe only frees the slot: {ctx}");
                }
            }
            assert!(!b.probing || b.open_until.is_some(), "a probe implies it was open: {ctx}");
        }
    }
}

/// A probe that never reports back (its thread died) is given up on after a cooldown and one more is
/// let through; and a failed probe re-opens it for a full cooldown from that moment.
#[test]
fn a_lost_probe_is_replaced_after_a_cooldown_and_a_failed_one_reopens_for_a_full_cooldown() {
    let t0 = Instant::now();
    for seed in 0..200u64 {
        let mut r = Rng::new(0xB4EA_1000 + seed);
        let cooldown = Duration::from_secs(r.range(2, 30));
        let mut b = state(1, cooldown);
        b.record(true, t0);
        let t_probe = t0 + cooldown + Duration::from_millis(r.below(5_000));
        assert!(b.admit(t_probe).is_ok(), "seed {seed}: first probe");
        assert!(b.admit(t_probe + cooldown - Duration::from_millis(1)).is_err(), "seed {seed}: still out");
        assert!(b.admit(t_probe + cooldown).is_ok(), "seed {seed}: given up on, another goes");
        let fail_at = t_probe + cooldown + Duration::from_millis(r.below(3_000));
        b.record(true, fail_at);
        assert_eq!(b.admit(fail_at), Err(cooldown), "seed {seed}: a full cooldown from the failure");
        assert!(b.admit(fail_at + cooldown).is_ok(), "seed {seed}: and then a probe again");
    }
}

/// Counting never overflows however long the outage, and off means off.
#[test]
fn the_failure_count_saturates_and_a_zero_threshold_never_opens() {
    let t0 = Instant::now();
    let mut b = state(5, Duration::from_secs(10));
    b.fails = u32::MAX - 2;
    for _ in 0..10 {
        b.record(true, t0);
    }
    assert_eq!(b.fails, u32::MAX);
    assert!(b.is_open(t0));
    b.record(false, t0);
    assert!(!b.is_open(t0) && b.fails == 0);
    let mut r = Rng::new(0xB4EA_2000);
    let mut off = state(0, Duration::from_secs(10));
    for _ in 0..5_000 {
        match r.below(4) {
            0 => off.record(true, t0),
            1 => off.record(false, t0),
            2 => off.release_probe(),
            _ => {}
        }
        assert!(off.admit(t0).is_ok() && !off.is_open(t0));
    }
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// A retain-until the transport will send is exactly one that is in the future and writable as a
/// four-digit-year date; and the header it writes parses back to the instant that was asked for.
#[test]
fn retain_until_is_accepted_only_in_the_future_and_the_header_round_trips() {
    let d = tempfile::tempdir().unwrap();
    let cfg = RustfsConfig { endpoint: "http://127.0.0.1:1".into(), region: "us-east-1".into(), root_access_key: "a".into(), root_secret_key: "b".into(), secrets_dir: d.path().join("s"), timeout: Duration::from_secs(1) };
    let be = RustfsBackend::new(cfg).unwrap();
    let mut r = Rng::new(0x4E7A_0001);
    let (mut ok, mut refused) = (0u32, 0u32);
    for i in 0..60_000 {
        let now = now_s();
        let until = match r.below(7) {
            0 => r.below(now.saturating_sub(10)),
            1 => r.pick(&[0, 1, now.saturating_sub(60)]),
            2 => r.pick(&[MAX_ISO_SECS, MAX_ISO_SECS - 1, MAX_ISO_SECS + 1, MAX_ISO_SECS + 2, u64::MAX, u64::MAX - 1, 1 << 63]),
            3 => r.range(MAX_ISO_SECS + 1, u64::MAX),
            _ => r.range(now + 120, MAX_ISO_SECS),
        };
        let mode = if r.chance(2) { LockMode::Compliance } else { LockMode::Governance };
        let hold = r.chance(3);
        let opts = PutOpts { if_none_match: r.chance(2), retention: Some(Retention { mode, until_unix_s: until }), legal_hold: hold };
        let want_ok = until > now + 120 && until <= MAX_ISO_SECS;
        let past_or_far = until <= now.saturating_sub(10) || until > MAX_ISO_SECS;
        match be.lock_headers(&opts) {
            Ok(h) => {
                ok += 1;
                assert!(!past_or_far, "case {i}: {until} should have been refused");
                let date = h.iter().find(|(k, _)| *k == "x-amz-object-lock-retain-until-date").map(|(_, v)| v.clone()).expect("date header");
                assert_eq!(parse_iso(&date), Some(until), "case {i}: header {date} for {until}");
                assert!(h.iter().any(|(k, v)| *k == "x-amz-object-lock-mode" && v == mode.as_str()), "case {i}");
                assert_eq!(h.iter().any(|(k, _)| *k == "x-amz-object-lock-legal-hold"), hold, "case {i}");
            }
            Err(e) => {
                refused += 1;
                assert!(!want_ok, "case {i}: {until} should have been accepted, got {e}");
                assert!(matches!(e, BackendError::BadKey(_)), "case {i}: a bad date is a bad request, not an I/O error: {e}");
            }
        }
    }
    assert!(ok > 10_000 && refused > 10_000, "both branches ran: {ok} accepted, {refused} refused");
    // No retention asked for: no headers beyond the hold.
    assert!(be.lock_headers(&PutOpts::default()).unwrap().is_empty());
}
