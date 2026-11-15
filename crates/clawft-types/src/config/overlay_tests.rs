use super::overlay::*;

fn parent() -> ParentView {
    ParentView {
        rule_ids: vec!["gov.base".into(), "Gov.Other".into()],
        deny_actions: vec!["tool.rm*".into()],
        require_approval_actions: vec!["workload.stop".into()],
        limits: Limits {
            risk_threshold: Some(0.7),
            max_processes: Some(64),
            spawn_budget: Some(8),
            human_approval_required: Some(false),
        },
    }
}

fn parse(s: &str) -> OverlayFile {
    OverlayFile::from_toml(s).unwrap()
}

fn err(s: &str) -> OverlayError {
    merge(&parent(), &parse(s)).unwrap_err()
}

const PLAN_EXAMPLE: &str = r#"
schema = 1
[[deny]]
id = "project.no-shell"
actions = ["tool.shell_exec", "workload.place*"]
reason = "this project never runs shell tools"
[[require_approval]]
actions = ["workload.start*"]
[limits]
risk_threshold = 0.5
max_processes = 32
spawn_budget = 4
human_approval_required = true
"#;

#[test]
fn plan_example_merges() {
    let e = merge(&parent(), &parse(PLAN_EXAMPLE)).unwrap();
    assert_eq!(
        e.deny_actions,
        ["tool.rm*", "tool.shell_exec", "workload.place*"]
    );
    assert_eq!(e.require_approval_actions, ["workload.start*", "workload.stop"]);
    assert_eq!(e.deny_rules.len(), 1);
    assert_eq!(e.approval_rules.len(), 1);
    assert_eq!(
        e.limits,
        Limits {
            risk_threshold: Some(0.5),
            max_processes: Some(32),
            spawn_budget: Some(4),
            human_approval_required: Some(true),
        }
    );
}

#[test]
fn empty_overlay_is_the_parent() {
    let e = merge(&parent(), &OverlayFile::default()).unwrap();
    assert_eq!(e.deny_actions, ["tool.rm*"]);
    assert_eq!(e.limits, parent().limits);
    assert!(e.deny_rules.is_empty());
    // And an empty file parses to the default.
    assert_eq!(parse(""), OverlayFile::default());
}

#[test]
fn equal_limits_are_not_a_relaxation() {
    let e = merge(
        &parent(),
        &parse("[limits]\nrisk_threshold = 0.7\nmax_processes = 64\nspawn_budget = 8\nhuman_approval_required = false"),
    )
    .unwrap();
    assert_eq!(e.limits, parent().limits);
}

#[test]
fn limits_apply_when_parent_is_unlimited() {
    let p = ParentView::default();
    let o = parse("[limits]\nmax_processes = 5\nhuman_approval_required = false");
    let e = merge(&p, &o).unwrap();
    assert_eq!(e.limits.max_processes, Some(5));
    assert_eq!(e.limits.risk_threshold, None);
    assert_eq!(e.limits.human_approval_required, Some(false));
}

#[test]
fn relaxation_errors_name_the_key() {
    let cases: &[(&str, &str)] = &[
        ("[limits]\nrisk_threshold = 0.9", "limits.risk_threshold"),
        ("[limits]\nmax_processes = 65", "limits.max_processes"),
        ("[limits]\nspawn_budget = 9", "limits.spawn_budget"),
    ];
    for (src, key) in cases {
        match err(src) {
            OverlayError::Relaxes { key: k, .. } => assert_eq!(&k, key),
            other => panic!("{src}: {other:?}"),
        }
    }
    // A parent with the flag on cannot be switched off.
    let mut p = parent();
    p.limits.human_approval_required = Some(true);
    let e = merge(&p, &parse("[limits]\nhuman_approval_required = false")).unwrap_err();
    assert!(matches!(e, OverlayError::Relaxes { ref key, .. } if key == "limits.human_approval_required"));
}

#[test]
fn invalid_risk_threshold_is_refused() {
    for v in ["-0.1", "1.5", "nan", "inf"] {
        let e = err(&format!("[limits]\nrisk_threshold = {v}"));
        assert!(matches!(e, OverlayError::InvalidLimit { .. }), "{v}: {e:?}");
    }
}

#[test]
fn permit_and_deactivate_are_forbidden() {
    assert_eq!(
        err("permit = [\"x\"]"),
        OverlayError::Forbidden("permit".into())
    );
    assert_eq!(
        err("deactivate = [\"gov.base\"]"),
        OverlayError::Forbidden("deactivate".into())
    );
    assert_eq!(err("bogus = 1"), OverlayError::UnknownKey("bogus".into()));
}

#[test]
fn nested_unknown_keys_are_parse_errors() {
    for src in [
        "[[deny]]\nid = \"a\"\nactions = [\"x\"]\npermit = true",
        "[limits]\nmystery = 1",
        "[[require_approval]]\nactions = [\"x\"]\ndeactivate = true",
    ] {
        assert!(matches!(
            OverlayFile::from_toml(src),
            Err(OverlayError::Parse(_))
        ), "{src}");
    }
}

#[test]
fn schema_must_match() {
    assert_eq!(err("schema = 2"), OverlayError::Schema(2));
}

#[test]
fn shadowing_a_parent_rule_id_is_refused_in_every_spelling() {
    for id in ["gov.base", "GOV.BASE", "gov.other"] {
        let e = err(&format!("[[deny]]\nid = \"{id}\"\nactions = [\"x\"]"));
        assert!(
            matches!(&e, OverlayError::ShadowsParent { key, .. } if key == "deny[0].id"),
            "{id}: {e:?}"
        );
    }
    // Padded ids are malformed, not a way around the check.
    let e = err("[[deny]]\nid = \" gov.base\"\nactions = [\"x\"]");
    assert!(matches!(e, OverlayError::MalformedId { .. }));
    // An approval entry's optional id is checked too.
    let e = err("[[require_approval]]\nid = \"gov.base\"\nactions = [\"x\"]");
    assert!(matches!(&e, OverlayError::ShadowsParent { key, .. } if key == "require_approval[0].id"));
}

#[test]
fn malformed_ids_and_duplicates() {
    for id in ["", "a b", "a/b", "é"] {
        let e = err(&format!("[[deny]]\nid = \"{id}\"\nactions = [\"x\"]"));
        assert!(matches!(e, OverlayError::MalformedId { .. }), "{id:?}");
    }
    let e = err("[[deny]]\nid = \"a\"\nactions = [\"x\"]\n[[deny]]\nid = \"A\"\nactions = [\"y\"]");
    assert!(matches!(&e, OverlayError::DuplicateId { key, .. } if key == "deny[1].id"));
}

#[test]
fn malformed_globs_are_refused_with_the_index() {
    for (pat, idx) in [("a*b", 1), ("**", 1), ("", 1), ("a b", 1), ("*a", 1), ("a**", 1)] {
        let src = format!("[[deny]]\nid = \"d\"\nactions = [\"ok\", \"{pat}\"]");
        match err(&src) {
            OverlayError::MalformedGlob { key, pattern } => {
                assert_eq!(key, format!("deny[0].actions[{idx}]"));
                assert_eq!(pattern, pat);
            }
            other => panic!("{pat:?}: {other:?}"),
        }
    }
    let e = err("[[require_approval]]\nactions = []");
    assert!(matches!(&e, OverlayError::EmptyActions { key } if key == "require_approval[0].actions"));
}

#[test]
fn glob_semantics() {
    assert!(valid_glob("a.b") && valid_glob("a.*") && valid_glob("*"));
    assert!(glob_matches("workload.place*", "workload.place"));
    assert!(glob_matches("workload.place*", "workload.placement.x"));
    assert!(!glob_matches("workload.place", "workload.placement"));
    assert!(glob_matches("*", "anything"));
}

#[test]
fn union_dedupes_and_sorts() {
    let o = parse("[[deny]]\nid = \"a\"\nactions = [\"tool.rm*\", \"z\", \"z\"]");
    let e = merge(&parent(), &o).unwrap();
    assert_eq!(e.deny_actions, ["tool.rm*", "z"]);
}

// ---- property-style: random parents and overlays, deterministic seed ----

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn some(&mut self) -> bool {
        self.below(3) != 0
    }
}

fn action(r: &mut Rng) -> String {
    let base = ["tool.a", "tool.b", "workload.place", "workload.start", "x"];
    let a = base[r.below(5) as usize];
    if r.below(2) == 0 { format!("{a}*") } else { a.to_owned() }
}

#[test]
fn property_merge_only_tightens() {
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut ok, mut rejected) = (0u32, 0u32);
    for case in 0..4000 {
        let pl = |r: &mut Rng| {
            let pick = |r: &mut Rng, v: u64| r.some().then_some(v);
            Limits {
                risk_threshold: pick(r, 101).map(|_| r.below(101) as f64 / 100.0),
                max_processes: pick(r, 100).map(|_| r.below(100)),
                spawn_budget: pick(r, 20).map(|_| r.below(20)),
                human_approval_required: pick(r, 2).map(|_| r.below(2) == 0),
            }
        };
        let parent = ParentView {
            rule_ids: (0..r.below(4)).map(|i| format!("p{i}")).collect(),
            deny_actions: (0..r.below(4)).map(|_| action(&mut r)).collect(),
            require_approval_actions: (0..r.below(3)).map(|_| action(&mut r)).collect(),
            limits: pl(&mut r),
        };
        let overlay = OverlayFile {
            deny: (0..r.below(4))
                .map(|i| OverlayDeny {
                    id: format!("o{i}"),
                    actions: (0..=r.below(3)).map(|_| action(&mut r)).collect(),
                    reason: None,
                    principal: None,
                })
                .collect(),
            require_approval: (0..r.below(3))
                .map(|_| OverlayApproval {
                    id: None,
                    actions: vec![action(&mut r)],
                    reason: None,
                    principal: None,
                })
                .collect(),
            limits: pl(&mut r),
            ..OverlayFile::default()
        };
        let Ok(e) = merge(&parent, &overlay) else {
            rejected += 1;
            continue;
        };
        ok += 1;
        for d in &parent.deny_actions {
            assert!(e.deny_actions.contains(d), "case {case}: lost parent deny {d}");
        }
        for d in &parent.require_approval_actions {
            assert!(e.require_approval_actions.contains(d), "case {case}");
        }
        for d in overlay.deny.iter().flat_map(|d| &d.actions) {
            assert!(e.deny_actions.contains(d), "case {case}: lost overlay deny {d}");
        }
        let (p, l) = (&parent.limits, &e.limits);
        if let Some(pv) = p.risk_threshold {
            assert!(l.risk_threshold.unwrap() <= pv, "case {case}");
        }
        if let Some(pv) = p.max_processes {
            assert!(l.max_processes.unwrap() <= pv, "case {case}");
        }
        if let Some(pv) = p.spawn_budget {
            assert!(l.spawn_budget.unwrap() <= pv, "case {case}");
        }
        if p.human_approval_required == Some(true) {
            assert_eq!(l.human_approval_required, Some(true), "case {case}");
        }
        // The overlay's own values are what they asked for, never looser.
        if let (Some(o), Some(e)) = (overlay.limits.max_processes, l.max_processes) {
            assert!(e <= o);
        }
    }
    assert!(ok > 200 && rejected > 200, "weak coverage: {ok} ok, {rejected} rejected");
}

#[test]
fn invalid_parent_limits_are_refused() {
    for v in [f64::NAN, -0.5, f64::INFINITY, f64::NEG_INFINITY, 1.5] {
        let mut p = parent();
        p.limits.risk_threshold = Some(v);
        let e = merge(&p, &OverlayFile::default()).unwrap_err();
        assert!(
            matches!(&e, OverlayError::InvalidLimit { key } if key == "parent.limits.risk_threshold"),
            "{v}: {e:?}"
        );
    }
}
