# WeftOS 0.8.0 cut status

**Date:** 2026-07-31  
**Branch:** `release/0.8-staging`  
**Plane:** WEFT-729 (release), WEFT-730 (score flywheel)

## Version

- Workspace `version = "0.8.0"` (was 0.6.20)
- Path-dep versions lockstep in root `Cargo.toml`
- CHANGELOG `[0.8.0] - 2026-07-31` + releases-mdx regenerated

## Tag (shipped)

| Field | Value |
|-------|--------|
| Tag | `v0.8.0` (annotated, published) |
| Tip commit | `dec3b28f` — pin Windows `dist.binaries` for weft-gui-egui |
| Release URL | https://github.com/WeftOS/weftos/releases/tag/v0.8.0 |
| cargo-dist run | [30671958877](https://github.com/WeftOS/weftos/actions/runs/30671958877) **success** |
| Assets | 77 (cli/weave/weftos/gui + installers + WASI + formulae + sha256) |
| Smoke test | `weft 0.8.0 (dec3b28f-dirty …)` from aarch64-apple-darwin archive |

### Cut history

1. `d563f3bd` — Windows `NamedPipeServer` E0603; SBOM eval quoting; WASM raw budget
2. `4403c8d9` — fixed above; Windows packaging looked for `weft-demo-lab.exe` (missing `x86_64-pc-windows-msvc` in `dist.binaries`)
3. **`dec3b28f`** — pin Windows binaries; **shipped**

## Publish legs

| Channel | Status |
|---------|--------|
| GitHub Releases | **Done** — published, not draft |
| Homebrew tap | **Done** — weftos/clawft-cli/clawft-weave/clawft-gui-egui 0.8.0 on `weave-logic-ai/homebrew-tap` |
| Browser WASM | **Done** — run 30671958668 success |
| WASI | **Done** — on release assets |
| Docker GHCR | In progress after Release success (run 30673703190) |
| Release SBOM | Upload timed out waiting for host (30m); **re-running** after release exists. Wait bumped to 60m for next cuts. |
| Release KB | Same timeout; **re-running**. Wait bumped to 60m. |
| crates.io | **Blocked** — no operator `CRATES_API_TOKEN` / cargo credentials |
| npm `@weftos/core` | **Blocked** — no `NPM_TOKEN` / npm auth |

## Residual (post-cut)

- SBOM/KB wait loop: 90×20s → 180×20s (committed on staging after tag)
- cargo-audit residual ignores (quick-xml, wasmtime, spin yanked, …)
- crates.io + npm when tokens available

## Operator follow-ups

```bash
# when tokens present
source .env && cargo login "$CRATES_API_TOKEN"
# publish order per .claude/skills/weftos-build-deploy/SKILL.md

# npm
wasm-pack build crates/clawft-wasm --scope weftos --target web --features browser
# fix pkg/package.json name/version/repo then npm publish --access public
```
