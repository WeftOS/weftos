# ADR-127: WeftOS-orchestrated CI: ephemeral container runners on a CI host, with RustFS-backed caches

- **Status**: Proposed (2026-10-29; owner direction, see Context)
- **Updated**: 2026-10-29 (§3's caches and per-job environments are re-expressed as composed RVF layer stacks in ADR-128)
- **Deciders**: owner
- **Prior decisions**: the decision store, consulted on 2026-10-29, holds only ADR-126 (the mesh object store) on this topic. No earlier ADR covers CI runners.
- **Builds on**:
  - ADR-099: governed placement and the `container.*` adapters;
  - ADR-126: the mesh object store, RustFS, and `weftos://` store names;
  - ADR-120: exposures and the activity log;
  - agent-git-flow: worktrees, land-by-rebase, `slot.sh`;
  - `scripts/diff-gate.sh`: the pre-build gate.

## Context

On 2026-10-29 the owner stopped all development. Agent lanes on the development Mac each ran full test suites in their own worktrees, at the same time. The load average reached 97 on 18 cores and swap filled (58.8 of 60.4 GB). Load-induced timeouts then produced false failures, which lanes re-ran to chase.

The owner's direction, the same day:

- "I want it so the dev work is done on this mac, and then the ci/cd workflow takes over and weftos can orchestrate."
- CI work runs on **the CI host**, not on PG. The host is a Windows workstation already on the tailnet: an Intel i9-14900K (32 threads), 128 GB RAM, a Samsung 9100 PRO 4 TB and a WD SN770 2 TB (both NVMe), and an RTX 4070 12 GB. Measured on 2026-10-29 in its existing WSL distro:
  - WSL2 (kernel 6.18), Debian 12;
  - 16 vCPUs and 31 GB RAM available to WSL;
  - 419 GB free on the root filesystem;
  - load average 0.2.
  
  No container runtime is installed yet. CUDA-on-WSL is present: `nvidia-smi` in `/usr/lib/wsl/lib` reports the RTX 4070 on driver 616.64. WSL is capped at 16 of 32 threads and 31 of 128 GB.
- "We need to build it able to run the containerized workflow and mount the rust s3 buckets so its all very fast."

Two gaps exist today:
- WeftOS lands work on `target-0.8.*` branches, and no workflow runs on pushes there. `pr-gates.yml` triggers only on main, master, `feat/**`, `fix/**`, `chore/**` and `docs/**`. CI therefore isn't the authority on the branch that matters.
- GitHub-hosted runners start cold, with no warm Rust or Node caches.

## Decision

### 1. Where each kind of work runs

| Stage | Where | What |
|---|---|---|
| Dev | the development Mac | Edit, plus a quick pre-push check of only what changed: the diff gate, typecheck, lint and affected tests, in minutes. Full suites never run locally. If one is unavoidable, it goes through agent-git-flow `slot.sh` with one slot. |
| CI | the CI host, through GitHub Actions | Every check: the diff gate first, then build, the full tests, clippy, wasm and size. **A lane is green when CI is green.** Failures are debugged from CI logs, not by re-running locally. |
| Orchestration | WeftOS | Places and scales the runners, queues jobs, routes results back to lanes and the board, and drives landing and retries. |

### 2. Ephemeral container runners as a workload kind

- **New workload kind `ci-runner`.** It's defined here and placed through ADR-099 onto the CI host's `container.podman` adapter, rootless.
  - Each runner is a GitHub Actions self-hosted runner in `--ephemeral` mode: one job, then the container is destroyed.
  - "Healthy" means registered and idle, or running a job.
- **The runner container is the job environment.** Images are built per project from a Containerfile in the repo, so the environment is versioned with the code:
  - `weftos-ci-rust`: the pinned toolchain from `rust-toolchain.toml`, with wasm targets, wasm-bindgen, nextest and sccache;
  - `ci-node`: Node and the Playwright dependencies.

  Workflow steps run directly in it; no nested Docker is needed. `container:` jobs aren't used on these runners.
- **Labels route jobs:** `self-hosted, weftos-ci, <project>`. GitHub-hosted runners stay as the fallback when the CI host is down: the same workflow, slower. The required checks don't change.
- **Capacity and queueing:**
  - The CI host gets a runner-pool limit per class. Each runner gets CPU and memory limits through podman, and WSL2's `.wslconfig` cap is part of the host setup.
    - Starting sizes, on today's WSL cap of 16 vCPUs and 31 GB: `rust-heavy` = 2 runners (about 7 vCPUs and 12 GB each); `node` = 1. The CI distro in §3c raises the cap.
    - The total is held under the WSL cap, leaving the host headroom.
  - Jobs beyond the pool wait in GitHub's queue, which is the queue. WeftOS keeps the pool at its target size.
  - This is the one place ADR-099's "no queueing" stance is relaxed: the placer maintains a pool, not single instances.
- **The GPU is opt-in.** A `gpu` label and a pool of one, using CUDA-on-WSL, which works today. Nothing needs it yet.

### 3. RustFS buckets make cold jobs warm

These are ADR-126 stores, reached from the runner containers over the tailnet:

| Store (ADR-126 name) | Used for | How it's reached |
|---|---|---|
| `projects/<ULID>/store`, prefix `ci/sccache/` | the Rust compile cache | sccache's own S3 backend (`SCCACHE_BUCKET`, `SCCACHE_ENDPOINT`, path style). No mount. |
| the same, prefix `ci/cargo/` | the cargo registry and git cache | an rclone mount at `$CARGO_HOME/registry` and `git` with a local VFS cache, read-mostly. A job that changes `Cargo.lock` writes through. |
| the same, prefix `ci/node/` | the npm cache, and node_modules keyed by lockfile hash | an rclone mount; a node_modules archive per lockfile hash, extracted locally |
| `projects/<ULID>/worktrees/<slug>/store` | per-lane artifacts and test results (JUnit, logs) | uploaded at job end and read by WeftOS for result routing |
| `projects/<ULID>/releases/<channel>/store` | release build outputs | write-once (ADR-126 S4) |

- **Credentials:**
  - Each runner gets a short-lived STS credential, which RustFS supports (ADR-126 S0), minted by the WeftOS daemon for that job and scoped to the project's prefixes.
  - No long-lived S3 key sits in an image or a workflow file.
  - The job ending, or revocation of the runner's identity, ends its access.
- **Speed:** the target is a warm-cache full WeftOS gate well under the GitHub-hosted time. C2 measures the baseline. Data stays on the tailnet.
- **Store placement is open.** The canonical RustFS is on PG (ADR-126). If tailnet latency or bandwidth to PG makes the caches slow, the CI host runs a RustFS **replica or cache node** for `ci/` prefixes only. It has 419 GB free. C2 decides by measurement.

### 3a. Disk: everything on the Linux filesystem, RustFS for warmth not speed

Measured on the CI host on 2026-10-29 with a 1 GiB direct-I/O file, and with small-file create and delete:

| Filesystem | Large write / read | Small files |
|---|---|---|
| WSL2's own ext4 (`/`) | 4.0 / 4.9 GB/s | 20,000 in 0.39 s |
| The Windows drive via `/mnt/c` (9P) | 176 MB/s write | 2,000 in 8.5 s, about 200x slower per file |

- **Every checkout, `target/`, `CARGO_HOME`, node_modules and runner work directory lives on the WSL ext4 filesystem,** in podman's rootless storage under the runner user. Nothing in a job touches `/mnt/c`. The C0 host setup checks this.
- **RustFS keeps caches warm across ephemeral runners and machines.** It is not a substitute for local disk. Over the tailnet it is far slower than 4 GB/s local I/O. So each class gets a **local cache directory on the CI host** that survives runner teardown:
  - sccache's local disk cache in front of its S3 backend;
  - an rclone VFS cache for the cargo and node mounts.
  
  RustFS behind them is the shared and persistent tier. A local RustFS cache node on the CI host (§3) is the next step if C2 shows the PG round trip still dominates.

### 3b. Option: native Linux on a dedicated drive

The owner offered (2026-10-29) to put a separate M.2 drive in the CI host and run it as a native Linux box, built for its hardware, instead of WSL2. What that would gain:
- native CUDA access to the RTX 4070 without the WSL GPU paravirtualisation layer (CUDA-on-WSL works today);
- all 128 GB and 32 threads, with no `.wslconfig` cap (WSL sees 31 GB and 16 threads today);
- no Windows sleep, update or VM-memory interference.

- no virtual-disk bloat. WSL2's `ext4.vhdx` grows as data is written but doesn't return space to Windows when files are deleted, unless the disk is sparse and trimmed. CI churn (ephemeral containers, `target/` directories, caches written and deleted all day) is the worst case. The host's root filesystem already reports 537 GB used of 1 TB.

Disk speed isn't a reason: WSL's ext4 already measures 4-5 GB/s.

Until a move to native Linux, the C0 host setup on WSL2 must handle the bloat:
- set the distro's disk sparse (`wsl --manage <distro> --set-sparse true`, run on Windows by the owner);
- run a scheduled `fstrim -v /` after runner cleanup;
- prune podman images, containers and volumes per class on a schedule;
- hard size caps on the local sccache and rclone VFS caches;
- report the vhdx file size alongside the in-WSL usage, as a host health fact.

Recommendation: start C0 to C2 on WSL2, which works today, and keep the same Terraform host role, so moving to native Linux is a re-provision, not a redesign. Switch when the GPU pool is needed, or when C2 or the vhdx size shows WSL memory, interference or disk-reclaim limits. The owner decides.

### 3c. The CI distro (owner offer, 2026-10-29: a fresh OS picked for this)

- **A new WSL2 distro just for CI:** Ubuntu 26.04 LTS (24.04 if 26.04 isn't offered, then upgraded by Terraform), named `weftos-ci`. It's kept separate from the host's existing distro.
- **Why Ubuntu 26.04:**
  - PG runs the same release, so the Terraform account and package modules reuse unchanged;
  - NVIDIA's CUDA-on-WSL targets Ubuntu first;
  - current rootless podman is in its repositories;
  - systemd is on by default, which user units need.
- **Owner steps on Windows:**
  - install the distro with its vhdx on the Samsung 9100 PRO;
  - `wsl --manage weftos-ci --set-sparse true`;
  - in `.wslconfig` under `[wsl2]`: `memory=96GB`, `processors=28`, `swap=16GB`, `sparseVhd=true`; under `[experimental]`: `autoMemoryReclaim=dropCache`. These are shared by every distro on the host;
  - a sudo user, and Tailscale joined as its own tagged node.
- **Everything after that is Terraform (C0).**
- **Pool sizes with that cap:** `rust-heavy` = 4 runners (about 6 vCPUs and 18 GB each), `node` = 2. The total stays under about 88 GB. Re-measure in C2.

### 4. Client isolation

- **Client projects get their own runner pool,** running as that client's own Linux user on the CI host, with images and caches in that client's own RustFS instance (ADR-126 S5).
- **A client runner never mounts another account's buckets.** WeftOS-account runners never run client jobs.
- **Client jobs register only with that client's repository** and never with the WeftOS org.

### 5. The diff gate first

The pre-build diff gate (`scripts/diff-gate.sh`) is the first job on every trigger. Build jobs `need` it, and on PRs it runs the base branch's copy. No crate builds on a rejected diff.

### 6. Triggers and landing

- **Push triggers:** `pr-gates.yml` and `diff-gate.yml` add push triggers for `target-*` branches. That is an owner-approved CI-workflow change.
- **agent-git-flow:**
  - its "full gate after landing" becomes "wait for CI on the landed commit";
  - its "affected check before landing" stays local and quick.
- **On a red CI,** WeftOS notes the lane, the commit and the failing job on the board card, and the lane fixes it forward. Auto-revert stays agent-git-flow's existing rule.

### 7. Results back

- **WeftOS follows CI** through the GitHub API, or a webhook to a WeftOS exposure: job state, conclusion, and the artifact pointers in the worktree store.
- **Lanes are told the result:** a lane waiting to land gets green or red, plus a link to the failure.
- **The board** gets a short status note.
- **Each runner placement, registration and teardown** is recorded in the activity log.

## Phases

| Phase | Scope | Done when |
|---|---|---|
| C0 | CI host setup through Terraform: an inventory entry, `.wslconfig` limits, rootless podman, a WeftOS daemon and `workload-host` | WeftOS can place a test container on the CI host |
| C1 | The `weftos-ci-rust` image and one ephemeral runner, placed by hand, with GitHub-hosted runners as fallback | a `pr-gates` run on a `target-*` push completes on the CI host |
| C2 | The RustFS caches: sccache on S3, cargo and node mounts, STS per job, and a measurement of PG versus a local cache node | a second run of the same commit shows sccache hits and no registry download, and the timings are recorded |
| C3 | The `ci-runner` workload kind and pools kept by WeftOS, with labels, the diff gate first, and results routed to lanes and the board | runners recycle per job, a lane receives its CI result, the board card updates |
| C4 | The agent-git-flow switch to "wait for CI", with local full gates removed from lane flows | no lane runs a full local suite; the Mac stays below its load target during a multi-lane day |
| C5 | Client runner pools under the client user (first client: by owner decision) | a client job runs isolated, and a WeftOS-account principal can't reach its runners or buckets |

## Consequences

- **The Mac stays a development machine,** and CI becomes the one source of truth for green.
- **Warm caches on our own host should make CI faster** than GitHub-hosted runners.
- **The CI host becomes part of the critical path,** mitigated by GitHub-hosted fallback runners and the unchanged required checks.
- **New moving parts:** runner images, a runner pool and STS minting. Each is phased and testable.
- **Security:** self-hosted runners on a public repository are a known risk, because a fork's PR could run code on the runner.
  - Fork PRs run only on GitHub-hosted runners.
  - Self-hosted labels are restricted to pushes to protected branches and to PRs from the same repository.
  - Runners are ephemeral and hold only job-scoped STS credentials.

## Open questions for the owner

1. **RustFS for CI caches:** keep one canonical store on PG, or run a replica or cache node on the CI host? Decided by the C2 measurement unless you'd rather choose now.
2. **The first client pool** for C5.
3. **Native Linux on a dedicated M.2 drive (§3b),** and when.
