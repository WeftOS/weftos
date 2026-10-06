# Cog protocol boundary

Branch `feat/cog-protocol-boundary`, parent `v0.8.3` (`fafd6168f8f97280ee24e5c2d2813b6e34918e15`). Worktree `/Users/mathewbeane/weftos-cog-protocol`. This note records the boundary. The kernel link stays for mesh, ECC, revocation import, trust anchors, claims, and the revoked-hash poll.

## What stays coupled

`crates/weftos-cog-host/Cargo.toml` depends on `clawft-kernel` with features `native`, `ecc`, `exochain`, and `mesh`. Production `HostLicence::check` on a Ready directory calls `cog.check_run` over the Unix socket. Import, status, claims, and the revoked-hash poll still use the kernel in process. `cfg(test)` `evaluate_local` still calls `check_run`, so the test listener can return a kernel verdict. Revocation import uses `clawft_kernel::mesh_swarm_revoke` and `clawft_kernel::revocation::RevocationList`. Trust anchors use `clawft_kernel::workload_pkg::TrustAnchors`.

`crates/weftos-cog-repo/Cargo.toml` depends on `clawft-types` with default features off, for runtime paths.

`clawft-weave` still links `clawft-kernel`. The daemon handler calls `check_run` on the grant store that daemon already owns.

## Dependency review of 5bc8a952

`5bc8a95203166a03a79e1a829790a45c9b592e66` adds `weftos-cog-protocol`. Dependencies in that commit are workspace `serde` (version "1", features `derive`), `serde_json` "1", and `thiserror` "2". The package inherits version 0.8.3, edition 2024, rust-version 1.95, license MIT OR Apache-2.0, and repository `https://github.com/weave-logic-ai/weftos`. `publish = false`. `Cargo.lock` in that commit adds 9 lines. The only new package name is `weftos-cog-protocol`. The crate does not link `clawft-kernel`, `clawft-types`, or `clawft-rpc`.

The Unix client added after that commit uses `std::os::unix::net::UnixStream` with read and write timeouts. It adds no crate dependency. Workspace `serde_json` has no `preserve_order` feature, so `json!` maps sort keys. Struct `Serialize` keeps declaration order. Golden map files use sorted keys, and the golden test compares those bytes.

Copying this crate into `weftos-cogs` requires those three workspace crates and the workspace package keys, with no path or git dependency back into WeftOS. The types commit travels with the client, the golden vectors, the host cut, and the `cog.check_run` handler.

## What the client and the daemon speak

Line-delimited JSON, one object terminated by `\n`, on `kernel.sock` under the runtime dir (`WEFTOS_RUNTIME_DIR`). An explicit socket wins over that directory. The cog client sends `method` `cog.check_run`, `params`, `id`, and `proto` 1. It omits `auth`. Params carry `protocol` `weftos.cog.v1`, `cog_id`, `version`, `sha256`, and `blake3`.

Success is `ok`, `result`, and the echoed `id`. `result.protocol` must be `weftos.cog.v1`. Verdicts are `not_seed_bound` or `permit` (`grant_id`, `approval_id`, `blake3`). A permit whose `blake3` differs from the request is `malformed_reply`. Refusal is `ok: false`, `error`, and `error_kind`. Kernel refusal codes stay `binding_inactive`, `no_grant`, `grant_lapsed`, `not_in_grant`, `hash_revoked`, `no_approval`, and `not_holder`. Transport codes are `daemon_unavailable`, `malformed_reply`, `timeout`, and `version_mismatch`. RPC `proto_mismatch` maps to `version_mismatch`. A missing runtime while the daemon is up is `binding_inactive`. A holder refusal is `not_holder` and is returned before the runtime lookup. Connect failure is `daemon_unavailable`.

Golden files live in `crates/weftos-cog-protocol/testdata/`: `request.json`, `verdict-not-seed-bound.json`, `verdict-permit.json`, `refusal-hash-revoked.json`, `event-run-permitted.json`, `event-run-refused.json`, and `proto-mismatch.json`. Host events are local. The supervisor does not emit them yet.

The daemon registers an exact Read route `cog.check_run` in both `rpc_ext` tables and in `workload_rpc::dispatch`. The licence-verb census includes that exact name.

## Receipts

Measured on this worktree with `scripts/build.sh test`, 2026-10-06.

| Crate | Filter | Nextest run | Result |
|---|---|---|---|
| `weftos-cog-protocol` | all | `ce028f9f-b49c-4631-9011-6372260ef609` | 7 passed, 0 skipped. Includes unavailable daemon, malformed reply, timeout, denial, and version mismatch. |
| `weftos-cog-host` | all | `1a41b9cb-5039-410f-8670-0d1c96e34feb` | 120 passed, 0 skipped. Ready `check` uses the socket. |
| `clawft-weave` | `cog_check` | `c2040657-58e5-45c8-9b23-972dfbc2e232` | 12 passed. The five `cog_check_rpc` tests passed. Seven `cog_checkout` tests matched the same substring and passed. |
| `clawft-weave` | `population_every_ext_route` | `99affbba-2254-4026-907c-b3456cee4f03` | 1 passed. |
| `clawft-weave` | `licence_verbs_are_machine_level` | `89812028-86ae-46b2-84db-5a1efc30e0b7` | 1 passed. |
| `clawft-weave` | `the_list_is_exactly` | `a4c43d26-bc03-43ec-be54-b44e93ff41d9` | 1 passed. |

This note names a local worktree. Leave it out of `weftos-cogs`.
