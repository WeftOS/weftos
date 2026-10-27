# Release Process

> **Public site:** [Fumadocs — Release Process](../src/content/docs/weftos/guides/deployment-release.mdx)
> (WEFT-468 dual surface — see [README.md](./README.md)).

WeftOS releases are tag-driven. Pushing a SemVer-shaped tag (`vX.Y.Z`) to
the `master` branch fans out into five workflows that produce binaries,
WASM artifacts, knowledge-base bundles, the crates.io publish, and the
multi-arch Docker image. A sixth workflow gates the result and flips the
release to "prerelease" if any leg fails.

## Version Numbering

Versions follow [Semantic Versioning](https://semver.org/):

- **MAJOR.MINOR.PATCH** (e.g., `0.6.19`).
- Bump **MAJOR** for breaking API or config changes.
- Bump **MINOR** for new, backwards-compatible features.
- Bump **PATCH** for bug fixes.

Pre-release suffixes use the standard form: `0.7.0-alpha.1`, `0.7.0-rc.1`.

The workspace `Cargo.toml` is the source of truth for the version. The
`Publish Crates` workflow refuses to run if the tag and the workspace
version disagree.

## Publish policy

Every workspace crate carries an explicit `publish = ...` line in its
`Cargo.toml`. This makes the policy grep-able and prevents silent
opt-outs by relying on the cargo default.

- **Default is `publish = true`.** Library crates that other consumers
  (in-tree or downstream) might depend on ship to crates.io.
- **`publish = false` requires an inline `# rationale: ...` comment**
  on the line above. Acceptable rationales today:
  - End-user binary shipped via `cargo-dist` / Homebrew, not crates.io
    (e.g. `clawft-cli`'s `weft`, `clawft-weave`'s `weaver`).
  - Internal build / test-harness tool (e.g. `clawft-casestudy-gen-qsr`,
    `clawft-lsp-extract`).
  - Hardware benchmark binary (`clawft-edge-bench`).

When adding a new crate, copy the policy from a sibling: add either
`publish = true` (preferred) or `publish = false` plus a one-line
`# rationale: ...` comment so future readers do not have to guess.

The `Publish Crates` workflow already iterates `publish = true` crates
in topological order (see [`#4 Publish Crates`](#4-publish-crates----publish-cratesyml)
below); flipping a crate to `publish = true` automatically enrolls it
in the next release.

## Tag, Push, Done

The shipping flow:

```bash
# 1. Bump the workspace version + every distributable crate that uses
#    a literal version (cargo-workspaces version handles this in lockstep).
cargo workspaces version patch  # or minor / major

# 2. Tag the resulting commit.
git tag -a v0.7.0 -m "Release v0.7.0"

# 3. Push the tag (this triggers all release workflows).
git push origin v0.7.0
```

That's the whole local flow. Everything downstream is automation.

### Pre-Tag Checklist

Before pushing the release tag:

- [ ] Move the `## [Unreleased]` block in `CHANGELOG.md` into a dated
      `## [X.Y.Z] - YYYY-MM-DD` section.
- [ ] Add the matching `[X.Y.Z]: ...compare/...` link to the footnote
      block at the bottom of `CHANGELOG.md`.
- [ ] Update the `[Unreleased]: ...compare/X.Y.Z...HEAD` link to point
      at the new tag.
- [ ] Run `scripts/build.sh releases-mdx` to regenerate the
      docs-site Release Notes page from `CHANGELOG.md`.
- [ ] Commit the changelog + regenerated MDX in the same commit as the
      version bump.
- [ ] Run `scripts/build.sh gate` (the phase gate) and confirm green.
- [ ] Run `scripts/build.sh release-dry-run` (or
      `scripts/build.sh gate --with-release-dry-run`) to rehearse the
      cargo-dist host-triple packaging locally before the tag push
      (WEFT-460). See [Local release dry-run](#local-release-dry-run)
      below.
- [ ] Confirm the `weftos-cogs` environment exists and holds the
      `WEAVELOGIC_RELEASE_KEY` secret. The `host` job signs the release
      with it and fails without it, and a required reviewer must approve
      that job (see [Release signature](#release-signature)).
- [ ] Tag with a `v` prefix (`vX.Y.Z`): `release.yml` triggers only on
      `v[0-9]+.[0-9]+.[0-9]+*`, and the signing script accepts the same tags.

The release tag should point at the commit that contains the dated
changelog entry, not the version-bump commit immediately before it.

### Local release dry-run

cargo-dist only runs on tag push in CI. To catch packaging breakage on
the host triple *before* pushing a tag:

```bash
# Requires cargo-dist matching [workspace.metadata.dist] cargo-dist-version
# (currently 0.31.0):
#   cargo install --locked cargo-dist --version 0.31.0

scripts/build.sh release-dry-run
```

What it does:

1. Resolves the host triple (`rustc -vV`) and asserts it is listed in
   `[workspace.metadata.dist] targets`.
2. Runs `dist build --artifacts=local --target <host>` (same local
   artifact class CI builds per matrix leg).
3. Verifies under `target/distrib/` that each dist-enabled app produced
   a non-trivial archive + checksum, and that the archive contains the
   expected binary plus `LICENSE` / `README.md`:

| Archive                                      | Binary inside   |
|----------------------------------------------|-----------------|
| `clawft-cli-<host>.tar.gz`                   | `weft`          |
| `clawft-weave-<host>.tar.gz`                 | `weaver`        |
| `weftos-<host>.tar.gz`                       | `weftos`        |
| `clawft-gui-egui-<host>.tar.gz`              | `weft-gui-egui` |

This is a full `profile = dist` (inherits release / LTO) build for the
host only — typically several minutes. It does **not** cross-compile
the rest of the matrix (that remains CI's job).

Optional phase-gate integration (off by default so a normal `gate` run
stays fast):

```bash
scripts/build.sh gate --with-release-dry-run
# or
GATE_RELEASE_DRY_RUN=1 scripts/build.sh gate
```

That adds check 17 to the phase gate. Preview without building:

```bash
scripts/build.sh release-dry-run --dry-run
```

## Release Workflows

Five tag-triggered workflows run in parallel; one gate workflow sweeps up
afterwards.

### 1. `Release` (cargo-dist) -- `release.yml`

Generated by [`cargo-dist`](https://opensource.axo.dev/cargo-dist/) v0.31.
Reads `[workspace.metadata.dist]` in the root `Cargo.toml` for its target
matrix and produces:

| Asset name pattern                                      | Target                          |
|---------------------------------------------------------|---------------------------------|
| `weft-cli-{version}-x86_64-unknown-linux-gnu.tar.gz`    | Linux glibc x86_64              |
| `weft-cli-{version}-aarch64-unknown-linux-gnu.tar.gz`   | Linux glibc ARM64               |
| `weft-cli-{version}-x86_64-unknown-linux-musl.tar.gz`   | Linux musl x86_64 (static)      |
| `weft-cli-{version}-aarch64-unknown-linux-musl.tar.gz`  | Linux musl ARM64 (static)       |
| `weft-cli-{version}-x86_64-apple-darwin.tar.gz`         | macOS Intel                     |
| `weft-cli-{version}-aarch64-apple-darwin.tar.gz`        | macOS Apple Silicon             |
| `weft-cli-{version}-x86_64-pc-windows-msvc.zip`         | Windows x86_64                  |
| `weft-cli-installer.sh`                                 | Universal POSIX shell installer |
| `weft-cli-installer.ps1`                                | Windows PowerShell installer    |
| `clawft-gui-egui-{version}-<triple>.tar.gz`             | Native egui shell (`weft-gui-egui`) per target (WEFT-499) |
| `clawft-gui-egui-installer.sh`                          | Shell installer for the GUI binary |
| `clawft-gui-egui.rb`                                    | Homebrew formula (tap publish) |
| `dist-manifest.json`                                    | Machine-readable manifest       |

Each CLI archive contains the `weft` (and where applicable `weaver`,
`weftos`) binaries plus `LICENSE` and `README.md`. The GUI app ships
as a **separate** cargo-dist package (`clawft-gui-egui`) whose archive
contains `weft-gui-egui` (production shell only — `weft-demo-lab` is
opt-in and not packaged). `github-attestations = true` is set, so each
archive ships with a sigstore provenance attestation that can be
verified with `gh attestation verify`.

#### Release signature

Before `gh release create`, the `host` job runs
`scripts/release/sign-release.sh artifacts <tag>`. It writes two more
release assets:

| Asset | Content |
|---|---|
| `weftos-release.json` | `{"schema":1,"kind":"weftos-release","tag":"v…","published":"<UTC RFC 3339>","assets":{"<file>":"<sha256>",…}}` covering every file uploaded, `dist-manifest.json` included. `weaver update` warns when the latest release's `published` is more than 90 days old. |
| `weftos-release.json.sig` | Hex Ed25519 signature over `weftos-release-v1\n` followed by the bytes of `weftos-release.json` |

The key is the `WEAVELOGIC_RELEASE_KEY` secret: the raw 32-byte Ed25519 seed
as hex, stored in the `weftos-cogs` environment. It is a dedicated release
key, separate from the COG-008 cog-signing key. Before signing, the script
checks that the key's public half equals the pinned
`WEFTOS_RELEASE_PUBKEY_HEX` in
`crates/clawft-weave/src/commands/update_signature.rs` (`8ac2a301…`).
It then verifies the signature it made. If the secret is missing or wrong,
the job fails and nothing is published, because `weaver update` refuses an
unsigned release anyway. To check a release by hand with OpenSSL 3, put the
pinned key in `pub.pem` (`MCowBQYDK2VwAyEA` followed by the base64 of the 32
key bytes, inside PEM `PUBLIC KEY` armour), then:

```bash
{ printf 'weftos-release-v1\n'; cat weftos-release.json; } > msg
xxd -r -p weftos-release.json.sig > sig
openssl pkeyutl -verify -pubin -inkey pub.pem -rawin -in msg -sigfile sig
```

**Protecting the key: the `weftos-cogs` environment.** The `host` job declares
`environment: weftos-cogs`, so GitHub only hands it secrets stored in that
environment. A repository administrator must set this up once, in the
repository's Settings, Environments:

1. Create an environment named `weftos-cogs`.
2. Under deployment branches and tags, allow only tags matching `v*`.
3. Add at least one required reviewer. Every release then waits for a
   person to approve the `host` job before the key is exposed.
4. Store `WEAVELOGIC_RELEASE_KEY` as an environment secret there, and
   delete any repository-level copy, so no other workflow or branch can
   read it.

The same setup with `gh` (steps 1–3 were done this way on 2026-10-03; the
reviewer is the account `aepod`, id 124563):

```bash
gh api -X PUT repos/WeftOS/weftos/environments/weftos-cogs --input - <<'EOF'
{"reviewers":[{"type":"User","id":124563}],
 "deployment_branch_policy":{"protected_branches":false,"custom_branch_policies":true}}
EOF
gh api -X POST repos/WeftOS/weftos/environments/weftos-cogs/deployment-branch-policies \
  -f name='v*' -f type=tag
# Step 4: prompts for the 64-hex seed; never pass it on the command line.
gh secret set WEAVELOGIC_RELEASE_KEY --env weftos-cogs -R WeftOS/weftos
```

**Dedicated key.** Releases are signed with their own key (generated
2026-10-03), not the COG-008 cog-signing key, so a cog signature can never pass
as a release signature or the reverse, and a leak of one key does not expose
the other. The `weftos-release-v1` prefix stays as a second layer. The seed is
kept in the operator's macOS Keychain (service `weftos-release-key`) with a
0600 copy at `~/.config/weftos/weftos-release.key`, and as the environment
secret; the public half is `~/.config/weftos/weftos-release.pub`.

Local GUI build (not just release CI):

```bash
scripts/build.sh native --gui     # weft + weaver + weft-gui-egui
scripts/build.sh gui-egui         # GUI binary only
```

This project does **not** emit `.deb` / `.dmg` installers; cargo-dist
is configured for `tar.gz` archives plus shell / Homebrew installers
for every dist app (CLI and GUI alike).

The Homebrew tap (`weave-logic-ai/homebrew-tap`) is auto-updated by the
`publish-jobs = ["homebrew"]` step in the same workflow.

### 2. `wasm32-wasip2` (WASI) — `build-wasi` in `release.yml`

**WEFT-476.** cargo-dist v0.31–v0.32 still does not list `wasm32-wasip2`
in its known target triples (only legacy `wasm32-wasi` and
`wasm32-unknown-unknown`), and `clawft-wasm` is a cdylib rather than a
cargo-dist binary package — so wasip2 cannot yet live in
`[workspace.metadata.dist].targets` (HP-16 / ADR-044).

Instead, the main `Release` workflow runs a parallel `build-wasi` job
that calls the reusable [`.github/workflows/release-wasi.yml`](../../.github/workflows/release-wasi.yml)
workflow. That job stages GHA artifacts named
`artifacts-wasi-wasm32-wasip2`; the `host` job requires `build-wasi`
success on publish and includes those files in `gh release create`
alongside cargo-dist archives. Wall-clock is therefore
`max(native matrix, wasi)` rather than `native + wasi + poll`.

| Asset | Target | Notes |
|-------|--------|-------|
| `clawft-wasm-wasip2-<tag>.wasm` | `wasm32-wasip2` | Server-side WASM via wasmtime |
| `clawft-wasm-wasip2-<tag>.VERSION.json` | — | Tag, git SHA, workspace version (WEFT-405) |
| `clawft-wasm-wasip2-<tag>.wasm.sha256` | — | Detached SHA-256 |
| `clawft-wasm-wasip2-<tag>.VERSION.json.sha256` | — | Detached SHA-256 |

Packaging uses `scripts/release/package-wasm-artifact.sh package-wasi`.
The reusable workflow also attaches GitHub Attestations (sigstore
provenance via `actions/attest-build-provenance`), matching native
cargo-dist archives.

**Fallback:** `workflow_dispatch` on `release-wasi.yml` re-attaches WASI
assets to an existing GitHub Release without re-running the full
cargo-dist matrix. Tag-push is not a trigger (scoped down by WEFT-476).

**Pipeline-time savings (WEFT-476):** the pre-0.8 path waited up to 30
minutes for `release.yml` to create the GitHub Release before
`gh release upload`. Folding WASI into `host` removes that poll
entirely (~0–30 min saved per tag, typically ~15 min) and builds WASI
in parallel with native targets.

**Upstream revisit:** when cargo-dist adds `wasm32-wasip2` (and cdylib
WASM packaging), add the triple to `[workspace.metadata.dist].targets`
and retire the hand-patched `build-wasi` job.

### 3. `Browser WASM` -- `wasm-browser.yml`

Builds `clawft-wasm` for `wasm32-unknown-unknown` with the `browser`
feature, runs `wasm-bindgen`, and on version tags attaches the same
version + checksum + attestation pipeline as WASI (WEFT-405):

| Asset | Notes |
|-------|-------|
| `clawft-browser-wasm-<tag>.tar.gz` | wasm-bindgen package (`browser-pkg/`, includes embedded `VERSION.json`) |
| `clawft-browser-wasm-<tag>.VERSION.json` | Standalone version manifest |
| `clawft-browser-wasm-<tag>.tar.gz.sha256` | Detached SHA-256 of the tarball |
| `clawft-browser-wasm-<tag>.VERSION.json.sha256` | Detached SHA-256 of the manifest |

Consumer verification steps live in
[docs/browser/verification.md](/docs/browser/verification.md).
PR / master pushes still upload the unbound `browser-wasm-pkg` Actions
artifact for docs playground consumption.

### 4. `Release (Knowledge Base)` -- `release-kb.yml`

Builds the RVF knowledge base bundle that powers the docs-site
playground tour guide:

| Asset                 | Notes                                              |
|-----------------------|----------------------------------------------------|
| `weftos-docs.rvf`     | Documentation knowledge base (HNSW + segments)     |

Uses `tools/build-kb` to walk `docs/src/content/docs/` and emit a single
RVF file. Attached to the same Release.

### 5. `Publish Crates` -- `publish-crates.yml`

Publishes every `publish = true` workspace crate to crates.io, in
dependency-topological order, via `cargo-workspaces`. The job:

1. Verifies the tag matches the workspace version.
2. Resolves a publish order across ~25 internal crates.
3. Skips already-published versions (idempotent on re-run).
4. Treats "version already exists" as success, anything else as failure.

Once all crates land on crates.io, the published rustdoc on docs.rs gets
the WeftOS ecosystem cross-link table because every distributable crate
sets `[package.metadata.docs.rs]` with `all-features = true`.

### 6. `Release (Docker)` -- `release-docker.yml`

Triggered by the `Release` workflow's `workflow_run` event (orchestration
gate: only publish Docker after a successful tag Release). The image
itself is **self-contained** (WEFT-594): a multi-stage Dockerfile compiles
`weft` from the tag checkout — it does **not** download cargo-dist musl
tarballs, so image publish is not coupled to the binary matrix (WEFT-593).

Multi-arch is built on **native runners** (no QEMU for arm64 Rust):

| Runner            | Platform      |
|-------------------|---------------|
| `ubuntu-latest`   | `linux/amd64` |
| `ubuntu-24.04-arm`| `linux/arm64` |

Per-platform digests are merged with `docker buildx imagetools create`
into:

| Image                                        | Architectures               |
|----------------------------------------------|-----------------------------|
| `ghcr.io/weftos/weftos:vX.Y.Z`       | `linux/amd64`, `linux/arm64`|
| `ghcr.io/weftos/weftos:latest`       | `linux/amd64`, `linux/arm64`|

Post-publish smoke: `GET /api/health` (WEFT-550). See
[`docker.md`](docker.md) for image internals, local builds, and macOS
runtimes (Docker Desktop / OrbStack / Apple container CLI).

### 7. `Release Gate` -- `release-gate.yml`

The supervisor. Triggers on `workflow_run` from `Publish Crates` and
`Release (Docker)`. If either of those failed, the gate:

1. Looks up the GitHub Release for the failing tag.
2. Marks it as `prerelease = true`.
3. Appends a "Release incomplete" footer to the release body, naming
   which workflow failed and linking to the run.

This means: any release that's not flagged "Pre-release" on the GitHub
Releases page passed every leg. Anything flagged as prerelease has at
least one downstream failure -- check the release body for the run
link, fix the issue, and re-run the failed workflow (or push a new
patch tag).

## Downloading a Release

### Universal Installer

```bash
curl -fsSL https://weftos.weavelogic.ai/install.sh | sh
```

This script (also at `scripts/install.sh` in the repo) detects platform,
fetches the matching cargo-dist tarball, and installs `weft`, `weaver`,
and `weftos` into `/usr/local/bin/` (override with `WEFTOS_INSTALL_DIR`).

### Direct Download

Download a specific archive from the
[Releases page](https://github.com/WeftOS/weftos/releases). For
example, the latest Linux musl x86_64 build:

```bash
LATEST=$(gh release view --repo WeftOS/weftos --json tagName -q .tagName)
VERSION="${LATEST#v}"

curl -fsSL -o weft.tar.gz \
  "https://github.com/WeftOS/weftos/releases/download/${LATEST}/weft-cli-${VERSION}-x86_64-unknown-linux-musl.tar.gz"

tar xzf weft.tar.gz
./weft-cli-${VERSION}-x86_64-unknown-linux-musl/weft --version
```

### Verifying Provenance

cargo-dist v0.31 emits a sigstore attestation alongside every archive.
Verify with the GitHub CLI:

```bash
gh attestation verify weft.tar.gz \
  --repo WeftOS/weftos
```

This proves the archive was built by the `Release` workflow on a tag in
the `WeftOS/weftos` repository -- not pulled from a tampered
mirror.

### Checksums

Each release also publishes a SHA-256 manifest. Verify a downloaded
archive against it:

```bash
curl -fsSL -o sha256sums \
  "https://github.com/WeftOS/weftos/releases/download/v0.6.19/sha256sums"
sha256sum -c sha256sums --ignore-missing
```

## Installing from a Release

`scripts/install.sh` is the canonical install path. If you'd rather
install manually:

```bash
# Linux / macOS
sudo install -m 755 weft /usr/local/bin/weft
sudo install -m 755 weaver /usr/local/bin/weaver
sudo install -m 755 weftos /usr/local/bin/weftos
weft --version

# User-local (no sudo)
mkdir -p ~/.local/bin
install -m 755 weft ~/.local/bin/weft
```

On Windows, extract the `.zip`, rename `weft.exe` if needed, and add the
target directory to `PATH`.

## Canonical Install Paths

WeftOS uses two on-disk root namespaces. They serve different purposes
and do not interchange. **`~/.clawft/` is the canonical install path
for everything user-facing.**

| Path                                        | Owner   | Purpose                                                                           |
|---------------------------------------------|---------|-----------------------------------------------------------------------------------|
| `~/.clawft/`                                | user    | Config, sessions, memory, skills, agents, workspace registry, identity files     |
| `~/.clawft/config.json`                     | user    | Global agent + LLM provider config (with `~/.nanobot/` legacy fallback)          |
| `~/.clawft/agents/<id>/`                    | user    | Per-agent isolation: dedicated `SOUL.md`, `AGENTS.md`, `USER.md`, sessions       |
| `~/.clawft/skills/`                         | user    | Skills installed via `weft skill install`                                        |
| `~/.clawft/workspaces.json`                 | user    | Registry of known workspaces                                                     |
| `~/.clawft/keys/`                           | user    | Ed25519 keys for skill signing (`weft skills keygen`)                            |
| `<workspace>/.clawft/`                      | project | Workspace-scoped overrides: `SOUL.md`, `IDENTITY.md`, journal, per-project skills|
| `.weftos/runtime/`                          | daemon  | Kernel runtime state (cluster peers, revoked hosts, paired hosts) -- NOT user-facing |

**Rule of thumb.**

- If a config or artifact belongs to *the user* or *an agent*, it lives
  under `~/.clawft/` (or `<workspace>/.clawft/` for workspace overrides).
- If state belongs to *the running kernel daemon*, it lives under
  `.weftos/runtime/` next to the project that owns the daemon. This
  directory is daemon-managed and should not be hand-edited.

`scripts/install.sh`, `scripts/deploy/vps-deploy.sh`, and
`scripts/deploy/docker-compose.yml` all use `~/.clawft/` as the host
config path. The container mounts it at `/home/weft/.clawft`. The
`.weftos/` namespace appears only inside running-daemon working
directories and is not part of any install or deploy flow.

Earlier `~/.nanobot/` paths exist as a read-only fallback for users
migrating from pre-rename builds; new installs always write to
`~/.clawft/`.

## Docker

```bash
docker pull ghcr.io/weftos/weftos:v0.6.19   # pinned
docker pull ghcr.io/weftos/weftos:latest    # rolling
```

See [`docker.md`](docker.md) for usage, configuration, and security
guidance.

## Building from Source

Source builds aren't part of the release flow but are sometimes useful
for development or air-gapped deployments.

### Prerequisites

- **Rust 1.93+** (edition 2024). Install via
  [rustup](https://rustup.rs/) and run `rustup update`.
- **Git**.

### Build

```bash
git clone https://github.com/WeftOS/weftos.git
cd weftos
scripts/build.sh native   # or: scripts/build.sh native-debug for fast iteration
```

The binary lands at `target/release/weft`. `scripts/build.sh --help`
covers the other subcommands (`wasi`, `browser`, `ui`, `gate`,
`release-dry-run`, `test`, `check`, `clippy`, `all`). For a local
cargo-dist packaging rehearsal on the host triple, see
[Local release dry-run](#local-release-dry-run).

### Cross-Compilation

To build for a target other than the host:

```bash
rustup target add aarch64-unknown-linux-musl
scripts/build.sh native --target aarch64-unknown-linux-musl
```

### WASM Builds

```bash
# Server-side WASM (wasip2)
scripts/build.sh wasi

# Browser WASM
scripts/build.sh browser
```

See [`wasm.md`](wasm.md) for runtime instructions and the size budgets
enforced in CI.

## CI gates

`pr-gates.yml` is the merge gate. Every job listed below is **required**;
none of them are allowed to skip on failure (no `|| true`, no
`::warning::` fallback for "feature not yet implemented"). A red gate
blocks merge.

| Job                              | Owner          | Notes                                                                 |
|----------------------------------|----------------|-----------------------------------------------------------------------|
| `Clippy lint`                    | clippy         | `-D warnings` workspace-wide.                                          |
| `Test suite`                     | cargo test     | Full workspace.                                                        |
| `WASM size gate`                 | wasm-size      | Asserts wasip2 binary < 300 KB raw / 120 KB gzipped.                  |
| `Binary size check`              | binary-size    | Asserts release `weft` < 10 MB.                                        |
| `Browser WASM check`             | wasm-browser-check | **Hard gate (WEFT-447)**: `cargo check` for `wasm32-unknown-unknown`, no warning fallback. |
| `Browser WASM regression suite`  | browser-wasm-tests | Headless Chrome via `wasm-pack` (M5-A / WEFT-388).                  |
| `Browser WASM bundle size`       | browser-wasm-bundle-size | Post-bindgen `_bg.wasm` budget gate (WEFT-389).               |
| `Voice feature check`            | voice-feature-check | Compilation check for the `voice` feature on `clawft-plugin`.      |
| `UI lint and type-check`         | ui-check       | Skipped only when `clawft-ui/` is absent.                              |
| `Docs site build`                | docs-build     | **WEFT-448**: Fumadocs Next.js build; runs only on `docs/src/**` changes. |
| `Cargo audit`                    | cargo-audit    | Mirrors `CARGO_AUDIT_IGNORES` in `scripts/build.sh`.                   |
| `Cargo Check`                    | check          | Fast workspace `cargo check`.                                          |
| `Assessment`                     | assess         | Runs `weft assess run --scope ci`.                                     |
| `Integration smoke test`         | smoke-test     | **WEFT-550**: builds Docker image, starts `weft gateway`, probes `/api/health` with a 30s deadline; fails on first-3s crash. |

To make any of these jobs **required for merge** in repository
settings, add the job name to `Settings -> Branches -> master ->
Require status checks to pass`. Job names match the `name:` field in
`pr-gates.yml`.

## Security audits

`scripts/build.sh gate` and the `pr-gates.yml` CI workflow both run
[`cargo audit`](https://crates.io/crates/cargo-audit) against the
workspace `Cargo.lock` on every gate run. The audit fails the build if
RustSec reports a new vulnerability or warning advisory that is not in
the explicit ignore-list.

The ignore-list lives in two places that must stay in sync:

- `scripts/build.sh` — `CARGO_AUDIT_IGNORES` array.
- `.github/workflows/pr-gates.yml` — `cargo-audit` job's
  `--ignore` flags.

Each ignored advisory is grouped in a comment by the followup Plane work
item that tracks the eventual fix:

| Followup | Cluster | Why deferred |
|----------|---------|--------------|
| WEFT-551 | wasmtime / wasmtime-wasi 33.0.2 → **45.0.3** (19 advisory IDs) | **DONE** (wave0c / `wave0c/weft-551-wasmtime-bump`). 45.0.3 is the highest on MSRV 1.93; 46+ needs rustc 1.94. All 19 RUSTSEC IDs cleared from cargo audit; gate ignores removed. See `docs/plans/wave-0c-WEFT-551-result.md`. |
| WEFT-552 | rustls-webpki via rustls / reqwest / quinn alignment (3 IDs) | **DONE** (wave0c) — ruvector-core 2.3 + pin rustls-webpki ≥0.103.13. |
| WEFT-553 | unmaintained crates + unsound `rand` (6 IDs) | **PARTIAL** (wave0i / `wave0i/weft-553-audit-deps`). Cleared: `serial` (portable-pty 0.9), `instant` (notify 8), `rustls-pemfile` 1 (gone with WEFT-552), `rand` ≥0.8.7/0.9.5. Residual ignores: `bincode` (RUSTSEC-2025-0141 via ruvector/hnsw_rs), `paste` (RUSTSEC-2024-0436 via tokenizers/egui_dock). See `docs/plans/wave-0i-WEFT-553-result.md`. |

The cold-run report from the gate's introduction is at
`.planning/reviews/0.7.0-release-gate/audit-findings/cargo-audit-cold-run-2026-04-28.md`.

### When a new advisory lands

1. Re-run `cargo audit` (or `scripts/build.sh audit`) to confirm the new
   ID and which crate brings it in.
2. If a `Solution:` line is offered and the bump is contained, fix it in
   the next PR — preferred path.
3. If the fix requires a coordinated multi-crate upgrade, file a Plane
   work item against the closest milestone (`0.7.x` if it blocks ship,
   `0.8.x` otherwise) using the labels
   `ws02-kernel,security,audit-finding`. Cite the advisory ID, the
   crate(s), and the report file.
4. Add the RUSTSEC-ID to **both** ignore-lists (`scripts/build.sh` and
   `.github/workflows/pr-gates.yml`), with a comment naming the
   followup WEFT-N. Keep the two lists identical.
5. When the followup lands, drop the IDs from both ignore-lists in the
   same commit so the gate tightens automatically.

### Running the audit locally

```bash
cargo install --locked cargo-audit   # one-time
scripts/build.sh audit               # workspace audit with current ignore-list
scripts/build.sh gate                # full 12-check gate including audit
```

`scripts/build.sh audit` is also useful before bumping a dependency: run
it, change the dep, run it again, and diff.

## Troubleshooting a Failed Release

**`host` is waiting.** The `weftos-cogs` environment needs a reviewer to
approve the job before it can read the signing key.

**`host` failed at "Sign release".** `WEAVELOGIC_RELEASE_KEY` is missing
from the `weftos-cogs` environment, is not 64 hex characters, or does not match
the pinned public key. Fix the
secret and re-run the workflow. Do not publish an unsigned release by
hand: `weaver update` refuses it.

**`Publish Crates` failed.** Most common causes: a crate has
`publish = false` but a dependent expects it published, or a crate was
renamed and crates.io rejects the new name. Re-run the workflow after
fixing the manifest. The Release Gate will clear the prerelease flag
once the workflow goes green.

**`Release (Docker)` failed.** The image build pulls the musl tarball
from the upstream `Release` workflow's GitHub Release; if `Release`
hadn't finished yet, the gate skips Docker. Re-trigger via
`workflow_dispatch` once `Release` is green.

**`Release WASI` failed but `Release` is green.** The WASI build is
independent. The release is still usable for native targets. Fix the
WASI build and re-run; Release Gate will re-evaluate prerelease status
on the next downstream success.

**Tag pushed but no workflows ran.** Make sure the tag matches the
trigger pattern (`v[0-9]+.[0-9]+.[0-9]+*` for `release.yml`;
`**[0-9]+.[0-9]+.[0-9]+*` for the KB, SBOM and browser-WASM workflows) and that you pushed the tag
itself (`git push origin <tag>`), not just the branch.
