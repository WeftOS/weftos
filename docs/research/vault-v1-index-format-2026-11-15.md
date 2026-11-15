# Vault V1: sealed index format and key rotation notes

Date: 2026-11-15. Board card 02301ad0 (Vault V1). Addendum to `docs/research/vault-v0-format-spec-2026-11-15.md`
(branch `w2/card-a6f0b37d`), which leaves the index only as "informative" in section 7.4. This file fixes it so the
code in `crates/weftos-vault/src/index.rs` and the spec cannot drift. Status: DRAFT, same review gate as V0. Every
name and value in the tests is fake.

## 1. Index blob (replaces section 7.4)

One blob at slot `index`: the section 7.1 layout, `type` 16, `policy` 0. Because it is an ordinary sealed blob it is
authenticated and encrypted under `blob_key_g`, with the slot, vault id, generation and revision in the AAD. There is no
separate signature. A tampered, truncated, extended, moved or other-vault index fails the AEAD tag and is refused.

No plaintext secret names reach the store: names, tags and sources exist only inside the sealed payload, and secret slots
are keyed hashes (`name_hash`, section 7.3). A grep of the stored bytes finds none of them (test
`no_plaintext_names_in_the_stored_bytes`, which first proves the same grep does find them in the plaintext).

### 1.1 Payload encoding

Section 7.4 suggested canonical JSON. The vault crate takes no JSON dependency, so V1 fixes a canonical binary encoding
instead. Integers are big-endian (section 2). `str` is `len` u8 then that many UTF-8 bytes.

| Field | Size | Rule |
|---|---|---|
| `index_version` | u16 | `1`. Anything else is refused |
| `name_generation` | u32 | VK generation whose `name_key` produced every `name_hash` below |
| `entry_count` | u32 | number of entries |
| entries | var | `entry_count` entries, strictly ascending by `name_hash`, no repeats |

Entry:

| Field | Size | Rule |
|---|---|---|
| `name_hash` | 32 | `BLAKE3.keyed_hash(name_key_{name_generation}, NFC(name))` |
| `type` | u8 | 1 to 4 (never 16: the index does not list itself) |
| `policy` | u8 | 0 or 1 |
| `revision` | u64 | revision of that secret's blob |
| `last_used_unix` | u64 | seconds, 0 = never |
| `name` | str | NFC, 1 to 255 bytes |
| `tag_count` | u8 | 0 to 16 |
| `tags` | `tag_count` x str | each 1 to 64 bytes |
| `source` | str | 0 to 64 bytes, empty = none. Reserved for imports (Q7) |

The whole payload plus the 6-byte plaintext header must fit the section 7.2 size classes (65530 payload bytes at most).
Values are never stored here.

### 1.2 Reader rules (reject, not repair)

The reader refuses: authentication failure, `type` other than 16, unknown `index_version`, a `name_generation` with no
key, unsorted or repeated hashes, a `name` that is not NFC or whose keyed hash differs from `name_hash`, out-of-range
lengths, and trailing bytes. A mismatch with the stored blobs is repaired by rebuilding the index from them, never by
merging.

### 1.3 Writer rules

Entries are sorted by `name_hash`, so the same set of entries always encodes to the same payload. Duplicate names,
non-NFC names, over-limit strings and an entry of type 16 are refused before sealing. After a VK rotation the index is
re-sealed under the new generation like any other blob; an index sealed under an older generation still opens through
the key history (section 2).

Rollback of one older index revision is the same residual gap as section 7.5: it is detected only against a local cache
of the index `revision`.

## 2. Rotation as implemented (section 9.4)

- `rotate_vk(passcode, recovery)` creates VK2 at generation g+1. Passcode wrappers that the given passcode opens are
  re-sealed (fresh salt and nonce). Recovery wrappers are re-sealed only when the recovery key is supplied and opens
  them, otherwise dropped. Device wrappers are dropped: the platform seal needs the device, so they are re-enrolled.
  If no passcode wrapper opens, nothing changes (a typo cannot lock the vault).
- Kind 4 (key history): the old VK is sealed under `HKDF(VK_new, "weftos-vault/v1/history-key" || g_new)` with the
  wrapper AAD of section 5.2 (`vk_generation` = the old generation). Existing kind 4 entries are re-sealed the same way,
  so every older generation opens in one step from the current VK, with no chain to walk.
- Keybag rule added: at most one kind 4 entry per generation, and each must be older than `current_vk_generation`.
- The signing seed is re-sealed under the new VK (same seed, same `sign_pub`), the sequence is bumped and the keybag is
  re-signed.
- Blobs: a reader opens a blob by its header generation, taking the VK from the history. New writes use the current
  generation. `reseal_secret(name, blob, expected_revision)` moves one secret to the current generation (refusing a blob at
  any other revision, round-2 R2-L2): it returns the new blob at the
  new slot (revision + 1), the stale slot, and a delete marker at `t/<old hash>` (type 3, empty payload).
  `drop_history(g)` removes a kind 4 entry once the pass is complete; blobs of that generation then stop opening.
