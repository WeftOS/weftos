# CDN Assets — rolling release + SHA snapshots

> **WEFT-454.** The docs site playground loads browser WASM + the RVF
> knowledge base from a GitHub Releases rolling tag (`cdn-assets`).
> Every upload also stamps a commit-SHA sibling so a bad clobber can be
> rolled back without rebuilding.

## Surfaces

| Surface | Role |
|---------|------|
| GitHub Release tag `cdn-assets` | Rolling asset store (prerelease) |
| [`.github/workflows/docs-assets.yml`](../../.github/workflows/docs-assets.yml) | Builds KB, pulls browser WASM, publishes |
| [`scripts/release/cdn-snapshot.sh`](../../scripts/release/cdn-snapshot.sh) | SHA stamp + retain-N prune + rollback |
| [`scripts/pull-assets.sh`](../../scripts/pull-assets.sh) | Local dev fetch (rolling or `--sha`) |
| [`docs/src/app/api/cdn/[...path]/route.ts`](../src/app/api/cdn/%5B...path%5D/route.ts) | Vercel proxy (CORS + edge cache) |

## Asset naming

### Rolling (live pointers)

These are what the production docs site loads by default. Every publish
overwrites them with `--clobber`:

| File | Content |
|------|---------|
| `clawft_wasm_bg.wasm` | Browser WASM binary |
| `clawft_wasm.js` | wasm-bindgen glue |
| `weftos-docs.rvf` | Docs knowledge base |
| `browser-wasm-pkg.tar.gz` | Full pkg tarball |
| `cdn-manifest.json` | Current SHA + asset map (audit trail) |

### SHA snapshots (rollback trail)

Alongside every upload, the same bytes are also stored under the
**12-char short SHA** of the commit that produced them:

| File | Content |
|------|---------|
| `clawft_wasm-{sha}.wasm` | Snapshot of the WASM binary |
| `clawft_wasm-{sha}.js` | Snapshot of the JS glue |
| `weftos-docs-{sha}.rvf` | Snapshot of the KB |
| `browser-wasm-pkg-{sha}.tar.gz` | Snapshot of the tarball (when present) |

Example URL:

```
https://github.com/WeftOS/weftos/releases/download/cdn-assets/clawft_wasm-a5124115b4fa.wasm
```

## Lifecycle / retention

Default retain window: **last 10 SHA groups** (`CDN_RETAIN=10`).

On every publish:

1. Stage rolling + SHA-stamped files + `cdn-manifest.json`.
2. `gh release upload … --clobber` (rolling names overwrite; SHA names are unique).
3. List SHA-stamped assets on the release, ordered by `createdAt` newest-first.
4. Delete asset files belonging to groups beyond the retain window.

Override retain:

```bash
# CI: workflow_dispatch input "retain", or env CDN_RETAIN
# Local / ops:
scripts/release/cdn-snapshot.sh prune --tag cdn-assets --retain 5
```

There is **no** separate `cdn-assets-history` companion release. Snapshots
live on the same rolling tag so the proxy origin stays a single URL.

## Cache-bust strategy (how the docs site decides which SHA to load)

### Production default — rolling + edge cache

1. The docs playground requests `/api/cdn/wasm/clawft_wasm_bg.wasm` (and
   the JS / KB siblings) on the Vercel deployment.
2. The API route proxies
   `https://github.com/…/releases/download/cdn-assets/<filename>`.
3. Response cache headers:

   | Directive | Value | Effect |
   |-----------|-------|--------|
   | `s-maxage` | 604800 (7d) | Vercel edge caches the body |
   | `stale-while-revalidate` | 86400 (1d) | Serve stale while refreshing |
   | `max-age` | 3600 (1h) | Browser cache is short |

Because the **filename is stable**, a publish does **not** automatically
invalidate the edge cache. After a bad deploy:

- Wait out `s-maxage`, **or**
- Redeploy the docs site (new deployment ID resets edge cache), **or**
- Pin to a SHA-stamped filename (below).

### Pinning a known-good SHA

**Ops rollback of the rolling pointer** (preferred for production):

```bash
# List retained snapshots
scripts/release/cdn-snapshot.sh list --tag cdn-assets

# Restore rolling filenames from a SHA snapshot (clobbers live pointers)
scripts/release/cdn-snapshot.sh rollback --sha a5124115b4fa
```

After rollback, browsers pick up the restored bytes within `max-age`
(1h); edge within `s-maxage` unless you also redeploy docs.

**Local / staging pin** without touching the rolling release:

```bash
scripts/pull-assets.sh --sha a5124115b4fa
```

**Proxy path pin** (no redeploy of assets; needs a client that requests
the stamped name):

```
/api/cdn/wasm/clawft_wasm-a5124115b4fa.wasm
/api/cdn/wasm/clawft_wasm-a5124115b4fa.js
/api/cdn/kb/weftos-docs-a5124115b4fa.rvf
```

Optional env on the docs deployment:

| Env | Effect |
|-----|--------|
| `CDN_ORIGIN` | Override the GitHub Releases base URL |
| `CDN_SHA` | When set, the proxy rewrites rolling filenames to the SHA-stamped siblings |

### Audit trail

`cdn-manifest.json` on the release always reflects the **last successful
publish** (or the last `rollback`):

```json
{
  "schema": "weftos.cdn-assets.v1",
  "git_sha": "…",
  "git_sha_short": "a5124115b4fa",
  "published_at": "2026-07-31T14:18:03Z",
  "rolling": { "wasm": "clawft_wasm_bg.wasm", "js": "clawft_wasm.js", "kb": "weftos-docs.rvf" },
  "snapshot": {
    "wasm": "clawft_wasm-a5124115b4fa.wasm",
    "js": "clawft_wasm-a5124115b4fa.js",
    "kb": "weftos-docs-a5124115b4fa.rvf"
  }
}
```

## CI wiring

[`docs-assets.yml`](../../.github/workflows/docs-assets.yml) runs after
**Browser WASM** succeeds on `master` (or on `workflow_dispatch`):

1. Checkout the workflow_run head SHA (so snapshot SHA matches the binary).
2. Download `browser-wasm-pkg` / build KB.
3. `scripts/release/cdn-snapshot.sh publish --sha $FULL_SHA …`.

Manual KB-only refresh:

```
workflow_dispatch → skip_wasm=true
```

## Operator cheat sheet

```bash
# Dry-run a publish locally (no gh upload)
scripts/release/cdn-snapshot.sh publish \
  --sha "$(git rev-parse HEAD)" \
  --wasm-dir browser-pkg \
  --kb weftos-docs.rvf \
  --dry-run

# List snapshots currently on the release
scripts/release/cdn-snapshot.sh list

# Roll production back to a known SHA
scripts/release/cdn-snapshot.sh rollback --sha <12-char-sha>

# Pull a snapshot into docs/src/public for local playground
scripts/pull-assets.sh --sha <12-char-sha>
```

## Acceptance criteria (WEFT-454)

- [x] Each upload also writes `clawft_wasm-{sha}.wasm` (and KB equivalent)
      alongside the rolling filename.
- [x] Lifecycle / cache-bust strategy documented (this page).
- [x] Rolling release retains the last N SHA-stamped artifacts
      (`CDN_RETAIN`, default 10) — no separate history release required.
