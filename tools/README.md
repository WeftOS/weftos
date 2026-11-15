# Out-of-workspace tools

This directory holds small, CI-only Rust binaries that are **intentionally not**
members of the root Cargo workspace (`Cargo.toml` at the repo root).

## Why not in the workspace? (WEFT-461)

| Concern | Workspace member | Out-of-workspace (`tools/*`) |
|---------|------------------|------------------------------|
| Build graph | Pulled into every `cargo check` / clippy / gate | Built only when a workflow or script needs it |
| Lockfile | Shares root `Cargo.lock` + rustup channel | Own `Cargo.lock` + own `[workspace]` table |
| Deps | Must resolve against the full ~40-crate graph | Pins only what the tool needs |
| CI cache | Tied to workspace fingerprint | Keyed on `tools/<name>/Cargo.lock` alone |

Moving these tools into the workspace would:

1. Couple KB / docs-asset jobs to the full workspace compile graph (minutes
   of extra cold work for a ~500-line utility).
2. Force shared resolution of tool-only crates (`rvf-types`,
   `weftos-rvf-wire` from crates.io, etc.) against the monorepo lockfile —
   a cross-cutting topology change for no product-code benefit.
3. Expand the build matrix / `scripts/build.sh` surface for tools that only
   run on release and docs-CDN paths.

**Decision (WEFT-461):** keep `tools/*` outside the workspace; document the
contract here and in [`docs/deployment/release.md`](../docs/deployment/release.md).

## Members

### `build-kb`

- **Purpose:** Walk Fumadocs MDX (`docs/src/content/docs/`) **and** repo
  markdown (`docs/` — ADRs, guides, research, brain, architecture, weftos
  specs). Chunk by heading, emit a binary `.rvf` knowledge base for the
  `/clawft` playground RAG + tag graph. Skips `docs/plans/` and generated
  `docs/src/{app,public,lib,e2e}` trees.
- **Local:** `scripts/build-kb.sh` (or
  `cargo build --release --manifest-path tools/build-kb/Cargo.toml`).
- **CI:** `.github/workflows/release-kb.yml`,
  `.github/workflows/docs-assets.yml`.
- **Lockfile:** `tools/build-kb/Cargo.lock` is committed.
- **Cache:** both workflows use
  `actions/cache` with key
  `${{ runner.os }}-cargo-build-kb-${{ hashFiles('tools/build-kb/Cargo.lock') }}`
  and restore paths `tools/build-kb/target` + cargo registry/git.

### `rustdoc-mdx`

- **Purpose:** Convert rustdoc JSON into MDX for the docs site API reference.
- **Same isolation model** as `build-kb` (own `[workspace]`, own lockfile).
- Invoked from docs generation scripts, not the root workspace gate.

### `diff-gate-tokens`

- **Purpose:** The token scanner behind `scripts/diff-gate.sh`'s Rust rules
  (proc-macro2 + syn): `use` aliases, globs, doctests, attributes, parse
  failures. See `docs/development/diff-gate.md` ("The token helper").
- **Built by the gate itself**, from the base ref's tip, never from the change
  under review, into a cache outside the checkout keyed by the tree hash.
  **Not** cached with `actions/cache` (contract item 4 does not apply: a
  restored binary would be one the next run trusts).
- **Changes need the owner:** anything under `tools/diff-gate-tokens/` is a
  `ci-workflow` finding, like the gate script.
- **Tests:** `cargo test --locked` in `tools/diff-gate-tokens/`.

## Contract for new tools under `tools/`

1. Declare a **standalone** `[workspace]` in the tool's `Cargo.toml` (empty
   table is fine) so cargo does not invent a virtual workspace root.
2. **Commit** `Cargo.lock` for reproducible CI.
3. Build via `--manifest-path tools/<name>/Cargo.toml` — never add the path
   to root `members` unless the tool becomes a real product dependency.
4. Cache CI builds on `hashFiles('tools/<name>/Cargo.lock')` and
   `tools/<name>/target`.
5. Prefer a thin wrapper under `scripts/` when humans need a one-liner.
6. If a tool later needs path-deps into workspace crates, re-evaluate
   membership then (and update this README + the release runbook).
