# Vault V1: KDF floor change (owner 2026-11-15)

Date: 2026-11-15. Board card 02301ad0 (Vault V1). Owner-approved change to section 3.1 of
`docs/research/vault-v0-format-spec-2026-11-15.md` (branch `w2/card-a6f0b37d`). Status: DRAFT, same external review
gate as V0 (owner Q6). The DRAFT flag stays set.

## Decision

Owner, 2026-11-15 3:42 PM ET: "Vault KDF floor: RAISE to 64 MiB, t=3. Update spec and code; this is an owner-approved
spec change."

**This supersedes the "Policy for NEW wrappers" column of spec 3.1.** The format-valid ranges in that table do not
change.

| Field | Format-valid range (unchanged) | Policy for NEW and REWRITTEN passcode wrappers (new) |
|---|---|---|
| `m_kib` | 8192 to 1048576 (8 MiB to 1 GiB) | at least 65536 (64 MiB); recommended 65536 |
| `t` | 1 to 10 | at least 3, at any `m_kib`; recommended 3 |
| `p` | 1 to 16 | recommended 1 (no floor beyond the format range) |

The old policy was `m_kib` at least 19456, and `t` at least 2 when `m_kib` was below 46080. Both conditions now hold
separately, so 64 MiB at t=2 and 32 MiB at t=3 are both refused.

"New and rewritten" means every write that seals a passcode wrapper: `Vault::create`, `Vault::change_passcode` and
`Vault::rotate_vk`. `Vault::create` and `Vault::change_passcode` check the policy before any Argon2id work.
`Vault::rotate_vk` checks it after opening a wrapper with the stored parameters (see "Rotation refuses"). The check is
`KdfParams::check_new_policy`, with constants `KdfParams::MIN_NEW_M_KIB = 65536` and `KdfParams::MIN_NEW_T = 3`.

## Rationale

The V0 benchmark (`docs/research/vault-v0-argon2-benchmark-2026-11-15.md`, commit c27130a36) measured the recommended
set, 64 MiB with t=3 and p=1, at about 107 ms on the M5 Max and 130 ms in the aarch64 VM on the same chip. That is
cheap enough on the owner's machines that keeping a lower floor bought nothing but a weaker option. The old floor was
the OWASP minimum (19 MiB, t=2). The new floor is RFC 9106's second recommended profile in memory and passes (64 MiB,
t=3), at one lane.

Consequence: the benchmark's "step down to the floor on a slow device" fallback (46 MiB, t=2) is gone. A device too
slow for 64 MiB at t=3 cannot create a vault or set a passcode. Phones and Pi-class cores are still unmeasured
(benchmark section 5), so if a supported device turns out too slow, that is a new owner decision, not a lowered floor in
code.

## Existing wrappers below the floor

A passcode wrapper written under the old floor is format-valid, so a vault that has one is not locked out.

- **Unlock.** `Vault::unlock` opens it with its stored parameters, as before.
- **Report.** `Vault::below_policy_wrappers` lists every current-generation passcode wrapper below the floor. A caller
  checks it after every unlock and, when it is not empty, asks the user to set the passcode again.
- **Rotation refuses when the given passcode opens a weak wrapper.** `Vault::rotate_vk` derives with each stored
  wrapper's own parameters and checks the policy only after a wrapper opens. If the passcode it was given opens a
  current-generation wrapper below the floor, it returns `KdfPolicy` and changes nothing, rather than re-sealing that
  wrapper under the new VK with its old parameters (review M2). A below-floor wrapper that passcode does not open is
  dropped like any other wrapper rotation cannot open, and the rotation succeeds, so nothing weak is carried into the
  new generation. The owner of that other passcode is locked out of the new keybag, as for any dropped passcode wrapper.
  A wrong passcode therefore answers `UnlockFailed` and the right one `KdfPolicy` on a vault whose only wrapper is
  weak; the caller already holds the unlocked VK, so this tells it nothing.
- **Passcode change refuses weak parameters.** `Vault::change_passcode` refuses any parameters below the floor. At the
  floor it replaces the wrapper (new salt, nonce and parameters), which clears it from `below_policy_wrappers` and
  unblocks rotation.
- **Other writes go through.** Adding a recovery or device wrapper, removing another wrapper and dropping history carry
  the legacy wrapper byte for byte. They do not touch its KEK or ciphertext, so its exposure is unchanged, and refusing
  them would block safety actions such as removing a lost device's wrapper or adding a recovery key.

Why not refuse to unlock: the data would be unreachable until a code path that does not exist yet (re-setting a
passcode without unlocking) is written, and the weak wrapper's ciphertext is already in the store either way. Refusing
to unlock protects nothing that a forced re-set after unlock does not.

## Test vectors and test profiles

- The spec 10 keybag vector already uses `m=65536, t=3, p=1`, which meets the new floor. No published vector changes.
- The 8 MiB, t=1 profile (`KdfParams::TEST`) is `#[cfg(test)]` and test-only. Tests reach it through crate-private
  entry points that skip the policy check: `change_passcode_unchecked` and `rotate_vk_unchecked` are `#[cfg(test)]`,
  and `create_unchecked` is the body `Vault::create` runs after its own policy check. No public path skips the check.
- The crate's unit tests that pay full-cost Argon2id are the published vector tests, review M2's regression test
  (passcode change and rotation at the floor) and `tests_kdf_floor::create_accepts_the_floor`. The other floor tests are
  refusals, which happen before any KDF work, or use the legacy 19 MiB profile.
