# Vault V1: rotation spec defect, pins and unlock limits

Date: 2026-11-15. Board card 02301ad0 (Vault V1). Addendum to `docs/research/vault-v0-format-spec-2026-11-15.md`
(branch `w2/card-a6f0b37d`) and `docs/research/vault-v1-index-format-2026-11-15.md`. It answers the crypto review
`reports/review-vault-v1-crypto-20261115.md` (H1, M1, M2, L2, L4, L5). Status: DRAFT, same external review gate as V0
(owner Q6). The DRAFT flag stays set.

## 1. The spec defect: 5.4 contradicts 9.4

- Section 9.4 is titled "Rotate VK (after a device compromise)".
- Section 5.4 keeps the same signing seed and `sign_pub` across a rotation, and says "any device that can unlock the
  vault can sign a new keybag".
- The V1 code also re-seals the passcode wrappers under the same passcode.

A device compromised before the rotation held the old VK and the signing seed, and probably saw the passcode typed.
After the rotation it can still:

1. re-sign any pre-rotation keybag at a higher sequence. Before this fix, a pinned device accepted it, and the passcode
   unlocked the old VK. New secrets were then sealed under a generation the attacker can read (review probe R1);
2. write any new keybag: drop wrappers, delete history, fork or brick the vault.

So rotation as specified does not recover from a device compromise. This is a defect in the spec. The code
followed the spec faithfully.

## 2. What V1 does now (implemented)

- **Generation pin (H1).** `Pins` records the highest `current_vk_generation` seen. `Keybag::parse` refuses a keybag
  with a lower generation (`GenerationRollback`), even when its sequence is higher and its signature verifies. This
  protects only devices that have already seen the rotation.
- **Own writes advance the pins, once the store has them (M1, round-2 R2-M1).** Writes are two-phase. Every write
  method (`change_passcode`, `add_recovery_wrapper`, `add_device_wrapper`, `remove_wrapper`, `rotate_vk`,
  `drop_history`) takes `&self` and `&Pins` and changes nothing: it returns a `PendingWrite` holding the new signed
  keybag. The caller stores `PendingWrite::keybag_bytes` with `put_if_match` against `Vault::keybag_bytes`. Only
  when the store accepts does the caller call `Vault::commit_write(pending, &mut pins)`. That installs the new keybag
  (and, after a rotation, the new VK) in the vault and records it in the pins, so a store operator cannot replay the
  keybag from before the write to the device that made it. What holds:
  - Preparing a write never changes the vault or the pins. It is refused when the pins belong to another signer
    (`SignerMismatch`) or are already past this vault's keybag (`Rollback`).
  - When the store refuses the write, the caller drops the `PendingWrite`. The vault and the pins are then byte for
    byte as they were before the attempt, so the device re-reads the store and unlocks the other device's keybag
    with no `Fork` and no `GenerationRollback` (review probes P1 and P1b are now tests).
  - A VK made by `rotate_vk` lives only inside the `PendingWrite`. The vault keeps sealing under the stored
    generation until the commit, so no secret is sealed under a VK that no stored wrapper holds.
  - `commit_write` refuses, changing nothing, a write built on other bytes than the vault holds (`StaleWrite`: a
    second write from the same base, or another vault's), and a write whose sequence the pins have already seen
    with other bytes (`Fork`) or passed (`Rollback`, `GenerationRollback`). That catches a caller that commits a
    write the store refused after it has already unlocked the store's keybag. It cannot catch a caller that commits
    a refused write before re-reading the store: `commit_write` trusts the caller's report that the store
    accepted. Such a caller is back in the round-2 P1 state, so the contract is "commit only after
    `put_if_match` succeeded".
  - `Vault::create` returns the creator's pins in `NewVault::pins`, so the creating device never trusts the store on
    first use. Creating a vault is not two-phase: the first `put_if_match` (expecting no object) either stores it
    or the caller discards the new vault and its pins.
- **Pins are crate-recorded only (R2-L1).** `Pins` fields are private and `record` is crate-private. The crate
  records a keybag only after it verified it (an unlock, a create, a committed write). Callers read the pins through
  getters and persist them with `Pins::to_bytes` / `Pins::from_bytes` (82 bytes, strict parse). The persisted
  bytes are device-local trusted state: anyone who can rewrite them can reset the device to trust on first use.
- **The rotation doc is corrected.** `rotate_vk` no longer claims to recover from a compromise.
- **Rotation does not carry weak KDF parameters forward (M2).** `rotate_vk` refuses (`KdfPolicy`) to re-seal a passcode
  wrapper whose parameters are below the policy for new wrappers. `Vault::below_policy_wrappers` lists those wrappers
  after an unlock, so the caller can prompt for `change_passcode` first.

## 3. Recommended spec fix (not implemented; needs the external format review, owner Q6)

Rotation becomes compromise recovery only when all of these change together:

1. **A new signing seed.** Rotation generates a new seed and `sign_pub`, sealed in the new kind 5 entry under the new
   VK. The old seed is never used again.
2. **A new passcode.** `rotate_vk` takes a new passcode (policy checked) and seals new passcode wrappers with it. The
   old passcode wrappers are dropped. Recovery wrappers are re-issued with a new recovery key.
3. **An authenticated pin transition.** A device pinned to the old `sign_pub` and generation g accepts a keybag signed
   by a new key at g+1 only after an unlock in which the new VK opens a kind 4 entry that yields the VK of the pinned
   generation g. Only the holder of the new VK can build that link, and the new VK is sealed only under the new
   passcode, recovery key and device wrappers. A compromised device that kept the old seed and VK cannot forge the
   link. It can still serve old keybags, and the generation pin refuses them.
4. **Unpinned devices.** A device that first sees the vault after the rotation pins whatever it is shown (TOFU, as
   today). Enrolment should carry the expected `sign_pub` and generation.

Until this lands, the user-facing response to a device compromise is to create a new vault and move the secrets.

## 4. Unlock and passcode limits (L2, L4)

- **At most 4 passcode wrappers** per keybag (`MAX_PASSCODE_WRAPPERS`). `unlock` tries each one.
- **Total KDF cost at most 1 GiB x 10 passes** (`MAX_UNLOCK_KDF_COST`, the sum of `m_kib * t` over all passcode
  wrappers). That is the cost of one wrapper at the format maximum. A signed keybag therefore cannot make one unlock
  attempt cost more than the single worst format-valid wrapper. Review probe R3 (63 wrappers at 1 GiB, t=10) is refused
  at parse, before any Argon2id run. Both checks are part of the structural rules, so they apply on parse and on every
  write.
- **Passcode rules when a passcode is set** (create, change, rotation re-seal). The passcode must not be empty. It
  must contain no unassigned, private-use, noncharacter or control code point, as judged by the pinned
  `unicode-normalization =0.1.26` (Unicode 18.0). NFKC of an unassigned code point can change when a later Unicode
  version assigns it, which would lock the user out after a dependency bump. The crate is pinned with `=`, so a bump
  is a deliberate, reviewed change. Unlock does not apply these rules, so an existing wrapper always opens.

## 5. Residuals (documented, no code change)

- **Blob and index rollback (L5, spec 7.5).** `open_secret` returns the revision but nothing anchors it, and the index
  has no anchor in the signed keybag. A store can replay a consistent older set of index and blobs. One option for
  the spec: put the index revision, or its hash, into the signed keybag.
- **Pins have no `vault_id` (L6).** Callers keep one `Pins` per vault. This is documented on `Pins`.
- **`drop_history` does not check the lazy re-seal pass (L6).** That check is the caller's job, as spec 9.4 says. The
  method doc says so.
- **Zeroization (L3).** Buffers holding the passcode, recovery key and recovery code are sized before they are filled
  and are zeroized, and `CreateInputs` holds its key material in `Zeroizing`. The normalizer's small internal buffer,
  and stack copies from `getrandom`, are outside the crate's control.
