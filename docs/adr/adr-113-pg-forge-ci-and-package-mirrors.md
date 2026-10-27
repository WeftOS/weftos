# ADR-113: A Forgejo forge, CI runners and package mirrors on PG, configured from the control plane

- **Status**: Proposed (2026-10-08). The owner chose Forgejo over GitLab CE on 2026-10-08;
  the open decisions are in §8.
- **Deciders**: owner
- **Builds on**: ADR-108 (projects installed on many machines; desired parameters and node
  actions over the heartbeat), ADR-112 (define once, render into the repo, lock and detect
  drift; this ADR uses the same path for pipelines), the PG primary-node decision
  (photo-gallery holds the main store and is the mesh hub), the Terraform environment
  interface (`~/weavelogic.terraform`, plan → owner approves → apply) and the governed
  placement layer for mesh workloads.

## Context

The owner asked whether PG could run CircleCI and GitLab and act as the primary mirror for
the Rust and npm packages we use, so that deployments can run through pipelines that
WeftOS defines and the control plane configures.

**What we have today:**

- CI runs only on GitHub-hosted runners. `weftos/.github/workflows/` holds 14 workflows
  (`pr-gates`, `release`, `release-gate`, the WASM and docs builds, crate publishing and
  others). Nearly every job uses `ubuntu-latest` or `ubuntu-22.04`, and a few use a matrix.
- The projects promoted to PG (accounts uid 2102 and up, `client_project_init`,
  `internal_git_source`) have checkouts but no CI. Their pipelines, if they have any, run
  wherever their upstream remote lives.
- Every build pulls from crates.io, npm and Docker Hub over the internet. Nothing is cached
  between builds or machines.
- PG has 16 cores, 188 GB of RAM, about 9.9 TB free on `/data1` and no GPU. It runs Podman
  5.7, rootless, and no Docker daemon.

**What the candidates offer** (checked on 2026-10-08):

| Candidate | Fit | Deciding fact |
|---|---|---|
| GitLab CE | Works on one host, but it is heavy | Pulling a GitHub repo in on a schedule (pull mirroring) needs Premium or Ultimate; the free tier only pushes out ([GitLab docs](https://docs.gitlab.com/user/project/repository/mirror)). Its CI language is `.gitlab-ci.yml`, so every existing workflow would have to be rewritten. |
| CircleCI Server | Does not fit | Needs a Kubernetes cluster and an enterprise licence ([CircleCI on-premise](https://support.circleci.com/en/collections/19634774-circleci-on-premise-solutions-server-runner), [pricing](https://circleci.com/pricing/server)). CircleCI's self-hosted runners still need circleci.com to schedule them. |
| **Forgejo + Forgejo Actions** | **Chosen** | Pull mirrors are free ([Forgejo mirrors](https://forgejo.org/docs/v15.0/user/repo-mirror/)). Actions reads GitHub-Actions-style workflows, and `DEFAULT_ACTIONS_URL` resolves `uses:` from `data.forgejo.org`, from github.com, or from local mirrors ([Forgejo Actions admin](https://forgejo.org/docs/v14.0/admin/actions/), [devroom write-up](https://www.devroom.io/2026/03/15/using-github-actions-in-self-hosted-forgejo/)). It also has package and container registries built in. |
| Kellnr | Rust registry | Self-hosted crate registry that also serves as a crates.io proxy cache. SQLite or Postgres. MIT/Apache-2.0 ([docs.rs](https://docs.rs/crate/kellnr/6.0.3)). |
| Verdaccio | npm registry | npm proxy cache with private packages (MIT). |
| `registry:2` in proxy mode | Container image cache | Pull-through cache for Docker Hub and other registries. Forgejo's own registry holds the images we build. |

The owner's direction on 2026-10-08 was: Forgejo, because it is compatible with GitHub, and
the runners can be built into Actions.

## Decision

### 1. PG runs a forge: Forgejo with Forgejo Actions

- Forgejo runs on PG as a rootless Podman service under its own service account,
  `weftforge`, with its data at `/data1/forge/forgejo`. It is reachable on the tailnet only
  and is never exposed publicly. TLS uses the tailnet certificate (decision D3).
- **Where the truth lives, phase 1:** GitHub stays the source of truth for repos that
  already live there, including the public `WeftOS/weftos`. Forgejo pull-mirrors
  each one on a schedule and on webhook. A repo whose upstream is PG itself, such as a
  client project with no GitHub remote, is primary on Forgejo.
- **Phase 2** (decision D1) can make Forgejo primary for private repos and push-mirror
  them out to GitHub. Public releases still ship from GitHub.
- We start with `DEFAULT_ACTIONS_URL = https://github.com` so existing workflows run
  unchanged. Once the actions we use are mirrored into a local `actions/` org, it becomes
  `self`, so a build does not depend on github.com being reachable. The actions we may use
  are a pinned list kept in this repo (§7).

### 2. Runners: one per project account, built into the Actions flow

- Each project on PG gets a `forgejo-runner` that runs as that project's own account (uid
  2102 and up), so one project's jobs cannot read another's home or checkout. Jobs run in
  rootless Podman containers.
- Each runner registers with project-scoped labels (`pg-<slug>`, `linux-x86_64`). It
  accepts jobs only from its own repo or org, never from the instance as a whole.
- **Shared runners** carry the `pg-shared` label and run under a dedicated `weftci`
  account. They are for WeftOS's own public CI. They never get a client repo.
- **Other hosts** register the same way: the Mac (macOS and the release signing lane), the
  OrbStack arm64 VM, and cog0 for armv7. They take jobs only for labels they advertise. This
  is how "runners built into Actions" reaches every architecture we gate on.
- **CircleCI** is out of scope. If a project ever needs CircleCI, it gets self-hosted
  CircleCI runners on its own account, set up in Terraform the same way.

### 3. Package mirrors

All mirrors run under the `weftmirror` account, store their data on `/data1/forge/` and
listen on the tailnet only.

| Ecosystem | Service | Role | How builds use it |
|---|---|---|---|
| Rust crates | Kellnr | crates.io proxy cache and private registry | `.cargo/config.toml` `[source.crates-io] replace-with = "pg"`, plus a registry entry for private crates |
| Rust toolchains | panamax (decision D4) | rustup mirror for the pinned toolchain | `RUSTUP_DIST_SERVER` |
| npm | Verdaccio | npm proxy cache and private scope | `.npmrc` `registry=`, plus scoped `@weave-logic-ai:registry=` |
| Container images | `registry:2` in proxy mode | Docker Hub pull-through cache | Podman `registries.conf` mirror entry |
| Images we build | Forgejo container registry | push target for CI | `podman push forge.<tailnet>/...` |

- Every mirror falls back to the public registry when it is down, and only the
  network-level settings change. Lock files keep their checksums: `Cargo.lock` hashes and
  npm `integrity` fields still verify what was fetched, so a poisoned mirror fails the
  build instead of shipping.
- Publishing private crates and npm packages needs a per-project token. Reads from the
  proxy are open on the tailnet.

### 4. Pipelines are project configuration in the control plane

This follows the ADR-112 apply path.

- **Templates** live in this repo under `ci/templates/<kind>/`, for example
  `rust-workspace`, `node-app`, `cog-cross` and `docs-only`. Each one is a Forgejo/GitHub
  workflow with typed parameters.
- **Selection** is a desired parameter on the project installation: `ci.template`,
  `ci.params.*`, `ci.runner_labels` and `ci.mirrors` (on/off for each ecosystem). It is
  edited in the dashboard project interface, beside the `source.*` rows (dashboard PR #15).
- **Render**: `weft project ci plan|apply` writes `.forgejo/workflows/*.yml` into the
  repo. It writes `.weftos/ci.lock` with the template id, version and the hash of each file
  it wrote, and it writes the mirror bindings (`.cargo/config.toml` fragment and `.npmrc`).
  It writes only files that are absent or that it already owns according to the lock. A
  hand-edited file is reported as drift and never overwritten.
- **Delivery**: the dashboard sends a node action over the ADR-108 heartbeat to the node
  that holds the checkout. That node renders the files and opens a change in Forgejo; it
  does not push straight to the default branch. A person merges.
- **Project facts** (gate command, build script, artifact paths) come from the project's
  `project-context.md`, the same keyed file the agent teams read. The templates hold method
  only.

### 5. Deployments come out of pipelines through the governed placement layer

- A pipeline's deploy job does not SSH to hosts. It calls the WeftOS node action API with a
  short-lived token scoped to one project. The governed placement layer decides where the
  workload runs, either on PG or on a mesh node, and records the decision on the project
  chain.
- Deploy tokens are minted per run by the node that runs the job, from the project's key.
  They are never stored as long-lived Forgejo secrets.
- A pipeline cannot create or change Terraform. Host, account and service changes stay on
  the plan → approve → apply path.

### 6. Provisioning goes through Terraform

New `terraform_data` resources in `~/weavelogic.terraform/environments/`:

| Resource | Creates |
|---|---|
| `pg_forge_accounts` | the `weftforge`, `weftmirror` and `weftci` accounts and the `/data1/forge/*` roots |
| `pg_forgejo` | the Forgejo quadlet or systemd unit, `app.ini`, the admin bootstrap, the mirror schedule |
| `pg_package_mirrors` | the Kellnr, Verdaccio and `registry:2` proxy units |
| `forge_project_runner` (one per project) | runner registration on that project's account, plus the label set |
| `forge_repo_mirror` (one per source) | a Forgejo pull mirror for each `internal_git_source` and promoted project |

- Secrets (the Forgejo admin password, runner registration tokens, mirror tokens) are
  generated on PG and written to root-only files under `/etc/weftos/forge/`. They are read
  from there by the services. They are never put in Terraform state, in argv or in this
  repo, the same way `pg_dashboard_token` is handled.
- Backups: `/data1/forge` joins the existing storage-root backups. The Forgejo dump runs
  nightly.

### 7. Security rules

- **Tailnet-only.** No forge, runner or mirror port listens on a public interface.
- **Runner isolation.** One account per project, rootless containers, and no host socket
  mounted. Jobs from forks or from PRs opened by outsiders never run on a project runner.
- **Allowed actions.** `ci/actions-allowlist.txt` pins every allowed `uses:` to a commit
  SHA. A workflow that references anything else fails the render check.
- **Secrets.** Repo or org secrets stay in Forgejo. The dashboard stores only the names of
  secrets a template needs, never their values.
- **Public versus client.** The public weftos repo's CI never runs on a runner that can see
  client data, and client repos are never mirrored to GitHub (decision D1).

## Consequences

- Builds on PG and on the mesh hit local caches. Repeat dependency fetches drop to
  near-zero, and a GitHub or registry outage no longer stops a build.
- Existing GitHub workflows can run on PG with few or no changes. Gaps in Forgejo Actions'
  compatibility (caching, artifact actions, matrix details) have to be checked workflow by
  workflow in F2. Workflows that need GitHub-only services keep running on GitHub.
- PG becomes a single point of failure for private CI. Backups and the public-registry
  fallback limit the damage, but there is no second forge.
- We own the upkeep: upgrades for Forgejo, the runner, Kellnr and Verdaccio. Forgejo is
  GPL-3.0-or-later; we run it unmodified, so there are no distribution obligations.
- The dashboard gains `ci.*` parameters and a CI section in the project interface.

## Phases

| Phase | Delivers | Done when |
|---|---|---|
| F0 | This ADR accepted; Terraform plan for accounts, roots and Forgejo | the owner approves the plan |
| F1 | Forgejo running; pull mirrors of `WeftOS/weftos` and the PG `internal_git_source` repos | mirrors sync on schedule; the web UI is reachable on the tailnet |
| F2 | `weftci` shared runner; `pr-gates.yml` running on PG against the mirror | one green PR gate on PG, with its timing compared to GitHub |
| F3 | Kellnr, Verdaccio and the image proxy; mirror bindings in `scripts/build.sh` behind an opt-in flag | a cold `scripts/build.sh gate` on PG fetches every crate and npm package through the mirrors |
| F4 | `ci/templates/`, `weft project ci plan|apply`, `.weftos/ci.lock`, dashboard `ci.*` parameters | one promoted project gets its pipeline from the dashboard, rendered as a change and merged |
| F5 | Per-project runners for the promoted projects; Mac, arm64 VM and cog0 runners; deploy jobs through node actions | a merged change in a promoted project deploys through placement, and the decision appears on its chain |

## 8. Open decisions for the owner

- **D1. Source of truth for private repos.** Keep GitHub primary with a Forgejo pull
  mirror, or make Forgejo primary and push-mirror to GitHub? Proposed: GitHub primary in
  phase 1, then revisit per repo; client repos are primary on Forgejo and are never
  mirrored out.
- **D2. GitHub CI after F2.** Keep running `pr-gates` on GitHub as well, or move it to PG
  and keep GitHub only for releases?
- **D3. TLS and name.** Use a tailnet certificate on a host name such as
  `forge.<tailnet>.ts.net`, or an internal CA?
- **D4. Toolchain mirror.** Use panamax for rustup, or rely on the cargo cache and
  pre-installed toolchains on the runners?
- **D5. Access.** Who gets Forgejo accounts beyond the owner, for example teammates who
  already have the dashboard, and is there SSO through the dashboard's identity?
