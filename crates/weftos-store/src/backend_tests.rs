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
