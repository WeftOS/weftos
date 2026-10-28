# weft-licence: the Seed licence proxy

Code: `crates/weft-licence` (service and CLI), `crates/weft-licence-wire` (the grant
wire format, shared with the kernel). Decision: ADR-106, section 2 and Phase 2.

`weft-licence` runs on the Cognitum Seed as its own binary, system user and
systemd unit. It is not part of `weft-cog-host` and shares nothing with it: not
the uid, not the bearer token, not the API. It holds the licence check, the grant
key and the checked-out binaries for the one WeftOS mesh the Seed is bound to.
The steward node (one admitted mesh node) is its only client.

## What it does

1. The steward asks for `cog X, version V, arch A`.
2. `weft-licence` checks the declared licence, fetches the cog from the Cognitum
   registry, checks the registry sha256 and size, computes BLAKE3 (the swarm
   content hash) and caches the bytes.
3. It writes the new `seq` to disk, fsynced, and only then releases the signed
   `CheckoutGrant`.
4. The steward pulls the bytes once, then pulls renewals every 12 h. Members
   fetch from peers, never from the Seed.

## Files

| Path | What |
|---|---|
| `/usr/local/bin/weft-licence` | the binary (installed signed-only, COG-008) |
| `/etc/weft-licence/config.toml` | configuration, see `crates/weft-licence/dist/config.example.toml` |
| `/etc/systemd/system/weft-licence.service` | the unit, from `crates/weft-licence/dist/weft-licence.service` |
| `/var/lib/weft-licence/` | state, mode 0700, owned by `weft-licence` |
| `.../grant.key` | the grant key, mode 0600 |
| `.../binding.json` | the operator-signed binding (one mesh only) |
| `.../slots.json` | checkouts with their `seq` and latest grants |
| `.../serves.json`, `nonces.json` | the serve ledger and replay nonces |
| `.../licence.json` | the operator-signed declared licence (you place it) |
| `.../overrides/` | operator-signed serve overrides |
| `.../cache/` | artifact cache named by BLAKE3, 256 MiB LRU |

## Install

On the Seed, as root.

```sh
useradd --system --no-create-home --shell /usr/sbin/nologin weft-licence
install -d -m 0700 -o weft-licence -g weft-licence /var/lib/weft-licence
install -d -m 0755 /etc/weft-licence
install -m 0755 weft-licence /usr/local/bin/weft-licence      # armv7 build, see "Build"
install -m 0644 config.toml /etc/weft-licence/config.toml     # edit device_id, listen, registry_url
install -m 0644 weft-licence.service /etc/systemd/system/weft-licence.service
```

Do not add `weft-licence` to the group of any cog uid, and do not run the cogs
as `weft-licence`. The key's protection is that no other user can read it.

After an unbind the key is deleted, so `ConditionPathExists` stops systemd from
starting the unit again until `init` runs for the next mesh; the running service goes
idle on its own. The unit's `RestrictAddressFamilies` includes `AF_UNIX` for name
resolution: step 4 of the on-device check says to see whether it is needed.

The unit assumes systemd. The Seed init system was not checked from this
repository; if it differs, run the same command line under it with the same user
and a 0700 state directory.

## `init` over USB

Run over the USB link, with someone at the device, **as the service user**. The key is never created or
learned over the network, so it cannot be swapped in transit.

```sh
sudo -u weft-licence weft-licence --state-dir /var/lib/weft-licence init --operator-key <operator pubkey hex>
```

Every command that writes state (`init`, `bind`, `override`) refuses to run unless
its uid owns the state directory: a file written by root could not be read by the
service. `sudo -u weft-licence weft-licence ...` is the form to use. As root, `init`
also refuses to create a missing state directory (it would be root-owned); create it
with the `install -d` line above.

It prints:

```
grant key written: /var/lib/weft-licence/grant.key
grant_pubkey:      <64 hex>
fingerprint:       ed25519:<16 hex>
```

`init` never overwrites an existing key: a second run fails with `a grant key
already exists`. `--operator-key` pins an operator public key in the state
directory (it can also be listed in `operator_pubkeys`).

## Confirm the fingerprint, then bind

1. On the operator machine, compare the printed `fingerprint` with the one
   `weaver` shows for this Seed. If they differ, stop and delete the key.
2. The operator signs the binding record (v2, domain `weft-licence-v1/binding`)
   with the pinned operator key. Its `grant_pubkey` is the printed one.
3. Apply it over USB:

   ```sh
   sudo -u weft-licence weft-licence --config /etc/weft-licence/config.toml bind binding.json
   ```

   A running service picks up the new binding (and an unbind) at its next request;
   no restart is needed.

The binding is checked against the pinned operator key, the Seed's `device_id`
and its own grant key, and its `seq` must exceed the stored one. A `bound`
record for another mesh while bound is refused with `seed_bound_elsewhere`.
There is no bind endpoint on the network. After an operator-signed `unbound`
record, the grant key is deleted and `init` runs again before the next bind. A running
service drops the key from memory and marks every checkout released at its next
request. Checkouts are tied to the mesh they were made for, so nothing carries over to
a later binding for another mesh (the next checkout is fetched again, and `seq` keeps
rising).

## Declared licence

`licence_file` normally lives in the state directory. If it is configured elsewhere
(`/etc/weft-licence/licence.json`, say), it must be readable by the `weft-licence`
user: `chown weft-licence: /etc/weft-licence/licence.json && chmod 0640 ...`. The file
is public-key signed, not secret, but an unreadable one answers `licence_unreadable`.

Until Cognitum provides a signed entitlement (phase 4), the licence is an
operator-signed record placed at `licence_file`. Domain
`weft-licence-v1/licence`, the signed envelope `{payload, public_key, signature}`
over this payload:

```json
{"v":1,"mesh_id":"<hex>","source":"cognitum","account_ref":"<label>",
 "cogs":["fall-detect"],"expires":1800000000,"issued_at":1791000000}
```

`cogs` is a list of cog ids or `["*"]`. `expires` is optional. Only the sha256 of
`account_ref` leaves the Seed. The file is re-read and re-verified on every
check. There is no `weaver` command to sign it yet; the signer is
`weft_licence::providers::sign_licence`.

## Run

```sh
systemctl daemon-reload && systemctl enable --now weft-licence
journalctl -u weft-licence -f
```

The unit refuses to start without `grant.key`. Log lines carry ids and counts
only: `checkout granted`, `byte transfer`, `renew`, `refused <code>`.

## Listener and tailnet ACL

The listener binds only the addresses in `listen`: the USB link-local interface
and the tailnet interface. `0.0.0.0` and `::` are refused at startup and in
`http::serve`. Only these ranges are accepted: loopback, `169.254.0.0/16`,
`fe80::/10`, `100.64.0.0/10` and `fd7a:115c:a1e0::/48`. Anything else (a LAN address)
needs `allow_lan_listen = true`. A `Host` header, when sent, must be one of the listen
addresses as `ip:port`, so address the service by IP. Each request has 10 s in total
for its headers and body, and one source address holds at most 4 connections. It serves plain HTTP; confidentiality comes from the link
(WireGuard on the tailnet, or the cable). Integrity does not depend on the
link: requests and grants are signed and bytes are checked against signed hashes.

Recommended tailnet ACL: allow the `weft-licence` port only from the steward host.

```json
{"action": "accept", "src": ["tag:weft-steward"], "dst": ["tag:seed:8700"]}
```

A LAN-only plain link needs the explicit per-Seed lab opt-in until TLS with a
pinned SPKI is added (ADR-106 section 7, decision W4).

## The steward side

The licence verbs (the `weaver cog checkout` verbs, and `weaver workload node bind`, `unbind` and `reset-floor`) run on the machine's licence holder only: in service mode the cluster owner's daemon, else the collapsed daemon. Run them there (for example `sudo -u <owner> weaver cog checkout status`); another user's daemon refuses with `licence_not_here` and names the holder.

The steward is the mesh node the binding names (`steward_node_id` and
`steward_pubkey`, its node key). To relay checkouts it needs the link to
`weft-licence`, in `licence-link.json` in its runtime dir (same owner and mode
rules as the other placement policy files):

```json
{"url": "http://100.64.0.10:8700", "allow_unpinned_lab_link": true}
```

| Field | What |
|---|---|
| `url` | `http(s)://<ip>[:port]`. Use the Seed's tailnet or USB address, by IP (the `Host` header is checked) |
| `tls_spki_sha256` or `tls_sha256` | pin an `https://` link (`spki-sha256:<64 hex>` / `sha256:<64 hex>`) |
| `allow_unpinned_lab_link` | the explicit opt-in for a plain `http://` link |
| `max_artifact_bytes` | largest artifact accepted (default 64 MiB) |
| `timeout_secs` | call timeout (default 30 s; artifacts get at least 120 s) |

A link that is neither pinned nor opted in is refused and no relay is built.
`weft-licence` serves plain HTTP until TLS is added (W4), so today every link
needs `allow_unpinned_lab_link`; keep it on the tailnet or the USB cable, which
give the confidentiality. Integrity does not depend on the link.

The daemon builds the relay at placement start, with the node's governance
gate (it asks `cog.checkout`) and the licence exchange as its grant flood.
Every request is signed for the binding in effect, so a bind or a steward
change needs no restart; while the binding does not name this node the relay
sends nothing and answers `not_steward`. A node without the file answers
`no_steward`. Responses are capped (256 KiB, or the artifact cap) and timed
out, redirects and proxies are not used, and any failure is
`licence_unreachable` for the member that asked.

Check it with `weaver cog checkout status` (the steward, whether it is
reachable, the relay) and `weaver doctor` (`licence.no_steward`,
`licence.approval_missing`, `licence.grant_expiring`,
`licence.approvals_orphaned`). The operator flow (checkout, approve, the run
gate) is in [cog-sources.md in weftos-cogs](https://github.com/WeftOS/weftos-cogs/blob/v0.1.4/docs/cogs/cog-sources.md), "In a mesh with a bound Seed".

The steward also pulls renewals: `POST /licence/v1/renew` every 12 h plus up
to 30 min of jitter (the first pass 5 min after the daemon starts), then pages
`GET /licence/v1/grants?since=<ctr>` to catch up on anything issued meanwhile.
New grants are installed and flooded to the mesh; withdrawals first, as soon
as a response holds one. Responses are bounded by the same caps as above, so a
renew answer over 256 KiB (roughly 200 active checkouts) fails the pass. A
pass the Seed does not answer backs off from 1 min, doubling, to 1 h. Each
installed renewal is chained as `cog.checkout.renewed`, each withdrawal as
`cog.checkout.lapsed`. Every active checkout is renewed: releasing unused
ones automatically is open question W3.

The catch-up cursor never moves past the counter the Seed reported in the
renew answer plus the grants a page carries; it restarts at 0 after a rebind
or for another Seed. Until a binding names this node the pull checks again
every 5 minutes. If the Seed's `weft-licence` state is wiped while its
`device_id` stays the same, its counter starts over and catch-up stalls until
the mesh is rebound or the steward restarts; `POST /renew` still delivers
grants on every pass.

On a member, the run gate stays on once the node has accepted a binding: a
`licence-bound.marker` file beside the licence store survives a deleted store
file. Deleting the whole `licence/` directory resets that, and the node then
reads as never bound until a peer re-syncs the binding. That is a known limit:
anyone who can delete that directory can already tamper with the daemon.

The operator can do both by hand, on the steward: `weaver cog checkout renew
<cog>@<version>` makes a renewal pass now, and `weaver cog checkout release
<cog>@<version>` sends `POST /licence/v1/renew` with `{"release":
[{"cog_id": ..., "version": ...}]}`, so the Seed signs a withdrawal for that
checkout (and renews the others, as every renewal does). Both install and
flood what comes back, and are chained as `cog.checkout.release` /
`cog.checkout.renew`. `weaver cog checkout list` shows the held grants, their
expiry and which artifacts are approved.

## Protocol

Every endpoint except identity needs a steward signature. The request headers are
`x-licence-node`, `x-licence-ts` (unix milliseconds), `x-licence-nonce` (16 to 64
alphanumerics) and `x-licence-sig`. The signed string, one field per line (the
`<seed_device_id>` line is the audience: a request signed for one Seed is refused by
another):

```
weft-licence-v1/request
<METHOD>
<path and query>
<node>
<seed_device_id>
<ts, unix ms>
<nonce>
<sha256 of the body, hex>
```

The layout is the COG-011 bridge's (`auth.rs` in the bridge cog) under its own domain.
The signature is Ed25519 (`verify_strict`) under the bound `steward_pubkey`. The
timestamp must be within 120 s of the Seed clock, and a nonce is refused a second
time, including across a restart.

| Endpoint | Auth | What |
|---|---|---|
| `GET /licence/v1/identity` | none | service, device id, grant key id and pubkey, bound mesh, clock state |
| `POST /licence/v1/checkout` | steward | `{request_id, cog_id, version\|"latest", arch}`, answers `{grant, artifacts}` |
| `GET /licence/v1/artifact/<blake3>` | steward | the bytes, 4 MiB/s, counted against the serve limit |
| `POST /licence/v1/renew` | steward | renews every active checkout, optional `{"release":[{cog_id,version}]}` |
| `GET /licence/v1/grants?since=<ctr>` | steward | the latest grant per checkout, paged at 256 |

A withdrawal is a renewal whose `expires_at <= issued_at`. It is issued for a
released checkout and for one the licence no longer covers. Seed keys never sign
revocation notices.

### Limits (configurable)

| Limit | Default |
|---|---|
| Checkouts or transfers in flight | 1 |
| Steward requests per minute | 10, charged after the signature verifies |
| Unsigned and refused requests per minute | 30 in total, a separate pool |
| Largest artifact | 64 MiB |
| Transfer rate | 4 MiB/s |
| Byte transfers per artifact per steward key per 24 h | 3, raised only by an operator-signed override |
| Cache | 256 MiB LRU, active checkouts are never evicted |
| Grant lifetime | 72 h, at most 7 days, never past the licence expiry |

Forged traffic costs only the unsigned pool, so it cannot use up the steward's
budget.

### Clock

The Seed has no RTC. Below the build-time floor (`CLOCK_FLOOR`, 2026-05-28) it answers
`clock_not_set` and neither verifies requests nor signs; the same applies below the
highest `issued_at` it ever signed. The floor equals the bridge cog's
`CLOCK_FLOOR_MS` (1,780,000,000,000 ms); keep them equal.

### Error codes

`cog_unlicensed`, `licence_expired`, `licence_unreadable`, `cog_not_found`,
`version_unavailable`, `arch_unavailable`, `size_unknown`, `artifact_too_large`,
`artifact_changed`, `verify_failed`, `fetch_failed`, `busy`, `rate_limited`,
`rate_limited_unsigned`, `serve_limit`, `no_grant`, `gone`, `cache_full`,
`clock_not_set`, `seed_not_bound`, `bad_signature`, `stale_request`, `replayed`,
`wrong_node`, `malformed_auth`, `persist_failed`, `grant_invalid`, `duplicate_artifact`, `no_key`,
`bad_request`, `not_found`, `binding_changed`, `cache_io`, `sign_failed`.

### Serve override

After a steward rebuild the 3-per-day limit can be raised for one artifact and
one steward key. The operator signs a `ServeOverride`
(`weft-licence-v1/serve-override`, fields `v, key_id, cog_id, version, arch,
extra, expires_at`) and installs it over USB:

```sh
weft-licence --config /etc/weft-licence/config.toml override override.json
```

## Build

```sh
scripts/build.sh licence-cross              # armv7 and aarch64, in the cogs cross image
scripts/build.sh licence-cross armv7        # one target
scripts/build.sh licence-uid-check          # Linux: another user cannot read the key
```

`licence-cross` uses the cogs cross image `weavelogic-cogs-cross:1.97.1` (built by
`scripts/cross-build.sh` in [WeftOS/weftos-cogs](https://github.com/WeftOS/weftos-cogs/blob/v0.1.4/scripts/cross-build.sh)), builds offline from the host
cargo cache (mounted read-only after an offline `cargo fetch` check), and writes `target/licence-cross/<triple>/release/weft-licence`.
It skips with a message when docker or the image is missing. The build enables
`--features net`, which adds the https registry reader (rustls).

Recorded on 2026-10-02 (release profile, already stripped):

| Target | Size |
|---|---|
| `armv7-unknown-linux-gnueabihf` | 2,874,508 bytes |
| `aarch64-unknown-linux-gnu` | 3,085,056 bytes |

`weftos-cog-sources` builds for `armv7-unknown-linux-gnueabihf` as part of this.

## Running cogs on the Seed

`weft-licence` holds the licence; `weft-cog-host` enforces it at start (ADR-106, "Phase 3 `weft-cog-host` start check"). The host reads its own licence directory, `<root>/.licence` by default (`$WEFT_COG_HOST_LICENCE_DIR` or `serve --licence-dir` override), so on a Seed that is `~/.weftos/cogs/.licence`. Without the directory the host behaves as before. Once it exists, every `source: cognitum` cog is refused (`binding_inactive`) until the binding is imported, and then needs a grant and an operator hash approval to start, including cogs installed earlier, so create the directory when the records are ready. The grant key the binding names must not be one of the operator keys in `trust.json`.

Set it up, as the user the host runs as:

```sh
L=~/.weftos/cogs/.licence
mkdir -p "$L" && chmod 700 "$L"
printf '{"mesh_id": "%s"}\n' "<64-hex mesh id the Seed is bound to>" > "$L/config.json"
cat > "$L/trust.json" <<'JSON'
{"schema": "weftos.workload-trust.v1",
 "operator_keys": [{"key_id": "acme-operator", "public_key": "<64-hex Ed25519 public key>"}]}
JSON
weft-cog-host licence status          # state "ready", binding not yet in effect
```

`trust.json` pins the operator keys that verify the binding, approvals and revocation notices; put in it the public key you bound with. Then import signed records, which the host verifies itself (grants must be signed by the grant key the binding names):

```sh
weft-cog-host licence import records.json     # or: POST /licence/records on :9480 with the host bearer token
weft-cog-host licence status
```

`records.json` is `{"binding": <SignedBinding>, "grants": [..], "approvals": [..], "revocations": [..]}`; every field is optional. Each record is judged on its own and the answer lists, in order, `applied`, `duplicate`, `ignored`, `applied_unsaved` or `refused` with the reason. The binding is the operator-signed v2 binding, the grants come from `weft-licence`, the approvals are the operator's hash approvals, and an `artifact_hash` revocation notice stops a running cog. No `weaver` verb writes this file today; the signed records are the ones the licence holder already has.

The check gates the host's supervised start path. It does not protect against a process with the host's own uid, which can edit this directory; for that, keep the directory root-owned and read-only to the host user and import as root (ADR-106, "Limits"). `GET /licence` and `POST /licence/records` both need the host bearer token.

A refused start shows in `GET /cogs` as `licence_refusal` with the stable code, and in the host log with the remedy. The state is re-read when its files change, so no restart is needed after an import.

## Pending Cognitum (phase 4)

| Stub | Where | Question |
|---|---|---|
| Declared licence instead of a Cognitum entitlement | `providers::LicenceProvider`, `LocalDeclaredLicence` | C1, C3 |
| Registry fetch of a public registry, no token | `providers::CogFetcher`, `registry::RegistryFetcher` | C2 |
| Device-key signing of the grant key | `providers::DeviceSigner`, `StubDeviceSigner` (returns none) | C4 |
| Binding does not check `device_pubkey` against `/api/v1/identity` | `bind::apply` | C4 |
| Only the armhf (`arm`) binary exists in the registry | `RegistryFetcher::fetch` | C7 |

C7 matters for acceptance: the Cognitum `app-registry.json` lists one `arm`
binary per cog, so a checkout for `aarch64` answers `arch_unavailable` until the
registry carries it. The grant and the arch union already handle any arch.

## Owner-run check on the real Seed

Not run from the repository. Steps, in order:

1. Build both targets with `scripts/build.sh licence-cross` and copy the armv7
   binary, `weft-licence.service` and `config.toml` to the Seed.
2. Install as above. Set `device_id`, `listen` (USB and tailnet addresses only)
   and `registry_url` in the config.
3. Run `init --operator-key <hex>` over USB. Compare the fingerprint with
   `weaver`. Sign and apply the binding. Place the signed `licence.json`.
4. `systemctl enable --now weft-licence`. If the unit fails to resolve the registry
   host, check whether `AF_UNIX` in `RestrictAddressFamilies` is what it needs, and
   drop it if the resolution works without. Then, from the steward host:
   `curl http://<seed tailnet address>:8700/licence/v1/identity` and confirm
   `"bound": true`, `"clock_ok": true` and the same `grant_key_id`.
5. Check the isolation on the Seed itself: as the user a cog runs under,
   `cat /var/lib/weft-licence/grant.key` and `ls /var/lib/weft-licence` must both
   fail with permission denied.
6. Check out `fall-detect` (the `arm` binary, since the registry has no aarch64
   until C7). `journalctl -u weft-licence` must show one `checkout granted` and
   one `byte transfer`.
7. With `licence-link.json` on the steward (above): `weaver cog checkout
   fall-detect@<version> --arch arm` on a second node, then `weaver cog
   checkout approve ... --confirm <content-key>`; the second node runs it from peers, and the
   journal still shows exactly one `byte transfer`.
8. Switch the Seed off for longer than the grant lifetime (72 h by default) and
   confirm sharing stops when the grants lapse.
9. Check the syscall filter: after step 4, `systemctl status weft-licence` must show
   `active (running)` and `journalctl -u weft-licence` must not show `status=31/SYS`
   (SIGSYS). The unit has `SystemCallFilter=~@privileged @resources`, which blocks
   `prlimit`/`setrlimit`; if the Rust runtime or the resolver calls one on this
   firmware, remove `@resources` from that line and note which call it was.
10. Check the clock floor: boot the Seed with no network time and confirm
   `"clock_ok": false` and `clock_not_set` on a signed request.

Stop there and report. The Pi and the Seed hardware are owner-run.
