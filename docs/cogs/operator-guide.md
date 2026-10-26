# Operating cogs: pack, sign, pair, place, revoke

For the person who takes a cog from a source to a running, governed workload on a real node. Design: [ADR-099](../adr/adr-099-governed-workload-placement.md) (placement, trust) and [ADR-100](../adr/adr-100-cog-workload-kind.md) (the cog kind). Where cogs come from: [cog-sources.md](cog-sources.md). Hardware lane: [test-pi.md](test-pi.md).

Two paths exist and they are different:

- **Appliance path**: `weaver cog install` puts a verified binary into a cog-host root (`weft-cog-host` supervises it). Verified, recorded, not a governed placement.
- **Governed path**: pack the cog into a signed `cogpkg`, then `weaver workload place` it onto a paired node. Every step is gated and chained.

Pick the catalog first: `weaver workload catalog --kind cog` shows run mode, resources, secrets, source and the default placement policy for each cog. The v1 scope is the 93 cogs in the clean `--once` group; the five persistent-listener health cogs need `--mode interval`; the nine `needs-*` cogs need seed peers, assets or other setup and are not v1.

## Pack

```sh
weaver workload pack --cog-dir <cogs checkout>/src/cogs/anomaly-detect \
    --bin aarch64=cog-anomaly-detect-aarch64 --bin armv7=cog-anomaly-detect-armv7 \
    --source-commit 8970f99 --out ./pkg/anomaly-detect
```

The package directory holds the unmodified `cog.toml`, one binary per arch, optional attestations, and `cogpkg.json` pinning each file by BLAKE3 and size. `--cognitum-record <file>` carries a Cognitum release record as an attestation; `--release-url` records where upstream binaries came from (provenance, never trust). An upstream binary is usable only after you hash and sign it here.

**Sharing over the mesh.** A package is seeded, advertised and served to other nodes only if it was packed with `--redistributable` (a signed manifest field, default off). Pass it for your own cogs that you want shared. `pack` looks for `provenance.json` in three places: `--provenance <file>`, the `--cog-dir`, and beside each `--bin` (where `weaver cog install` writes it; an installed cog dir holds the binary and `provenance.json` but no `cog.toml`, so point `--bin` at the installed binary and `--cog-dir` at the source dir). When any says `trust = "cognitum-sha256"` (a licensed Cognitum install), `--redistributable` is refused and `pack` stamps a Cognitum attestation (`cognitum.install.provenance.v1`, no licence account) so the swarm never redistributes the package. A malformed `provenance.json` fails the pack. `--redistributable` also needs evidence the binary is not a Cognitum one: a `provenance.json` with another trust and a sha256 that matches the packed binary. With none, it is refused unless you pass `--no-provenance-ok` (your own build from source; you are asserting it is not a Cognitum binary). Every `--bin` must be covered by a provenance whose sha256 equals that binary's (one beside a binary covers only that binary), and its `trust` must be `ed25519-signed` (what `weaver cog install` writes for a signed source) or `source-build`; other values are refused. With `--no-provenance-ok`, the signed manifest records an `operator.no-provenance-asserted` attestation. The swarm treats a package as Cognitum-origin on either of two signals: a `cognitum.*` attestation (`--cognitum-record`, or the stamp above) or a `--release-url` containing `cognitum`; the URL is a substring backstop that can only add Cognitum standing. Nodes built before this change reject manifests packed with `--redistributable`; upgrade every node before using it. The full rules: [swarm throughput, "Safety rules the swarm enforces"](../research/mesh-placement/swarm-throughput.md#safety-rules-the-swarm-enforces).

## Sign

```sh
weaver workload keygen --out ./operator.seed --key-id acme-operator   # 64-hex seed, mode 0600, never overwrites
weaver workload pack ... --key ./operator.seed --out ./pkg/anomaly-detect
weaver workload verify ./pkg/anomaly-detect --trust ./workload-trust.json
```

`workload-trust.json` (`weftos.workload-trust.v1`) lists the operator-pinned signer keys the node accepts, plus optional pinned Cognitum release keys. Without it only the compiled-in WeftOS signer set is trusted. Verify exits with a distinct status per failure: missing signature, untrusted signer, bad signature, tampered file, bad manifest. Details and the signer provisioning steps: the key management section of [cog-sources.md](cog-sources.md).

Verifying a package trusted only through a Cognitum record (`--cognitum-release`) needs more than the record: the record's `sourceCommit` must equal the package's source commit and you must pin the `cog.toml` hash you reviewed (`--cog-toml-pin <blake3>`), because a record does not cover `cog.toml`. Sign the package yourself and none of that is needed. With `--cognitum-release` on, an attached record is checked even when your signature is present.

## Pair

A node that can run workloads is described to the controller in the daemon's runtime directory (ADR-099 section 8, `workload_place_policy.rs`):

- `workload-peers.json`: `[{"addr": "host:port", "tier": "paired", "key": "<64 hex node key>"}]`. A named peer is only `discovered` until you give it a tier. With a `key` the tier belongs to that node key; without one it goes to the key first seen at the address, and a different key answering later never inherits it.
- `workload-permits.json`: the permit rules for gated actions (default deny: missing file means no permits).
- `workload-trust.json`: the package signers this node accepts.
- On the target node, `workload-host.json` makes it serve `workload-host` (Noise XX to the named controller). Its optional `lease_secs` (30 to 86400, default off; it must exceed the controller's 10 s tick) makes the node stop the continuous workloads other nodes placed on it once no remote controller has been heard for that long; set it at or below the controller's `dead_after` (30 s) so a partitioned node stops its copy before the controller starts a replacement (ADR-099 section 7).

Two things share the word "paired" and are not the same. Placement's tier is the one above: you assign it, in `workload-peers.json`. The `trust=` column of `weaver cluster nodes --facts` is the mesh's own view of a peer's advertised facts: a peer whose node id the mesh admitted shows as `mesh-verified (... not operator-paired)`, which only says the key matches the id, and gives that peer no placement tier. A tier you set shows as plain `paired` or `pinned`; an unadmitted peer shows `discovered`. Placement never reads the mesh's column. `--facts` prints the signed facts when the daemon's copy disagrees with the signature (apart from the receiver's provenance cap and live updates) and says so; `--refresh` probes at most once every 30 s (a probe that hangs is abandoned after 60 s), each probe is a `node.facts.refresh` chain event, and a refresh the limit skipped prints "refresh skipped ... cached as of ...". The signature line reads "id-bound signature (ed25519)": the key matches the node id; it does not mean the node is trusted. Advertised facts never include external volume names or user-named docker contexts.

Tiers gate what may run: `discovered` nothing by default, `paired` operator-signed workloads (native cog placement needs this), `pinned` workloads that carry secrets. A Cognitum Seed is addressed as an operator-assigned node id in `workload-seeds.json`, with pinned store cogs (`id`, `version`) and its token under `secrets/workload.seed/<node_id>.token`. Cogs from a Cognitum source reach a Seed only through that store path with the cog pinned; a Seed's store does not list `anomaly-detect`.

## Place

```sh
weaver workload explain ./pkg/anomaly-detect            # candidates, scores, failed constraints; dispatches nothing
weaver workload place   ./pkg/anomaly-detect --mode interval --interval 10
weaver workload place   ./pkg/anomaly-detect --pin <node-id>       # that node or fail naming the constraint
weaver workload place-seed seed-kitchen fall-detect@1.0.0 --sha256 <hex>
weaver workload status [<instance>]
weaver workload logs <instance>
weaver workload stop <instance>
weaver workload unload <instance>
```

Placement picks real ARM hardware over a Mac container over emulation, prefers the node on the sensor's LAN, and refuses emulation unless you pass `--allow-emulated`. The target re-checks the package at admission (a package whose `aarch64` binary is really x86-64 is refused there and retried on the next candidate). An unsigned package is refused. Each place, load, start, stop, unload and refusal is a chain event.

## Revoke

```bash
weaver workload revoke --package <id> [--reason "..."]
weaver workload revoke --signer <64 hex key>
weaver workload revoke --hash <64 hex blake3>
```

Revocation is by package id, signer key or artifact hash (`RevocationKind::{Package, SignerKey, ArtifactHash}`), persisted in `revoked_subjects.json` beside the host ban file. Name exactly one. The package id is the BLAKE3 of the package's signed statement (a placed instance's record carries it as `package_id`: `weaver workload status --json`); for a `workload install` catalog entry it is the catalog name. Admin only. One call:

1. records the revocation, chained as `workload.revoke` (`revoked_by: operator`, with your reason); a failed write of `revoked_subjects.json` is reported (`persisted: false`) but the revocation still holds in memory and is still chained;
2. drops this node's grants and cached bytes for it;
3. stops and unloads every instance this node hosts from that package, signer or artifact, drops the controller's record of it, and chains each step (`workload.stop`, `workload.unload`, with `forced_by_revocation`). This does not need a stop permit in `workload-permits.json`;
4. when this node's key is a pinned operator key in `workload-trust.json`, signs a notice and floods it to every connected peer, where steps 1 to 3 run on arrival (each peer chains `workload.revoke` as `revoked_by: mesh:<signer>`). Otherwise the verb prints `notice: not issued: ...` and the revocation holds on this node only.

A revoked package, or one signed by a revoked key, is refused at verify, place, load and start. A request that names no package, signer or artifact is refused too, so a caller cannot omit them to avoid the check. Stopping or unloading a revoked instance is never blocked. To lift one, `unrevoke` is a kernel call that chains `workload.unrevoke`; it is undone on any node whose peers re-send the notice, so lift it on every node and delete `revocation-notices.json` from each runtime directory (see the revocation limits in `docs/research/mesh-placement/swarm-throughput.md`).

`weaver cog install` and `weft-cog-repo verify` / `install` / `publish` also refuse a cog signed by a revoked signer key (the weftos release key or a private-repo key): `weaver` reads the kernel's list in the runtime dir and refuses outright if that file cannot be read; `weft-cog-repo` and the cog host's `/install` read `--revocations <file>` (CLI only), else `revoked_subjects.json` in the same runtime dir `weaver` resolves (`$WEFTOS_RUNTIME_DIR`, the project's `.weftos/runtime`, or `~/.clawft`), and fail if that dir cannot be located (no `$HOME`, no override) or the file is unreadable.

Revoking a signer key is how you respond to a leaked package key; also remove its pin from `workload-trust.json` (or the compiled set in a release). For a private-repo key, revoking is on the consumer side: remove or replace the key in each project's source (`weaver cog source remove` then `add --key <new>`).

**Limits:** instances on a Cognitum Seed are not stopped by a revocation (the Seed holds its own store; a revoked store cog is refused at the next place or start). A remote `workload-host` that is not connected when you revoke keeps its instances until it receives the notice or you stop them.

The catalog RPCs `workload.install` and `workload.unload` (daemon RPC only, with no weaver CLI verb) are default-deny: they need an entry in `workload-permits.json`. They decide as the principal `catalog`, on packages that count as `unsigned` because the catalog only records a name, kind and manifest hash and verifies nothing. So the permit has to say both, and a permit that accepts unsigned packages without naming principals is refused when the file is read (the daemon then refuses every catalog verb until it is fixed):

```json
[{"id": "catalog", "actions": ["workload.install", "workload.unload"], "kinds": ["cog"],
  "min_package_trust": "unsigned", "principals": ["catalog"]}]
```

**Permits for native cogs need `"max_network": "egress"`.** A native cog runs with the host's network (ADR-100 s4), so placement classes it `egress`, while a permit's `max_network` defaults to `lan`. A `workload.place` permit for kind `cog` written with defaults therefore refuses every native cog. The daemon logs a warning when it reads such a permit, and the refusal names the failing condition, for example `permit 'cog': network Egress exceeds max_network Lan`. A working placement permit:

```json
[{"id": "cogs", "actions": ["workload.place", "workload.load", "workload.start", "workload.stop"],
  "kinds": ["cog"], "max_network": "egress"}]
```

Without a matching permit the verb is refused and the refusal is chained. Policy files in the runtime dir (`workload-permits.json`, `workload-trust.json`, peers, container, seeds) must be owned by the daemon's user (or root) and not group- or world-writable (`chmod 600`); the daemon refuses a file that is not.

**Upgrading: three things now stop the placement plane from building.** When the daemon cannot read its policy files it refuses all placement (`place`, `explain`, `status`, `stop`, `logs`, `unload`) with "placement unavailable: ..." until you fix the file; `weaver workload revoke` still works throughout. The causes, and the fix for each:

- a permit in `workload-permits.json` with `"min_package_trust": "unsigned"` and no `"principals"`: add `"principals": ["catalog"]` (the permit above), or raise the trust floor;
- a policy file with mode 0664 or any group/world write bit: `chmod 600 <runtime dir>/workload-*.json`;
- a policy file owned by another user: `chown <daemon user> <runtime dir>/workload-*.json`.

The error names the file and the rule it broke. Fix it and the next call rebuilds the plane; no restart is needed.

**Catalog revocation is by name only.** The catalog does not verify what it records, so `workload revoke --package <name>` is the only revocation that reaches a `workload install` entry (and a `blake3:` manifest hash revokes it as an artifact hash). Revoking the signer key of a package you installed elsewhere does not stop a catalog entry of the same name: revoke the name too.

**Store (Seed) cogs** are revoked by the synthetic package id `store.<cog id>.<version>`, for example `weaver workload revoke --package store.fall-detect.1.0.0`. A package id that is 64 hex characters is matched case-insensitively.

**Retries.** A forced unload that fails (an adapter busy) is retried every 60 seconds by the daemon, and once at start-up, so you do not have to revoke again.
