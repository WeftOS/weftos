//! Anonymous shared memory for stream rings and cursor pages.
//!
//! [`create`] returns one object with two descriptors: a read-write one for
//! the broker and the writer, and a read-only one for readers, so a reader
//! that tries to map the ring writable gets `EACCES` from the kernel.
//!
//! - Linux: `memfd_create` sealed against shrink and grow (a writer cannot
//!   truncate the ring under its readers), with the read-only descriptor
//!   reopened through `/proc/self/fd`.
//! - macOS: `shm_open` under a random name, opened a second time read-only
//!   and unlinked at once, so no name outlives the call. Development only.
//! - Anything else: `Unsupported`.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;
use std::sync::atomic::AtomicU64;

/// One shared-memory object.
#[derive(Debug)]
pub(crate) struct Shm {
    /// Read-write descriptor.
    pub rw: OwnedFd,
    /// Read-only descriptor of the same object.
    pub ro: OwnedFd,
}

fn last_err() -> io::Error {
    io::Error::last_os_error()
}

fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 { Err(last_err()) } else { Ok(rc) }
}

/// A new zero-filled object of `len` bytes.
pub(crate) fn create(len: usize) -> io::Result<Shm> {
    if len == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty shm"));
    }
    let (rw, ro) = platform::create(len)?;
    Ok(Shm { rw, ro })
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    pub(super) fn create(len: usize) -> io::Result<(OwnedFd, OwnedFd)> {
        let name = c"cogbus";
        let flags = libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING;
        // SAFETY: a valid C string and flags; the result is checked.
        let fd = check(unsafe { libc::memfd_create(name.as_ptr(), flags) })?;
        // SAFETY: `fd` was just returned to us and is owned by nobody else.
        let rw = unsafe { OwnedFd::from_raw_fd(fd) };
        let size = libc::off_t::try_from(len).map_err(|_| io::ErrorKind::InvalidInput)?;
        // SAFETY: plain syscalls on a descriptor we own.
        check(unsafe { libc::ftruncate(rw.as_raw_fd(), size) })?;
        let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
        // SAFETY: as above.
        check(unsafe { libc::fcntl(rw.as_raw_fd(), libc::F_ADD_SEALS, seals) })?;
        let path = std::ffi::CString::new(format!("/proc/self/fd/{}", rw.as_raw_fd()))
            .map_err(|_| io::ErrorKind::InvalidInput)?;
        // SAFETY: a valid C string; the result is checked.
        let ro = check(unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) })?;
        // SAFETY: `ro` was just returned to us.
        Ok((rw, unsafe { OwnedFd::from_raw_fd(ro) }))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn cloexec(fd: &OwnedFd) -> io::Result<()> {
        // SAFETY: plain syscall on a descriptor we own.
        check(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) }).map(|_| ())
    }

    pub(super) fn create(len: usize) -> io::Result<(OwnedFd, OwnedFd)> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let salt = blake3::hash(&super::super::now_ns().to_le_bytes());
        // macOS caps shm names at 31 bytes.
        let name = format!(
            "/cb{:x}.{:x}.{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            &salt.to_hex()[..8]
        );
        let c = std::ffi::CString::new(name).map_err(|_| io::ErrorKind::InvalidInput)?;
        let mode: libc::c_uint = 0o600;
        // SAFETY: a valid C string; the result is checked.
        let fd = check(unsafe {
            libc::shm_open(
                c.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                mode,
            )
        })?;
        // SAFETY: `fd` was just returned to us.
        let rw = unsafe { OwnedFd::from_raw_fd(fd) };
        let opened = (|| {
            let size = libc::off_t::try_from(len).map_err(|_| io::ErrorKind::InvalidInput)?;
            // SAFETY: plain syscall on a descriptor we own.
            check(unsafe { libc::ftruncate(rw.as_raw_fd(), size) })?;
            // SAFETY: a valid C string; the result is checked.
            let ro = check(unsafe { libc::shm_open(c.as_ptr(), libc::O_RDONLY, 0) })?;
            // SAFETY: `ro` was just returned to us.
            Ok::<_, io::Error>(unsafe { OwnedFd::from_raw_fd(ro) })
        })();
        // SAFETY: a valid C string; unlinking only removes the name.
        unsafe { libc::shm_unlink(c.as_ptr()) };
        let ro = opened?;
        cloexec(&rw)?;
        cloexec(&ro)?;
        Ok((rw, ro))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use super::*;

    pub(super) fn create(_len: usize) -> io::Result<(OwnedFd, OwnedFd)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cogbus stream rings need Linux (memfd) or macOS (shm_open)",
        ))
    }
}

/// A mapping of a shared-memory object into this process.
#[derive(Debug)]
pub(crate) struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
    writable: bool,
}

// SAFETY: the mapping is plain shared memory accessed only through atomics.
unsafe impl Send for Mapping {}
// SAFETY: as above.
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Map `len` bytes of `fd`, refusing an object smaller than `len` (an
    /// access past its end would fault).
    pub(crate) fn new(fd: BorrowedFd<'_>, len: usize, writable: bool) -> io::Result<Self> {
        // SAFETY: fstat fills `st` for a descriptor we hold.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        check(unsafe { libc::fstat(fd.as_raw_fd(), &mut st) })?;
        if len == 0 || (st.st_size as u64) < len as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("shm holds {} bytes, need {len}", st.st_size),
            ));
        }
        let prot = if writable {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };
        // SAFETY: a fresh shared mapping of a descriptor we hold; checked.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                prot,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(last_err());
        }
        let ptr = NonNull::new(p.cast::<u8>()).ok_or_else(last_err)?;
        Ok(Self { ptr, len, writable })
    }

    /// Mapped length.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// The 8-byte word at `off`. Panics when `off` is unaligned or out of
    /// range: offsets come from validated geometry, so that is a bug here.
    pub(crate) fn word(&self, off: usize) -> &AtomicU64 {
        assert!(
            off.is_multiple_of(8) && off + 8 <= self.len,
            "shm word {off} of {}",
            self.len
        );
        // SAFETY: in bounds and 8-aligned (mmap is page-aligned); the memory
        // lives as long as `self`, and every access to it is atomic.
        unsafe { &*self.ptr.as_ptr().add(off).cast::<AtomicU64>() }
    }

    /// True if mapped read-write.
    pub(crate) fn writable(&self) -> bool {
        self.writable
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what `new` mapped.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}
