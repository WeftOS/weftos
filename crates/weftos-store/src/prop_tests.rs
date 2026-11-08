//! Property-style tests: many seeded random cases against stated laws, not hand-picked examples.
//! A failure prints the seed and the case so it reproduces.

use crate::grant_seq::{CEILING, MAX_GAP, admit};
use crate::testrng::Rng;

/// Values worth hitting on every draw: the edges of `u64` and of the bounds.
fn edgy(r: &mut Rng) -> u64 {
    match r.below(8) {
        0 => r.pick(&[0, 1, 2, u64::MAX, u64::MAX - 1, 1 << 63, (1 << 63) - 1]),
        1 => r.pick(&[CEILING - 1, CEILING, CEILING + 1, CEILING - MAX_GAP, CEILING + MAX_GAP]),
        2 => r.below(2 * MAX_GAP),
        3 => r.next() >> r.below(64),
        _ => r.next(),
    }
}

/// `admit` is exactly `last < seq <= min(CEILING, last + MAX_GAP)`, checked in wide arithmetic, and
/// never panics or wraps whatever the inputs.
#[test]
fn admit_matches_its_definition_over_the_whole_u64_range() {
    let mut r = Rng::new(0x5EED_0001);
    for i in 0..200_000 {
        let (last, seq) = (edgy(&mut r), edgy(&mut r));
        let want = u128::from(seq) > u128::from(last) && u128::from(seq) <= u128::from(CEILING) && u128::from(seq) - u128::from(last) <= u128::from(MAX_GAP);
        assert_eq!(admit(last, seq), want, "case {i}: last={last} seq={seq}");
        // Near neighbours of a decision are the likeliest to be wrong.
        for d in [1u64, 2] {
            let (l2, s2) = (last.saturating_add(d), seq.saturating_sub(d));
            let want2 = u128::from(s2) > u128::from(l2) && u128::from(s2) <= u128::from(CEILING) && u128::from(s2) - u128::from(l2) <= u128::from(MAX_GAP);
            assert_eq!(admit(l2, s2), want2, "case {i}: neighbour last={l2} seq={s2}");
        }
    }
}

/// A consumer that applies only what `admit` allows, fed a stream of honest next-events mixed with
/// forgeries: it never goes backwards, never passes the ceiling, and a forgery never stops the next
/// honest event (`last + 1`) from applying, until the ceiling itself.
#[test]
fn a_consumer_cannot_be_frozen_by_forged_sequence_numbers() {
    for seed in 0..40u64 {
        let mut r = Rng::new(0xF0_0000 + seed);
        let mut last = if r.chance(3) { CEILING - r.below(5_000) } else { r.below(10_000) };
        let (mut honest, mut forged_refused) = (0u64, 0u64);
        for step in 0..20_000 {
            let before = last;
            if r.chance(3) {
                let forged = edgy(&mut r);
                if admit(last, forged) {
                    // Admitted only inside the bound: a legitimate gap, not a jump to the top.
                    assert!(forged > last && forged - last <= MAX_GAP && forged <= CEILING, "seed {seed} step {step}: admitted {forged} after {last}");
                    last = forged;
                } else {
                    forged_refused += 1;
                    assert_eq!(last, before, "a refused event must not move the consumer");
                }
            } else if last < CEILING {
                assert!(admit(last, last + 1), "seed {seed} step {step}: honest next event refused at last={last}");
                last += 1;
                honest += 1;
            } else {
                assert!(!admit(last, last + 1), "nothing is admitted past the ceiling");
            }
            assert!(last >= before && last <= CEILING, "seed {seed} step {step}: last={last} before={before}");
        }
        assert!(honest > 0 && forged_refused > 0, "seed {seed}: the stream exercised both");
    }
}

/// The freeze needs about CEILING / MAX_GAP admitted forgeries in a row; one is never enough.
#[test]
fn one_forgery_at_any_last_cannot_exhaust_the_sequence() {
    let mut r = Rng::new(0xF0_F0);
    for _ in 0..50_000 {
        let last = edgy(&mut r).min(CEILING);
        let forged = edgy(&mut r);
        if admit(last, forged) {
            assert!(CEILING - forged >= CEILING.saturating_sub(last + MAX_GAP), "an admitted event leaves at least the ceiling minus the gap");
            if last < CEILING - MAX_GAP {
                assert!(forged < CEILING, "far from the ceiling, no single admitted event reaches it: last={last} forged={forged}");
            }
        }
    }
}

#[cfg(feature = "rustfs")]
mod dates {
    use super::*;
    use crate::sigv4::{MAX_ISO_SECS, iso_date, parse_iso};

    /// Seconds worth hitting: the calendar's awkward places and the format's edges.
    fn secs(r: &mut Rng) -> u64 {
        match r.below(8) {
            0 => r.pick(&[0, 1, 86_399, 86_400, 951_782_399, 951_782_400, 951_868_800, 2_147_483_647, 2_147_483_648, 4_102_444_800, MAX_ISO_SECS, MAX_ISO_SECS - 1, 253_402_214_400]),
            // 29 Feb of random leap years, and the day either side.
            1 => {
                let y = 4 * r.range(493, 2499); // 1972..=9996, includes 2100 (not leap) and 2400 (leap)
                let days = days_from_civil(y, 2, 28) + r.below(3);
                (days * 86_400 + r.below(86_400)).min(MAX_ISO_SECS)
            }
            _ => r.range(0, MAX_ISO_SECS),
        }
    }

    /// Howard Hinnant's days-from-civil, written out here as the oracle's own calendar.
    fn days_from_civil(y: u64, m: u64, d: u64) -> u64 {
        let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
        let era = y / 400;
        let yoe = y - era * 400;
        let doy = (153 * m + 2) / 5 + d - 1;
        era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
    }

    #[test]
    fn what_iso_date_writes_parse_iso_reads_back_exactly() {
        let mut r = Rng::new(0xDA7E_0001);
        for i in 0..100_000 {
            let s = secs(&mut r);
            let text = iso_date(s);
            assert_eq!(text.len(), 24, "case {i}: {s} -> {text}: fixed width, a four-digit year");
            assert_eq!(parse_iso(&text), Some(s), "case {i}: {s} -> {text}");
        }
    }

    #[test]
    fn iso_dates_sort_as_the_instants_do_and_later_than_the_year_9999_clamps() {
        let mut r = Rng::new(0xDA7E_0002);
        for i in 0..50_000 {
            let (a, b) = (secs(&mut r), secs(&mut r));
            assert_eq!(a.cmp(&b), iso_date(a).cmp(&iso_date(b)), "case {i}: {a} vs {b}");
        }
        for _ in 0..1_000 {
            let s = r.range(MAX_ISO_SECS, u64::MAX);
            assert_eq!(iso_date(s), iso_date(MAX_ISO_SECS), "{s}");
        }
        assert_eq!(iso_date(u64::MAX), "9999-12-31T23:59:59.000Z");
    }

    /// Whatever text goes in, `parse_iso` does not panic; and an answer always names the date that
    /// was written (a 31 February is refused, not quietly turned into March).
    #[test]
    fn parse_iso_never_panics_and_never_moves_a_date_it_accepts() {
        let alphabet: Vec<char> = "0123456789-:TZ.+ x\u{e9}".chars().collect();
        let mut r = Rng::new(0xDA7E_0003);
        let (mut accepted, mut refused) = (0u32, 0u32);
        for i in 0..200_000 {
            let text: String = match r.below(4) {
                // Pure noise from the format's own alphabet.
                0 => (0..r.below(40)).map(|_| r.pick(&alphabet)).collect(),
                // A valid date with one character replaced, dropped or doubled.
                1 | 2 => {
                    let mut c: Vec<char> = iso_date(secs(&mut r)).chars().collect();
                    let at = r.below(c.len() as u64) as usize;
                    match r.below(3) {
                        0 => c[at] = r.pick(&alphabet),
                        1 => {
                            c.remove(at);
                        }
                        _ => c.insert(at, r.pick(&alphabet)),
                    }
                    c.into_iter().collect()
                }
                // A well-shaped date with arbitrary fields, many of them impossible.
                _ => format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", r.range(1960, 10_001), r.range(0, 14), r.range(0, 33), r.range(0, 26), r.range(0, 62), r.range(0, 62)),
            };
            match parse_iso(&text) {
                None => refused += 1,
                Some(v) => {
                    accepted += 1;
                    assert!(v <= MAX_ISO_SECS + 1, "case {i}: {text:?} -> {v} is past the last second (+1 for a leap second)");
                    // Leap second 60 is the one accepted second that has no spelling of its own.
                    let back = iso_date(v);
                    // Read the fields the way the format defines them, then compare with what came back.
                    let written = text.trim();
                    let written = written.strip_suffix('Z').or_else(|| written.strip_suffix("+00:00")).unwrap_or(written);
                    let (d, t) = written.split_once('T').unwrap();
                    let t = t.split('.').next().unwrap();
                    let f = |x: &str| x.parse::<u64>().unwrap_or_else(|_| panic!("case {i}: accepted {text:?} but field {x:?} is not a number"));
                    let (ds, ts): (Vec<u64>, Vec<u64>) = (d.split('-').map(f).collect(), t.split(':').map(f).collect());
                    if ts[2] < 60 {
                        let want = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", ds[0], ds[1], ds[2], ts[0], ts[1], ts[2]);
                        assert_eq!(&back[..19], want, "case {i}: {text:?} was moved to {back}");
                    }
                }
            }
        }
        assert!(accepted > 10_000 && refused > 10_000, "both branches ran: {accepted} accepted, {refused} refused");
    }

    /// Every (year, month, day) is accepted exactly when the calendar has that day.
    #[test]
    fn parse_iso_accepts_exactly_the_days_the_calendar_has() {
        let leap = |y: u64| (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400);
        let dim = |y: u64, m: u64| match m {
            2 => 28 + u64::from(leap(y)),
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
        for y in [1970, 1971, 1999, 2000, 2023, 2024, 2025, 2100, 2399, 2400, 9996, 9999] {
            for m in 1..=12 {
                for d in 1..=33 {
                    let text = format!("{y:04}-{m:02}-{d:02}T12:00:00Z");
                    assert_eq!(parse_iso(&text).is_some(), d <= dim(y, m), "{text}");
                    if d <= dim(y, m) {
                        assert_eq!(parse_iso(&text), Some(days_from_civil(y, m, d) * 86_400 + 12 * 3600), "{text}");
                    }
                }
            }
        }
    }
}
