---
name: weftos-build-deploy
description: Build, test, publish, and deploy WeftOS across all channels (GitHub Releases, crates.io, npm, Docker, Homebrew)
triggers:
  - deploy
  - release
  - publish
  - build and deploy
  - cut a release
  - publish crates
  - push to npm
  - docker build
  - tag and release
---

# WeftOS Build & Deploy Skill

You are the WeftOS release engineer. You handle builds, tagging, publishing, and deployment across all distribution channels.

## Project Identity

- **Repo**: `WeftOS/weftos` (GitHub)
- **Binaries**: `weft` (CLI), `weaver` (operator), `weftos` (kernel daemon)
- **Naming**: `clawft-*` = framework crates, `weftos` = product facade
- **Workspace version**: single version in `[workspace.package]` in root `Cargo.toml`

## Distribution Channels

| Channel | Package | Registry/Location |
|---------|---------|-------------------|
| **GitHub Releases** | 3 binaries x 5 platforms + installers | `WeftOS/weftos/releases` |
| **crates.io** | 10 crates (8 clawft-*, 2 weftos-*) | `crates.io` |
| **npm** | `@weftos/core` (WASM browser module) | `npmjs.com` |
| **Docker** | `ghcr.io/weftos/weftos` | GHCR (multi-arch amd64+arm64) |
| **Homebrew** | `clawft-cli.rb`, `clawft-weave.rb`, `weftos.rb` | `weave-logic-ai/homebrew-tap` |

## Secrets & Auth

| Secret | Location | Purpose |
|--------|----------|---------|
| `HOMEBREW_TAP_TOKEN` | GitHub repo secret | Push formulae to homebrew-tap |
| `CRATES_API_TOKEN` | `.env` (local) | `cargo publish` to crates.io |
| `NPM_TOKEN` | `.env` (local) | `npm publish` to npmjs.com (needs "Bypass 2FA" permission) |

**NEVER commit `.env` or expose tokens in logs.**

## Common Tasks

### Full Release (new version)

This is the most common operation. It publishes everywhere.

1. **Bump version**:
   - Edit `[workspace.package] version` in root `Cargo.toml`
   - Run `cargo check --workspace` to verify
   - Commit: `git add Cargo.toml Cargo.lock && git commit -m "chore: bump workspace version to X.Y.Z"`
   - Push: `git push origin master`

2. **Tag** (triggers cargo-dist automatically):
   ```bash
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

3. **Monitor** (~12-14 min for all platforms):
   ```bash
   gh run list --repo WeftOS/weftos --limit 2
   gh run view <RUN_ID> --repo WeftOS/weftos
   ```

4. **Verify release**:
   ```bash
   gh release view vX.Y.Z --repo WeftOS/weftos
   ```

5. **Publish to crates.io** (manual, not automated by CI):
   ```bash
   source .env && cargo login "$CRATES_API_TOKEN"
   ```
   Publish in dependency order with ~10 min gaps (crates.io rate limit for new crates):
   ```
   1. clawft-types        (leaf)
   2. clawft-platform     (-> types)
   3. clawft-plugin       (leaf)
   4. clawft-llm          (-> types)
   5. exo-resource-tree   (leaf)
   6. clawft-core         (-> types, platform, llm, plugin)
   7. clawft-kernel       (-> core, types, platform, plugin, exo-resource-tree)
   8. weftos              (-> all above)
   ```
   Command: `cargo publish -p <crate-name>`

   Rate limit: ~1 new crate per 10 min window. Existing crate version bumps are not rate-limited.

6. **Publish to npm** (manual):
   ```bash
   source .env
   echo "//registry.npmjs.org/:_authToken=${NPM_TOKEN}" > ~/.npmrc
   wasm-pack build crates/clawft-wasm --scope weftos --target web --features browser
   ```
   Fix `package.json` in `crates/clawft-wasm/pkg/`:
   - Ensure `name` is `@weftos/core`
   - Ensure `repository.url` is `https://github.com/WeftOS/weftos`
   - Ensure `version` matches the release
   ```bash
   cd crates/clawft-wasm/pkg && npm publish --access public
   ```

### Patch Release (hotfix)

Same as full release but only bump patch version (e.g., 0.1.1 -> 0.1.2).

### Re-tag (if a release fails mid-pipeline)

```bash
git tag -d vX.Y.Z                    # delete local tag
git push origin :refs/tags/vX.Y.Z    # delete remote tag
# fix the issue, commit, push to master
git tag vX.Y.Z                       # re-create tag on new HEAD
git push origin vX.Y.Z               # triggers fresh release
```

### Publish Only crates.io (no binary release)

If you only changed library code and need to update crates.io without a binary release:
```bash
source .env && cargo login "$CRATES_API_TOKEN"
cargo publish -p <crate-name>
```
No tag needed. Version in Cargo.toml must be bumped and not already published.

### Publish Only npm (WASM update)

```bash
source .env
echo "//registry.npmjs.org/:_authToken=${NPM_TOKEN}" > ~/.npmrc
wasm-pack build crates/clawft-wasm --scope weftos --target web --features browser
# Edit pkg/package.json: name=@weftos/core, version, repo URL
cd crates/clawft-wasm/pkg && npm publish --access public
```

### Verify Installation

```bash
# Binary (download from release)
gh release download vX.Y.Z --repo WeftOS/weftos \
  --pattern 'clawft-cli-aarch64-unknown-linux-gnu.tar.gz' --dir /tmp/test
tar xzf /tmp/test/clawft-cli-*.tar.gz -C /tmp/test
/tmp/test/clawft-cli-*/weft --version
/tmp/test/clawft-cli-*/weft status
/tmp/test/clawft-cli-*/weft tools list

# Homebrew
brew install weave-logic-ai/tap/clawft-cli
weft --version

# Docker
docker pull ghcr.io/weftos/weftos:X.Y.Z
docker run --rm ghcr.io/weftos/weftos:X.Y.Z --version

# crates.io
cargo install weftos
weftos --version

# npm
npm info @weftos/core version
```

## What cargo-dist Does Automatically (on tag push)

1. **Plan** — reads `[workspace.metadata.dist]`, resolves targets
2. **Build** — compiles 3 binaries on 5 platforms:
   - `x86_64-unknown-linux-gnu` (~5 min)
   - `aarch64-unknown-linux-gnu` (~5 min, cross-compiled)
   - `x86_64-apple-darwin` (~14 min, slowest)
   - `aarch64-apple-darwin` (~7 min)
   - `x86_64-pc-windows-msvc` (~10 min)
3. **Global artifacts** — SHA256 checksums, shell + PowerShell installers
4. **Host** — creates GitHub Release with all artifacts
5. **Homebrew** — pushes `.rb` formulae to `weave-logic-ai/homebrew-tap`
6. **Announce** — marks release as published

**Docker** runs in parallel via `release-docker.yml` (~2h for multi-arch QEMU build).

## crates.io Dependency Graph

```
weftos-rvf-crypto 0.3.0  (our ML-DSA fork — depends on rvf-types)
weftos-rvf-wire 0.2.0    (our fork — depends on rvf-types 0.2)
rvf-types 0.2.0           (upstream ruvnet)
rvf-runtime 0.2.0         (upstream ruvnet)
ruvector-cluster 2.0.6    (upstream ruvnet)
ruvector-raft 2.0.6       (upstream ruvnet)
ruvector-replication 2.0.6 (upstream ruvnet)
cognitum-gate-tilezero 0.1.1 (upstream ruvnet)

clawft-types 0.1.x       (leaf)
clawft-platform 0.1.x    (-> clawft-types)
clawft-plugin 0.1.x      (leaf)
clawft-llm 0.1.x         (-> clawft-types)
exo-resource-tree 0.1.x  (leaf)
clawft-core 0.1.x        (-> types, platform, llm, plugin)
clawft-kernel 0.1.x      (-> core, platform, types, plugin, exo-resource-tree,
                           weftos-rvf-crypto, weftos-rvf-wire, rvf-types,
                           rvf-runtime, ruvector-*, cognitum-gate-tilezero)
weftos 0.1.x             (facade -> kernel, core, types, platform, exo-resource-tree)
```

## Cargo.toml Key Sections

### Workspace version (must match tag)
```toml
[workspace.package]
version = "X.Y.Z"
repository = "https://github.com/WeftOS/weftos"
```

### Workspace deps (version + path for publishability)
```toml
clawft-types = { version = "X.Y.Z", path = "crates/clawft-types", default-features = false }
```

### cargo-dist config
```toml
[profile.dist]
inherits = "release"

[workspace.metadata.dist]
cargo-dist-version = "0.31.0"
ci = "github"
targets = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
]
installers = ["shell", "powershell", "homebrew"]
tap = "weave-logic-ai/homebrew-tap"
publish-jobs = ["homebrew"]
github-attestations = true
```

## Gotchas & Hard-Won Knowledge

1. **Tag must match workspace version** — `v0.1.1` tag requires `version = "0.1.1"` in Cargo.toml. cargo-dist errors with "nothing to Release" otherwise.

2. **`[profile.dist]` must exist** — cargo-dist uses this profile, not `release`. Missing = "profile dist is not defined" error.

3. **`repository` URL in Cargo.toml** — cargo-dist bakes this into installer shell scripts as the download URL. Wrong URL = installer downloads from nonexistent repo. Always verify after repo renames.

4. **Windows builds** — `clawft-weave` (weaver) daemon uses Unix sockets and `nix` signals. All gated behind `#[cfg(unix)]`. Windows gets stub `DaemonClient::connect()` that returns `None` (query commands fall through to ephemeral kernel). The `nix` crate is under `[target.'cfg(unix)'.dependencies]`.

5. **LICENSE file** — must exist at repo root with exact name `LICENSE`. The glob `LICENSE*` in cargo-dist include config fails because cargo-dist treats it literally.

6. **crates.io rate limit** — max ~1 new crate per 10-minute window. Version bumps of existing crates are not limited. Plan publish order accordingly.

7. **npm 2FA** — requires granular access token with "Bypass 2FA on publish" enabled for the `@weftos` scope. Regular tokens get 403.

8. **Docker multi-arch** — takes 30-60 min (arm64 via QEMU emulation). Runs in parallel with cargo-dist, doesn't block the binary release.

9. **weftos-rvf-crypto / weftos-rvf-wire** — our forks of upstream `rvf-crypto` / `rvf-wire` published under new names because we don't own the upstream crate names on crates.io. If upstream catches up with ML-DSA support, we can switch back to their names.

10. **Workspace deps need version + path** — `{ version = "X.Y.Z", path = "crates/..." }` for crates.io publishability. Path alone works locally but `cargo publish` rejects it.

## Docker

- **Base image**: `gcr.io/distroless/cc-debian12` (no shell, minimal attack surface)
- **Multi-stage build**: cargo-chef for dep caching -> builder -> distroless runtime
- **Platforms**: linux/amd64, linux/arm64
- **Tags**: `X.Y.Z`, `X.Y`, `X`, `latest`
- **Health check**: `weft status` (exec form, no shell)
- **Registry**: `ghcr.io/weftos/weftos`

## Platform Support

| Platform | Binary | Docker | WASM | Status |
|----------|--------|--------|------|--------|
| Linux x86_64 | Yes | Yes | - | Shipping |
| Linux aarch64 | Yes | Yes | - | Shipping |
| macOS Intel | Yes | - | - | Shipping |
| macOS Apple Silicon | Yes | - | - | Shipping |
| Windows x86_64 | Yes | - | - | Shipping (weaver daemon limited) |
| Browser | - | - | Yes | Shipping (@weftos/core) |
| Linux x86_64 musl | Planned | - | - | v0.2 |
| Linux aarch64 musl | Planned | - | - | v0.2 |
| Windows ARM | Planned | - | - | v0.2 |
| WASI (wasip2) | Planned | - | - | v0.2 |
