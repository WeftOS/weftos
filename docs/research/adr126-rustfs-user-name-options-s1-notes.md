# S1's notes for the RustFS user-name options page (2026-11-08)

For the end of `docs/research/adr126-rustfs-user-name-options.md`, which is on `feat/adr126-s3`; it is S3's file
and S1 did not edit that branch, so these notes live here until they are pasted in. They answer "Option B:
S1 adopts `node-<id>`", which is built and tested on `opt/adr126-s1-node-names` (not merged).

## What the branch does

- `backend::node_user_name(label)` returns `node-<id>` for a `node:<id>` label whose id passes S3's node-id rule (lowercase
  letters, digits, `-`, 1 to 48 characters, no leading or trailing `-`), else `None`. `RustfsBackend::key_id_for` uses it and
  falls back to `wk-<hash>`. Every other principal kind, and a node whose id is outside the rule (S1 principal ids also allow
  capitals, `.`, `_`, `@` and up to 128 characters), keeps `wk-<hash>`. The two prefixes cannot collide.
- Existing keys are not renamed behind anyone's back. `StoreService::rekey_principal` / `rekey_node_principals`, RPC
  `store.rekey {principal?}`, CLI `weaver store rekey [principal]` move a key issued under the old name: create the new user, give it the
  principal's current policy and enabled state, switch the saved state, then disable and delete the old user. No moment without a
  working key, and a suspended principal stays disabled. Chain event `store.key.rekey` (not a grant event, no `seq`). So the
  page's "RustFS cannot rename a user" is handled as delete-and-create, in that order.
- Tests: 5 service tests on the in-memory backend (new name, same policy, suspended stays off, a revoked grant stays revoked,
  bulk form, no-op cases), one naming test (20 000 random principals: unique, at most 53 characters, `node-` always means `node:`),
  an RPC test. Live, against a throwaway loopback RustFS (127.0.0.1:19311, stopped afterwards): a node principal's user is `node-<id>`
  and works; a hashed-name key is moved to `node-<id>`, the old user is gone, access is kept, and a revoke disables the new user;
  a 53-character name is accepted. Planted defects (node name ignored; old key not removed) each fail a test.
  `weftos-store` 121 passed, 17 ignored; `clawft-weave` store tests 45 passed; clippy clean; the diff gate adds nothing.

## Corrections and additions to the page

- **"S1 has users and secrets in the field" is not so.** S1 has deployed nothing: no store-capable weaver runs on any machine,
  so no `wk-` user exists outside throwaway test instances. The migration cost the page counts against B is zero today, and
  `store.rekey` exists for any daemon that did create hashed-name keys.
- **Same-name collision under B, not tested.** Under B both lanes address one user, `node-<id>`. RustFS `add-user` on an existing
  user replaces its secret, so S3's `provision_node` running after S1 created the user would leave S1's stored secret
  (`store-secrets/node-<id>`, 0600) stale. The page's follow-on (one creator: S1 creates and scopes the user, S3 only assumes
  roles and disables/enables it, reading the secret through `NodeKeys`) is therefore required for B, not optional. It is also what A
  needs, since both options leave two creators today.
- **Readable names have a cost too:** `node-<id>` puts a node-chosen string into a RustFS identity name. The id rule makes this safe
  (lowercase, digits, `-`), and S1 keeps the same check before it builds the name, so an unusual id gets the hash.

## S1's view

Either works for S1. B costs S1 about 10 lines, a one-time `weaver store rekey` where hashed-name keys exist (none known), and no change
to S3, whose default already agrees. A costs S3 a `blake3` dependency and unreadable names in RustFS output. With no S1 users in the
field, the strongest argument for A on the page falls away; the choice is readable names (B) against one generic rule for every principal (A).
Whichever is chosen, build the one-creator follow-on.

If B: merge `opt/adr126-s1-node-names`, no S3 change. Run `weaver store rekey` on any daemon that issued keys under the old naming.
