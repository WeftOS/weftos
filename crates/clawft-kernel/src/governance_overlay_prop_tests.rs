//! Property and fuzz tests for the governance overlay (ADR-103 D8, package E):
//! the effective policy only tightens the parent, and mutated overlay and
//! parent files never panic, never loosen, and never verify unless they mean
//! exactly what was signed. Deterministic seeds, bounded iteration counts.

use clawft_types::config::overlay::{Limits, OverlayApproval, OverlayDeny, OverlayFile};
use clawft_types::project::canon::canonical_json;

use crate::governance::{
    EffectVector, GovernanceDecision, GovernanceRequest, RuleSeverity,
};
use crate::governance_overlay::{Overlay, OverlayError, merge, sorted_rule_values};
use crate::governance_overlay_tests::{base_parent, engine_of, rule, user_key};
use crate::parent_policy::{ParentPolicy, verify_parent_policy};

// ── tiny deterministic RNG ───────────────────────────────────────────────

pub(crate) struct Rng(pub u64);
impl Rng {
    pub(crate) fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub(crate) fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    pub(crate) fn coin(&mut self) -> bool {
        self.next() & 1 == 1
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

const GLOBS: &[&str] = &[
    "tool.shell*",
    "workload.*",
    "workload.place*",
    "net.fetch",
    "x.y*",
    "tool.shell_exec",
    "workload.placement.x",
];
const BAD_GLOBS: &[&str] = &["a*b", "", "has space", "*x*"];
const RULE_IDS: &[&str] = &["a", "B", "c.d", "E-1", "workload.deny", "gov-1"];
const OVERLAY_IDS: &[&str] = &["a", "b", "DENY1", "gov-1", "n.1", " pad ", "new-one", "", "b"];

fn random_limits(r: &mut Rng, allow_high: bool) -> Limits {
    let risk = [None, Some(0.3), Some(0.5), Some(0.9)];
    let procs = [None, Some(4u64), Some(16)];
    let spawn = [None, Some(1u64), Some(8)];
    let human = [None, Some(true), Some(false)];
    let mut l = Limits {
        risk_threshold: *r.pick(&risk),
        max_processes: *r.pick(&procs),
        spawn_budget: *r.pick(&spawn),
        human_approval_required: *r.pick(&human),
    };
    if allow_high && r.below(6) == 0 {
        l.max_processes = Some(1_000);
    }
    l
}

fn random_parent(r: &mut Rng) -> ParentPolicy {
    let sev = [
        RuleSeverity::Advisory,
        RuleSeverity::Warning,
        RuleSeverity::Blocking,
        RuleSeverity::Critical,
    ];
    let mut p = base_parent();
    p.rules = (0..r.below(7))
        .map(|_| {
            let sel = r.coin().then(|| *r.pick(GLOBS));
            let id = *r.pick(RULE_IDS);
            rule(id, r.pick(&sev).clone(), sel, r.coin(), r.below(4) != 0)
        })
        .collect();
    p.limits = random_limits(r, false);
    p
}

fn random_overlay(r: &mut Rng) -> OverlayFile {
    let actions = |r: &mut Rng| -> Vec<String> {
        (0..=r.below(3))
            .map(|_| {
                if r.below(8) == 0 {
                    (*r.pick(BAD_GLOBS)).to_owned()
                } else {
                    (*r.pick(GLOBS)).to_owned()
                }
            })
            .collect()
    };
    let mut f = OverlayFile::default();
    for _ in 0..r.below(4) {
        f.deny.push(OverlayDeny {
            id: (*r.pick(OVERLAY_IDS)).to_owned(),
            actions: actions(r),
            reason: r.coin().then(|| "why".to_owned()),
            principal: None,
        });
    }
    for _ in 0..r.below(3) {
        f.require_approval.push(OverlayApproval {
            id: r.coin().then(|| (*r.pick(OVERLAY_IDS)).to_owned()),
            actions: actions(r),
            reason: None,
            principal: None,
        });
    }
    f.limits = random_limits(r, true);
    f
}

fn decision_class(d: &GovernanceDecision) -> u8 {
    match d {
        GovernanceDecision::Permit => 0,
        GovernanceDecision::PermitWithWarning(_) => 1,
        GovernanceDecision::EscalateToHuman(_) | GovernanceDecision::Deny(_) => 2,
    }
}

/// The tighten-only invariants for one accepted merge.
fn assert_tightens(parent: &ParentPolicy, e: &crate::governance_overlay::Effective, r: &mut Rng) {
    // Every parent rule survives verbatim, except that an overlay-raised
    // `human_approval_required` hardens the parent's blocking rules into
    // overlay denies (review M1), which never escalate.
    let eff_rules = sorted_rule_values(&e.rules);
    let raised = e.limits.human_approval_required == Some(true)
        && parent.limits.human_approval_required != Some(true);
    for pr in &parent.rules {
        let mut want = pr.clone();
        if raised
            && matches!(want.severity, RuleSeverity::Blocking | RuleSeverity::Critical)
            && want.sop_category.as_deref() != Some(crate::governance::OVERLAY_APPROVAL_TAG)
        {
            want.sop_category = Some(crate::governance::OVERLAY_DENY_TAG.to_owned());
        }
        let pv = sorted_rule_values(std::slice::from_ref(&want)).remove(0);
        assert!(eff_rules.contains(&pv), "parent rule lost: {}", canonical_json(&pv));
    }
    // Deny set is a superset.
    let view = crate::governance_overlay::parent_view(parent);
    for g in &view.deny_actions {
        assert!(e.deny_actions.contains(g), "parent deny {g} lost");
    }
    // Limits only go down; flags only up.
    let (pl, el) = (&parent.limits, &e.limits);
    if let Some(p) = pl.risk_threshold {
        assert!(el.risk_threshold.unwrap() <= p);
    }
    if let Some(p) = pl.max_processes {
        assert!(el.max_processes.unwrap() <= p);
    }
    if let Some(p) = pl.spawn_budget {
        assert!(el.spawn_budget.unwrap() <= p);
    }
    if pl.human_approval_required == Some(true) {
        assert_eq!(el.human_approval_required, Some(true));
    }
    // Behaviour: whatever the parent blocks, the effective set blocks too.
    let parent_engine = engine_of(
        parent.rules.clone(),
        pl.risk_threshold.unwrap_or(0.7),
        pl.human_approval_required.unwrap_or(false),
    );
    let eff_engine = engine_of(e.rules.clone(), e.risk_threshold(0.7), e.human_approval(false));
    let mut actions: Vec<String> = GLOBS.iter().map(|g| g.trim_end_matches('*').to_owned()).collect();
    actions.extend(["other.thing".to_owned(), "workload.placement.deep".to_owned()]);
    for a in &actions {
        let mag = (r.below(100) as f64) / 100.0;
        let req = GovernanceRequest::new("agent", a.as_str()).with_effect(EffectVector {
            risk: mag,
            security: mag,
            ..EffectVector::default()
        });
        let (pd, ed) = (
            parent_engine.evaluate(&req).decision,
            eff_engine.evaluate(&req).decision,
        );
        if decision_class(&pd) == 2 {
            assert_eq!(decision_class(&ed), 2, "{a}: parent {pd:?} but effective {ed:?}");
        }
    }
}

#[test]
fn property_effective_policy_only_tightens_the_parent() {
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let (mut ok, mut refused) = (0, 0);
    for _ in 0..2000 {
        let parent = random_parent(&mut r);
        let overlay = Overlay::from_file(random_overlay(&mut r));
        match merge(&parent, &overlay) {
            Ok(e) => {
                ok += 1;
                assert_tightens(&parent, &e, &mut r);
            }
            Err(_) => refused += 1,
        }
    }
    assert!(ok > 40 && refused > 40, "generator must exercise both outcomes: {ok}/{refused}");
}

// ── fuzz: overlay and parent files (bounded, deterministic) ──────────────

fn mutate(r: &mut Rng, src: &str) -> Option<String> {
    let mut b = src.as_bytes().to_vec();
    for _ in 0..=r.below(4) {
        if b.is_empty() {
            break;
        }
        let i = r.below(b.len());
        match r.below(5) {
            0 => b[i] ^= 1 << r.below(8),
            1 => b[i] = b"\"[]{}=,.*\n #-0aZ"[r.below(16)],
            2 => {
                b.remove(i);
            }
            3 => b.insert(i, b"\"[]{}=,.*\n #-0aZ"[r.below(16)]),
            _ => b.truncate(i.max(1)),
        }
    }
    String::from_utf8(b).ok()
}

const VALID_OVERLAY: &str = r#"schema = 1
[[deny]]
id = "project.no-shell"
actions = ["tool.shell_exec", "workload.place*"]
reason = "never"
[[require_approval]]
id = "needs.ok"
actions = ["cron.add*"]
[limits]
risk_threshold = 0.5
max_processes = 32
spawn_budget = 4
human_approval_required = true
"#;

#[test]
fn fuzz_overlay_file_never_panics_and_never_loosens() {
    let mut r = Rng(0xDEAD_BEEF_CAFE_F00D);
    let parent = base_parent();
    let mut parsed = 0;
    for _ in 0..3000 {
        let Some(text) = mutate(&mut r, VALID_OVERLAY) else { continue };
        let Ok(o) = Overlay::from_toml(&text) else { continue };
        parsed += 1;
        if let Ok(e) = merge(&parent, &o) {
            assert_tightens(&parent, &e, &mut r);
        }
    }
    assert!(parsed > 20, "fuzz corpus too weak: {parsed}");
}

#[test]
fn fuzz_parent_file_only_the_signed_content_verifies() {
    let pk = user_key().verifying_key().to_bytes();
    let good = base_parent();
    let text = serde_json::to_string_pretty(&good).unwrap();
    let want = good.signed_bytes();
    let mut r = Rng(0x0123_4567_89AB_CDEF);
    let (mut parsed, mut verified) = (0, 0);
    for _ in 0..4000 {
        let Some(m) = mutate(&mut r, &text) else { continue };
        let Ok(p) = ParentPolicy::from_json(&m) else { continue };
        parsed += 1;
        if verify_parent_policy(&p, &pk).is_ok() {
            verified += 1;
            // Whatever still verifies must mean exactly what was signed.
            assert_eq!(p.signed_bytes(), want, "a changed policy verified: {m}");
        }
    }
    assert!(parsed > 20, "fuzz corpus too weak: {parsed}");
    // Whitespace-only edits survive; anything else must not.
    assert!(verified < parsed);
}

#[test]
fn oversized_and_non_regular_files_fail_closed_never_empty() {
    let t = tempfile::tempdir().unwrap();
    let big = t.path().join("big.toml");
    std::fs::write(&big, vec![b'#'; (crate::governance_overlay::MAX_FILE_BYTES + 1) as usize]).unwrap();
    assert!(matches!(
        crate::governance_overlay::load_overlay(&big),
        Err(OverlayError::TooLarge { .. })
    ));
    // A directory where the file should be is an error, not "no overlay".
    let dir = t.path().join("overlay.toml");
    std::fs::create_dir(&dir).unwrap();
    assert!(matches!(
        crate::governance_overlay::load_overlay(&dir),
        Err(OverlayError::Io { .. })
    ));
    // Not UTF-8.
    let bin = t.path().join("bin.toml");
    std::fs::write(&bin, [0xff, 0xfe, 0x00]).unwrap();
    assert!(crate::governance_overlay::load_overlay(&bin).is_err());
    // Missing is the empty overlay.
    let none = crate::governance_overlay::load_overlay(&t.path().join("nope.toml")).unwrap();
    assert_eq!(none, Overlay::empty());
}
