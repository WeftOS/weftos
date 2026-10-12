# ADR-103 leaf implementation plan and evidence

Ticket: `444c7964-0a35-40f2-9462-11c020419cfe` (claimed by the lead on 2026-10-04). Worktree: `leaf-finish/weftos`.

## Existing path and contract

Before this work, both ESP32-S3 display clients identified themselves by a MAC string, connected to a fixed LAN address, and sent unsigned `MeshIpcEnvelope` frames. The daemon already required an Ed25519 signature for `substrate.publish`, but `node.register` did not require a parent certificate or declare capabilities. ADR-103 D7 and the Leaf phase require a provisioned key, a certificate from a user or project key, unified SHA-256 node id, signed publishes, parent discovery, and replay of offline observations.

## Implementation sequence

1. Define one versioned, domain-separated leaf certificate and publish wire format usable on `no_std` ESP32 and host. Make parent scope, leaf identity, topic/path, sequence and payload part of the signed bytes. Provide verification with expiry, parent key pin, key id, and replay checks.
2. Add an explicit provisioning command that creates a random leaf seed and parent-signed certificate, writes the seed only to an operator-selected private output, and writes a public registration artifact for the parent. Bind a user scope to its user key ID, or a project scope to a user signed project key certificate. Never use a MAC-derived identity as the cryptographic node id.
3. Wire the parent receive path to require the certificate and signed publishes for leaf data. It must accept only the certified leaf's own topic, persist accepted sequence before acknowledging, and refuse old, altered, expired, revoked or cross-scope messages. An exact duplicate of the last committed frame gets a new ACK without a second data delivery; a duplicate subscribe refreshes its connection-local route.
4. Add authenticated parent discovery and a bounded durable outbound journal. The leaf verifies the provisioned machine pin and nonce-bound discovery signatures, records observations before sending, and removes them only after parent acknowledgment; reconnect replays in sequence.
5. Replace the ESP32 hardcoded parent and unsigned input publish path with the shared protocol. Keep display reception scoped to the certified identity.
6. Verify with isolated host tests (temporary directories under the worktree, isolated `HOME`/`WEFTOS_RUNTIME_DIR`, in-memory channels) and `scripts/build.sh`; record exact commands and results below. Firmware compilation is a separate check and must not flash or probe hardware.

## Implemented in this worktree

- `weftos-leaf-types::link` supplies the `no_std` certificate, signed publish, signed ACK and signed discovery wire types, a canonical tenant scope parser, and a bounded sequence journal. Certificate capabilities and every publish path are checked before the leaf signature is accepted. Node IDs use the first 16 bytes of SHA-256 of the Ed25519 public key.
- `weaver leaf provision` creates a random leaf seed, a parent-signed certificate and a public enrollment artifact. A user scope must equal the key ID of the parent user key; a project scope requires `--project-cert` containing the user signed certificate for the issuing project key. `weaver leaf enroll` checks the scope chain, pinned parent and machine keys and installs the exact certificate in the machine service registry. `weaver leaf enqueue` durably writes a signed journal snapshot before transmission; `weaver leaf replay` discovers a signed endpoint, replays the oldest frame and removes it only after a machine-signed ACK. Files are bounded, private files use mode 0600, and the queue directory must be owner-private.
- The collapsed kernel and machine mesh service load an installed leaf registry at boot. The mesh receive pump accepts `WLF1` on its real connection path, validates the installed certificate, user or project issuer chain, sequence floor, signature, capability and inner topic, and converts the signed `ipc.publish` wrapper into a message on the certified leaf's actual input or announce topic. It routes that message to the tenant named by the certificate, strips the envelope's caller-controlled source scope and overwrites its destination scope. The machine service additionally requires the user key named by that chain to match its current live tenant registration on discovery, admission and push. The service router refuses an unavailable parent tenant so such a message is not ACKed. A successful local delivery is followed by a synced replay-floor rename and then a signed ACK. Raw traffic from that signed connection is refused.
- The existing Noise-authenticated, admission-enforced `CAP_LEAF` route retains its `substrate/<own-id>/...` publish limit and `mesh.subscribe` control route. Its identity is bound by the verified Noise hello. This route cannot publish to certified `mesh.leaf.*` input or announce topics, and a plaintext or unadmitted peer cannot publish to protected leaf or substrate topics. The new WLF1 certificate and replay requirements govern the separate certified input/announce path; they do not silently remove the earlier authenticated substrate route.
- Project scoped leaves use a user signed `ProjectCert` carried in enrollment. The user daemon reads its journal-backed current project identity view and signs `AddressAdd` claims for each live project key on service connect and every second; successful issue, rekey and revoke wake that sync immediately. It removes a stale claim before replacing it on rekey and removes revoked or expired claims. The machine service verifies each claim signature against the registered user key and stores the current project key alongside the route. Its leaf authority check requires the current principal binding, no forced revocation, and an exact match between the enrolled project certificate key and the live project claim. A disconnected daemon eventually loses all claims when the machine closes that registration. Issue, rekey, revoke and repair wait for the connected mesh session to acknowledge the resulting address set before returning success. After any service session existed, a missing session reports an incomplete mutation because remote cleanup might still be pending; only a daemon that never connected to the service can skip the barrier. A failed or timed-out sync also reports an incomplete mutation after the journal write. This closes the earlier unroutable project scope and asynchronous user-binding revocation gap.
- When a leaf registry is installed, the mesh listener binds a separate TCP listener on the mesh port plus two for certified WLF1 frames only. The ordinary node-to-node listener keeps its configured Noise requirement. Signed UDP discovery on mesh port plus one advertises the dedicated leaf endpoint only after the TCP bind succeeds. Only enrolled leaf IDs receive an answer; advertisements bind a request nonce, the machine key, the certificate scope, the endpoint and a 30-second expiry. Both listener paths have global and per-IP connection limits.
- The IDF client accepts per-device build inputs `WEFTOS_LEAF_SEED_FILE=<provision>/leaf.seed` and `WEFTOS_LEAF_CERT_FILE=<provision>/leaf.cbor`. Its build script embeds those bytes, and an image without both stays offline. It discovers the certified parent and journals GT911 pointer down/move/up observations with their signed input envelopes in the `weft_leaf` NVS namespace before sending. A bounded channel backpressures the touch reader when the NVS journal fills. The client replays oldest-first and removes each entry only after a machine-signed ACK is verified and committed. On reconnect it drains the durable backlog and sends a fresh signed subscription; pushed frames arriving before ACK are demultiplexed and rendered. While a display session (ADR-117, `docs/design/display-session-transport.md`) holds the screen, the IDF variant hit-tests touch against its displayed scene and sends the resolved node in the session's `Input` frame; with no session, input envelopes still carry coordinates and no resolved node.
- The older bare-metal firmware's unsigned mesh tasks now compile and run only with `legacy-unsigned-mesh-bringup`. The default image does not start Wi-Fi or publish to the mesh. That opt-in is for isolated bring-up only; the provisioned IDF image is the supported signed path.

The operator creates the service-owned registry at `<mesh state dir>/leaf/registry` with mode 0700, then runs `weaver leaf enroll` as the service account with the pinned raw parent and machine public keys. For a project leaf, pass its user signed project certificate to `weaver leaf provision --project-cert`; it is copied into the public `leaf.json` enrollment artifact. For collapsed mode the registry location is `<runtime root>/leaf/registry`. Keep `leaf.seed` outside the registry; the service reads only `leaf.json`. Build each IDF image with the two file env vars above and arrange flash encryption/secure boot according to the deployment's key-handling policy. The IDF journal lives in the default NVS partition, which must have capacity for its 4096-byte bound alongside Wi-Fi state.

## Evidence

All host build and test commands used `scripts/build.sh` with `HOME=$PWD/.leaf-test-home`, `TMPDIR=$PWD/.leaf-test-home/tmp`, `WEFTOS_TEST_RUNTIME_ROOT=$PWD/.leaf-test-home/test-runtime`, `CARGO_TARGET_DIR=$PWD/target`, `CARGO_NET_OFFLINE=true`, `CARGO_HOME=~/.cargo`, `RUSTUP_HOME=~/.rustup`, and `WEFTOS_RUNTIME_DIR` unset. The final focused gate also set `CARGO_INCREMENTAL=0`. Here `$PWD` was `~/.codex/worktrees/leaf-finish/weftos`. Build and test jobs were detached to `.grok/compose/logs/` and polled. The tests created only private worktree test files and in-memory channels. No test used a real daemon or hardware. The disposable `.leaf-test-home` directory was removed after the final host gate.

| Command | Result |
|---|---|
| `scripts/build.sh check weftos-leaf-types clawft-kernel clawft-mesh-service clawft-weave --verbose` | Passed after the dedicated listener and idle revocation checks (`leaf-review-final-gates.log`, `CHECK_EXIT=0`). |
| `scripts/build.sh test weftos-leaf-types --verbose` | 42 of 42 tests passed (`leaf-tests-round4.log`); includes signature, scope, discovery and offline queue checks. |
| `scripts/build.sh test clawft-kernel --filter certified_leaf --verbose` | 3/3 passed after the send-time enrollment check (`leaf-sendtime-gates.log`, `TEST_EXIT=0`): actual in-memory receive pump, unsigned first-frame rejection, signed scoped delivery and duplicate suppression, and no pushed frame after idle enrollment revocation. An earlier loopback test failed under the sandbox with `Operation not permitted` and was replaced. Two earlier harness reruns lacked the test runtime directory or preinstalled Rustup/Cargo paths; both were corrected. |
| `scripts/build.sh test clawft-kernel --filter discovery_only_answers_enrolled_leaf --verbose` | 1 passed (`leaf-tests-round5.log`); tests signed advertisement and nonce, with no UDP bind. |
| `scripts/build.sh test clawft-mesh-service --filter certified_leaf_can_reach_only_its_scoped_parent --verbose` | 1 passed (`leaf-tests-round5.log`); other tenant receives nothing and missing parent returns an error. |
| `scripts/build.sh native-debug --verbose` | Host `weft` and `weaver` binaries built after the final CLI and receive edits (`leaf-host-build-final.log`, `Finished dev profile` in 26.57 s; 68.24 MB and 141.86 MB respectively). |
| `scripts/build.sh clippy weftos-leaf-types clawft-kernel clawft-mesh-service clawft-weave --verbose` | Passed with warnings as errors after the send-time enrollment check (`leaf-sendtime-gates.log`, `CLIPPY_EXIT=0`). |
| `scripts/build.sh test clawft-kernel --filter enrolled_frame_replays_exactly_once_across_restart --verbose` | 1/1 passed (`leaf-review-final-gates.log`, `FLOOR_TEST_EXIT=0`); validates persisted floor, exact duplicate ACK, altered-frame rejection, certificate expiry and revocation by enrollment removal. |
| `scripts/build.sh check clawft-mesh-local clawft-kernel clawft-mesh-service clawft-weave --verbose` | Passed after project claim routing and live binding checks (`leaf-project-routing-check.log`, `CHECK_EXIT=0`). |
| `scripts/build.sh check clawft-weave clawft-mesh-service clawft-kernel --verbose` | Passed after the acknowledged project claim barrier (`leaf-claim-barrier-check.log`, `BARRIER_CHECK_EXIT=0`). The task paused further compilation when free disk fell below 5 GB; barrier unit test, Clippy and final host rebuild still require a sequential compiler lane. |
| `scripts/build.sh test clawft-mesh-service --filter certified_project_key_tracks_rekey_revoke_and_disconnect --verbose` | 1/1 passed (`leaf-project-routing-tests.log`, `REGISTRY_TEST_EXIT=0`). |
| `scripts/build.sh test clawft-mesh-service --filter certified_project_leaf_route_disappears_on_revoke --verbose` | 1/1 passed (`leaf-project-routing-tests.log`, `ROUTER_TEST_EXIT=0`). |
| `scripts/build.sh test clawft-mesh-service --filter project_address_claim_binds_user_project_and_key --verbose` | 1/1 passed (`leaf-final-unit-and-clippy.log`, `CLAIM_SIGNATURE_TEST_EXIT=0`); proves a changed user, project ID or project key breaks the signed address claim. |
| `scripts/build.sh test clawft-kernel --filter scope_chain_rejects_another_users_scope --verbose` | 1/1 passed after final host integration (`leaf-project-routing-tests.log`, `SCOPE_TEST_EXIT=0`). |
| `scripts/build.sh test clawft-kernel --filter certified_leaf --verbose` | 3/3 passed after final host integration (`leaf-project-routing-tests.log`, `LEAF_TEST_EXIT=0`). |
| `scripts/build.sh clippy weftos-leaf-types clawft-mesh-local clawft-kernel clawft-mesh-service clawft-weave --verbose` | Passed with warnings as errors after project claim integration (`leaf-final-unit-and-clippy.log`, `CLIPPY_EXIT=0`). |
| `scripts/build.sh native-debug --verbose` | Final host binaries built after project claim integration (`leaf-project-final-build.log`, `HOST_BUILD_EXIT=0`, `weft` 67.53 MB, `weave` 140.84 MB). |
| `scripts/build.sh test clawft-weave --filter signed_project_address_claim_routes_and_revocation_removes_route --verbose` | Built, then blocked at the test service's loopback bind: `Operation not permitted (os error 1)` from the local sandbox (`leaf-project-routing-compiled-r2.log`, `SANDBOX_TEST_EXIT=100`). An unrestricted isolated host run of the corrected test is pending; the test itself was retained. |
| `CARGO_NET_OFFLINE=true cargo fetch --manifest-path crates/clawft-edge-pad-idf/Cargo.toml --locked --target xtensa-esp32s3-espidf` | Blocked: `no matching package named esp-idf-hal found` in the local cache. This fetched nothing and touched no device. |
| Isolated ARM64 ESP-IDF Xtensa release build, snapshot r1, run by the lead from a source snapshot with dummy Wi-Fi secrets and no device mounts | Reached the new firmware source. Five `E0277` codec to `anyhow` conversions in `mesh.rs` failed; all five were fixed here. |
| Isolated ARM64 ESP-IDF Xtensa release build, snapshot r2, run by the lead with no device mounts | Passed in 3m55s (`FIRMWARE_BUILD_PASS` in `esp-firmware-build-r2.log`). This snapshot preceded the final GT911 producer and dedicated leaf listener edits; a final snapshot build is still needed. |
| Isolated ARM64 ESP-IDF Xtensa release build, snapshot r3 | Snapshot omitted the new local `weftos-leaf-scene` path dependency, so it stopped before compiling the final source; the snapshot was corrected without a source edit. |
| Isolated ARM64 ESP-IDF Xtensa release build, snapshot r4, with dummy Wi-Fi secrets and no device mounts | Passed in 56.30 s (`FIRMWARE_BUILD_PASS` in `esp-firmware-build-r4.log`). This snapshot includes the GT911 signed input producer, CBOR `leaf_input` wire shape, NVS journal, signed replay and discovery. No device was touched. |

An earlier tenant authority gate passed `check` (`CHECK_EXIT=0`), `scope_chain_rejects_another_users_scope` (1/1, `SCOPE_TEST_EXIT=0`), and `certified_leaf` (3/3, `LEAF_TEST_EXIT=0`) in `leaf-scope-final-gates.log`. Its Clippy run found two `type_complexity` lints; the callback was factored into a type alias before the passing final Clippy gate above. The r4 firmware snapshot is unchanged by these host-only patches.

An isolated host run of the first project route integration test reached the service and found that the test sent a deliberately invalid signature before the valid claim on the same connection. The service closed that connection, so the valid request failed with `BrokenPipe` at test line 74. The test now sends the forged claim last and checks it installed no key; this preserves the service's fail-closed behavior. The corrected binary compiled in the sandbox, but its in-sandbox execution stops at the loopback bind above. A fresh host result is required before this integration test can be called passing.

## Remaining gaps

- The bare-metal `clawft-edge-pad` still contains a MAC-derived ID, a hardcoded parent IP and unsigned JSON envelopes behind the explicit `legacy-unsigned-mesh-bringup` feature. Its default image disables Wi-Fi and mesh tasks. Its protected input and subscribe topics are refused by the new parent receive gate. It lacks a durable flash journal and provisioned-key loader; this worktree therefore does **not** complete that variant. The IDF signed input source compiled in snapshot r4; real hardware smoke remains unexecuted. The hardware was not probed or flashed. An unprovisioned image is intended to park offline; a provisioned image requires both env vars.
- The machine service hands a verified WLF1 publish to the tenant's local queue before ACK. That handoff is not a durable end-to-end substrate commit. The WLF1 `substrate/<leaf-id>/...` capability remains reserved in the wire contract, but the CLI rejects it at provisioning, enrollment and enqueue because the certified mesh receive path has no daemon `substrate.publish` bridge. The older Noise-authenticated `CAP_LEAF` route still accepts only its own substrate topic, as its admission contract requires; it does not gain the new certificate/replay guarantee.
- Enrollment checks the leaf certificate and its user ID or user signed project certificate chain. Machine service delivery checks the current live user registration, principal binding and project key claim. Collapsed mode has no machine registration to consult and relies on the self-certifying user key ID and signed project chain. User key rotation requires a new leaf enrollment artifact and, for project scopes, an updated project certificate. Project rekey/revoke RPC success waits for a service ACK, but a publish concurrent with the journal mutation can still arrive before the service applies the new claim; a timeout reports an incomplete mutation. The machine service does not independently hold the user daemon's project identity journal, so a compromised user daemon that still controls its signing key could assert an old project claim.
- The dedicated signed-leaf TCP listener authenticates each publish but does not encrypt payloads. Inbound display pushes remain unsigned JSON on that connection, so confidential observations and authenticated display output require a leaf transport encryption layer. The normal mesh Noise listener is unchanged.
- The IDF touch producer does not resolve scene node IDs; it signs coordinate events. A touch already sampled but still in the bounded handoff channel can be lost on sudden power failure before NVS commit. The durable replay guarantee begins once NVS `set_blob` returns; touch hardware and NVS are not one atomic transaction.
- A crash between local delivery and replay-floor commit can redeliver once. The current guarantee is at-least-once delivery; downstream effects need idempotency keyed by `(leaf id, sequence)` for exactly-once effects.
- Replacing or losing a journal while its parent floor survives prevents sequence continuation. Re-enrollment under a fresh key is the recovery path. There is no hardware-backed protection for the leaf seed in this host provisioning flow.

## Frozen source handoff

Source was frozen after the acknowledged claim barrier compiled (`BARRIER_CHECK_EXIT=0`). The local `mesh_p3_e2e-d166f60cf1c0716c` binary was built at 16:24 local time, while `project_cert_rpc.rs` and `mesh_local_glue/session.rs` changed at 16:30. **That binary is stale for the barrier; rebuild it before any final host routing run.** The corrected test body itself was saved at 16:23. The barrier unit test, final Clippy and host binary build also remain to run after this freeze. There are no task-owned active compiler jobs.

Post-freeze review correction changed `crates/clawft-weave/src/project_cert_rpc.rs`, `crates/clawft-weave/src/mesh_boot.rs`, and this plan. `ClaimSyncState` now records service mode synchronously in `mesh_boot::prepare`, before the first link task polls. A missing lease during first session startup or after a dropped session cannot make a concurrent mutation succeed before the machine acknowledges the current claims. A collapsed daemon has no machine claims and can proceed. The `disconnect_gap_is_incomplete_until_successor_installs` unit test covers both the first-session and disconnect gaps; it is source-only here. No compilation was started after the lead reserved the compiler lane for main integration. The exact delta must be imported before the main gate.

Combined-gate regression correction changed `crates/clawft-kernel/src/mesh_serve.rs`, `crates/clawft-kernel/src/mesh_admit_tests.rs`, and this plan. The new blanket `PeerLimits::Leaf` drop had blocked an already admitted Noise `CAP_LEAF` peer from publishing under `substrate/<own-id>/`, contradicting the established grant and failing `enforce_limits_leaf_publishes_to_its_substrate_prefix`. The screen now restores the exact own-prefix/subscription limit while continuing to deny other substrate IDs and certified `mesh.leaf.*` topics; unsigned/unadmitted protected traffic is still refused earlier. A focused socket-free regression test was added. This delta is source-only and needs the main compiler lane's combined gate.

Changed tracked paths (`M`):

```text
Cargo.lock
crates/clawft-edge-pad-idf/Cargo.toml
crates/clawft-edge-pad-idf/build.rs
crates/clawft-edge-pad-idf/src/main.rs
crates/clawft-edge-pad-idf/src/mesh.rs
crates/clawft-edge-pad/Cargo.toml
crates/clawft-edge-pad/src/main.rs
crates/clawft-kernel/Cargo.toml
crates/clawft-kernel/src/boot.rs
crates/clawft-kernel/src/lib.rs
crates/clawft-kernel/src/mesh_admit_tests.rs
crates/clawft-kernel/src/mesh_runtime.rs
crates/clawft-kernel/src/mesh_serve.rs
crates/clawft-kernel/src/mesh_serve_tests.rs
crates/clawft-mesh-local/src/proto.rs
crates/clawft-mesh-service/src/handlers.rs
crates/clawft-mesh-service/src/main_loop.rs
crates/clawft-mesh-service/src/registry.rs
crates/clawft-mesh-service/src/registry_tests.rs
crates/clawft-mesh-service/src/router.rs
crates/clawft-mesh-service/src/router_tests.rs
crates/clawft-weave/Cargo.toml
crates/clawft-weave/src/commands/leaf_cmd.rs
crates/clawft-weave/src/mesh_boot.rs
crates/clawft-weave/src/mesh_local_glue.rs
crates/clawft-weave/src/mesh_local_glue/session.rs
crates/clawft-weave/src/project_cert_rpc.rs
crates/clawft-weave/tests/mesh_e2e/mod.rs
crates/clawft-weave/tests/mesh_p3_e2e.rs
crates/clawft-weave/tests/mesh_service_client.rs
crates/weftos-leaf-types/Cargo.toml
crates/weftos-leaf-types/src/lib.rs
```

New paths (`??`):

```text
crates/clawft-kernel/src/mesh_leaf.rs
crates/weftos-leaf-types/src/link.rs
docs/plans/adr-103-leaf-implementation.md
```
