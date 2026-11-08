# ADR-126: which name does a node's RustFS user get? (S3 and S1 disagree), 2026-11-08

Owner call. S3 and S1 have not been changed to agree; neither option is merged. Both sides are prepared as branches so the answer is one step. S1 adds its side's notes at the end of this page.

## What the live test showed

`s1_s3_node_bucket_names_agree_but_rustfs_user_names_do_not` (in `crates/weftos-release/tests/adr126_integ_live.rs`, passing on `feat/adr126-s5`, run 2026-11-08 against a throwaway loopback RustFS) pins the gap:

| | Bucket | RustFS user for node `n1` |
|---|---|---|
| S3 (`weftos-edge-creds`, `policy::principal_for`) | `n-n1` (agrees) | `node-n1` |
| S1 (`weftos-store`, `RustfsBackend::key_id_for("node:n1")`) | `n-n1` (agrees) | `wk-` and the first 16 hex of blake3 of `node:n1` |

What it breaks today: S3's revoke path disables `node-n1` and assumes roles for `node-n1`; S1 creates, scopes and enables/disables `wk-...`. S3 would disable a user S1 never made, and the user S1 did make would stay enabled. The bucket rule is not affected.

It is not only a naming difference. Both lanes also create the user, a policy and a secret:

| | S1 (`StoreService::apply_principal`) | S3 (`RustfsMinter::provision_node`) |
|---|---|---|
| User | `create_access_key`, key id kept in state (`keys` map), re-used on restart | `add-user` with a secret from the parent's `NodeKeys` |
| Policy | `p-<key id>`, built from the principal's grants; replaced on every grant change | named like the user; write on the node bucket |
| Enabled | live only while a grant is enabled and the principal is not suspended | enabled by provisioning, disabled by revoke |
| Secret | file under S1's secrets dir, mode 0600 | whatever `NodeKeys` returns |

Under either option below, exactly one side must create the user and attach the policy, or each overwrites the other's policy. Recommendation for that (needed in both options): S1 creates and scopes the user; S3 only assumes roles and disables/enables it, reading the secret through `NodeKeys` from S1's secrets dir; S3's `provision_node` reduces to creating the bucket. That follow-on is not built.

## Option A: S3 adopts `wk-<hash>`

Prepared: branch `opt/adr126-s3-wk-names` (code in `601cc5e9`, on top of `feat/adr126-s5`; the tip also carries this page). Adds `policy::UserNaming { NodePrefix (default), WkHash }`; `BrokerConfig.user_naming` and `RustfsConfig.user_naming` select it; `keygen::node_key_named`. The default is unchanged, so merging the branch alone changes nothing; a host chooses `WkHash` to adopt. `blake3` (already a workspace dependency) is added to `weftos-edge-creds`; the diff gate did not flag it. Tests: 5 unit tests (default unchanged; format and stability; revoke disables the named user and no other, both ways; lease for the named user; generated keys carry the name) and the S1 x S3 parity test, which asserts equality with S1's `key_id_for` for five node ids. Planted defects (wrong hash label, broker ignoring the naming on disable) each fail a test. 99 unit tests pass, clippy and the wasm check are clean.

What it breaks or costs:
- The name is no longer readable in RustFS admin output and logs (`wk-3f9a...` instead of `node-n1`); S3 audit must record the mapping.
- S3's docs and tests that say `node-<id>` need updating when a host adopts it.
- A node user S3 already provisioned under `node-<id>` would be orphaned. S3 has provisioned only throwaway users (live use is gate C6, not yet run), so there is nothing to migrate.
- The host's `NodeKeys::key_for` must be keyed by the `wk-` name (S1's secret file is already keyed that way).

Good: S1's persisted state, key ids and secrets stay as they are, so no migration on the S1 side; one rule for every principal; 19 characters, fixed length.

## Option B: S1 adopts `node-<id>` for node principals

Prepared by S1: branch `opt/adr126-s1-node-names` (S1's lane; notes below when it lands).

What it breaks or costs:
- S1's `key_id_for` takes any principal label; node principals would need a special case, and `node-<id>` can be up to 53 characters. RustFS 1.0.4 accepts access-key ids up to at least 128 characters (checked 2026-11-08 on a throwaway instance at lengths 3, 19, 20, 21, 32, 53, 64 and 128), so length is not a blocker.
- Users S1 already created under `wk-...` stay under that name (the state keeps the key id), so the field would hold two rules until they are re-created. RustFS cannot rename a user: it is delete, create, new secret, so live sessions drop.
- A node id is chosen by the paired node's owner, not by S1; `node-<id>` puts that text into a RustFS identity name where `wk-<hash>` cannot be influenced. The id is validated (lowercase, digits, hyphen, 48 characters), so this is a tidiness point, not a hole.

Good: readable names; S3's code, docs and tests stay as they are.

## Recommended default

Option A. S1 owns principals, grants and policy and already has a stable, tested, generic rule that every other principal uses; S1 has users and secrets in the field and S3 has none; A needs no migration and no special case. Take A together with the follow-on above (one creator of the user). Choose B only if readable names in RustFS output matter more than avoiding two live rules while users are re-created.

One-line switch if A: a host builds `BrokerConfig`/`RustfsConfig` with `user_naming = UserNaming::WkHash` and its `NodeKeys` keyed by the `wk-` name. If B: S1 merges `opt/adr126-s1-node-names` and the S3 default (`NodePrefix`) already agrees.

## Rejected: a daemon dependency on `weftos-edge-creds` (owner, 2026-11-08 11:39)

The idea was an adapter in the daemon implementing S3's `NodeKeys` over S1's `StoreService::node_key`, so the daemon would link `weftos-edge-creds`. The owner rejected it: S3 and S1 stay separate, and S3's node keys are not wired to S1 through that crate. No adapter code was committed; the option branch was not kept. What it would have cost, for the record: a new `[dependencies]` entry in `clawft-weave` (a diff-gate `new-dep` finding needing an owner approval entry); the daemon linking a second crate with its own types for keys and sessions; and a lookup that is linear in the mesh's node count whenever the principal is S1's `wk-` form, because the hash cannot be reversed to a node id. Whatever connects S3's `NodeKeys` to S1's node key later has to be chosen without that dependency; nothing is planned.

## S1's notes

(S1 appends here.)
