# Skill Signing and Trust Root

This guide documents how clawft signs and verifies skill packages, where
the trust-root configuration lives, how to manage signing keys, and how
key rotation works.

If you are looking to install or write a skill, start with
[skills-and-agents.md](skills-and-agents.md). This document is the
operator-facing companion that covers the cryptographic side.

> **Status**: Skill signing ships behind the `signing` Cargo feature on
> the native build. The end-to-end *trust-root* path -- a registry of
> trusted Ed25519 public keys that determines which signatures are
> accepted on install -- is *partially implemented* as of 0.7.0. The
> sections below mark `[CURRENT]` for behavior shipped today and
> `[PLANNED]` for behavior that lands in a follow-up release. Plane
> issue: WEFT-69.

## Quick reference

| Item | Path |
|------|------|
| Author keypair (private) | `~/.clawft/keys/skill-signing.key` (mode `0600`) |
| Author keypair (public) | `~/.clawft/keys/skill-signing.pub` |
| Workspace trust override | `<workspace>/.clawft/trusted-keys.json` `[PLANNED]` |
| Global trust root | `~/.clawft/trusted-keys.json` `[PLANNED]` |
| Cargo feature gate | `--features signing` (cryptographic Ed25519 path) |
| Implementation | `crates/clawft-core/src/security/signing.rs` |
| CLI | `weft skills keygen`, `weft skills publish` |

## What signing covers

A signed skill package carries:

1. A **content hash** -- a SHA-256 over every non-hidden, non-`target/`
   file in the skill directory, computed deterministically (sorted
   relative paths, length-prefixed contents). Implementation:
   `compute_content_hash` in `signing.rs`.
2. An **Ed25519 signature** over the hex-encoded content hash.
3. The **public key** of the signer, hex-encoded, embedded in the
   signature blob. Implementation: `SkillSignature` in `signing.rs`.

Verification recomputes the content hash against the package on disk
and checks the signature against the embedded public key. The signing
feature uses `ed25519-dalek` for both signing and verification.

The install path uses `verify_signature` from
`clawft-core::security::signing`. A standalone verify command is not yet
implemented.

## The author keypair

`weft skills keygen` writes the author's Ed25519 keypair to
`~/.clawft/keys/`:

```text
~/.clawft/keys/
  skill-signing.key   # 32-byte private key, hex-encoded, mode 0600
  skill-signing.pub   # 32-byte public key, hex-encoded
```

The private key is the secret that proves the author's identity when
publishing. Treat it the same way you treat an SSH private key:

- Mode `0600` is set automatically on Unix at create time.
- Back up the file out-of-band (the same way you'd back up an SSH key).
  If you lose it, you cannot publish updates that verify against the
  same public key.
- Never commit it to a repository. It is *not* gitignored by default
  because it lives outside any project tree.

`weft skills keygen` refuses to run if the key file already exists, to
protect against accidental overwrite:

```text
$ weft skills keygen
error: signing key already exists at ~/.clawft/keys/skill-signing.key.
       Remove it manually to generate a new one.
```

### Cargo feature gate

The `signing` feature on `clawft-core` enables real Ed25519 derivation
via `ed25519-dalek`. Builds *without* `--features signing` ship a
seed-only keygen (the private key file is real, the `.pub` file
contains the placeholder string `(derived on first sign)`). This is
intentional: we refuse to fall back to a non-cryptographic hash, but
we also don't carry `ed25519-dalek` for users who never sign.

Recommended build for publishers:

```bash
scripts/build.sh native --features signing,services
```

The `services` feature adds the ClawHub publish path; `signing` adds
the cryptographic primitives. WEFT-MW-1 (Plane) tracks defaulting
`signing` on for the standard release build so the placeholder pubkey
case stops shipping silently.

## The trust root `[PLANNED]`

The trust root is the operator-side answer to: "whose signatures will
this clawft installation accept?". A trust root is needed for the
verify step at install time -- without one, `verify_signature` only
proves the signer holds the key paired with the embedded public key,
not that the operator considers that key authoritative.

### Planned location

```text
~/.clawft/trusted-keys.json     # global, applies to every workspace
<workspace>/.clawft/trusted-keys.json  # workspace override
```

The workspace file overrides the global file via the same deep-merge
rules used for `config.json`: most-specific wins, with `null` deletes
and array replacement (no concatenation). Implementation will piggy-
back on `clawft-core::config_merge::deep_merge`.

### Planned format

```json
{
  "version": 1,
  "keys": [
    {
      "id": "operator-team-ci",
      "algorithm": "ed25519",
      "public_key": "0123456789abcdef...",
      "added_at": "2026-04-28T10:00:00Z",
      "comment": "Team CI signing key, rotation due 2027-01"
    },
    {
      "id": "alice-personal",
      "algorithm": "ed25519",
      "public_key": "fedcba9876543210...",
      "added_at": "2026-03-15T09:30:00Z"
    }
  ]
}
```

Field semantics:

- `id` -- operator-readable label (unique within the file). Used in
  CLI output and error messages.
- `algorithm` -- always `"ed25519"` today. Reserved for future curves.
- `public_key` -- 64-char hex (32 bytes). Matches the format of
  `skill-signing.pub`.
- `added_at` -- RFC-3339 timestamp. For audit-log purposes only; not
  used in verification.
- `comment` -- free-form. Encouraged for tracking rotation due dates,
  team membership, etc.

### Planned CLI

A `keys` command group (list, add, remove, verify) is planned and not yet
implemented: `weft skills` has no `keys` subcommand.

Until it lands, operators can manage the file by hand. The schema
above is the contract.

### Planned workspace override semantics

When clawft loads a workspace and resolves a skill at install time, the
trust root in effect is:

1. Start with `~/.clawft/trusted-keys.json` (if present).
2. Deep-merge `<workspace>/.clawft/trusted-keys.json` (if present) on
   top.
3. Iterate the resulting `keys` array; a signature verifies if its
   embedded public key matches any entry's `public_key`.

A workspace can therefore *add* keys that are trusted only inside that
workspace, *remove* a globally-trusted key by setting its entry to
`null` in the workspace overlay, or *replace* the entire `keys` array
to take a hard stance.

This matches the pattern the rest of clawft uses for layered config
(see [workspaces.md](workspaces.md), "Config Merging").

## Today's behavior `[CURRENT]`

In 0.7.0, the install path *generates* and *checks* signatures
mathematically but does not yet consult a trust-root file. The
practical implication:

- Signed skills installed from ClawHub verify the signature against
  the embedded pubkey only. There is no operator-controllable allowlist.
- Unsigned skills install when `--allow-unsigned` is set; the CLI
  prints a warning otherwise.
- Local installs (`weft skills install <path>`) do not touch the
  signing path at all -- they copy the directory verbatim.

If you need a stronger posture today, run with `--features signing`
*and* avoid `--allow-unsigned`; this guarantees that any signature
present is cryptographically valid even though the trust-root
allowlist isn't enforced yet.

## Key rotation

### When to rotate

- The private key file is suspected compromised (lost laptop, leaked
  backup, accidental commit).
- A team member leaves and held a copy of the key.
- Periodic rotation per the comment hint in `trusted-keys.json`
  (recommended: every 12 months for team keys, 24 months for personal).

### How to rotate

1. **Generate a new keypair** under a different filename so the old key
   stays available for the rollover window:

   ```bash
   mv ~/.clawft/keys/skill-signing.key ~/.clawft/keys/skill-signing.key.old
   mv ~/.clawft/keys/skill-signing.pub ~/.clawft/keys/skill-signing.pub.old
   weft skills keygen
   ```

   This produces a fresh `skill-signing.{key,pub}` pair.

2. **Update the trust root**: add the new public key to
   `~/.clawft/trusted-keys.json` (or the workspace overlay). Keep the
   old key for the rollover window so already-installed skills with
   the old signature still verify.

3. **Re-publish** all skills you control with the new key. Use
   `weft skills publish <path>`; the new `skill-signing.key` is picked
   up automatically.

4. **Remove the old key** from `trusted-keys.json` once the rollover
   window closes (recommended: 30-90 days). Delete the
   `skill-signing.{key,pub}.old` files at the same time.

### Rollover window guidance

- **24-72 hours**: emergency rotation (suspected compromise). Long
  enough to push out re-signed packages, short enough to limit damage.
- **30-90 days**: scheduled rotation. Gives downstream consumers time
  to sync the new public key before the old one stops verifying.

### Rotating the trust-root file itself

The trust-root file is plain JSON; any normal rotation flow works:

- Add the new key with a new `id` and `added_at`.
- Optionally bump `version` if you change the schema (not for
  add/remove; only for shape changes).
- For multi-operator teams, store the trust-root file in a shared
  location and symlink (`~/.clawft/trusted-keys.json -> /shared/...`),
  or vendor it via your config-management tool (Ansible, Chef,
  Nix-home-manager, etc.).

## Threat model and limitations

What signing protects against:

- **Tampering in transit**: a modified package fails the content-hash
  check at install time.
- **Identity confusion**: a ClawHub upload from a different account
  cannot impersonate yours without your private key.

What signing does *not* protect against today:

- **Trust-root allowlist** is not yet enforced (`[PLANNED]` -- WEFT-69
  follow-up work). Until it lands, a signed-but-untrusted key still
  passes mathematical verification.
- **Time-of-check/time-of-use** for already-installed skills. The
  install-time verification proves the package matched the signature
  at install; subsequent edits inside `~/.clawft/skills/` are not
  re-checked. This is intentional -- operators may edit installed
  skills.
- **Key revocation**. There is no published revocation list. Removing
  a key from `trusted-keys.json` stops *new* installs but does not
  retroactively un-trust already-installed skills.
- **Out-of-band trust establishment**. Adding a key to
  `trusted-keys.json` is itself a trust decision; clawft does nothing
  to verify that the operator's chosen pubkey actually belongs to
  the named author. Use the same out-of-band channels you'd use for
  GPG (key signing parties, fingerprint confirmation over a second
  channel, vendor-published fingerprints).

## See also

- `crates/clawft-core/src/security/signing.rs` -- implementation.
- `crates/clawft-cli/src/commands/skills_cmd.rs` -- `weft skills`
  command surface.
- `crates/clawft-kernel/src/wasm_runner/registry.rs` -- analogous
  trust-root handling for kernel-level WASM tools (in-memory
  `add_trusted_key` API; shape mirrors what the skill trust root
  will look like on disk).
- [skills-and-agents.md](skills-and-agents.md) -- skill authoring and
  install workflow.
- [workspaces.md](workspaces.md) -- the layered config-merge rules
  the trust root will reuse.
- ADR-021 (CLI/kernel compliance) -- why `weft skills publish` routes
  through the daemon.
