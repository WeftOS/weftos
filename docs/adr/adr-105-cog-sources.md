# ADR-105: Cog sources, a multi-repository cog catalog for projects

- **Status**: Accepted (2026-10-02, owner requirement: "a project can pull from Cognitum (if licensed), WeftOS and their own private cog repo, and we should document how that works")
- **Successor**: [COG-105](https://github.com/WeftOS/weftos-cogs/blob/v0.1.4/docs/decisions/COG-105-cog-sources.md) in WeftOS/weftos-cogs (Cogs extraction, 2026-10-27). Cogs owns registry formats, source resolution, publishing, catalog merging and private-source tooling. This ADR still owns project configuration and trust enforcement. The Cogs product crates it names now live in that repository without the `weftos-` prefix. This record stays as written.
- **Date**: 2026-10-02
- **Deciders**: Owner / platform
- **Depends-On**: ADR-099 (governed workload placement, section 8 trust), ADR-100 (cog workload kind, sections 1 and 6), ADR-103 (projects, user daemon), COG-008 (signed registry format, in the private cogs repo; implemented in `crates/weftos-cog-repo`)
- **Relates-To**: ADR-104 (sensor guides), `docs/cogs/cog-sources.md` (how-to), `docs/cogs/operator-guide.md` (pack, sign, pair, place, revoke)
- **Implementation**: `crates/weftos-cog-sources` (library), `crates/weftos-cog-repo` (private repo commands), `crates/clawft-weave/src/commands/cog_cmd.rs` (`weaver cog ...`, `weaver workload catalog`), `crates/clawft-kernel/src/workload_pkg/` (trust fixes)

## Context

Until now the cog catalog merged exactly two fixed inputs (`weftos-cog-market`): the WeaveLogic signed registry and a read-only mirror of Cognitum's `app-registry.json`, and `weftos-cog-host` knew three origins (`weavelogic`, `cognitum`, `local`). A WeftOS project could not choose where its cogs come from, could not add a registry of its own, and had no rule for Cognitum cogs that need a licence. The three origins also have three different trust properties, and the trust code in the kernel had four open findings (card 78167c90).

## Decision

### 1. Sources are configured per project, with a user default

A **cog source** is one entry of a `[[cog_source]]` list:

| Field | Meaning |
|---|---|
| `name` | namespace, `[a-z0-9][a-z0-9_-]{0,31}`, never contains `:` |
| `kind` | `weftos`, `cognitum` or `private` |
| `url` | `registry.json` / `app-registry.json` URL or path, or a repo directory or URL prefix holding `registry.json` |
| `pinned_keys` | Ed25519 public keys, 64 hex. Required for `private`. **Refused on `weftos`**: the WeftOS anchors are compiled in and a project file cannot add to them. Accepted but unused today on `cognitum` (reserved for the optional release-record verifier) |
| `allow_insecure` | default false; development only, lets a `cognitum` source use `http://` or a local path. Honoured only from the user file (a project file's value is dropped in the merge, with a warning); `weaver cog source add --allow-insecure` requires `--user` |
| `priority` | integer, default 0, higher wins |
| `enabled` | default true; a disabled source is kept in the file and skipped |

Where it is stored (decided): **`<project root>/.weftos/cog-sources.toml`** for the project and **`~/.weftos/cog-sources.toml`** for the user default. The effective list is the user default overlaid with the project file; a project entry with the same `name` replaces the user entry (including to disable it). Licence entries (section 3) merge the same way, project first.

Why not inside `project.toml` or `~/.weftos/projects/<id>.toml`: `project.toml` is the project's immutable identity (ADR-103 D5), and the manifest is rewritten by the user daemon and the supervisor under a lock, so an operator CLI editing it would race those writers. A separate file keeps the CLI a plain file edit, can be committed with the project (it holds public keys, URLs and licence declarations, never a secret), and leaves a later move into the manifest open (open question 1).

There is no built-in default `weftos` source URL: the WeaveLogic registry has no fixed public location in this tree, so the operator adds it (`weaver cog source add weftos --kind weftos --url ...`). A `cognitum` source defaults to Cognitum's public `app-registry.json`.

### 2. The three kinds and what is trusted

| Kind | Listing | Install requires | Binary check | Placement-eligible |
|---|---|---|---|---|
| `weftos` | COG-008 `registry.json` | nothing | size, sha256, Ed25519 signature by the pinned WeaveLogic release key or a compiled-in WeftOS package signer. Only these may label a cog WeaveLogic | yes, after packing (below) |
| `private` | same COG-008 format | nothing | size, sha256, Ed25519 signature by a key in `pinned_keys`. The WeaveLogic key is never implicitly trusted here | yes, after packing |
| `cognitum` | Cognitum `app-registry.json` | a licence entitlement for that cog (section 3) | sha256 listed by the registry (a registry entry without a usable sha256 is refused); the source and the binary location must be `https://` (`insecure_transport`), and a redirect from https to http is refused; optional release record | no, until an operator hashes and signs it (ADR-100 6.3) |

The project file is untrusted input (a clone ships its own). It cannot widen the `weftos` anchors, it cannot change the kind of a source the user defined, and a project entry that replaces a user entry is reported with what changed; `weaver cog install` states the resolved `source:id` and signer count before fetching and asks for the namespaced id (or a flag) before `--enable` on a bare id that resolved to a project-defined source.

Signed-only is unchanged for the first two kinds: unsigned, signed by another key, or a size or hash mismatch are each refused, and the check runs before the binary is written anywhere. "Placement-eligible after packing" means: governed placement (ADR-099) needs a `cogpkg` signed by a pinned signer (`weaver workload pack`), whatever source supplied the binary. Installing a cog from a registry into a cog-host root (the appliance path, `weftos-cog-host`) and placing it as a governed workload are two different operations; this ADR covers the first and records the second as the follow-up (open question 3).

### 3. Cognitum licensing

A project declares what it is entitled to with a `[[cog_licence]]` record:

```toml
[[cog_licence]]
source  = "cognitum"            # the name of the source
cogs    = ["fall-detect", "baby-cry"]   # or  cogs = "all"
account = "acme"                # informational; recorded in provenance
expires = "2027-01-31"          # YYYY-MM-DD (valid through that UTC day) or RFC 3339; absent = no expiry
```

- Cognitum cogs are always listable and searchable.
- Installing one checks, before any download: a licence names the source, covers the cog (listed or `all`), and has not expired. Otherwise the install is refused with `cog_unlicensed` or `licence_expired`. An expired licence next to a valid one for the same cog does not block it.
- **The check is presence, coverage and expiry. It does not verify payment and it is not a cryptographic proof.** The licence is a declared local record, not a signed token. Cognitum's 30% per cog is contractual and is not enforced in code (ADR-100 6.4, ADR-099 8.4). The record exists so a project cannot install a licensed cog by mistake and so provenance can show which licence allowed an install.
- A Cognitum binary is not signed, so it stays subject to ADR-100 trust: the registry sha256 is checked, a Cognitum ADR-154/155 release record is an optional extra verifier (`weaver workload verify --cognitum-release`, applies to `cogpkg` packages), and an upstream binary can be used in governed placement only after an operator hashes and signs it into a package. The install result says so (`placement_eligible = false`).

### 4. Private repositories

A private repo is the same COG-008 format, signed by a key the project holds and pins. `weft-cog-repo` gets the authoring commands: `init <dir> --name N`, `keygen --out key.pem [--repo dir]`, `add <dir> --binary ...`, `sign <dir> --key key.pem`, `verify <dir>`, `publish <dir> --to <out>`. Rules:

- `keygen` writes a PKCS#8 PEM with mode 0600 created with `create_new` (an existing key is never overwritten), refuses to write inside the repo directory, and `init` writes a `.gitignore` for `*.pem`, `*.key`, `*.seed`. Keys are never committed by the tool.
- `repo.toml` holds the repo name and the public key. `sign` refuses a key that does not match it, so the repo cannot be signed with the wrong key by accident.
- `publish` verifies every artifact first, refuses to copy anything that looks like key material, and copies to a directory. **It never uploads.** Hosting that directory (static HTTPS, object storage, a file share) is the operator's job. It prints the `[[cog_source]]` snippet for consumers.
- Consumers refuse a wrong key, an unsigned artifact, and a hash mismatch (tested).

### 5. Resolution

Cog ids are namespaced `source:cog-id` (`weftos:fall-detect`, `cognitum:fall-detect`, `acme-private:fall-detect`). A namespaced id goes to that source only (`unknown_source`, `source_disabled`, `cog_not_found` otherwise). A bare id is looked up in every enabled source: one match resolves; several resolve to the highest `priority`; if two sources tie at the top the error is `ambiguous`, naming every namespaced form to use. There is no silent pick.

The resolved package carries its provenance: source name and kind, registry location, version, arch, binary sha256, trust (`ed25519-signed` or `cognitum-sha256`), signer public key and key id (`ed25519:` plus 16 hex of the key's sha256), and licence account. It is written to `<cog dir>/provenance.json` on install, and `Provenance::chain_payload()` gives the `cog.source.resolved` event body. **Emitting that event onto a chain is not wired** (open question 2); the file is the record today.

### 6. Package trust fixes (card 78167c90)

Four findings in `workload_pkg`, each with a test:

1. **`WEFTOS_PINNED_SIGNERS` provisioning** is now documented (`docs/cogs/cog-sources.md`, "Key management"): how the WeftOS package signer is generated, kept in a secret and pinned, with a test of the documented flow. The set is still empty in this tree: the key has not been provisioned, so every accepted package signature is operator-pinned.
2. **Record-only trust is bound to `cog.toml` and the source.** A Cognitum release record binds cog id, version, source commit and one binary digest; it does not cover `cog.toml`. A package trusted through a record alone is now accepted only if the package's `cog.toml` BLAKE3 is in `VerifyPolicy.record_cog_toml_pins` (`--cog-toml-pin`, compared case-insensitively). **That pin is the binding.** The record's `sourceCommit` is checked too, but only as a consistency check (both sides at least 7 hex characters, case-insensitive prefix match): it ties the record to the same source the package claims, it does not bind `cog.toml`. Otherwise the package needs an operator signature, which covers `cog.toml`.
3. **An attached record is checked on operator-signed packages.** With the verifier on, a record that is forged, mismatched or unpinned fails the package even when a valid operator signature is also present. With the verifier off the attachment is inert, as before.
4. **One bad signature entry does not fail the whole verify.** An entry whose public key or signature is not valid hex of the right length is skipped like an unpinned one. It neither authenticates nor vetoes. A well-formed signature from a pinned key that fails to verify is still fatal (a tampered manifest still fails).

### 7. Tooling and catalog

`weaver cog source add|list|remove|enable|disable`, `weaver cog licence add|list|remove`, `weaver cog search`, `info`, `install`, and `weaver workload catalog --kind cog`. These run locally and need no daemon. Edits go to the project file (found by walking up to `.weftos/project.toml`), or to the user default with `--user`.

The command is `weaver cog ...`, not `weft cog ...`: `weaver` already owns the project and workload operator verbs (`weaver project`, `weaver workload`), and `weft` is the agent-facing CLI.

`weaver workload catalog --kind cog` lists every cog of every enabled source with arch coverage, hardware requirement, `[resources]` and secret config keys (from `cog.toml` when `--cog-toml-dir` points at the cog source tree), run mode and group (from the committed conformance baseline `scripts/cogs/expectations.json`), whether the id is in the Seed store (a Cognitum-kind source lists it) and which other sources list it, install access (`signed`, `licensed`, `needs-licence`, `licence-expired`), and a derived **default placement policy**: minimum node tier `paired`, signed package required, emulation operator opt-in only, runtime preference, capability requirements (`cpu.arch.*`, `runtime.native`, `mem.system>=`, `x.hw.*`, `feed.esp32-csi-udp`) and whether the cog is in the v1 scope (the clean `--once` group). The baseline reproduces the headline groups: **93 clean, 5 need `--interval`, 9 need seed peers, assets or other CLI**, plus 1 with no aarch64 build (108 entries; a Cognitum registry of the 107 buildable cogs gives 93 / 5 / 9). Of the nine, six are run mode `needs-seed` (`cognitive-pipeline`, `swarm-backup-restore`, `swarm-delta-sync`, `swarm-deploy`, `swarm-distributed-store`, `swarm-edge-orchestrator`: they need seed peers or the Seed API) and three are `needs-extra` (`cloud-inference`, `swarm-mqtt-bridge`, `tailscale`: other setup such as a broker, an auth key or their own CLI).

## Consequences

Positive: a project chooses and orders its cog sources; a private registry is a first-class source with a clear key story; unlicensed Cognitum installs are refused before any download; every install records where it came from and what vouched for it; the four trust findings are closed with tests.

Cost and limits: a licence is a declaration, not enforcement. Registry installs land in a cog-host root and are not yet governed placements. No chain event is emitted for an install. The WeftOS signer set is still empty.

## Open questions

1. **Manifest home.** Should the source list move into the user-daemon project manifest (and be served by the daemon, so `weaver cog` works against a remote user daemon) once the manifest has a safe single-writer path for operator edits?
2. **Chain events.** Emit `cog.source.resolved` (and an install refusal event) through the user or project chain when the install goes through the daemon. The payload exists; the wiring does not.
3. **From registry to placement.** A `weaver workload pack --from-source <source:cog>` that builds and signs a `cogpkg` from a verified registry artifact, so a registry cog can be placed as a governed workload in one step. Needs the cog's `cog.toml`, which COG-008 registries do not carry.
4. **Signed licences.** A signed entitlement record (issued by Cognitum, verified against a pinned Cognitum key) would turn the declaration into a proof. Cognitum has no such artifact today.
5. **Registry hosting for WeftOS.** The WeaveLogic registry has no fixed public URL in this tree, hence no default `weftos` source.
6. **Compiled-in signer provisioning.** Generate and pin the first `WEFTOS_PINNED_SIGNERS` key (owner action; see the key management section of the guide). Until then the compiled set is empty.
7. **Cognitum release records on the registry path.** Installs from a Cognitum source check sha256 only; verifying a release record there needs the record published next to the binary, which the app registry does not do (only `anomaly-detect` is release-eligible upstream).
8. **Rollback and substitution in COG-008.** COG-008 signs only the binary bytes and `registry.json` itself is unsigned, so a mirror can serve an older signed binary (rollback) or a different signed binary under another cog id (cross-id substitution); version and id are not covered. Future fix: sign a statement over `(id, version, arch, sha256)` and verify it with the binary signature.
