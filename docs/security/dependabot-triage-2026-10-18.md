# Dependabot triage, 2026-10-18

Open alerts on `master` (= v0.8.3, `fafd6168f`): **22**. The 226 counted on 2026-10-17 dropped
when master was fast-forwarded from v0.6.20 to v0.8.3 (291 alerts now `fixed`, 4
`auto_dismissed`, 0 dismissed). Each alert below was checked read-only against `target-0.8.4`
(`8282dd5ee`, v0.8.4-rc.14) by three triage passes; nothing was changed or dismissed.

Command used for the count:
`gh api --paginate "repos/WeftOS/weftos/dependabot/alerts?state=<s>&per_page=100"`.

## Summary

| Area | Alerts | Fixed on 0.8.4? | Ships? | Verdict |
|---|---|---|---|---|
| docs/src Next.js + esbuild | 333-338, 156 | yes (next 16.4.0, esbuild 0.28.2) | docs site (Vercel) | closes when 0.8.4 stable lands on master |
| root `package-lock.json` sprintf-js | 340 | no patch exists | no (devDep chain agentic-flow → @huggingface/transformers → global-agent → roarr) | dismiss `tolerable_risk` |
| xxhash-rust (root + tools/build-kb) | 331, 341 | no | yes (weaver, weft via weftos-rvf-wire) | lockfile bump; only `xxh3_128` default-secret is called, not exploitable |
| rustls (clawft-core/fuzz) | 332 | no | no (fuzz only, no TLS) | lockfile refresh; fuzz lock is stale (pins clawft-core 0.6.20) |
| rpassword 5 → 7.5 | 90, 91 | no | yes (`weft onboard`, `onboard.rs:162`) | bump; non-TTY behaviour changes |
| jsonwebtoken 9 → 10.3 | 187, 192 | no | no (`teams` feature, off by default) | bumped (`dba07db36`); the Teams webhook now also verifies signatures (`097a23360`) |
| failure (no patch) | 328, 329 | no | no (build-dep of webrtc-audio-processing-sys 0.3, `webrtc-aec` feature off) | port to webrtc-audio-processing 2.1, or dismiss `not_used` |
| lexical-core | 330 | no | no (imap 2.4.1, `email` feature off) | dismiss `tolerable_risk` until imap 3 is stable |
| gui/src-tauri: tauri, glib, rand, serde_with | 81, 21, 70, 284 | no | no (ADR-038 superseded by egui; no workflow builds it) | retire the crate (closes all four), or bump + dismiss glib/rand 0.7 |

## Finding outside Dependabot

One security finding in a feature-gated channel adapter (off by default) was filed on the
board as card `4366dc71`. Fixed in `097a23360`: the Teams webhook previously only parsed the JWT header
and never verified the signature.

## Cards

1. `975e420f` **Land 0.8.4 stable on master** (333-338, 156). No code change; confirm the Vercel docs
   project deploys from the fixed lock.
2. `c6920f2b` **Lockfile-only refresh** (331, 341, 332): xxhash-rust 0.8.19 in root and build-kb, rustls
   0.23.46 and a regenerated fuzz lock; add `scripts/build.sh` lanes for build-kb and fuzz-check.
3. `923ac3d6` **rpassword 7.5** (90, 91), with a decision on piped stdin.
4. `06eb23e8` **jsonwebtoken 10.3** (187, 192). Done in `dba07db36`: resolves to 10.4.0 with the
   `rust_crypto` backend (no aws-lc-rs).
5. `4366dc71` **Channel adapter auth hardening** (security, no alert). Done in `097a23360`: the Teams
   webhook now verifies the Bot Framework JWT (RS256 against the published JWKS, issuer, audience,
   expiry/not-before with 5 minutes skew) in `clawft-channels/src/teams/jwt.rs`; the channel refuses to
   start without an app id.
6. `0623d6a7` **Drop `failure`** (328, 329): webrtc-audio-processing 0.3 → 2.1 and port
   `clawft-voice-aec/src/dsp.rs`; or dismiss `not_used`.
7. `a8c686d4` **Retire gui/src-tauri** (81, 21, 70, 284): owner decision; alternative is a lock bump plus
   dismissing glib and rand 0.7.3.
8. `5e907457` **Dismiss the unpatchable residuals** (340, 330) with justifications recorded in
   `npm-audit-residual.md` / `cargo-audit-residual.md`; optional removal of the agentic-flow devDep.

Raw per-alert notes from the triage passes are summarised here; the evidence lines (file:line
and commands) are carried in each card's description.

## Status, 2026-10-23 (branch `wt/lane-deps`, not yet merged)

| Card | Status | Commit |
|---|---|---|
| `923ac3d6` rpassword 7.5 | Done: lock at 7.5.4; non-TTY stdin read via `read_password_from_bufread`; two tests | `c9a77d10e` |
| `c6920f2b` lockfile refresh | Done: xxhash-rust 0.8.19 (root and build-kb), fuzz lock regenerated (rustls 0.23.46, clawft-core current); `build.sh kb` and `fuzz-check` pass | `54265af3f` |
| `0623d6a7` drop `failure` | Done: webrtc-audio-processing 2.1 port; `failure` gone from the lock; ignores removed; AEC tests pass with `webrtc-aec` | `d6cd7c079` |
| `5e907457` residual docs | Done (docs only; no GitHub dismissals made) | `d7a4ea472` |

`scripts/build.sh audit` is not green on this branch for reasons unrelated to these cards:
it reports advisories published after the lockfile was last refreshed (wasmtime, zip, rustls,
tokio, webpki and others). None mention `failure` or `webrtc`.
