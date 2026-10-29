# rclone as a shared working filesystem for the mesh

Date: 2026-10-29. Status: research, owner direction. Not built.

## What the owner wants

Several desktops, laptops, people and agents working on the same files, so
swarming can go horizontal across machines:

- gitignored files (models, data, local config, build inputs) are already in place
  on whichever machine an agent lands on;
- a local environment can work against a remote one.

rclone remote credentials come from Terraform as a 0600 `rclone.conf`, the same as
other host secrets. Agents never see them.

## Governance: what we can and cannot control

The owner's rule: we may not be able to stop a transfer that is under way, but every
connection is logged, and a connection that breaks the rules is removed.

| Need | What rclone gives us (rclone.org docs, checked 2026-10-29) | Gap |
|---|---|---|
| Who may connect | `rclone serve webdav/sftp/nfs/s3` with `--htpasswd`, `--authorized-keys`, mTLS via `--client-ca`, or `--auth-proxy`. The proxy is a program that sees each login (user, password or public key, client IP) and returns the backend config, or refuses. | none |
| Per-identity scope | The auth proxy returns a backend per login, with its own `_root`, so each node, person or agent gets its own subtree and read-only flag. | none |
| Logging | `--log-file` plus `--use-json-log`. At INFO, rclone logs each transfer; transfer lines carry `size`. On serve, access logs need `-v`. | The docs don't promise that every rename, delete or mkdir is logged at INFO. Prove it with a test before relying on it. |
| Rate and volume limits | `--bwlimit` (runtime change via `rclone rc core/bwlimit`), `--max-transfer` with a hard cutoff. | Process-wide, not per identity |
| Revoking a connection | The proxy result is cached per user + credential hash + client IP. The docs give no expiry for password or key logins; only S3 keys are rechecked every 5 min. | **rclone alone can't drop one live session.** |

### Design that closes the revocation gap

1. **Front rclone with the WeftOS daemon.** `rclone serve` binds loopback only. The
   daemon publishes it as an ADR-120 exposure, with its on/off switch and activity log,
   and proxies the connection. Switching the exposure off, or a rule hit, closes that
   peer's connection in the daemon. This works the same way as the infer proxy (ADR-101).
2. **The WeftOS daemon is the `--auth-proxy`.** Each login is checked against mesh
   identity and governance, and recorded in the activity log. A revoked identity is
   refused at its next login.
3. **Hard stop:** restarting the serve process drops every cached backend at once. That
   is the fallback when a rule is broken and the daemon proxy isn't in place yet.
4. **Logs** go from the JSON log into the node's activity log (ADR-120 section 5), so
   the dashboard shows who read and wrote what.

A transfer already under way finishes or is cut when its connection closes. That
matches the owner's "may not be able to stop file transfers" expectation.

## Limits that shape how we use it

These come from the `rclone mount` docs.

- **No file locking.** Two writers on the same file: the last one to close wins.
- **Per-client caches.** Each mount has its own VFS cache, and two instances sharing a
  cache dir can corrupt data. Changes from other clients appear only after
  `--dir-cache-time` (default 5 min) or `--poll-interval` (default 1 min), and only on
  backends that support polling.
- **Writes upload on close,** after `--vfs-write-back`.
- **On macOS** the recommended mount is `rclone nfsmount`, with no kext. macFUSE and
  FUSE-T also work. Without `--vfs-cache-mode` an NFS mount is read-only.
- **On Windows** mounting needs WinFsp. `rclone serve` needs nothing extra.

So:

- **Don't** put a live git working tree, `.git`, or a Cargo `target/` on the shared
  mount. Concurrent git operations race on `index.lock`, and builds over the network are
  slow. Each agent keeps its own local worktree (agent-git-flow); git stays the sync
  for tracked files.
- **Do** share the gitignored layer: models, datasets, fixtures, `.env` templates
  (not secrets), generated assets, archives. A per-project shared directory is
  symlinked into each worktree, so those files "are just there".
- **One writer per path.** Give each agent or machine its own subtree for outputs
  (`shared/<project>/agents/<id>/`), and treat the common subtree as read-mostly.
  Overlap is where data gets lost.
- **Build cache:** sccache can use a WebDAV backend, so one `rclone serve webdav` per
  project could hold a shared compile cache across machines. Check the sccache version
  and its WebDAV support before relying on this.

## RustFS as the backing store

RustFS (rustfs.com, github.com/rustfs/rustfs) is an S3-compatible object store
written in Rust and pitched as a MinIO replacement.

- Apache-2.0.
- 1.0.0 GA on 2026-09-16; 1.0.4 on 2026-10-25.
- About 35k stars, active daily.
- Release builds for Linux x86_64 and aarch64 (gnu and musl), macOS aarch64 and
  Windows x86_64, so it covers PG, the Venq, the Mac and the Windows laptop.

Its source tree has `iam`, `policy`, `audit`, `notify`, `lock`, `replication`,
`lifecycle` and `kms` crates, and erasure coding (Reed-Solomon) across drives and
nodes.

Why it matters here: it closes most of the governance gaps rclone has on its own.

| Need | rclone alone | RustFS behind rclone |
|---|---|---|
| Per-identity access | auth proxy, cached login | IAM users or access keys per person, agent or node, plus bucket and prefix policies |
| Per-operation log | INFO transfer lines | audit crate (MinIO-style audit targets): every S3 request, with the identity |
| Revoke | restart the serve process, or the daemon proxy | disable the access key. Each S3 request is signed and checked, so it should be refused at the next request. **Test this.** |
| Watch for changes | polling | `notify` bucket events. Could feed the WeftOS activity log and invalidate rclone caches (`vfs/forget`) |
| Durability | none | erasure coding, replication |

The shape this suggests:
- **RustFS on PG** is the canonical store, one bucket per project.
- **rclone on each machine** uses its S3 backend: `nfsmount` on the Mac, `mount` on
  Linux, WinFsp on Windows, or `bisync` where a mount isn't wanted.
- **Credentials:** Terraform writes each machine's access key into its 0600
  `rclone.conf`.
- **Governance:** the audit targets and bucket notifications go to the daemon's
  activity log. A rule hit disables that access key. RustFS stays bound to the tailnet
  or loopback, and the daemon exposure (ADR-120) provides the on/off switch.

Caveats:
- It has been GA for six weeks. The vendor says to validate the S3 operations you
  depend on; it doesn't claim full parity.
- An object store isn't POSIX. Through an rclone mount, a rename is a copy plus a
  delete, and there's still no cross-client file locking, so the one-writer-per-path
  rule stays.
- Not yet verified: whether audit logs cover every request type, how quickly a disabled
  key is refused, and the RAM footprint (PG is fine; it doesn't belong on the Venq).

## Proposed first slice

1. Run RustFS on PG with a bucket per project under the weftos account (or, without RustFS, `rclone serve webdav` over a per-project directory); either way bound to loopback or the tailnet,
   weftos account, bound to loopback, with the daemon as auth proxy. Connections
   arrive over the tailnet through the daemon exposure.
2. The Mac mounts it with `rclone nfsmount`, and the Venq and Linux with `rclone mount`.
   The Windows laptop uses WinFsp, or `rclone bisync` where a mount isn't wanted.
3. Tests:
   - each create, rename, delete and write shows in the activity log with the identity;
   - switching the exposure off closes a live mount within seconds;
   - a revoked identity is refused at its next login.
4. Client data stays out. Client files keep living under `/data1` as the client user
   (PG isolation rules). No client tree is shared until the owner says so.

## Open questions for the owner

- Which host holds the canonical copy: PG, a cloud bucket (R2 or S3), or both, with
  `rclone sync` between them?
- Is one shared tree per project the right unit (matching the per-project daemon
  topology)?
- Should agents in other environments (cloud sessions) get write access, or only read?
