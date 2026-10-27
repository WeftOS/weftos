# Verifying the Browser WASM Release Bundle

WEFT-405 gives the browser WASM artefact the same supply-chain treatment
as the WASI release (`release-wasi.yml`) and cargo-dist native archives:

| Mechanism | What it proves |
|-----------|----------------|
| **`VERSION.json`** | Tag, workspace version, git SHA, target, build time |
| **`SHA256SUMS` / `*.sha256`** | Content integrity of the downloaded files |
| **GitHub Attestations (sigstore)** | The tarball was built by the `Browser WASM` workflow on a tag in this repo |

There is **no separate browser-only code-signing root**. Integrity is
release attachment + CI provenance (see [ADR-083](../adr/adr-083-browser-wasm-support.md)
§7). Capability signing for skills/panels (ADR-071) is a different surface.

## Release assets (per tag)

On every version tag, `wasm-browser.yml` attaches:

| Asset | Description |
|-------|-------------|
| `clawft-browser-wasm-<tag>.tar.gz` | wasm-bindgen `browser-pkg/` (`.wasm`, JS glue, `VERSION.json`) |
| `clawft-browser-wasm-<tag>.VERSION.json` | Standalone copy of the version manifest |
| `clawft-browser-wasm-<tag>.tar.gz.sha256` | Detached SHA-256 of the tarball |
| `clawft-browser-wasm-<tag>.VERSION.json.sha256` | Detached SHA-256 of the manifest |
| `clawft-browser-wasm-<tag>.SHA256SUMS` | Combined checksums (browser set only) |

WASI (for comparison / parity):

| Asset | Description |
|-------|-------------|
| `clawft-wasm-wasip2-<tag>.wasm` | wasip2 binary |
| `clawft-wasm-wasip2-<tag>.VERSION.json` | Version manifest |
| `clawft-wasm-wasip2-<tag>.wasm.sha256` | Detached SHA-256 |
| `clawft-wasm-wasip2-<tag>.VERSION.json.sha256` | Detached SHA-256 |
| `clawft-wasm-wasip2-<tag>.SHA256SUMS` | Combined checksums (WASI set only) |

## `VERSION.json` schema

```json
{
  "schema": "weftos.wasm-artifact.v1",
  "kind": "browser",
  "target": "wasm32-unknown-unknown",
  "package": "clawft-wasm",
  "workspace_version": "0.6.20",
  "tag": "v0.6.20",
  "git_sha": "…full 40-char SHA…",
  "git_sha_short": "…12-char…",
  "built_at": "2026-07-31T13:43:24Z",
  "name": "clawft-browser-wasm-v0.6.20"
}
```

| Field | Meaning |
|-------|---------|
| `schema` | Stable document type id |
| `kind` | `browser` or `wasi` |
| `target` | Rust target triple used for the build |
| `workspace_version` | Version from root `Cargo.toml` at build time |
| `tag` | Git tag that triggered the release workflow |
| `git_sha` | Full commit SHA of the build |
| `built_at` | UTC timestamp when packaging ran |

The same file is **embedded inside** the tarball at `./VERSION.json` so a
deployed `pkg/` tree remains self-describing after extract.

## Verify checksums

```bash
TAG=v0.6.20   # or: TAG=$(gh release view --repo WeftOS/weftos --json tagName -q .tagName)
REPO=https://github.com/WeftOS/weftos/releases/download/${TAG}

curl -fsSL -o "clawft-browser-wasm-${TAG}.tar.gz" \
  "${REPO}/clawft-browser-wasm-${TAG}.tar.gz"
curl -fsSL -o "clawft-browser-wasm-${TAG}.tar.gz.sha256" \
  "${REPO}/clawft-browser-wasm-${TAG}.tar.gz.sha256"

# GNU coreutils:
sha256sum -c "clawft-browser-wasm-${TAG}.tar.gz.sha256"

# macOS:
shasum -a 256 -c "clawft-browser-wasm-${TAG}.tar.gz.sha256"
```

## Verify sigstore attestation

Same root of trust as native cargo-dist binaries (`scripts/install.sh`,
WEFT-451):

```bash
gh attestation verify "clawft-browser-wasm-${TAG}.tar.gz" \
  --repo WeftOS/weftos
```

This proves the archive was produced by a GitHub Actions run in
`WeftOS/weftos` with the OIDC identity of the release workflow —
not a mirror or a re-uploaded blob from an unrelated actor.

Requires [GitHub CLI](https://cli.github.com/) (`gh`) ≥ 2.49.

## Local packaging (no tag)

After `scripts/build.sh browser` + wasm-bindgen into a directory:

```bash
# Write VERSION.json only
scripts/release/package-wasm-artifact.sh write-version \
  --kind browser \
  --target wasm32-unknown-unknown \
  --out-dir browser-pkg \
  --tag unreleased

# Full release-shaped package (tarball + checksums)
scripts/release/package-wasm-artifact.sh package-browser \
  --pkg-dir browser-pkg \
  --tag v0.0.0-local \
  --out-dir dist/
```

Attestation is CI-only (needs `id-token: write` + `attestations: write`).

## CI sources of truth

| Path | Role |
|------|------|
| [`.github/workflows/wasm-browser.yml`](../../.github/workflows/wasm-browser.yml) | Browser build, package, attest, upload |
| [`.github/workflows/release-wasi.yml`](../../.github/workflows/release-wasi.yml) | WASI build/package/attest (reusable; called from `release.yml` build-wasi — WEFT-476) |
| [`scripts/release/package-wasm-artifact.sh`](../../scripts/release/package-wasm-artifact.sh) | Shared VERSION.json + SHA-256 helper |

## Related

- [deployment.md](deployment.md) — hosting headers, CORS, CSP
- [building.md](building.md) — local browser build
- [docs/deployment/release.md](../deployment/release.md) — full release pipeline
- [ADR-083](../adr/adr-083-browser-wasm-support.md) — browser WASM architecture + signing model
