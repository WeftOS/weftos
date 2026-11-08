# Handoff to `weftos-pg`: the Linux bwrap side of the store scoped view (2026-11-08)

From the Mac session on branch `wt/adr126-e1-scoped-view` (owner item E-1). The interface spec is
[`docs/architecture/store-scoped-view.md`](../architecture/store-scoped-view.md). Read sections 2, 4,
6.2, 7 and 8 first. This note says what to build on Linux, what already exists, and what not to do.

## What is done here

- **Library.** `crates/weftos-store/src/drive/view.rs` and `drive/view/` (`spec`, `lease`, `tree`,
  `work`). It provides:
  - `ScopedView::open`, `load`, `refresh`, `publish`, `resolve`, `renew`, `status` and `close`;
  - the `view.json` manifest;
  - the single-writer lease at `.drive/leases/<hash>.json`;
  - typed errors for every failure in spec section 7.

  It is built on the ADR-131 drive. No new dependency.
- **Tests.**
  - 23 unit tests on the in-memory backend: `drive::view::tests` and `drive::view::tests_fail`.
  - One `#[ignore]` live test, `drive::view::live_tests::live_view_open_refresh_publish_close`. It
    passed against a throwaway RustFS on the Mac's loopback.
- **Not done here.**
  - The `weaver store view` verbs (specified in spec 6.3; not built).
  - Anything that runs bwrap, overlayfs or Landlock. The Mac has none of them.
  - Overlay whiteouts are detected in code (a character device 0/0 in the upper means a delete) but
    have never been exercised, because the Mac cannot create one.

## What to build on Linux

1. **A launcher shim that reads `view.json`** and builds the bwrap command from `binds`, in order.
   For a view whose base is a root image:

   ```
   bwrap \
     --ro-bind <root>/base / \
     --bind <root>/work /work \
     --dev /dev --proc /proc --tmpfs /tmp \
     --unshare-all --die-with-parent --new-session \
     -- <landlock-launcher> -- <command>
   ```

   `base/` already contains the mount points listed in `mount_points` (always `work`, plus what the
   spec asked for, for example `dev`, `proc`, `tmp`), because a read-only bind cannot get new
   directories. For a project view, not a root image, bind the host's read-only system directories
   as usual. Put `base/` under a chroot path such as `/view`, and prefix every `dest` with it. For
   `work_start: "empty"`, the second bind is the overlay entry:

   ```
   --overlay-src <root>/lower --overlay <root>/work <root>/.state/ovl-work /work
   ```

   Rules:
   - Never bind `<root>/.state` or `<root>/view.json`.
   - Bind nothing that is not in `binds`.
   - Refuse a manifest whose `schema` is not 1, or whose paths are not under `root`.
2. **Landlock inside the bwrap child.** Allow write access only beneath `/work`, plus `/tmp` if it
   is a tmpfs. Allow read and execute beneath `/`. Record the Landlock ABI version you got. The
   host survey found that PG blocks a bare `unshare`, so Landlock goes inside the bwrap child (seed
   drive plan, item 5).
3. **The refresh trigger.** The supervisor asks the daemon for a refresh (`store.view.refresh` with
   `{root}`) on a timer, and before a command that should see fresh base content. It never refreshes
   by touching the files itself. Changes to `base/` appear live through the read-only bind. For
   `empty`, the daemon does not refresh the work prefix while the overlay is mounted
   (`work_not_refreshed: true`). That is by design: overlayfs forbids changes to a mounted lower.
4. **The checks to run and record**, on `weftos-ci` (the CI host) with a throwaway RustFS of your
   own:
   - **C2 isolation, native:**
     - `/` cannot be written;
     - nothing outside `/work` can be written (Landlock);
     - `.state` is not visible;
     - no network with `--unshare-net`.
   - **Overlay mode end to end:**
     - edit, create, `rm` (a whiteout) and `rm -r` of a directory (whiteout of a directory) in
       `/work`;
     - leave the session;
     - `publish`, then check the store: puts, delete, tombstone.
   - **`rmdir d && mkdir d` in the session** (an opaque directory). Record what publish does. The
     library does not read the opaque xattr, so expect the lower files under `d` to survive (spec
     section 7, open question 4).
   - **The `clone` route**, the same checks, on a host without unprivileged overlayfs.
   - **Hard links.** `<root>/base` and `<root>/.state/drives/*` must be on the same file system,
     otherwise the mirror falls back to copies. Record whether it did (compare inode numbers).

## What to report back

- The bwrap and Landlock versions, and the kernel. The exact command line used.
- Pass or fail for each check above, with the command and its output.
- Overlay: the publish report (puts, deletes, conflicts, `conditional`) and the store listing
  afterwards.
- Step times for open, refresh, publish and close. Note the host's load.
- Anything in `view.json` the launcher needed and did not have.

Write the results to `docs/grok-build-fork/spike-results/` (or the Tapestry repo's equivalent) and
send the path back. Do not paste credentials or env files.

## What not to do

- **No mounts on production.** No FUSE, no overlay and no bwrap on PG's production paths. Run on
  `weftos-ci` or a throwaway directory.
- **No restart of PG's RustFS**, and no writes to it. Use a throwaway RustFS with its own data
  directory and random credentials in a 0600 file, killed by PID afterwards.
- **No PG credentials on the Mac**, and none in a message. Refer to a credential file by path.
- **No edits to this branch's files, and no push of it.** Report back and let the lead merge.
- **No entries in `config/diff-gate-approvals.toml`.** If overlay mode needs `libc` for the opaque
  xattr, that is an owner decision (spec open question 4).

## How to run the library tests

```
scripts/build.sh test weftos-store --features rustfs --filter view
WEFTOS_STORE_TEST_RUSTFS_ENV=<0600 env file of a throwaway RustFS> \
  NEXTEST_SUCCESS_OUTPUT=final \
  scripts/build.sh test weftos-store --features rustfs --ignored --filter live_view
```

The env file holds `MESH_S3_ENDPOINT`, `MESH_S3_ACCESS_KEY`, `MESH_S3_SECRET_KEY` and
`MESH_S3_REGION`. The view module is `#[cfg(unix)]`, so it builds on Linux unchanged.
