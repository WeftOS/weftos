//! Descriptors over the broker socket (`SCM_RIGHTS`, ADR-122 section 4).
//!
//! The broker attaches descriptors to the frame that announces them (its
//! `fds` field says how many). [`send_frame`] writes one frame with its
//! descriptors in a single `sendmsg`; [`FdReader`] is the client's
//! `AsyncRead` over `recvmsg` that queues every descriptor it receives, in
//! order, for the frame parser to take.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use tokio::io::{AsyncRead, AsyncWriteExt, Interest, ReadBuf};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

/// Most descriptors one frame carries.
pub(crate) const MAX_FDS: usize = 4;

fn cmsg_space() -> usize {
    // SAFETY: CMSG_SPACE is a pure size computation.
    unsafe { libc::CMSG_SPACE((MAX_FDS * std::mem::size_of::<RawFd>()) as u32) as usize }
}

/// Write `bytes` with `fds` attached to the first byte, then the rest.
pub(crate) async fn send_frame(
    w: &mut OwnedWriteHalf,
    bytes: &[u8],
    fds: &[OwnedFd],
) -> io::Result<()> {
    if fds.is_empty() {
        w.write_all(bytes).await?;
        return w.flush().await;
    }
    if fds.len() > MAX_FDS {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "too many fds"));
    }
    let raw: Vec<RawFd> = fds.iter().map(AsRawFd::as_raw_fd).collect();
    let stream = w.as_ref();
    let sent = stream
        .async_io(Interest::WRITABLE, || {
            sendmsg_fds(stream.as_raw_fd(), bytes, &raw)
        })
        .await?;
    w.write_all(&bytes[sent..]).await?;
    w.flush().await
}

fn sendmsg_fds(sock: RawFd, bytes: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    let mut ctl = vec![0u8; cmsg_space()];
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    // SAFETY: zeroed msghdr is a valid empty header; every pointer set below
    // points into a live local buffer sized for what it describes.
    let rc = unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = ctl.as_mut_ptr().cast();
        let data_len = std::mem::size_of_val(fds) as u32;
        msg.msg_controllen = libc::CMSG_SPACE(data_len) as _;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(data_len) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c).cast::<RawFd>(), fds.len());
        libc::sendmsg(sock, &msg, 0)
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(rc as usize)
}

/// `recvmsg` into `buf`, pushing received descriptors onto `out`.
fn recvmsg_fds(sock: RawFd, buf: &mut [u8], out: &mut VecDeque<OwnedFd>) -> io::Result<usize> {
    let mut ctl = vec![0u8; cmsg_space()];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;
    // SAFETY: as in `sendmsg_fds`; the kernel writes at most the lengths we
    // pass, and we walk only the control messages it reports.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = ctl.as_mut_ptr().cast();
        msg.msg_controllen = ctl.len() as _;
        let n = libc::recvmsg(sock, &mut msg, flags);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c);
                let bytes = (*c).cmsg_len as usize - (data as usize - c as usize);
                for i in 0..bytes / std::mem::size_of::<RawFd>() {
                    let fd = std::ptr::read_unaligned(data.cast::<RawFd>().add(i));
                    #[cfg(not(target_os = "linux"))]
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                    out.push_back(OwnedFd::from_raw_fd(fd));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
        if msg.msg_flags & libc::MSG_CTRUNC != 0 {
            tracing::warn!(target: "cogbus", "broker sent more descriptors than a frame may carry");
        }
        Ok(n as usize)
    }
}

/// The client's read half: an `AsyncRead` that keeps passed descriptors.
pub(crate) struct FdReader {
    half: OwnedReadHalf,
    fds: VecDeque<OwnedFd>,
}

impl FdReader {
    pub(crate) fn new(half: OwnedReadHalf) -> Self {
        Self {
            half,
            fds: VecDeque::new(),
        }
    }

    /// The next `n` queued descriptors (fewer if fewer arrived).
    pub(crate) fn take(&mut self, n: usize) -> Vec<OwnedFd> {
        let n = n.min(self.fds.len());
        self.fds.drain(..n).collect()
    }
}

impl AsyncRead for FdReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        loop {
            let stream = me.half.as_ref();
            ready!(stream.poll_read_ready(cx))?;
            let dst = buf.initialize_unfilled();
            let fds = &mut me.fds;
            match stream.try_io(Interest::READABLE, || {
                recvmsg_fds(stream.as_raw_fd(), dst, fds)
            }) {
                Ok(n) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }
}
