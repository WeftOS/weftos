# JuiceFS on RustFS spike (2026-10-29)

Throwaway spike. Goal: a shared POSIX filesystem whose data lives in RustFS on PG, mounted
from the Mac, so a large checked-out tree is worked on in place and never copied from remote
to local. No WeftOS code and no Terraform were involved.

## Status: ended by the owner (2026-10-30)

The owner ended the spike after the single-node, Mac and cross-machine measurements above. These phases were started but **not completed or reported**, so no results are claimed for them:
- the concurrent stress phase (parallel builds, git under load, small-file storms, appends, 5 GB read-while-write, a killed mount with fsck after);
- the role split (git on PG, build on the CI host, test on PG or the CI host);
- the Venq read-mostly load;
- the full transparency checks: the same mount path everywhere, and watchers across nodes. Uid behaviour is partly covered under Setup.

Leftover load on the CI host and in the VM was stopped by PID. The CI host, Venq and Mac (NFS) mounts were unmounted. The PG volume A mount, the VM volumes A and B, both Redis instances and the `jfs-spike` bucket are **left in place** for later use; the teardown is below.

## Setup

| Piece | Where | Detail |
|---|---|---|
| JuiceFS 1.4.2 | PG (linux amd64), Linux VM on the Mac (arm64) | Release tarballs verified against the sha256 GitHub publishes. Installed in the spike dir, not system-wide. |
| Metadata A | PG, dedicated `redis-server` on tailnet port 9710 | `requirepass` from a 0600 file, AOF persistence in the spike dir. |
| Metadata B | Linux VM on the Mac, `redis-server` on loopback port 6390 | Same, in the VM user's spike dir. |
| Object store | RustFS 1.0.4 on PG, bucket `jfs-spike` | Dedicated RustFS user with a policy for that bucket only. Keys in 0600 files. |
| Volume A | `jfs-spike`, metadata A | Mounted on PG and in the VM. Default JuiceFS cache settings. |
| Volume B | `jfs-spike-b`, metadata B | Mounted in the VM with `--writeback` and 5 s attr/entry/open caches. Same bucket, separate prefix. |
| Mac access | NFS from the VM | `nfs-kernel-server` exports the VM mounts. Mounted on the Mac as `/Volumes/jfsb` (volume B) and `/Volumes/jfs` (volume A), through NetFS, no root needed. |

Things that did not work, and why:

- **FUSE mount on PG under the spike dir.** The Ubuntu AppArmor profile for `fusermount3`
  only allows mount points under `$HOME`, `/mnt`, `/media`, `/tmp` and `/run/user`. Editing
  the profile is a system change, so PG's mount point is `~weftos/jfs-spike-mnt`. Data and
  cache stay in the spike dir.
- **OrbStack's `~/OrbStack/<vm>/...` path.** It shows the VM's root filesystem and does not
  cross into FUSE mounts. A file written through it landed on the VM's underlying `/mnt/jfs`
  directory, not in the volume. NFS from the VM is the working route without macFUSE.
- **Non-root `mount_nfs` on the Mac.** Refused. `osascript -e 'mount volume "nfs://..."'`
  mounts under `/Volumes` without root.
- **Cross-user permissions.** JuiceFS stores uids, so a directory made by the PG `weftos`
  user (uid 2101) is not writable by the VM user (uid 501) unless the mode is opened.

## Numbers

Network RTT from the Mac to PG is about 56 ms (tailnet). The VM reaches PG through the Mac.
All Mac-side times are wall clock on a Mac that was busy with other agents, so treat small
numbers as noisy. "Local" is a scratch dir on an APFS external volume.

### Mac side (NFS to the VM FUSE mount, volume B, local metadata, data on PG)

| Operation | Local disk | Volume B via Mac | Ratio |
|---|---|---|---|
| `git clone` weftos (5.8k files, 794 MB) | 72 s | 106 s | 1.5x |
| `git status` weftos, first | 0.39 s | 8.9 s | 23x |
| `git status` weftos, warm | 0.09 to 0.22 s | 0.28 to 0.34 s | ~2 to 3x |
| `git checkout` older branch (4k files changed) | 4.0 s | 25.4 s | 6x |
| `git status` right after that checkout | 0.09 s | 2.24 s | 25x |
| Create 10k small files | 8.1 s | 66 s | 8x |
| `git clone --depth 1` Linux kernel (97k files) | 92 s | 393 s | 4.3x |
| `git status` Linux, first | 0.56 s | 78 s | 140x |
| `git status` Linux, warm | 0.31 s | 26.5 s | 85x |

### Inside the VM (native FUSE mount, no NFS)

| Operation | VM local disk | Volume B | Notes |
|---|---|---|---|
| `git status` weftos, first / warm | 0.21 / 0.014 s | 1.13 / 0.11 s | |
| `git status` Linux, first / warm | not run | 27.9 / 0.73 s | NFS adds about 2.8x on the first and 36x on the warm run |
| `cp -a` weftos tree, mount to local disk | | 4.9 s | |
| `juicefs clone` of the whole weftos tree | | 0.41 s | Metadata only. `diff -rq` found 0 differences, 5814 files tracked. |
| `git status` in the cloned tree | | 1.1 s | |
| `cargo check -p weftos-cogbus`, source local, target local | 9.9 s | | cold target |
| `cargo check -p weftos-cogbus`, source on mount, target local | | 7.8 s | cold target, page cache warm for sources |
| `cargo check -p weftos-cogbus`, source on mount, target on mount | | 32.4 s | cold target |
| Same, second run | 0.31 s | 0.29 s | Nothing to rebuild |

### Volume A (metadata on PG, as first specified)

| Operation | Result |
|---|---|
| Create small files, from the VM, native mount | 100 files in 84 s, 0.84 s per file |
| `git clone` weftos | Abandoned after more than 20 minutes in `index-pack`. At 0.84 s per file the tree would take hours. |
| Create small files from the Mac over NFS (sync export) | Worse still: 99 files in about 10 minutes |

`juicefs stats` during the abandoned clone showed 66 metadata ops per second at 66 ms each,
and 1 MB per second of writes. Each file create costs about 15 metadata round trips (Redis
WATCH/MULTI/EXEC transactions plus the slice commit), so the cost is the round trip time
times 15.

### Cross-machine consistency (volume A, PG and the VM, default 1 s caches)

NTP is synchronised on both hosts, so clock skew is under about 50 ms.

| Check | Result |
|---|---|
| VM creates a file, PG sees the name | 330 to 480 ms after the create started |
| VM writes content, PG reads it | Visible at or up to about 0.7 s after the writer's close returns (the 1 s attribute cache on PG) |
| PG creates, VM sees the name | 0 to 400 ms (VM watcher polled continuously) |
| PG writes content, VM reads it | About 0.3 to 0.8 s after the writer's close returns |
| `mv` on PG, VM watching for the new name and the old one gone | Seen 300 ms after `mv` returned |
| `flock` exclusive held on the VM | PG `flock -n` denied. PG blocking `flock` acquired 205 ms after the VM released. Correct. |

## Does git and cargo behave correctly on the mount

Yes. `git clone`, `status`, `checkout` of a branch with 4k changed files, and back all
completed with exit 0. `git status` after checkout was clean. `cargo check` passed with the
source on the mount and with the target dir on the mount. `juicefs clone` produced an
identical tree. Locks and rename work across machines. Correctness was never the issue.

## Bottleneck

Metadata round trips, in two places:

1. **Metadata over the WAN (volume A).** 56 ms RTT times about 15 round trips per file create
   makes file-creating work (clone, checkout, `cargo` writing a target dir) unusable.
2. **NFS to the Mac (volume B).** Local metadata fixes the WAN cost, but every NFS operation
   on the Mac is a stat or lookup that the NFS client does not cache for long. `git status`
   on 97k files is 85x slower than local even with metadata in the VM. Reads and
   already-cached trees are acceptable; stat storms are not.

Data upload to RustFS was not the limit once `--writeback` was on. About 2.7 GB landed in the
bucket for roughly 5 GB of logical data (compression).

## Conclusions

- The "work in place, nothing copied to the Mac" shape works end to end with plain tools:
  JuiceFS plus RustFS plus Redis, Mac access over NFS, no macFUSE.
- Put the metadata engine next to the machine doing the work. Metadata on PG with a
  Mac-side worker is unusable. Metadata on the Mac side with data on PG is usable for a
  weftos-size tree (clone 1.5x local, warm `git status` sub-second, `cargo check` fine).
- Work from inside the VM (native FUSE) rather than through NFS on the Mac whenever a tool
  does many stats: Linux `git status` was 0.73 s in the VM against 26.5 s over NFS.
- The Linux kernel tree is the stress case. It is usable only from the VM.
- The cost of the local-metadata layout is that PG cannot mount volume B at low latency: it
  would pay the 56 ms RTT the other way. For both machines to work on one tree, pick one
  home for the metadata (or a replicated engine) and accept the other side as the remote one.
- `juicefs clone` of a whole checkout in 0.4 s is the strongest feature for agents:
  per-agent working copies of a 5.8k-file tree cost no data and no copy time.
- Mac NFS sync export was too slow for volume A. Volume B is exported `async`, which can
  lose acknowledged writes if the VM dies. Fine for a spike, not for state that matters.
- RustFS bucket usage: about 2.7 GiB at the end. AOF durability on both Redis instances is
  `everysec`.

## Mac native (macFUSE), 2026-10-30

macFUSE 5.4.0, JuiceFS 1.4.2 built from source for this M5 (the official darwin binary segfaults). Both volumes were mounted natively on the Mac, with no NFS and no tunnels. The Mac stayed busy with other agents, so treat the numbers as noisy. The leftover mounts from the NFS phase were already gone after the reboot.

Metadata round trip from the Mac: volume B Redis (in the VM, reached on the Mac's loopback because OrbStack forwards the VM's loopback listener) 0.16 ms; volume A Redis on PG 88 ms at mount time (ICMP to PG 60 to 73 ms). The VM's Redis for volume B had stopped over the reboot and was restarted with its existing config. No bind, auth or firewall change was made.

Mount commands (credentials come from 0600 env files under the jfs-mac dir, `META_URL` and `META_PASSWORD` exported from there):

```
J=<jfs-mac>/bin/juicefs-m5
# volume A (metadata on PG), defaults
$J mount -d --cache-dir <jfs-mac>/cache-a --cache-size 10240 --log <jfs-mac>/log/mount-a.log "$META_URL" <jfs-mac>/mnt/a
# volume B (metadata in the VM), same flags as the VM mount
$J mount -d --cache-dir <jfs-mac>/cache-b --cache-size 20480 --writeback \
  --attr-cache 5 --entry-cache 5 --dir-entry-cache 5 --open-cache 5 \
  --log <jfs-mac>/log/mount-b.log "$META_URL" <jfs-mac>/mnt/b
```

Both mounts are left up. Unmount with `umount` on the mount point, or kill the mount by PID.

### Volume B, same trees under `bench/`

| Operation | Mac over NFS | Mac native macFUSE | VM native | Mac local disk |
|---|---|---|---|---|
| `git status` weftos, first | 8.9 s | 5.1 s | 1.13 s | 0.39 s |
| `git status` weftos, warm | 0.28 to 0.34 s | 2.1 to 2.5 s | 0.11 s | 0.09 to 0.22 s |
| `git status` Linux, first | 78 s | 42 s | 27.9 s | 0.56 s |
| `git status` Linux, warm | 26.5 s | 31 to 34 s | 0.73 s | 0.31 s |
| `juicefs clone` of the weftos tree | not run | 1.7 s | 0.41 s | |
| `git status` in the clone, first | | 473 s | 1.1 s | |
| Create 10k small files | 66 s | 63 s | | 8.1 s |
| Write 256 MB (random data) | | 6.8 s (39 MB/s) | | |

Read throughput was not measured meaningfully: a re-read of the 256 MB file came from the page cache. Volume A was mounted and listed but not benchmarked, because the 88 ms metadata round trip already put it in the unusable band measured earlier (0.84 s per create). `git clone` was not rerun.

Findings:

- Native macFUSE is about the same as NFS for stat-heavy work: faster on the first cold run, no better on warm runs (warm Linux `git status` 31 to 34 s against 26.5 s over NFS), and about 30 to 100x slower than the same operation inside the VM. The cost is the macFUSE kernel round trip per `lstat`, not metadata latency (0.16 ms).
- The first `git status` in a fresh `juicefs clone` took 473 s, because a clone shares chunks but every inode is new and the first pass populates the Mac's caches. In the VM it took 1.1 s.
- Small-file creates did not improve (63 s against 66 s for 10k).
- Working in the VM remains the only usable route for large trees. The Mac mount is acceptable for browsing and for single-file reads and writes.

Not tried: tuning the kernel-side attribute caching (macFUSE `-o` options, longer `--attr-cache`), `git fsmonitor`, or `core.untrackedCache`. These are the next levers if a Mac-side mount is still wanted.

## Mount, unmount, teardown

Paths: `<spike>` is the spike dir on PG under the weftos services tree (`juicefs-spike`).
Commands on PG run as the `weftos` user (`sudo -n -u weftos`).

Mount:

```
# PG, volume A (metadata on PG). Redis must be running first.
sudo -n -u weftos /usr/bin/redis-server <spike>/redis/redis.conf &   # skip if already up
sudo -n -u weftos <spike>/mount.sh                                    # mounts at ~weftos/jfs-spike-mnt

# VM on the Mac
orb -m weftos-arm64 ~/jfs-spike/mount.sh                              # volume A at /mnt/jfs (allow_other)
orb -m weftos-arm64 redis-server ~/jfs-spike/redisb/redis.conf        # skip if already up
orb -m weftos-arm64 ~/jfs-spike/mountb.sh                             # volume B at /mnt/jfsb
orb -m weftos-arm64 sudo exportfs -ra                                 # NFS exports in /etc/exports

# Mac
osascript -e 'mount volume "nfs://<vm-ip>/mnt/jfsb"'                  # appears at /Volumes/jfsb
osascript -e 'mount volume "nfs://<vm-ip>/mnt/jfs"'                   # volume A, /Volumes/jfs
```

The VM address is shown by `orb list`.

Unmount:

```
umount /Volumes/jfsb /Volumes/jfs                                     # Mac
orb -m weftos-arm64 bash -c '~/jfs-spike/bin/juicefs umount /mnt/jfsb; ~/jfs-spike/bin/juicefs umount /mnt/jfs'
sudo -n -u weftos <spike>/bin/juicefs umount ~weftos/jfs-spike-mnt    # PG
```

Teardown (removes everything the spike created):

```
# 1. unmount everything (above), then stop the Redis instances by pid
sudo -n -u weftos kill $(sudo -n -u weftos cat <spike>/run/redis.pid)          # PG
orb -m weftos-arm64 bash -c 'kill $(cat ~/jfs-spike/redisb/redis.pid)'         # VM

# 2. destroy the volumes and the bucket data
sudo -n -u weftos bash -c '. <spike>/env.sh; <spike>/bin/juicefs destroy "$META_URL" <volume-uuid>'
# or remove the bucket directly:
sudo -n -u weftos <services>/rustfs/rcroot.sh rb --force root/jfs-spike

# 3. remove the RustFS user and policy
sudo -n -u weftos <services>/rustfs/rcroot.sh admin user rm root/ jfsspike
sudo -n -u weftos <services>/rustfs/rcroot.sh admin policy rm root/ jfs-spike-rw

# 4. remove files
sudo -n -u weftos rm -rf <spike> ~weftos/jfs-spike-mnt
orb -m weftos-arm64 bash -c 'rm -rf ~/jfs-spike; sudo rmdir /mnt/jfs /mnt/jfsb; sudo sed -i "/jfs/d" /etc/exports; sudo exportfs -ra'
rm -rf /Volumes/ai-models/jfs-spike-local                                       # Mac scratch
```

## Left running at the end of the spike

PG: Redis on port 9710 and the volume A mount. Mac VM: Redis on loopback port 6390, volumes A
and B mounted, NFS server exporting both, and the Mac NFS mounts at `/Volumes/jfs` and
`/Volumes/jfsb`. Volume B holds the weftos clone, a `juicefs clone` copy, the Linux kernel
checkout and a cargo target dir under `bench/`. A `redis-server` on the default port in the
VM was installed by apt as a side effect and is unrelated.
