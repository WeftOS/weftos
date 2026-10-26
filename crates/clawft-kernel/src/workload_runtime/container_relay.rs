//! In-container ingest relay (ADR-100 section 4).
//!
//! A cog always posts its vectors to `127.0.0.1:80/api/v1/store/ingest`
//! (the address is compiled into the released binaries). Inside a
//! container, or an Apple `container` VM, that loopback is the container's
//! own, so nothing answers unless something listens there. When the host
//! contract names an ingest upstream (the node's ingest bridge as the
//! container sees it, e.g. the VM gateway), the adapter builds the image
//! with a small relay as the entrypoint:
//!
//! 1. the relay binds `127.0.0.1:80` while still root (the only reason the
//!    container starts as root, with every capability dropped except
//!    `NET_BIND_SERVICE`, `SETUID` and `SETGID`);
//! 2. it forks the cog, which drops to `nobody` and sets `no_new_privs`
//!    before `exec`, so the cog runs with no capabilities and cannot regain
//!    any through a setuid binary (Apple `container` has no
//!    `--security-opt no-new-privileges`, so the relay sets it itself);
//! 3. it drops to `nobody` (and `no_new_privs`) itself and forwards each connection, byte for
//!    byte, to the upstream (bounded connections, bytes and time);
//! 4. it forwards SIGTERM / SIGINT to the cog and exits with the cog's
//!    status.
//!
//! The relay is a Python script, so a relay image needs `python3` in the
//! operator-pinned base; the build checks for it and fails loudly.

use std::net::SocketAddr;

use super::container_cmd::{COG_PATH, CONTAINER_USER};
use super::host_contract::HostContract;
use super::types::RuntimeError;

/// Where the relay script lives in the image.
pub const RELAY_PATH: &str = "/weftos-ingest-relay.py";
/// Build-context file name of the relay script.
pub const RELAY_FILE: &str = "ingest-relay.py";
/// The fixed address every cog posts its ingest to.
pub const COG_INGEST_ADDR: &str = "127.0.0.1:80";
/// Capabilities a relay container keeps (all others are dropped).
pub const RELAY_CAPS: [&str; 3] = ["CAP_NET_BIND_SERVICE", "CAP_SETUID", "CAP_SETGID"];

/// The upstream the relay forwards to, if the image needs a relay.
///
/// Refuses combinations that cannot work: `network=none` (no route out),
/// `network=host` (the cog already reaches the node's own `127.0.0.1:80`;
/// a relay would collide with it), and a loopback upstream (inside the
/// container, loopback is the container itself).
pub fn relay_upstream(
    host: &HostContract,
    network: Option<&str>,
) -> Result<Option<SocketAddr>, RuntimeError> {
    let Some(up) = host.ingest_upstream else {
        return Ok(None);
    };
    let bad = |m: &str| Err(RuntimeError::InvalidConfig(m.into()));
    match network {
        Some("none") => bad("an ingest upstream needs a network; network=none has none"),
        Some("host") => bad(
            "with network=host the cog reaches the node's 127.0.0.1:80 directly; \
             leave ingest_upstream unset",
        ),
        _ if up.ip().is_loopback() || up.ip().is_unspecified() || up.port() == 0 => bad(
            "the ingest upstream must be an address the container can reach \
             (not loopback or unspecified, non-zero port)",
        ),
        _ => Ok(Some(up)),
    }
}

/// Dockerfile for a cog image fronted by the ingest relay.
///
/// `user` is the `(uid, gid)` the cog and relay drop to; the image's
/// `nobody` when `None`.
pub fn dockerfile(base_image: &str, upstream: SocketAddr, user: Option<(u32, u32)>) -> String {
    let (uid, gid) = user.map_or_else(
        || {
            let (u, g) = CONTAINER_USER.split_once(':').unwrap_or(("65534", "65534"));
            (u.to_string(), g.to_string())
        },
        |(u, g)| (u.to_string(), g.to_string()),
    );
    format!(
        "FROM {base_image}\n\
         COPY cog {COG_PATH}\n\
         COPY {RELAY_FILE} {RELAY_PATH}\n\
         RUN [\"python3\", \"-B\", \"-c\", \"import ctypes, os, socket, threading\"]\n\
         USER 0:0\n\
         WORKDIR /\n\
         ENTRYPOINT [\"python3\", \"-B\", \"-u\", \"{RELAY_PATH}\", \"--listen\", \"{COG_INGEST_ADDR}\", \
         \"--upstream\", \"{upstream}\", \"--uid\", \"{uid}\", \"--gid\", \"{gid}\", \"--\", \"{COG_PATH}\"]\n"
    )
}

/// The relay script (see the module docs).
pub const RELAY_SCRIPT: &str = r#"import os, signal, socket, sys, threading

MAX_CONNS = 8
MAX_BYTES = 2 * 1024 * 1024
IDLE_SECS = 30


def log(msg):
    sys.stderr.write("[ingest-relay] %s\n" % msg)


def addr(s):
    host, _, port = s.rpartition(":")
    return host.strip("[]"), int(port)


def parse(argv):
    opts, i = {}, 0
    while i < len(argv) and argv[i] != "--":
        opts[argv[i]] = argv[i + 1]
        i += 2
    cmd = argv[i + 1:]
    if not cmd:
        raise SystemExit("usage: --listen H:P --upstream H:P [--uid N --gid N] -- cmd ...")
    return (addr(opts["--listen"]), addr(opts["--upstream"]),
            int(opts.get("--uid", "65534")), int(opts.get("--gid", "65534")), cmd)


def drop(uid, gid):
    if os.geteuid() == 0:
        os.setgroups([])
        os.setgid(gid)
        os.setuid(uid)
    no_new_privs()


def no_new_privs():
    """prctl(PR_SET_NO_NEW_PRIVS): setuid/setgid bits and file capabilities
    are ignored from here on (Linux only; a no-op elsewhere)."""
    if not sys.platform.startswith("linux"):
        return
    import ctypes
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(38, 1, 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "prctl(PR_SET_NO_NEW_PRIVS)")


def pump(src, dst):
    total = 0
    try:
        while True:
            data = src.recv(65536)
            if not data:
                break
            total += len(data)
            if total > MAX_BYTES:
                log("connection over %d bytes, closed" % MAX_BYTES)
                break
            dst.sendall(data)
    except OSError:
        pass
    for s in (src, dst):
        try:
            s.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass


def serve(conn, upstream, slots):
    try:
        try:
            up = socket.create_connection(upstream, timeout=10)
        except OSError as e:
            log("upstream %s:%d: %s" % (upstream[0], upstream[1], e))
            return
        conn.settimeout(IDLE_SECS)
        up.settimeout(IDLE_SECS)
        back = threading.Thread(target=pump, args=(up, conn), daemon=True)
        back.start()
        pump(conn, up)
        back.join(IDLE_SECS)
        up.close()
    finally:
        conn.close()
        slots.release()


def accept_loop(srv, upstream):
    slots = threading.BoundedSemaphore(MAX_CONNS)
    while True:
        conn, _ = srv.accept()
        if not slots.acquire(blocking=False):
            conn.close()
            continue
        threading.Thread(target=serve, args=(conn, upstream, slots), daemon=True).start()


def main():
    listen, upstream, uid, gid, cmd = parse(sys.argv[1:])
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(listen)
    srv.listen(MAX_CONNS)
    pid = os.fork()
    if pid == 0:
        try:
            srv.close()
            drop(uid, gid)
            os.execv(cmd[0], cmd)
        except BaseException as e:
            log("cog exec failed: %s" % e)
            os._exit(126)
    drop(uid, gid)
    for sig in (signal.SIGTERM, signal.SIGINT):
        signal.signal(sig, lambda s, _f: os.kill(pid, s))
    threading.Thread(target=accept_loop, args=(srv, upstream), daemon=True).start()
    while True:
        try:
            _, status = os.waitpid(pid, 0)
            break
        except InterruptedError:
            continue
    code = os.waitstatus_to_exitcode(status)
    sys.exit(code if code >= 0 else 128 - code)


main()
"#;
