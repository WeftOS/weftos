# Release Process

> **Public site:** [Fumadocs — Release Process](../src/content/docs/weftos/guides/deployment-release.mdx)
> (WEFT-468 dual surface — see [README.md](./README.md)).

WeftOS releases are tag-driven. Pushing a SemVer-shaped tag (`vX.Y.Z`) to
the `master` branch fans out into workflows that produce binaries,
WASM artifacts, CycloneDX SBOMs, knowledge-base bundles, the crates.io
publish, and the multi-arch Docker image. A gate workflow flips the
release to "prerelease" if the crates.io publish or the Docker image fails.

## Version Numbering

Versions follow [Semantic Versioning](https://semver.org/):

- **MAJOR.MINOR.PATCH** (e.g., `0.8.4`).
- Bump **MAJOR** for breaking API or config changes.
- Bump **MINOR** for new, backwards-compatible features.
- Bump **PATCH** for bug fixes.

Pre-release suffixes use the standard form: `0.8.5-alpha.1`, `0.8.5-rc.1`.

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
# 1. Bump the workspace version, every literal crate version pin, the
#    agent catalog and the lockfiles. The 0.8.4 cut did this with a sed
#    codemod over an explicit file list; the recipe is in
#    docs/plans/release-0.8.4.md section 2.

# 2. Tag the resulting commit.
git tag -a v0.8.5 -m "Release v0.8.5"

# 3. Push the tag (this triggers all release workflows).
git push origin v0.8.5
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

That adds step 20 to the phase gate. Preview without building:

```bash
scripts/build.sh release-dry-run --dry-run
```

## Release Workflows

Several tag-triggered workflows run in parallel (`Release`, `Release SBOM`,
`Release (Knowledge Base)`, `Browser WASM` and `Publish Crates`); `Release
(Docker)` follows a successful `Release`, and a gate workflow sweeps up
afterwards.

### 1. `Release` (cargo-dist) -- `release.yml`

Generated by [`cargo-dist`](https://opensource.axo.dev/cargo-dist/) v0.31.
Reads `[workspace.metadata.dist]` in the root `Cargo.toml` for its target
matrix and produces:

| Asset name pattern | Contents |
|--------------------|----------|
| `clawft-cli-<triple>.tar.gz` | `weft` |
| `clawft-weave-<triple>.tar.gz` | `weaver` |
| `weftos-<triple>.tar.gz` | `weftos` |
| `clawft-gui-egui-<triple>.tar.gz` | Native egui shell (`weft-gui-egui`; WEFT-499) |
| `<archive>.sha256` | Detached SHA-256 per archive |
| `sha256.sum` | Combined SHA-256 list |
| `<package>-installer.sh`, `<package>-installer.ps1` | Shell and PowerShell installers, one pair per package above |
| `<package>.rb` | Homebrew formula (also published to the tap) |
| `dist-manifest.json` | Machine-readable cargo-dist manifest |
| `source.tar.gz` | Source archive |

`<triple>` is one of `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`,
`x86_64-apple-darwin`, `aarch64-apple-darwin` (`.tar.gz`) or
`x86_64-pc-windows-msvc` (`.zip`). Archive names do not contain the version.
Beyond the cargo-dist output the release also carries the signature files
(`weftos-release.json`, `weftos-release.json.sig`), the WASI module, the SBOMs
and `weftos-docs-<tag>.rvf`. Asset names above were read from the v0.8.3
release (`gh release view v0.8.3 -R WeftOS/weftos --json assets`).

Each archive contains its package's binary plus `LICENSE` and `README.md`. The GUI app ships
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

The same setup with `gh` (replace `<REVIEWER_USER_ID>` with the numeric id of
the person who approves releases):

```bash
gh api -X PUT repos/WeftOS/weftos/environments/weftos-cogs --input - <<'EOF'
{"reviewers":[{"type":"User","id":<REVIEWER_USER_ID>}],
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
the other. The `weftos-release-v1` prefix stays as a second layer. The seed
lives only in operator-held storage and in the environment secret; where the
operator keeps it is deliberately not documented in this public repository.
The public half is compiled into `weaver update` for verification.

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
[docs/browser/verification.md](../browser/verification.md).
PR / master pushes still upload the unbound `browser-wasm-pkg` Actions
artifact for docs playground consumption.

### 4. `Release (Knowledge Base)` -- `release-kb.yml`

Builds the RVF knowledge base bundle that powers the docs-site
playground tour guide:

| Asset                    | Notes                                              |
|--------------------------|----------------------------------------------------|
| `weftos-docs-<tag>.rvf`  | Documentation knowledge base (HNSW + segments)     |

Uses `tools/build-kb` (via `scripts/build-kb.sh`) over the docs-site
content and the repository's `docs/*.md` trees, and attaches the result to
the same Release. It waits for the `Release` run to create the release
first (0.8.4 changelog).

### 5. `Publish Crates` -- `publish-crates.yml`

Runs `scripts/publish-crates.sh`, which publishes every `publish = true`
workspace crate to crates.io in dependency order. It runs on stable tags
only (`vX.Y.Z`, no suffix); release candidates are skipped, and
`workflow_dispatch` re-runs it. The script:

1. Verifies (in the workflow) that the tag matches the workspace version.
2. Computes the order from `cargo metadata` (40 crates at v0.8.4; list
   them with `scripts/publish-crates.sh --dry-run`).
3. Skips crate versions already on crates.io, so a re-run is safe.
4. Waits and retries on HTTP 429. crates.io limits new crates to a burst of
   5 then one per 10 minutes, so a first publish of many crates takes hours.

Once all crates land on crates.io, the published rustdoc on docs.rs gets
the WeftOS ecosystem cross-link table because every distributable crate
sets `[package.metadata.docs.rs]` with `all-features = true`.

`clawft-cli` and `clawft-weave` are `publish = false`; they reach users
through the cargo-dist archives, installers and Homebrew, not crates.io.

### 6. `Release (Docker)` -- `release-docker.yml`

Triggered by the `Release` workflow's `workflow_run` event (orchestration
gate: only publish Docker after a successful tag Release). The image
itself is **self-contained** (WEFT-594): a multi-stage Dockerfile compiles
`weft` and `weaver` from the tag checkout — it does **not** download
cargo-dist musl tarballs, so image publish is not coupled to the binary matrix (WEFT-593).

Multi-arch is built on **native runners** (no QEMU for arm64 Rust):

| Runner            | Platform      |
|-------------------|---------------|
| `ubuntu-latest`   | `linux/amd64` |
| `ubuntu-24.04-arm`| `linux/arm64` |

Per-platform digests are merged with `docker buildx imagetools create`
into:

| Image                                        | Architectures               |
|----------------------------------------------|-----------------------------|
| `ghcr.io/weftos/weftos:X.Y.Z` (no `v`) | `linux/amd64`, `linux/arm64`|
| `ghcr.io/weftos/weftos:latest`       | `linux/amd64`, `linux/arm64`|

The workflow does not exclude release-candidate tags, so `latest` can point
at a pre-release.

Post-publish smoke: `GET /api/health` (WEFT-550). See
[`docker.md`](docker.md) for image internals, local builds, and macOS
runtimes (Docker Desktop / OrbStack / Apple container CLI).

### 7. `Release Gate` -- `release-gate.yml`

The supervisor. Triggers on `workflow_run` from `Publish Crates` and
`Release (Docker)`. It only acts when one of those failed, and then:

1. Looks up the GitHub Release for the failing tag.
2. Marks it as `prerelease = true`.
3. Appends a "Release incomplete" footer to the release body, naming
   which workflow failed and linking to the run.

This means: any release that's not flagged "Pre-release" on the GitHub
Releases page passed every leg. Anything flagged as prerelease has at
least one downstream failure -- check the release body for the run
link, fix the issue, and re-run the failed workflow (or push a new
patch tag). The gate never clears the flag again; after a successful re-run,
remove "Pre-release" by editing the release.

## Downloading a Release

### Universal Installer

```bash
curl -fsSL https://raw.githubusercontent.com/WeftOS/weftos/master/scripts/install.sh | sh
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

curl -fsSL -O \
  "https://github.com/WeftOS/weftos/releases/download/${LATEST}/clawft-cli-x86_64-unknown-linux-musl.tar.gz"

tar xzf clawft-cli-x86_64-unknown-linux-musl.tar.gz
./clawft-cli-x86_64-unknown-linux-musl/weft --version
```

(`$VERSION` is not part of the archive name.) `weaver` is in
`clawft-weave-<triple>.tar.gz` and `weftos` in `weftos-<triple>.tar.gz`.

### Verifying Provenance

cargo-dist v0.31 emits a sigstore attestation alongside every archive.
Verify with the GitHub CLI:

```bash
gh attestation verify clawft-cli-x86_64-unknown-linux-musl.tar.gz \
  --repo WeftOS/weftos
```

This proves the archive was built by the `Release` workflow on a tag in
the `WeftOS/weftos` repository -- not pulled from a tampered
mirror.

### Checksums

Each archive has a `.sha256` file beside it, and the release carries a
combined `sha256.sum`. Verify a downloaded archive against it:

```bash
curl -fsSL -O \
  "https://github.com/WeftOS/weftos/releases/download/${LATEST}/sha256.sum"
sha256sum -c sha256.sum --ignore-missing
```

These prove integrity only. The signed `weftos-release.json` is what
`weaver update` trusts; see [Release signature](#release-signature).

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
| `~/.weftos/run/`                            | daemon  | Runtime root of the per-user daemon (`weaver kernel start --profile user`, ADR-103): socket, PID, log, node key, chain |
| `~/.weftos/`                                | user    | Installed apps, models, and per-project child-kernel run directories |
| `<project>/.weftos/runtime/`                | daemon  | Runtime root of a kernel started inside a project -- NOT user-facing |

**Rule of thumb.**

- If a config or artifact belongs to *the user* or *an agent*, it lives
  under `~/.clawft/` (or `<workspace>/.clawft/` for workspace overrides).
- If state belongs to *the running kernel daemon*, it lives under
  `~/.weftos/run/` (the per-user daemon) or `<project>/.weftos/runtime/`
  (a kernel started in a project). Both are daemon-managed and should not
  be hand-edited. The runtime root is resolved by
  `crates/clawft-types/src/runtime_paths.rs` (`$WEFTOS_RUNTIME_DIR`, then
  the project, then the user daemon, then legacy `~/.clawft`).

`scripts/install.sh`, `scripts/deploy/vps-deploy.sh`, and
`scripts/deploy/docker-compose.yml` all use `~/.clawft/` as the host
config path. The container mounts it at `/home/weft/.clawft` and declares
`/home/weft/.weftos` as a second volume for kernel state.

Earlier `~/.nanobot/` paths exist as a read-only fallback for users
migrating from pre-rename builds; new installs always write to
`~/.clawft/`.

## Docker

```bash
docker pull ghcr.io/weftos/weftos:0.8.4     # pinned (no leading v)
docker pull ghcr.io/weftos/weftos:latest    # rolling
```

See [`docker.md`](docker.md) for usage, configuration, and security
guidance.

## Building from Source

Source builds aren't part of the release flow but are sometimes useful
for development or air-gapped deployments.

### Prerequisites

- **Rust 1.95+** (edition 2024; `rust-toolchain.toml` pins `1.95`). Install via
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

The jobs include clippy (`-D warnings`), the test suite, the WASM and
binary size gates, the browser WASM check, regression suite and bundle-size
gate, the voice feature check, the UI lint and type-check, the docs site
build, `cargo audit` and `npm audit`, the agent directory check, the
assessment run, the Docker integration smoke test, and the VS Code panel
build. The list changes; read the `name:` fields in
`.github/workflows/pr-gates.yml` for the current set. `scripts/build.sh gate`
runs the local equivalent.

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
| WEFT-551 | wasmtime / wasmtime-wasi 33.0.2 → 45.0.3 (19 advisory IDs) | **DONE** (wave0c). All 19 RUSTSEC IDs cleared from cargo audit; gate ignores removed. The workspace has since moved on (`wasmtime = "48.0.4"` in `Cargo.toml`; CHANGELOG 0.8.4-rc.15 records 48.0.7). See `docs/plans/wave-0c-WEFT-551-result.md`. |
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
scripts/build.sh gate                # full phase gate including audit
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
`publish = false` but a dependent expects it published, a crate was
renamed and crates.io rejects the new name, or `CRATES_IO_TOKEN` is
missing. Re-run the workflow after fixing the manifest; the script skips
versions already published. The Release Gate has already marked the
release "Pre-release" and does not clear it, so edit the release by hand
once the run is green.

**`Release (Docker)` failed.** The image compiles from the tag checkout and
does not need the `Release` assets; the workflow only waits for `Release`
to succeed. Look at the failing step in the run (Rust build, push to GHCR,
or the post-publish smoke). Re-trigger via `workflow_dispatch` once it is
fixed.

**`Release WASI` failed but `Release` is green.** The WASI build is
independent. The release is still usable for native targets. Fix the
WASI build and re-run; Release Gate only reacts to `Publish Crates` and `Release (Docker)`
failures and never clears the Pre-release flag itself.

**Tag pushed but no workflows ran.** Make sure the tag matches the
trigger pattern (`v[0-9]+.[0-9]+.[0-9]+*` for `release.yml`;
`**[0-9]+.[0-9]+.[0-9]+*` for the KB, SBOM and browser-WASM workflows) and that you pushed the tag
itself (`git push origin <tag>`), not just the branch.
