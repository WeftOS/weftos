//! Held-out-by-day split: whole days go to train or held-out, never both.

use alloc::vec::Vec;

use super::{EvalVeto, Transition};

/// Result of a by-day split.
pub(super) struct DaySplit {
    pub train: Vec<Transition>,
    pub held: Vec<Transition>,
    pub n_train_days: usize,
    pub n_held_days: usize,
    /// Held-out day ids, ascending.
    pub held_days: Vec<u16>,
}

/// Distinct day ids, ascending.
fn distinct_days(days: &[u16]) -> Vec<u16> {
    let mut d: Vec<u16> = days.to_vec();
    d.sort_unstable();
    d.dedup();
    d
}

/// Hold out the last `holdout_days` distinct days (by id order). Transitions keep their
/// original order inside each side. Needs at least one train day and one held-out day.
pub(super) fn by_day(
    transitions: &[Transition],
    days: &[u16],
    holdout_days: usize,
) -> Result<DaySplit, EvalVeto> {
    if transitions.len() != days.len() {
        return Err(EvalVeto::TooFewTransitions);
    }
    let distinct = distinct_days(days);
    let n_held_days = holdout_days.max(1);
    if distinct.len() < n_held_days + 1 {
        return Err(EvalVeto::TooFewDays);
    }
    let cut = distinct.len() - n_held_days;
    let held_days: Vec<u16> = distinct[cut..].to_vec();
    let first_held = held_days[0];
    let (mut train, mut held) = (Vec::new(), Vec::new());
    for (t, &d) in transitions.iter().zip(days) {
        if d >= first_held {
            held.push(*t);
        } else {
            train.push(*t);
        }
    }
    Ok(DaySplit { train, held, n_train_days: cut, n_held_days, held_days })
}
