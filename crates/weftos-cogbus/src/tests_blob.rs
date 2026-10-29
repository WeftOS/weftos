//! The content store: handles, TTL, quota, access, restart.

use super::*;

const P1: &str = "01J9ZXW0PRJCTAAAAAAAAAAAAA";
const P2: &str = "01J9ZXW0PRJCTBBBBBBBBBBBBB";

fn cfg(quota: u64) -> BlobConfig {
    BlobConfig { quota_bytes: quota, per_instance_bytes: quota, max_blob_bytes: quota, ..Default::default() }
}

fn put(s: &BlobStore, now: u64, owner: &str, project: Option<&str>, bytes: &[u8], ttl: Option<u64>) -> Result<BlobHandle, BlobError> {
    let up = s.begin(now, owner, project, "image/png", bytes.len() as u64, ttl)?;
    let mut off = 0;
    let mut out = None;
    let mut it = bytes.chunks(1000).peekable();
    while let Some(c) = it.next() {
        out = s.chunk(now, owner, up, off, c, it.peek().is_none())?;
        off += c.len() as u64;
    }
    Ok(out.expect("a handle on the last chunk"))
}

fn read_all(s: &BlobStore, now: u64, who: &str, project: Option<&str>, h: &BlobHandle) -> Result<Vec<u8>, BlobError> {
    let mut out = Vec::new();
    loop {
        let (_, b, eof) = s.get(now, who, project, &h.hash, out.len() as u64, CHUNK)?;
        out.extend(b);
        if eof {
            return Ok(out);
        }
    }
}

#[test]
fn a_blob_round_trips_by_its_blake3_handle_and_expires_on_ttl() {
    let tmp = tempfile::tempdir().unwrap();
    let s = BlobStore::open(tmp.path(), BlobConfig::default(), 0).unwrap();
    let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    let h = put(&s, 1_000, "cam", Some(P1), &data, Some(5_000)).unwrap();
    assert_eq!(h.hash, blake3::hash(&data).to_hex().to_string());
    assert_eq!((h.size, h.media_type.as_str(), h.expires_ms), (100_000, "image/png", 6_000));
    h.validate_for_test();
    assert_eq!(read_all(&s, 5_999, "cam", Some(P1), &h).unwrap(), data, "resolves until the last millisecond");
    assert!(matches!(s.get(6_000, "cam", Some(P1), &h.hash, 0, 10), Err(BlobError::NotFound(_))), "expired at its TTL");
    assert!(!blob_path(tmp.path(), &h.hash).exists() && !meta_path(tmp.path(), &h.hash).exists(), "files removed");
    assert_eq!(s.stats().blobs, 0);
}

trait Check {
    fn validate_for_test(&self);
}
impl Check for BlobHandle {
    fn validate_for_test(&self) {
        self.validate().unwrap();
    }
}

#[test]
fn ttl_defaults_and_limits() {
    let tmp = tempfile::tempdir().unwrap();
    let s = BlobStore::open(tmp.path(), BlobConfig::default(), 0).unwrap();
    assert_eq!(put(&s, 0, "a", None, b"x", None).unwrap().expires_ms, 600_000);
    assert!(matches!(s.begin(0, "a", None, "image/png", 1, Some(0)), Err(BlobError::BadRequest(_))));
    assert!(matches!(s.begin(0, "a", None, "image/png", 1, Some(25 * 3_600_000)), Err(BlobError::BadRequest(_))));
    assert!(matches!(s.begin(0, "a", None, "PNG", 1, None), Err(BlobError::BadRequest(_))));
    assert!(matches!(s.begin(0, "a", None, "image/png", 0, None), Err(BlobError::BadRequest(_))));
}

#[test]
fn the_quota_holds_until_blobs_expire_and_live_blobs_are_never_evicted() {
    let tmp = tempfile::tempdir().unwrap();
    let s = BlobStore::open(tmp.path(), cfg(100), 0).unwrap();
    let a = put(&s, 0, "a", None, &[1u8; 60], Some(1_000)).unwrap();
    let b = put(&s, 0, "b", None, &[2u8; 40], Some(5_000)).unwrap();
    assert_eq!(s.stats().bytes, 100);
    // Full: a new blob is refused and neither live blob is touched.
    assert!(matches!(s.begin(500, "c", None, "image/png", 1, None), Err(BlobError::Quota(_))));
    assert!(read_all(&s, 500, "a", None, &a).is_ok() && read_all(&s, 500, "b", None, &b).is_ok());
    // a expires: its 60 bytes are free again.
    let c = put(&s, 1_000, "c", None, &[3u8; 60], Some(1_000)).unwrap();
    assert!(matches!(s.get(1_000, "a", None, &a.hash, 0, 1), Err(BlobError::NotFound(_))));
    assert!(read_all(&s, 1_000, "b", None, &b).is_ok() && read_all(&s, 1_000, "c", None, &c).is_ok());
    assert_eq!(s.stats().bytes, 100);
}

#[test]
fn declared_sizes_are_reserved_when_an_upload_begins() {
    let tmp = tempfile::tempdir().unwrap();
    let s = BlobStore::open(tmp.path(), cfg(100), 0).unwrap();
    let up = s.begin(0, "a", None, "image/png", 80, None).unwrap();
    assert_eq!(s.stats().reserved, 80);
    assert!(matches!(s.begin(0, "b", None, "image/png", 30, None), Err(BlobError::Quota(_))));
    // An abandoned upload frees its reservation after the idle limit.
    assert!(s.begin(61_000, "b", None, "image/png", 30, None).is_ok());
    assert!(matches!(s.chunk(61_000, "a", up, 0, b"x", false), Err(BlobError::NotFound(_))));
}

#[test]
fn one_instance_cannot_fill_the_node() {
    let tmp = tempfile::tempdir().unwrap();
    let c = BlobConfig { quota_bytes: 1000, per_instance_bytes: 300, ..Default::default() };
    let s = BlobStore::open(tmp.path(), c, 0).unwrap();
    put(&s, 0, "hog", None, &[1u8; 200], None).unwrap();
    assert!(matches!(s.begin(0, "hog", None, "image/png", 101, None), Err(BlobError::Quota(_))));
    assert!(s.begin(0, "other", None, "image/png", 101, None).is_ok());
}

#[test]
fn uploads_are_sequential_complete_and_owned() {
    let tmp = tempfile::tempdir().unwrap();
    let s = BlobStore::open(tmp.path(), BlobConfig::default(), 0).unwrap();
    let up = s.begin(0, "a", None, "image/png", 10, None).unwrap();
    assert!(matches!(s.chunk(0, "a", up, 3, b"abc", false), Err(BlobError::BadRequest(_))), "offset must follow");
    assert!(matches!(s.chunk(0, "b", up, 0, b"abc", false), Err(BlobError::NotFound(_))), "not the owner's upload");
    s.chunk(0, "a", up, 0, b"abcde", false).unwrap();
    assert!(matches!(s.chunk(0, "a", up, 5, b"fghijk", false), Err(BlobError::BadRequest(_))), "more than declared");
    assert!(matches!(s.chunk(0, "a", up, 5, b"fg", true), Err(BlobError::BadRequest(_))), "last before complete");
    // The failed commit left the upload usable.
    let h = s.chunk(0, "a", up, 7, b"hij", true).unwrap().unwrap();
    assert_eq!(h.hash, blake3::hash(b"abcdefghij").to_hex().to_string(), "the short last chunk's bytes were kept");
    assert!(matches!(s.chunk(0, "a", 99, 0, b"x", false), Err(BlobError::NotFound(_))));
    let big = vec![0u8; CHUNK + 1];
    let up2 = s.begin(0, "a", None, "image/png", big.len() as u64, None).unwrap();
    assert!(matches!(s.chunk(0, "a", up2, 0, &big, true), Err(BlobError::BadRequest(_))));
}

#[test]
fn the_open_upload_cap_is_per_instance() {
    let tmp = tempfile::tempdir().unwrap();
    let s = BlobStore::open(tmp.path(), BlobConfig::default(), 0).unwrap();
    for _ in 0..4 {
        s.begin(0, "a", None, "image/png", 1, None).unwrap();
    }
    assert!(matches!(s.begin(0, "a", None, "image/png", 1, None), Err(BlobError::Quota(_))));
    assert!(s.begin(0, "b", None, "image/png", 1, None).is_ok());
    s.drop_uploads("a");
    assert!(s.begin(0, "a", None, "image/png", 1, None).is_ok());
}

#[test]
fn access_is_owner_project_or_a_handle_passed_on() {
    let tmp = tempfile::tempdir().unwrap();
    let s = BlobStore::open(tmp.path(), BlobConfig::default(), 0).unwrap();
    let h = put(&s, 0, "svc", Some(P1), b"secret frame", None).unwrap();
    assert!(read_all(&s, 1, "svc", Some(P1), &h).is_ok(), "owner");
    assert!(read_all(&s, 1, "sibling", Some(P1), &h).is_ok(), "same project");
    assert!(matches!(s.get(1, "stranger", Some(P2), &h.hash, 0, 1), Err(BlobError::Denied(_))));
    assert!(matches!(s.get(1, "bucket", None, &h.hash, 0, 1), Err(BlobError::Denied(_))), "the node bucket is not project P1");
    // The owner hands the handle on; a non-owner cannot hand on someone else's.
    s.grant(std::slice::from_ref(&h.hash), (Some(P2), "stranger"), (Some(P2), "thief"));
    assert!(matches!(s.get(1, "thief", Some(P2), &h.hash, 0, 1), Err(BlobError::Denied(_))));
    s.grant(std::slice::from_ref(&h.hash), (Some(P1), "svc"), (Some(P2), "stranger"));
    assert!(read_all(&s, 1, "stranger", Some(P2), &h).is_ok());
    s.grant(std::slice::from_ref(&h.hash), (Some(P2), "stranger"), (Some(P2), "friend"));
    assert!(read_all(&s, 1, "friend", Some(P2), &h).is_ok(), "a holder may pass it on");
    assert!(matches!(s.get(1, "x", None, &"0".repeat(64), 0, 1), Err(BlobError::NotFound(_))));
}

#[test]
fn identical_bytes_share_one_file_and_the_expiry_only_moves_out() {
    let tmp = tempfile::tempdir().unwrap();
    let s = BlobStore::open(tmp.path(), BlobConfig::default(), 0).unwrap();
    let a = put(&s, 0, "a", Some(P1), b"same", Some(10_000)).unwrap();
    let b = put(&s, 0, "b", Some(P2), b"same", Some(2_000)).unwrap();
    assert_eq!(a.hash, b.hash);
    assert_eq!(b.expires_ms, 10_000, "never shortened");
    assert_eq!(s.stats().bytes, 4);
    assert!(read_all(&s, 1, "b", Some(P2), &b).is_ok(), "the second putter holds the bytes");
    let c = put(&s, 5_000, "a", Some(P1), b"same", Some(20_000)).unwrap();
    assert_eq!(c.expires_ms, 25_000);
}

#[test]
fn a_restarted_store_finds_live_blobs_and_drops_the_rest() {
    let tmp = tempfile::tempdir().unwrap();
    let (live, dead);
    {
        let s = BlobStore::open(tmp.path(), BlobConfig::default(), 0).unwrap();
        live = put(&s, 0, "a", Some(P1), b"live", Some(100_000)).unwrap();
        dead = put(&s, 0, "a", Some(P1), b"dead", Some(1_000)).unwrap();
        s.begin(0, "a", None, "image/png", 5, None).unwrap();
    }
    std::fs::write(blob_path(tmp.path(), &"9".repeat(64)), b"orphan").unwrap();
    let s = BlobStore::open(tmp.path(), BlobConfig::default(), 5_000).unwrap();
    assert_eq!(read_all(&s, 5_000, "a", Some(P1), &live).unwrap(), b"live");
    assert!(matches!(s.get(5_000, "a", Some(P1), &dead.hash, 0, 1), Err(BlobError::NotFound(_))));
    assert_eq!(s.stats().blobs, 1);
    assert!(!std::fs::read_dir(tmp.path()).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with("up-")));
}

#[test]
fn a_ranged_read_returns_the_slice_and_eof() {
    let tmp = tempfile::tempdir().unwrap();
    let s = BlobStore::open(tmp.path(), BlobConfig::default(), 0).unwrap();
    let h = put(&s, 0, "a", None, b"0123456789", None).unwrap();
    let (_, b, eof) = s.get(1, "a", None, &h.hash, 2, 3).unwrap();
    assert_eq!((b.as_slice(), eof), (&b"234"[..], false));
    let (_, b, eof) = s.get(1, "a", None, &h.hash, 8, 100).unwrap();
    assert_eq!((b.as_slice(), eof), (&b"89"[..], true));
    let (_, b, eof) = s.get(1, "a", None, &h.hash, 50, 10).unwrap();
    assert!(b.is_empty() && eof);
}

#[test]
fn an_empty_chunk_cannot_keep_an_upload_alive() {
    let tmp = tempfile::tempdir().unwrap();
    let s = BlobStore::open(tmp.path(), BlobConfig::default(), 0).unwrap();
    let up = s.begin(0, "a", None, "image/png", 4, None).unwrap();
    s.chunk(0, "a", up, 0, b"ab", false).unwrap();
    assert!(matches!(s.chunk(50_000, "a", up, 2, b"", false), Err(BlobError::BadRequest(_))));
    // The refused empty chunk did not refresh the idle clock: it times out.
    assert!(s.begin(61_000, "b", None, "image/png", 1, None).is_ok());
    assert!(matches!(s.chunk(61_000, "a", up, 2, b"cd", true), Err(BlobError::NotFound(_))));
}

#[test]
fn re_putting_known_bytes_cannot_pin_anothers_blob_or_resurrect_it() {
    let tmp = tempfile::tempdir().unwrap();
    let s = BlobStore::open(tmp.path(), BlobConfig::default(), 0).unwrap();
    let a = put(&s, 0, "owner", Some(P1), b"payload", Some(10_000)).unwrap();
    // Another project holding the same bytes asks for a day: it does not move the expiry.
    let b = put(&s, 1_000, "other", Some(P2), b"payload", Some(86_400_000)).unwrap();
    assert_eq!(b.expires_ms, a.expires_ms, "{b:?}");
    assert!(matches!(s.get(10_000, "owner", Some(P1), &a.hash, 0, 1), Err(BlobError::NotFound(_))), "expired on schedule");
    // An expired blob the sweep has not reached is replaced, not revived with its old readers.
    let c = put(&s, 20_000, "other", Some(P2), b"payload", Some(5_000)).unwrap();
    assert_eq!(c.expires_ms, 25_000);
    assert!(matches!(s.get(21_000, "owner", Some(P1), &c.hash, 0, 1), Err(BlobError::Denied(_))), "the old owner is not a reader of the new blob");
    // The owner and holders it passed the handle to may extend.
    let d = put(&s, 21_000, "other", Some(P2), b"payload", Some(60_000)).unwrap();
    assert_eq!(d.expires_ms, 81_000);
}

#[test]
fn several_instances_of_one_project_share_its_quota() {
    let tmp = tempfile::tempdir().unwrap();
    let c = BlobConfig { quota_bytes: 1000, per_instance_bytes: 1000, per_project_bytes: 300, ..Default::default() };
    let s = BlobStore::open(tmp.path(), c, 0).unwrap();
    put(&s, 0, "i1", Some(P1), &[1u8; 150], None).unwrap();
    put(&s, 0, "i2", Some(P1), &[2u8; 150], None).unwrap();
    assert!(matches!(s.begin(0, "i3", Some(P1), "image/png", 1, None), Err(BlobError::Quota(_))), "the project is full");
    assert!(s.begin(0, "x", Some(P2), "image/png", 100, None).is_ok(), "another project is not");
    assert!(s.begin(0, "y", None, "image/png", 100, None).is_ok(), "nor the node bucket");
}

#[cfg(unix)]
#[test]
fn an_existing_blob_directory_is_made_private() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("blobs");
    std::fs::create_dir(&d).unwrap();
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
    BlobStore::open(&d, BlobConfig::default(), 0).unwrap();
    assert_eq!(std::fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700);
}

#[test]
fn an_instance_id_re_placed_under_another_project_is_not_the_old_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let s = BlobStore::open(tmp.path(), BlobConfig::default(), 0).unwrap();
    let h = put(&s, 0, "cam-1", Some(P1), b"private frame", None).unwrap();
    // The same instance id, now placed for P2 (or the node bucket), has no claim.
    for other in [Some(P2), None] {
        assert!(matches!(s.get(1, "cam-1", other, &h.hash, 0, 1), Err(BlobError::Denied(_))), "{other:?}");
    }
    assert!(read_all(&s, 1, "cam-1", Some(P1), &h).is_ok(), "the real owner still reads");
    // A handle passed to cam-1 in P1 does not carry over to cam-1 in P2.
    let h2 = put(&s, 0, "svc", Some(P1), b"passed on", None).unwrap();
    s.grant(std::slice::from_ref(&h2.hash), (Some(P1), "svc"), (Some(P1), "reader"));
    assert!(read_all(&s, 1, "reader", Some(P1), &h2).is_ok());
    assert!(matches!(s.get(1, "reader", Some(P2), &h2.hash, 0, 1), Err(BlobError::Denied(_))));
    // Nor may the old id re-put the bytes to move the expiry as if it were the owner.
    let again = put(&s, 5, "cam-1", Some(P2), b"private frame", Some(86_400_000)).unwrap();
    assert_eq!(again.expires_ms, h.expires_ms);
}

#[test]
fn release_returns_the_quota_and_only_a_putter_may_release() {
    let tmp = tempfile::tempdir().unwrap();
    let s = BlobStore::open(tmp.path(), cfg(1000), 0).unwrap();
    let h = put(&s, 0, "a", Some(P1), &[1u8; 600], None).unwrap();
    assert!(put(&s, 0, "a2", Some(P1), &[2u8; 600], None).is_err(), "the store is nearly full");
    // A reader of the project (or one handed the handle) is not a putter.
    s.grant(std::slice::from_ref(&h.hash), (Some(P1), "a"), (Some(P2), "friend"));
    assert!(matches!(s.release("friend", Some(P2), &h.hash), Err(BlobError::Denied(_))));
    assert!(matches!(s.release("mate", Some(P1), &h.hash), Err(BlobError::Denied(_))));
    assert!(matches!(s.release("a", Some(P2), &h.hash), Err(BlobError::Denied(_))), "same id, other project");
    assert_eq!(s.stats().bytes, 600);
    assert!(s.release("a", Some(P1), &h.hash).unwrap());
    assert_eq!(s.stats().bytes, 0);
    assert!(matches!(s.get(1, "friend", Some(P2), &h.hash, 0, 10), Err(BlobError::NotFound(_))), "handle readers lose it");
    assert!(put(&s, 0, "a2", Some(P1), &[2u8; 600], None).is_ok(), "the quota is back");
    assert!(matches!(s.release("a", Some(P1), &h.hash), Err(BlobError::NotFound(_))));
}

#[test]
fn bytes_put_by_two_principals_survive_one_release() {
    let tmp = tempfile::tempdir().unwrap();
    let s = BlobStore::open(tmp.path(), BlobConfig::default(), 0).unwrap();
    let a = put(&s, 0, "a", Some(P1), b"same", None).unwrap();
    let b = put(&s, 0, "b", Some(P2), b"same", None).unwrap();
    assert_eq!(a.hash, b.hash);
    // The first putter lets go: the second still holds and reads them.
    assert!(!s.release("a", Some(P1), &a.hash).unwrap());
    assert_eq!(s.stats().bytes, 4);
    assert_eq!(read_all(&s, 1, "b", Some(P2), &b).unwrap(), b"same");
    assert!(matches!(s.release("a", Some(P1), &a.hash), Err(BlobError::Denied(_))), "a already let go");
    // It survives a restart with its holds.
    drop(s);
    let s = BlobStore::open(tmp.path(), BlobConfig::default(), 1).unwrap();
    assert!(matches!(s.release("a", Some(P1), &a.hash), Err(BlobError::Denied(_))));
    assert!(s.release("b", Some(P2), &a.hash).unwrap());
    assert_eq!(s.stats().blobs, 0);
}
