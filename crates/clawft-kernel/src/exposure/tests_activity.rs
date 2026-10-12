use super::activity::{ActivityLog, GENESIS, NewActivity, Retention};

fn act(exp: &str, node: &str, outcome: &str) -> NewActivity {
    NewActivity {
        exposure: exp.into(),
        caller_node: node.into(),
        caller_user: Some("mat".into()),
        bytes_in: 10,
        bytes_out: 20,
        duration_ms: 5,
        outcome: outcome.into(),
    }
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("expose-act-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn records_chain_and_verify() {
    let log = ActivityLog::memory(Retention::default());
    let a = log.append(act("infer.chat", "n-peer", "ok"), 1_000);
    let b = log.append(act("infer.chat", "n-peer", "refused:off"), 2_000);
    assert_eq!(a.prev, GENESIS);
    assert_eq!(b.prev, a.hash);
    assert!(log.verify().is_ok());
    assert_eq!(log.query(Some("infer.chat"), 10).len(), 2);
    assert_eq!(log.query(Some("other"), 10).len(), 0);
}

#[test]
fn record_carries_no_body_field_and_strips_control_chars() {
    let log = ActivityLog::memory(Retention::default());
    let r = log.append(act("infer.chat", "n-peer", "error:bad\nbody {\"secret\":1}"), 1);
    assert!(!r.outcome.contains('\n'));
    let json = serde_json::to_value(&r).unwrap();
    assert!(json.get("body").is_none());
}

#[test]
fn tampering_breaks_the_chain_on_open() {
    let d = tmpdir("tamper");
    let p = d.join("a.jsonl");
    {
        let log = ActivityLog::open(&p, Retention::default(), 1).unwrap();
        log.append(act("infer.chat", "n-a", "ok"), 10);
        log.append(act("infer.chat", "n-a", "ok"), 20);
    }
    let text = std::fs::read_to_string(&p).unwrap().replace("\"bytes_out\":20", "\"bytes_out\":21");
    std::fs::write(&p, text).unwrap();
    let err = ActivityLog::open(&p, Retention::default(), 30).err().expect("refuses a broken chain");
    assert!(err.contains("chain broken"), "{err}");
}

#[test]
fn removed_line_is_detected() {
    let d = tmpdir("removed");
    let p = d.join("a.jsonl");
    {
        let log = ActivityLog::open(&p, Retention::default(), 1).unwrap();
        for t in [10, 20, 30] {
            log.append(act("infer.chat", "n-a", "ok"), t);
        }
    }
    let text = std::fs::read_to_string(&p).unwrap();
    let kept: Vec<&str> = text.lines().enumerate().filter(|(i, _)| *i != 1).map(|(_, l)| l).collect();
    std::fs::write(&p, kept.join("\n")).unwrap();
    assert!(ActivityLog::open(&p, Retention::default(), 40).is_err());
}

#[test]
fn reopen_continues_the_chain() {
    let d = tmpdir("reopen");
    let p = d.join("a.jsonl");
    let first = {
        let log = ActivityLog::open(&p, Retention::default(), 1).unwrap();
        log.append(act("infer.chat", "n-a", "ok"), 10)
    };
    let log = ActivityLog::open(&p, Retention::default(), 20).unwrap();
    let second = log.append(act("infer.chat", "n-a", "ok"), 30);
    assert_eq!(second.prev, first.hash);
    assert_eq!(second.seq, 1);
    assert!(log.verify().is_ok());
}

#[test]
fn retention_by_age_keeps_a_verifiable_chain() {
    let d = tmpdir("age");
    let p = d.join("a.jsonl");
    let ret = Retention { max_age_ms: 1_000, max_bytes: u64::MAX };
    let log = ActivityLog::open(&p, ret, 0).unwrap();
    log.append(act("infer.chat", "n-a", "ok"), 100);
    log.append(act("infer.chat", "n-a", "ok"), 200);
    log.append(act("infer.chat", "n-a", "ok"), 5_000);
    assert_eq!(log.len(), 1, "the two old records are gone");
    assert!(log.verify().is_ok());
    drop(log);
    let again = ActivityLog::open(&p, ret, 5_100).unwrap();
    assert_eq!(again.len(), 1);
    assert!(again.verify().is_ok());
}

#[test]
fn retention_by_size_drops_oldest_but_keeps_the_newest() {
    let ret = Retention { max_age_ms: u64::MAX, max_bytes: 700 };
    let log = ActivityLog::memory(ret);
    for t in 0..20 {
        log.append(act("infer.chat", "n-a", "ok"), t);
    }
    assert!(log.len() < 20 && !log.is_empty());
    assert!(log.verify().is_ok());
    let tiny = ActivityLog::memory(Retention { max_age_ms: u64::MAX, max_bytes: 1 });
    tiny.append(act("infer.chat", "n-a", "ok"), 1);
    assert_eq!(tiny.len(), 1, "the newest record is never dropped for size alone");
}

#[test]
fn default_retention_is_fourteen_days_or_fifty_megabytes() {
    let r = Retention::default();
    assert_eq!(r.max_age_ms, 14 * 24 * 3600 * 1000);
    assert_eq!(r.max_bytes, 50 * 1024 * 1024);
}

#[test]
fn counts_use_the_window_and_remember_last_use() {
    let log = ActivityLog::memory(Retention::default());
    log.append(act("infer.chat", "n-a", "ok"), 1_000);
    log.append(act("infer.chat", "n-a", "refused:off"), 90_000_000);
    let c = log.counts(90_000_000, 3_600_000);
    let c = &c["infer.chat"];
    assert_eq!((c.total, c.failed, c.last_ts_ms), (1, 1, Some(90_000_000)));
    let all = log.counts(90_000_000, u64::MAX);
    assert_eq!(all["infer.chat"].total, 2);
}
