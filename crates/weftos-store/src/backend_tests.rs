use std::io::Read;
use std::sync::Mutex;

use super::*;
use crate::mem::MemBackend;

const MIB: usize = 1024 * 1024;

/// A backend that only records what the streaming upload asks of it.
#[derive(Default)]
struct Rec {
    calls: Mutex<Vec<String>>,
    fail_part: Option<u32>,
}
impl Rec {
    fn log(&self, s: String) {
        self.calls.lock().unwrap().push(s);
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}
impl ObjectBackend for Rec {
    fn put(&self, _: &str, k: &str, d: &[u8], _: bool) -> Result<ObjectMeta, BackendError> {
        self.log(format!("put {}", d.len()));
        Ok(ObjectMeta { key: k.into(), size: d.len() as u64, etag: "e".into() })
    }
    fn put_if_match(&self, _: &str, _: &str, _: &[u8], _: &str) -> Result<ObjectMeta, BackendError> {
        unreachable!()
    }
    fn put_locked(&self, _: &str, k: &str, d: &[u8], _: &PutOpts) -> Result<LockedPut, BackendError> {
        self.log(format!("put_locked {}", d.len()));
        Ok(LockedPut { meta: ObjectMeta { key: k.into(), size: d.len() as u64, etag: "e".into() }, version_id: Some("v".into()) })
    }
    fn create_multipart(&self, _: &str, _: &str, o: &PutOpts) -> Result<String, BackendError> {
        self.log(format!("create retention={}", o.retention.is_some()));
        Ok("up1".into())
    }
    fn upload_part(&self, _: &str, _: &str, id: &str, n: u32, d: &[u8]) -> Result<String, BackendError> {
        assert_eq!(id, "up1");
        if self.fail_part == Some(n) {
            return Err(BackendError::Unavailable("down".into()));
        }
        self.log(format!("part {n} {}", d.len()));
        Ok(format!("etag{n}"))
    }
    fn complete_multipart(&self, _: &str, k: &str, _: &str, parts: &[Part], _: &PutOpts) -> Result<LockedPut, BackendError> {
        self.log(format!("complete {}", parts.iter().map(|p| p.etag.clone()).collect::<Vec<_>>().join(",")));
        Ok(LockedPut { meta: ObjectMeta { key: k.into(), size: 0, etag: "m".into() }, version_id: Some("v".into()) })
    }
    fn abort_multipart(&self, _: &str, _: &str, _: &str) -> Result<(), BackendError> {
        self.log("abort".into());
        Ok(())
    }
    fn get(&self, _: &str, _: &str) -> Result<Vec<u8>, BackendError> {
        unreachable!()
    }
    fn head(&self, _: &str, _: &str) -> Result<ObjectMeta, BackendError> {
        unreachable!()
    }
    fn delete(&self, _: &str, _: &str) -> Result<(), BackendError> {
        unreachable!()
    }
    fn copy(&self, _: &str, _: &str, _: &str, _: &str) -> Result<ObjectMeta, BackendError> {
        unreachable!()
    }
    fn list(&self, _: &str, _: &str) -> Result<Vec<ObjectMeta>, BackendError> {
        unreachable!()
    }
}

/// `n` bytes of a repeating pattern, produced as they are read (never all at once).
fn bytes(n: usize) -> impl Read {
    std::io::repeat(7).take(n as u64)
}

#[test]
fn a_large_stream_goes_up_in_parts_and_only_the_last_is_short() {
    let b = Rec::default();
    let r = b.put_stream("b", "k", &mut bytes(11 * MIB), 5 * MIB, &PutOpts::default()).unwrap();
    assert_eq!(b.calls(), vec!["create retention=false", "part 1 5242880", "part 2 5242880", "part 3 1048576", "complete etag1,etag2,etag3"]);
    assert_eq!(r.meta.size, 11 * MIB as u64, "the size is the total, not the last part");
    assert_eq!(r.version_id.as_deref(), Some("v"));
}

#[test]
fn an_exact_multiple_has_no_empty_last_part_and_a_small_part_size_is_raised_to_the_minimum() {
    let b = Rec::default();
    b.put_stream("b", "k", &mut bytes(10 * MIB), 1, &PutOpts::default()).unwrap();
    assert_eq!(b.calls(), vec!["create retention=false", "part 1 5242880", "part 2 5242880", "complete etag1,etag2"]);
}

#[test]
fn an_object_under_one_part_is_one_request_with_the_lock_options() {
    let b = Rec::default();
    b.put_stream("b", "k", &mut bytes(1000), 5 * MIB, &PutOpts::default()).unwrap();
    b.put_stream("b", "k", &mut bytes(0), 5 * MIB, &PutOpts::default()).unwrap();
    assert_eq!(b.calls(), vec!["put_locked 1000", "put_locked 0"]);
}

#[test]
fn a_failed_part_aborts_the_upload_and_nothing_is_completed() {
    let b = Rec { fail_part: Some(2), ..Default::default() };
    let e = b.put_stream("b", "k", &mut bytes(11 * MIB), 5 * MIB, &PutOpts::default()).unwrap_err();
    assert!(e.is_transient());
    let c = b.calls();
    assert_eq!(c.last().unwrap(), "abort");
    assert!(!c.iter().any(|x| x.starts_with("complete")), "{c:?}");
}

#[test]
fn a_reader_that_fails_midway_aborts_the_upload() {
    struct Dies(usize);
    impl Read for Dies {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0 == 0 {
                return Err(std::io::Error::other("pipe closed"));
            }
            let n = buf.len().min(self.0);
            buf[..n].fill(1);
            self.0 -= n;
            Ok(n)
        }
    }
    let b = Rec::default();
    assert!(b.put_stream("b", "k", &mut Dies(6 * MIB), 5 * MIB, &PutOpts::default()).is_err());
    assert_eq!(b.calls().last().unwrap(), "abort");
}

#[test]
fn local_backends_read_it_whole_put_it_once_and_refuse_lock_options() {
    let m = MemBackend::new();
    AdminBackend::create_bucket(&m, "b", &[]).unwrap();
    let r = m.put_stream("b", "small", &mut bytes(10), 5 * MIB, &PutOpts::default()).unwrap();
    assert_eq!((r.meta.size, r.version_id), (10, None));
    m.put_stream("b", "big", &mut bytes(6 * MIB), 5 * MIB, &PutOpts { if_none_match: true, ..Default::default() }).unwrap();
    assert_eq!(m.get("b", "big").unwrap().len(), 6 * MIB);
    assert!(matches!(m.put_stream("b", "big", &mut bytes(6 * MIB), 5 * MIB, &PutOpts { if_none_match: true, ..Default::default() }), Err(BackendError::Exists(_))), "if_none_match still holds");
    let lock = PutOpts { retention: Some(Retention { mode: LockMode::Compliance, until_unix_s: 4_000_000_000 }), ..Default::default() };
    assert!(matches!(m.put_stream("b", "l", &mut bytes(6 * MIB), 5 * MIB, &lock), Err(BackendError::Unsupported(_))));
    assert!(matches!(m.put_stream("b", "l", &mut bytes(10), 5 * MIB, &lock), Err(BackendError::Unsupported(_))));
}

/// Only the retention calls: what a version holds now, and what was asked to change.
struct Held {
    now: Mutex<Option<Retention>>,
    puts: Mutex<Vec<Retention>>,
}
impl Held {
    fn with(r: Option<Retention>) -> Self {
        Self { now: Mutex::new(r), puts: Mutex::default() }
    }
}
impl ObjectBackend for Held {
    fn get_retention(&self, _: &str, _: &str, _: &str) -> Result<Option<Retention>, BackendError> {
        Ok(*self.now.lock().unwrap())
    }
    fn put_retention(&self, _: &str, _: &str, _: &str, r: &Retention, bypass: bool) -> Result<(), BackendError> {
        assert!(!bypass, "extending never asks to bypass governance");
        self.puts.lock().unwrap().push(*r);
        *self.now.lock().unwrap() = Some(*r);
        Ok(())
    }
    fn put(&self, _: &str, _: &str, _: &[u8], _: bool) -> Result<ObjectMeta, BackendError> {
        unreachable!()
    }
    fn put_if_match(&self, _: &str, _: &str, _: &[u8], _: &str) -> Result<ObjectMeta, BackendError> {
        unreachable!()
    }
    fn get(&self, _: &str, _: &str) -> Result<Vec<u8>, BackendError> {
        unreachable!()
    }
    fn head(&self, _: &str, _: &str) -> Result<ObjectMeta, BackendError> {
        unreachable!()
    }
    fn delete(&self, _: &str, _: &str) -> Result<(), BackendError> {
        unreachable!()
    }
    fn copy(&self, _: &str, _: &str, _: &str, _: &str) -> Result<ObjectMeta, BackendError> {
        unreachable!()
    }
    fn list(&self, _: &str, _: &str) -> Result<Vec<ObjectMeta>, BackendError> {
        unreachable!()
    }
}

#[test]
fn extend_retention_only_ever_lengthens_and_keeps_the_current_mode() {
    let comp = |t| Retention { mode: LockMode::Compliance, until_unix_s: t };
    let b = Held::with(Some(comp(1000)));
    // Equal and shorter are refused, and nothing is sent.
    assert!(matches!(b.extend_retention("b", "k", "v", 1000, LockMode::Governance), Err(BackendError::BadKey(_))), "equal is not an extension");
    assert!(matches!(b.extend_retention("b", "k", "v", 999, LockMode::Governance), Err(BackendError::BadKey(_))));
    assert!(matches!(b.extend_retention("b", "k", "v", 0, LockMode::Governance), Err(BackendError::BadKey(_))));
    assert!(b.puts.lock().unwrap().is_empty(), "a refused extension sends nothing");
    // One second longer is enough; the mode stays Compliance whatever the caller names.
    assert_eq!(b.extend_retention("b", "k", "v", 1001, LockMode::Governance).unwrap(), comp(1001));
    assert_eq!(*b.puts.lock().unwrap(), vec![comp(1001)]);
    // An object with no retention takes the caller's mode and any date.
    let none = Held::with(None);
    assert_eq!(none.extend_retention("b", "k", "v", 5, LockMode::Governance).unwrap(), Retention { mode: LockMode::Governance, until_unix_s: 5 });
}

#[test]
fn a_key_segment_over_255_bytes_is_refused_wherever_it_sits_and_255_is_fine() {
    let seg = |n: usize| "a".repeat(n);
    assert!(check_key(&seg(255)).is_ok());
    assert!(matches!(check_key(&seg(256)), Err(BackendError::BadKey(_))), "256 bytes in one segment");
    for k in [format!("{}/x", seg(256)), format!("x/{}/y", seg(256)), format!("x/{}", seg(256)), format!("{}/{}", seg(255), seg(256))] {
        assert!(matches!(check_key(&k), Err(BackendError::BadKey(_))), "{} bytes", k.len());
    }
    // Several full segments are fine while the total stays inside 512; the limit is per segment.
    assert!(check_key(&format!("{}/{}", seg(255), seg(256 - 2))).is_ok());
    // Bytes, not characters: 128 two-byte characters are 256 bytes.
    assert!(check_key(&"é".repeat(127)).is_ok());
    assert!(matches!(check_key(&"é".repeat(128)), Err(BackendError::BadKey(_))));
    // The old total rule still holds.
    assert!(matches!(check_key(&format!("{}/{}/{}", seg(200), seg(200), seg(113))), Err(BackendError::BadKey(_))), "513 bytes");
    assert!(check_key(&format!("{}/{}/{}", seg(200), seg(200), seg(110))).is_ok(), "512 bytes");
}

#[test]
fn check_key_agrees_with_its_rules_over_random_keys_and_a_backend_refuses_the_same() {
    let mut r = crate::testrng::Rng::new(0x5E6_1E57);
    let mem = MemBackend::new();
    crate::backend::AdminBackend::create_bucket(&mem, "b", &[]).unwrap();
    for i in 0..30_000 {
        let nseg = r.range(1, 4) as usize;
        let segs: Vec<String> = (0..nseg)
            .map(|_| {
                let n = match r.below(5) {
                    0 => 255,
                    1 => 256,
                    2 => r.range(250, 260),
                    _ => r.range(1, 40),
                } as usize;
                if r.chance(8) { "é".repeat(n / 2 + 1) } else { "k".repeat(n) }
            })
            .collect();
        let key = segs.join("/");
        let want = key.len() <= 512 && segs.iter().all(|s| s.len() <= 255);
        assert_eq!(check_key(&key).is_ok(), want, "case {i}: segments {:?} total {}", segs.iter().map(String::len).collect::<Vec<_>>(), key.len());
        if segs.iter().any(|s| s.len() > 255) {
            assert!(matches!(mem.put("b", &key, b"x", false), Err(BackendError::BadKey(_))), "case {i}: the backend takes a key the check refuses");
        }
    }
}

// ---- conditional write and delete contract (from the Mac drive branch, S1 error variants) ----

/// The contract every backend with a conditional write must meet.
fn conditional_contract<B: ObjectBackend + AdminBackend>(b: &B) {
    b.create_bucket("b-cond", &[]).unwrap();
    let v1 = b.put("b-cond", "k", b"one", false).unwrap();
    // Right etag: written, new etag returned.
    let v2 = b.put_if_match("b-cond", "k", b"two", &v1.etag).unwrap();
    assert_eq!(v2.etag, etag_of(b"two"));
    assert_eq!(b.get("b-cond", "k").unwrap(), b"two");
    // Stale etag: refused, object untouched.
    assert!(matches!(b.put_if_match("b-cond", "k", b"three", &v1.etag), Err(BackendError::Stale(_))));
    assert_eq!(b.get("b-cond", "k").unwrap(), b"two");
    // Missing key: NoObject, nothing created.
    assert!(matches!(b.put_if_match("b-cond", "gone", b"x", &v1.etag), Err(BackendError::NoObject(_))));
    assert!(b.get("b-cond", "gone").is_err());
    // Conditional delete: stale refused and untouched, right one deletes.
    assert!(matches!(b.delete_if_match("b-cond", "k", &v1.etag), Err(BackendError::Stale(_))));
    assert_eq!(b.get("b-cond", "k").unwrap(), b"two");
    b.delete_if_match("b-cond", "k", &v2.etag).unwrap();
    assert!(matches!(b.get("b-cond", "k"), Err(BackendError::NoObject(_))));
    assert!(matches!(b.delete_if_match("b-cond", "k", &v2.etag), Err(BackendError::NoObject(_))));
}

#[test]
fn mem_backend_meets_the_conditional_contract() {
    conditional_contract(&crate::mem::MemBackend::new());
}

#[test]
fn fs_backend_meets_the_conditional_contract() {
    let d = tempfile::tempdir().unwrap();
    conditional_contract(&crate::fs::FsBackend::open(d.path()).unwrap());
}

#[test]
fn a_backend_with_no_conditional_delete_says_unsupported() {
    struct Bare;
    impl ObjectBackend for Bare {
        fn put(&self, _: &str, k: &str, d: &[u8], _: bool) -> Result<ObjectMeta, BackendError> {
            Ok(ObjectMeta { key: k.into(), size: d.len() as u64, etag: etag_of(d) })
        }
        fn put_if_match(&self, _: &str, _: &str, _: &[u8], _: &str) -> Result<ObjectMeta, BackendError> {
            Err(BackendError::Unsupported("bare".into()))
        }
        fn get(&self, _: &str, _: &str) -> Result<Vec<u8>, BackendError> { Err(BackendError::Unsupported("bare".into())) }
        fn head(&self, _: &str, _: &str) -> Result<ObjectMeta, BackendError> { Err(BackendError::Unsupported("bare".into())) }
        fn delete(&self, _: &str, _: &str) -> Result<(), BackendError> { Ok(()) }
        fn copy(&self, _: &str, _: &str, _: &str, _: &str) -> Result<ObjectMeta, BackendError> { Err(BackendError::Unsupported("bare".into())) }
        fn list(&self, _: &str, _: &str) -> Result<Vec<ObjectMeta>, BackendError> { Ok(vec![]) }
    }
    assert!(matches!(Bare.delete_if_match("b", "k", "e"), Err(BackendError::Unsupported(_))));
}
