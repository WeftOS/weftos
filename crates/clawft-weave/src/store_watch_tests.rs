use std::io::Write;
use std::sync::Arc;

use serde_json::Value;
use weftos_store::audit::MemSink;
use weftos_store::service::{Config, DenialRule, Parties};
use weftos_store::{Access, ChainSink, ExposureGate, MemBackend, Principal, StoreName};

use super::*;

struct Open;
impl ExposureGate for Open {
    fn register(&self, _: &str, _: &str) {}
    fn check(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}
#[derive(Default)]
struct Chain(std::sync::Mutex<Vec<String>>);
impl ChainSink for Chain {
    fn record(&self, k: &str, _: Value) {
        self.0.lock().unwrap().push(k.to_owned());
    }
}

struct Rig {
    svc: Arc<StoreService>,
    be: Arc<MemBackend>,
    sink: Arc<MemSink>,
    chain: Arc<Chain>,
    store: StoreName,
    key: String,
}

fn rig() -> Rig {
    let be = Arc::new(MemBackend::new());
    let sink = Arc::new(MemSink::default());
    let chain = Arc::new(Chain::default());
    let svc = Arc::new(StoreService::open(Config { node_id: "pg-1".into(), state_path: None, clock: || 1_000 }, be.clone(), be.clone(), Arc::new(Open), sink.clone(), chain.clone()).unwrap());
    let store = StoreName::node("weft", "pg-1").unwrap();
    svc.create_store(&store, "daemon", Parties::default()).unwrap();
    svc.grant(&store, Principal::parse("cog:c1").unwrap(), Access::Write, "op").unwrap();
    let key = be.keys_of("cog:c1").pop().unwrap();
    Rig { svc, be, sink, chain, store, key }
}

fn denied(r: &Rig) -> String {
    format!(r#"{{"timestamp":"2026-11-07T06:36:23.965138209-05:00","level":"WARN","message":"request denied by authorization layer","event":"s3_authorization_denied","reason":"no_policy_allows_action","action":"S3Action(GetObjectAction)","bucket":"{}","object":"x","version_id":"None","account":"{}","is_owner":false,"target":"rustfs::storage::access"}}"#, r.store.bucket(), r.key)
}

fn append(p: &std::path::Path, s: &str) {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p).unwrap();
    f.write_all(s.as_bytes()).unwrap();
}

#[test]
fn the_tail_starts_at_the_end_reads_whole_lines_and_survives_truncation_and_rotation() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("rustfs.log");
    let mut t = LogTail::new(&p);
    let mut got = Vec::new();
    assert_eq!(t.poll(|l| got.push(l.to_owned())).unwrap(), 0, "no file yet is not an error");
    append(&p, "old1\nold2\n");
    assert_eq!(t.poll(|l| got.push(l.to_owned())).unwrap(), 0, "the first poll skips what was already there");
    // A line that has not ended is held back until it does.
    append(&p, "alpha\nbet");
    t.poll(|l| got.push(l.to_owned())).unwrap();
    assert_eq!(got, ["alpha"]);
    append(&p, "a\n\ngamma\n");
    t.poll(|l| got.push(l.to_owned())).unwrap();
    assert_eq!(got, ["alpha", "beta", "gamma"], "blank lines are dropped, the split line is whole");
    // Truncated in place: read again from the start.
    std::fs::write(&p, "after-truncate\n").unwrap();
    t.poll(|l| got.push(l.to_owned())).unwrap();
    assert_eq!(got.last().unwrap(), "after-truncate");
    // Rotated: the old file moved, a new one at the path. Detected by inode, which only Unix has
    // (elsewhere a replacement is noticed only if it is shorter than what was read).
    #[cfg(unix)]
    {
        std::fs::rename(&p, d.path().join("rustfs.log.1")).unwrap();
        append(&p, "new-file-line\n");
        t.poll(|l| got.push(l.to_owned())).unwrap();
        assert_eq!(got.last().unwrap(), "new-file-line");
    }
    // A huge line is skipped whole, and the next one is still read.
    append(&p, &format!("{}\nnext\n", "x".repeat(200_000)));
    t.poll(|l| got.push(l.to_owned())).unwrap();
    assert_eq!(got.last().unwrap(), "next");
    assert!(got.iter().all(|l| l.len() < 1000));
    let mut all = LogTail::from_start(&p);
    let mut n = 0;
    all.poll(|_| n += 1).unwrap();
    assert!(n >= 2, "from_start reads what is there");
}

#[test]
fn log_lines_reach_the_service_through_the_tail_and_a_run_of_them_suspends_the_key() {
    let r = rig();
    r.svc.set_denial_rule(Some(DenialRule { max: 3, window_ms: 60_000 }));
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("rustfs.log");
    append(&p, &format!("{}\n", denied(&r)));
    let mut t = LogTail::new(&p);
    assert_eq!(pump(&mut t, &r.svc).unwrap(), 0);
    let sig = r#"{"level":"ERROR","error":"S3Error { code: SignatureDoesNotMatch, message: \"x\", .. }","target":"s3s::ops"}"#;
    append(&p, &format!("{}\n{sig}\n{{\"level\":\"INFO\",\"message\":\"nothing\"}}\nnot json at all\n", denied(&r)));
    assert_eq!(pump(&mut t, &r.svc).unwrap(), 2, "the denial and the bad signature are records; the rest are not");
    let ev = r.sink.events();
    assert!(ev.iter().any(|e| e.op == "GetObject" && e.principal == "cog:c1"));
    assert!(ev.iter().any(|e| e.op == "Authenticate"), "the unattributed refusal is in the activity log too");
    assert_eq!(r.be.key_enabled(&r.key), Some(true), "one refusal is under the rule");
    append(&p, &format!("{}\n{}\n", denied(&r), denied(&r)));
    pump(&mut t, &r.svc).unwrap();
    assert_eq!(r.be.key_enabled(&r.key), Some(false), "the third disabled the key");
    assert_eq!(r.chain.0.lock().unwrap().iter().filter(|k| *k == "store.principal.suspend").count(), 1);
}

#[test]
fn the_spawned_tail_follows_a_file_by_itself() {
    let r = rig();
    r.svc.set_denial_rule(Some(DenialRule { max: 2, window_ms: 60_000 }));
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("rustfs.log");
    std::fs::write(&p, "").unwrap();
    spawn_tail(&r.svc, p.clone(), std::time::Duration::from_millis(20));
    std::thread::sleep(std::time::Duration::from_millis(80));
    append(&p, &format!("{}\n{}\n", denied(&r), denied(&r)));
    for _ in 0..100 {
        if r.be.key_enabled(&r.key) == Some(false) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(r.be.key_enabled(&r.key), Some(false), "the thread read the file and the rule fired");
}

#[test]
fn the_spawned_tail_marks_the_service_only_once_it_has_opened_the_file_and_not_if_the_file_is_missing() {
    let r = rig();
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("rustfs.log");
    assert!(!r.svc.log_tail_active());
    spawn_tail(&r.svc, p.clone(), std::time::Duration::from_millis(20));
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(!r.svc.log_tail_active(), "a file that does not exist is not a tail that reports denials");
    std::fs::write(&p, "").unwrap();
    for _ in 0..100 {
        if r.svc.log_tail_active() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(r.svc.log_tail_active(), "marked once the file was opened");
}

/// Rotation by rename or removal leaves no file for a moment: the tail is not reading anything,
/// so the audit stream's copy of a denial must count again until the file is back (finding M1).
#[test]
fn the_service_is_unmarked_while_the_log_file_is_gone_and_marked_again_when_it_returns() {
    let r = rig();
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("rustfs.log");
    std::fs::write(&p, "").unwrap();
    spawn_tail(&r.svc, p.clone(), std::time::Duration::from_millis(20));
    let wait = |want: bool| {
        for _ in 0..100 {
            if r.svc.log_tail_active() == want {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("flag never became {want}");
    };
    wait(true);
    std::fs::remove_file(&p).unwrap();
    wait(false);
    std::fs::write(&p, "").unwrap();
    wait(true);
}

#[test]
fn the_flag_is_cleared_when_the_tail_thread_ends_even_by_a_panic() {
    let r = rig();
    r.svc.set_log_tail_active(true);
    let weak = Arc::downgrade(&r.svc);
    let h = std::thread::spawn(move || {
        let _g = TailGuard(weak);
        panic!("tail died");
    });
    assert!(h.join().is_err());
    assert!(!r.svc.log_tail_active(), "an unwinding tail hands denials back to the audit stream");
}

/// `File::open` succeeds on a directory on unix; a directory is not a log, so nothing is being read.
#[test]
fn a_log_path_that_is_a_directory_never_marks_the_service() {
    let r = rig();
    let d = tempfile::tempdir().unwrap();
    let mut t = LogTail::new(d.path());
    assert_eq!(t.poll(|_| {}).unwrap(), 0);
    assert!(!t.opened(), "a directory is not a file being tailed");
    spawn_tail(&r.svc, d.path().to_path_buf(), std::time::Duration::from_millis(20));
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(!r.svc.log_tail_active());
}

/// A blocking `open` of a FIFO would park the tail thread before it could clear the flag.
#[cfg(unix)]
#[test]
fn a_live_tail_whose_path_becomes_a_fifo_clears_the_flag_instead_of_blocking_in_open() {
    let r = rig();
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("rustfs.log");
    std::fs::write(&p, "").unwrap();
    spawn_tail(&r.svc, p.clone(), std::time::Duration::from_millis(20));
    let wait = |want: bool| {
        for _ in 0..100 {
            if r.svc.log_tail_active() == want {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    };
    assert!(wait(true));
    // Swap the file for a FIFO in one rename, so the tail never sees the path absent (which would
    // clear the flag for the wrong reason).
    let fifo = d.path().join("rustfs.fifo");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
    std::fs::rename(&fifo, &p).unwrap();
    assert!(wait(false), "a FIFO is not a log file; the tail must not park in open with the flag still set");
}

#[test]
fn the_rule_comes_from_the_environment_as_on_by_default_off_or_a_count_and_window() {
    assert_eq!(denial_rule_from(None).unwrap(), Some(DenialRule::default()));
    assert_eq!(denial_rule_from(Some("")).unwrap(), Some(DenialRule { max: 20, window_ms: 60_000 }));
    assert_eq!(denial_rule_from(Some("off")).unwrap(), None);
    assert_eq!(denial_rule_from(Some(" 5/30 ")).unwrap(), Some(DenialRule { max: 5, window_ms: 30_000 }));
    for bad in ["5", "0/10", "5/0", "x/y", "5/30/1", "-1/5"] {
        assert!(denial_rule_from(Some(bad)).is_err(), "{bad}");
    }
}
