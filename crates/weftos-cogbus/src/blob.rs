//! The node's content store (ADR-122 section 3, phase B5): blobs addressed
//! by BLAKE3, each with a size, media type and expiry.
//!
//! - **Files.** `<dir>/<hash>.blob` holds the bytes and `<dir>/<hash>.meta`
//!   its metadata, so a restarted daemon finds what is still alive. Uploads
//!   stage in `<dir>/up-<n>.tmp` and are renamed on commit; stale ones are
//!   removed at open.
//! - **Quota.** The node holds at most [`BlobConfig::quota_bytes`] (default
//!   2 GiB) and one instance at most [`BlobConfig::per_instance_bytes`]. Room
//!   is made only by dropping expired blobs; a live blob is never evicted for
//!   a newer one, so a handle that was handed out stays good until its TTL.
//!   Declared upload sizes are reserved when the upload begins.
//! - **TTL.** Every blob has an expiry (`ttl_ms`, default 10 min, at most
//!   24 h). [`BlobStore::sweep`] and every read drop what has expired. Putting
//!   bytes that are already stored extends the expiry and never shortens it.
//! - **Access.** A blob is readable by its owner, by instances of the
//!   owner's project (or of the node bucket for a project-less owner), and by
//!   instances the owner named by passing them its handle ([`BlobStore::grant`]:
//!   a service call, its reply, a published reading). The broker checks the
//!   token, uid and governance before it gets here.
//!
//! Time is passed in (`now_ms`), so tests need no clock.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use clawft_types::cog_topics::{BlobHandle, valid_media_type};
use serde::{Deserialize, Serialize};

/// Largest slice a chunk or a read carries.
pub const CHUNK: usize = 32 * 1024;

/// Store limits.
#[derive(Debug, Clone)]
pub struct BlobConfig {
    /// Bytes the node holds (ADR-122 decision 4: 2 GiB).
    pub quota_bytes: u64,
    /// Bytes one instance owns at most.
    pub per_instance_bytes: u64,
    /// Bytes one project (the node bucket counts as one) owns at most, so
    /// several instances of a project cannot fill the node together.
    pub per_project_bytes: u64,
    /// TTL when the put names none.
    pub default_ttl_ms: u64,
    /// Longest TTL a put may ask for.
    pub max_ttl_ms: u64,
    /// Largest single blob.
    pub max_blob_bytes: u64,
    /// An upload idle this long is dropped.
    pub upload_idle_ms: u64,
    /// Uploads one instance may have open.
    pub max_uploads_per_instance: usize,
}

impl Default for BlobConfig {
    fn default() -> Self {
        Self {
            quota_bytes: 2 << 30,
            per_instance_bytes: 512 << 20,
            per_project_bytes: 1 << 30,
            default_ttl_ms: 10 * 60_000,
            max_ttl_ms: 24 * 3_600_000,
            max_blob_bytes: 256 << 20,
            upload_idle_ms: 60_000,
            max_uploads_per_instance: 4,
        }
    }
}

/// Why a store call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlobError {
    /// Bad media type, size, ttl, offset or chunk.
    #[error("{0}")]
    BadRequest(String),
    /// Over the node's or the instance's quota, or too many uploads open.
    #[error("{0}")]
    Quota(String),
    /// Over the single-blob limit.
    #[error("{0}")]
    TooLarge(String),
    /// No such blob or upload (never stored, expired, or not yours).
    #[error("{0}")]
    NotFound(String),
    /// The blob belongs to another project and was not handed to you.
    #[error("{0}")]
    Denied(String),
    /// The disk failed.
    #[error("{0}")]
    Io(String),
}

fn io(e: std::io::Error) -> BlobError {
    BlobError::Io(e.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Meta {
    size: u64,
    media_type: String,
    expires_ms: u64,
    owner: String,
    project_id: Option<String>,
    #[serde(skip)]
    readers: HashSet<String>,
}

struct Upload {
    owner: String,
    project_id: Option<String>,
    media_type: String,
    size: u64,
    ttl_ms: u64,
    received: u64,
    hasher: blake3::Hasher,
    file: File,
    path: PathBuf,
    last_ms: u64,
}

#[derive(Default)]
struct Inner {
    blobs: HashMap<String, Meta>,
    uploads: HashMap<u64, Upload>,
    next: u64,
}

/// What the store holds, for `cogbus list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlobStats {
    /// Live blobs.
    pub blobs: usize,
    /// Bytes they hold.
    pub bytes: u64,
    /// Bytes reserved by open uploads.
    pub reserved: u64,
    /// Node quota.
    pub quota: u64,
}

/// The content store.
pub struct BlobStore {
    dir: PathBuf,
    cfg: BlobConfig,
    inner: Mutex<Inner>,
}

fn blob_path(dir: &Path, hash: &str) -> PathBuf {
    dir.join(format!("{hash}.blob"))
}

fn meta_path(dir: &Path, hash: &str) -> PathBuf {
    dir.join(format!("{hash}.meta"))
}

impl BlobStore {
    /// Open (creating) the store in `dir`: blobs still alive at `now_ms` are
    /// found again, expired or damaged ones and stale uploads are removed.
    pub fn open(dir: &Path, cfg: BlobConfig, now_ms: u64) -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        // A directory that already existed keeps its mode; the blobs inside
        // must not be readable by other cog uids.
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut inner = Inner::default();
        for e in std::fs::read_dir(dir)? {
            let p = e?.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            if name.starts_with("up-") {
                let _ = std::fs::remove_file(&p);
                continue;
            }
            let Some(hash) = name.strip_suffix(".meta") else { continue };
            let meta: Option<Meta> = std::fs::read(&p).ok().and_then(|b| serde_json::from_slice(&b).ok());
            let alive = meta.as_ref().filter(|m| {
                m.expires_ms > now_ms
                    && std::fs::metadata(blob_path(dir, hash)).is_ok_and(|f| f.len() == m.size)
            });
            match alive {
                Some(m) => {
                    inner.blobs.insert(hash.to_string(), m.clone());
                }
                None => {
                    let _ = std::fs::remove_file(&p);
                    let _ = std::fs::remove_file(blob_path(dir, hash));
                }
            }
        }
        Ok(Self { dir: dir.to_path_buf(), cfg, inner: Mutex::new(inner) })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The store's limits.
    pub fn config(&self) -> &BlobConfig {
        &self.cfg
    }

    fn drop_blob(&self, inner: &mut Inner, hash: &str) {
        if inner.blobs.remove(hash).is_some() {
            let _ = std::fs::remove_file(blob_path(&self.dir, hash));
            let _ = std::fs::remove_file(meta_path(&self.dir, hash));
        }
    }

    fn drop_expired(&self, inner: &mut Inner, now: u64) -> usize {
        let dead: Vec<String> = inner
            .blobs
            .iter()
            .filter(|(_, m)| m.expires_ms <= now)
            .map(|(h, _)| h.clone())
            .collect();
        for h in &dead {
            self.drop_blob(inner, h);
        }
        let idle = self.cfg.upload_idle_ms;
        let stale: Vec<u64> = inner
            .uploads
            .iter()
            .filter(|(_, u)| now.saturating_sub(u.last_ms) > idle)
            .map(|(i, _)| *i)
            .collect();
        for i in stale {
            if let Some(u) = inner.uploads.remove(&i) {
                let _ = std::fs::remove_file(&u.path);
            }
        }
        dead.len()
    }

    /// Drop everything expired (and idle uploads); returns the blobs dropped.
    pub fn sweep(&self, now_ms: u64) -> usize {
        let mut g = self.lock();
        self.drop_expired(&mut g, now_ms)
    }

    fn used(inner: &Inner) -> (u64, u64) {
        let stored: u64 = inner.blobs.values().map(|m| m.size).sum();
        let reserved: u64 = inner.uploads.values().map(|u| u.size).sum();
        (stored, reserved)
    }

    /// Start an upload of `size` bytes; returns the upload id.
    pub fn begin(
        &self,
        now_ms: u64,
        owner: &str,
        project_id: Option<&str>,
        media_type: &str,
        size: u64,
        ttl_ms: Option<u64>,
    ) -> Result<u64, BlobError> {
        if !valid_media_type(media_type) {
            return Err(BlobError::BadRequest(format!("bad media type {media_type:?}")));
        }
        if size == 0 {
            return Err(BlobError::BadRequest("a blob has at least 1 byte".into()));
        }
        if size > self.cfg.max_blob_bytes {
            return Err(BlobError::TooLarge(format!(
                "{size} bytes exceeds the {} byte blob limit",
                self.cfg.max_blob_bytes
            )));
        }
        let ttl = ttl_ms.unwrap_or(self.cfg.default_ttl_ms);
        if ttl == 0 || ttl > self.cfg.max_ttl_ms {
            return Err(BlobError::BadRequest(format!(
                "ttl_ms must be 1..={}",
                self.cfg.max_ttl_ms
            )));
        }
        let mut g = self.lock();
        self.drop_expired(&mut g, now_ms);
        if g.uploads.values().filter(|u| u.owner == owner).count() >= self.cfg.max_uploads_per_instance {
            return Err(BlobError::Quota("too many uploads open".into()));
        }
        let (stored, reserved) = Self::used(&g);
        if stored + reserved + size > self.cfg.quota_bytes {
            return Err(BlobError::Quota(format!(
                "the store holds {} of {} bytes; {size} more does not fit until blobs expire",
                stored + reserved,
                self.cfg.quota_bytes
            )));
        }
        let mine: u64 = g.blobs.values().filter(|m| m.owner == owner).map(|m| m.size).sum::<u64>()
            + g.uploads.values().filter(|u| u.owner == owner).map(|u| u.size).sum::<u64>();
        if mine + size > self.cfg.per_instance_bytes {
            return Err(BlobError::Quota(format!(
                "this instance holds {mine} of its {} bytes",
                self.cfg.per_instance_bytes
            )));
        }
        let theirs: u64 = g.blobs.values().filter(|m| m.project_id.as_deref() == project_id).map(|m| m.size).sum::<u64>()
            + g.uploads.values().filter(|u| u.project_id.as_deref() == project_id).map(|u| u.size).sum::<u64>();
        if theirs + size > self.cfg.per_project_bytes {
            return Err(BlobError::Quota(format!(
                "this project holds {theirs} of its {} bytes",
                self.cfg.per_project_bytes
            )));
        }
        g.next += 1;
        let id = g.next;
        let path = self.dir.join(format!("up-{id}.tmp"));
        let file = OpenOptions::new().create_new(true).write(true).open(&path).map_err(io)?;
        g.uploads.insert(
            id,
            Upload {
                owner: owner.into(),
                project_id: project_id.map(String::from),
                media_type: media_type.into(),
                size,
                ttl_ms: ttl,
                received: 0,
                hasher: blake3::Hasher::new(),
                file,
                path,
                last_ms: now_ms,
            },
        );
        Ok(id)
    }

    /// Append `data` at `offset` (strictly sequential). With `last`, the
    /// upload must be complete: it is committed and its handle returned.
    pub fn chunk(
        &self,
        now_ms: u64,
        owner: &str,
        upload: u64,
        offset: u64,
        data: &[u8],
        last: bool,
    ) -> Result<Option<BlobHandle>, BlobError> {
        if data.len() > CHUNK {
            return Err(BlobError::BadRequest(format!("a chunk is at most {CHUNK} bytes")));
        }
        // An empty slice only completes an upload; it never keeps one alive.
        if data.is_empty() && !last {
            return Err(BlobError::BadRequest("an empty chunk".into()));
        }
        let mut g = self.lock();
        let u = g
            .uploads
            .get_mut(&upload)
            .filter(|u| u.owner == owner)
            .ok_or_else(|| BlobError::NotFound(format!("no upload {upload} (finished, idle too long or not yours)")))?;
        if offset != u.received {
            return Err(BlobError::BadRequest(format!("expected offset {}, got {offset}", u.received)));
        }
        if u.received + data.len() as u64 > u.size {
            return Err(BlobError::BadRequest("more bytes than declared".into()));
        }
        u.file.write_all(data).map_err(io)?;
        u.hasher.update(data);
        u.received += data.len() as u64;
        u.last_ms = now_ms;
        if !last {
            return Ok(None);
        }
        if u.received != u.size {
            return Err(BlobError::BadRequest(format!("last chunk at {} of {} bytes", u.received, u.size)));
        }
        let Some(u) = g.uploads.remove(&upload) else {
            return Err(BlobError::NotFound(format!("no upload {upload}")));
        };
        let hash = u.hasher.finalize().to_hex().to_string();
        let expires = now_ms + u.ttl_ms;
        u.file.sync_all().map_err(io)?;
        drop(u.file);
        let handle = |expires_ms| BlobHandle {
            hash: hash.clone(),
            size: u.size,
            media_type: u.media_type.clone(),
            expires_ms,
        };
        if g.blobs.get(&hash).is_some_and(|m| m.expires_ms <= now_ms) {
            // Expired and not yet swept: it is gone, not resurrected.
            self.drop_blob(&mut g, &hash);
        }
        if let Some(m) = g.blobs.get_mut(&hash) {
            // Already stored: the new putter holds the bytes, so it may read
            // them. Only the owner (or a holder it passed the handle to)
            // moves the expiry out; anyone else gets the existing expiry, so
            // re-putting known bytes cannot pin another project's blob.
            let _ = std::fs::remove_file(&u.path);
            if m.owner == owner || m.readers.contains(owner) {
                m.expires_ms = m.expires_ms.max(expires);
            }
            m.readers.insert(owner.to_string());
            let meta = m.clone();
            std::fs::write(meta_path(&self.dir, &hash), serde_json::to_vec(&meta).unwrap_or_default()).map_err(io)?;
            return Ok(Some(BlobHandle {
                hash,
                size: meta.size,
                media_type: meta.media_type,
                expires_ms: meta.expires_ms,
            }));
        }
        std::fs::rename(&u.path, blob_path(&self.dir, &hash)).map_err(io)?;
        let meta = Meta {
            size: u.size,
            media_type: u.media_type.clone(),
            expires_ms: expires,
            owner: u.owner.clone(),
            project_id: u.project_id.clone(),
            readers: HashSet::new(),
        };
        std::fs::write(meta_path(&self.dir, &hash), serde_json::to_vec(&meta).map_err(|e| BlobError::Io(e.to_string()))?)
            .map_err(io)?;
        g.blobs.insert(hash.clone(), meta);
        Ok(Some(handle(expires)))
    }

    fn may_read(m: &Meta, instance: &str, project_id: Option<&str>) -> bool {
        m.owner == instance || m.readers.contains(instance) || m.project_id.as_deref() == project_id
    }

    /// Read up to [`CHUNK`] bytes of blob `hash` from `offset`; the flag says
    /// the end was reached. An expired blob is dropped and `NotFound`.
    pub fn get(
        &self,
        now_ms: u64,
        instance: &str,
        project_id: Option<&str>,
        hash: &str,
        offset: u64,
        len: usize,
    ) -> Result<(BlobHandle, Vec<u8>, bool), BlobError> {
        let mut g = self.lock();
        let Some(m) = g.blobs.get(hash).cloned() else {
            return Err(BlobError::NotFound(format!("no blob {hash}")));
        };
        if m.expires_ms <= now_ms {
            self.drop_blob(&mut g, hash);
            return Err(BlobError::NotFound(format!("blob {hash} expired")));
        }
        if !Self::may_read(&m, instance, project_id) {
            return Err(BlobError::Denied(format!("blob {hash} belongs to another project")));
        }
        drop(g);
        let want = len.min(CHUNK) as u64;
        let take = want.min(m.size.saturating_sub(offset)) as usize;
        let mut buf = vec![0u8; take];
        if take > 0 {
            let mut f = File::open(blob_path(&self.dir, hash)).map_err(io)?;
            f.seek(SeekFrom::Start(offset)).map_err(io)?;
            f.read_exact(&mut buf).map_err(io)?;
        }
        let eof = offset + take as u64 >= m.size;
        let handle = BlobHandle {
            hash: hash.to_string(),
            size: m.size,
            media_type: m.media_type,
            expires_ms: m.expires_ms,
        };
        Ok((handle, buf, eof))
    }

    /// `from` passes its handles to `to`: each blob `from` owns becomes
    /// readable by `to`. Blobs `from` does not own are ignored, so a
    /// service cannot hand on someone else's handle.
    pub fn grant(&self, hashes: &[String], from: &str, to: &str) {
        let mut g = self.lock();
        for h in hashes {
            if let Some(m) = g.blobs.get_mut(h)
                && (m.owner == from || m.readers.contains(from))
            {
                m.readers.insert(to.to_string());
            }
        }
    }

    /// An instance went away: its open uploads are dropped (finished blobs
    /// stay until their TTL).
    pub fn drop_uploads(&self, owner: &str) {
        let mut g = self.lock();
        let ids: Vec<u64> = g.uploads.iter().filter(|(_, u)| u.owner == owner).map(|(i, _)| *i).collect();
        for i in ids {
            if let Some(u) = g.uploads.remove(&i) {
                let _ = std::fs::remove_file(&u.path);
            }
        }
    }

    /// Counts and bytes.
    pub fn stats(&self) -> BlobStats {
        let g = self.lock();
        let (bytes, reserved) = Self::used(&g);
        BlobStats { blobs: g.blobs.len(), bytes, reserved, quota: self.cfg.quota_bytes }
    }
}

#[cfg(test)]
#[path = "tests_blob.rs"]
mod tests;
