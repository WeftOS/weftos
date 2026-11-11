# Move the coordinator role to photo-gallery

- **Status:** Executed 2026-10-05 (steps 1–7; D6 trust pinning still open).
  - v0.8.2 published and signed (run 37254990484, 79 assets, `weftos-release.json.sig` verifies under `8ac2a301…`).
  - PG upgraded by Terraform (`pg_weftos_release`, saved plan `7f740e4e309f…`). One-time key bridge: PG's interim build pinned the old COG-008 key, so the step checks that PG's downloaded `weftos-release.json` has the SHA-256 verified on the Mac (`8e989140…`) before `weaver update --insecure-skip-signature --force`. PG now runs `weaver 0.8.2`, which pins the release key.
  - Gateway on PG (`weftos-gateway.service`) listens on `100.90.170.87:18789` only; `/api/health` ok over the tailnet; the LAN address does not answer. Read-only console token in `~/.weftos/pg-console-readonly.token` on the Mac (0600); `/api/fleet/snapshot` returns 200 with it.
  - Coordinator runtime archived to `/data1/weavelogic-internal/_archive/weave-coordinator-20261005/runtime/` after the stop; checksum dry run: 11 files, 0 differing.
  - Mac coordinator and its gateway stopped (`kernel stop`); nothing listens on `:9489`/`:18789` on the Mac. The coordinator directory is untouched.
  - Mac binaries: official v0.8.2 aarch64 archives, SHA-256 checked against the signed manifest, installed in `~/.cargo/bin` (`.prev` kept). Mac user daemon runs with node `c12684d4…` (the former coordinator key), profile `user`, seed PG; PG's fleet shows the Mac active (`discovered`, `legacy unverified`).
- **Direction:** the owner made photo-gallery (PG) the primary node and main store; the
  MacBook stays the owner's main computer and joins the mesh as a member (2026-10-04).
- **Depends on:** v0.8.2 published and signed (run 37254990484); ADR-103 (topology),
  `docs/guides/kernel.md` (owner migration), `docs/guides/weftos-deployment-sops.md`
  (machine mesh service), `docs/plans/dashboard-fleet-terraform-integration.md` (PG
  accounts and the saved-plan rule), `~/weavelogic.terraform` (all PG host changes).

## What "the coordinator" is today

`<client workspace>/cognitum/weave-coordinator` on the Mac is a collapsed, project-rooted daemon
(`bin/weaver kernel start --foreground`, weaver 0.8.1). It is not a git checkout and has no
project content: 70 MB, mostly `bin/` and `kernel.log`. Its state is `.weftos/runtime/`
(6 MB): `chain.rvf`/`chain.key`/`chain.tree.json`, `node.key` (node id `c12684d4…`),
`cluster_peers.json`. It does three jobs:

1. Mesh listener on `0.0.0.0:9489` (Noise), now seeding PG `100.90.170.87:9489`.
2. Fleet manager state (`fleet.snapshot`, peer pings).
3. Backend for `weft gateway` (`127.0.0.1:18789`, config `.weftos/gateway.json`, CORS for
   the console at `127.0.0.1:18998`, read-only console token in
   `.weftos/console-readonly.token`).

No workloads, apps, licence state or fleet location labels live in its runtime.

PG already runs the WeftOS user daemon as `weftos` (UID 2101, node `0569a790…`, mesh on
`100.90.170.87:9489`) with project `01M43SGRRXEJJ1SAZNXVD0TPAP` as a supervised child.

## Decisions (recommended; owner confirms before step 1)

| # | Decision | Recommendation | Why |
|---|---|---|---|
| D1 | Which node identity is the hub? | PG keeps its own `0569a790…`. | A node id names a machine. The hub is now a different machine. |
| D2 | What happens to `c12684d4…`? | It stays the **Mac's** identity: copy the coordinator's `node.key` to `~/.weftos/run/node.key` before the Mac user daemon first starts (owner chose to keep it on 2026-10-04). | Nothing off the Mac pins it today (checked cog0, PG and repo on 2026-10-05). Keeping it means the Mac remains the same node to PG. |
| D3 | The coordinator's chain history | Archive it, do not merge it. Verified copy to PG `/data1/weavelogic-internal/_archive/weave-coordinator-20261005/`; the Mac copy stays untouched. | It is a project chain with no project. The PG hub keeps its own chain; merging two chains would fork history. |
| D4 | Where the gateway runs | On PG as `weftos`, a systemd user unit next to `weftos.service`, created by a Terraform resource (saved plan, owner approval). | Same account as the hub daemon it fronts; no new UID. |
| D5 | How the Mac console reaches it | **Decided:** bind the gateway to PG's tailnet address `100.90.170.87:18789` with `--dangerously-plain-http`, bearer token required, CORS for the Mac console origin. | The gateway refuses a non-loopback bind without TLS unless that flag is set. The owner relies on the tailnet as the network boundary (WireGuard-encrypted, members only) and on WeftOS keyed mesh membership; requiring TLS per device was rejected. Never the LAN address. |
| D6 | Verified trust Mac ↔ PG | Separate, later step: machine mesh service on both (`--adopt-node-key` on each), then `weaver mesh trust` with fingerprints compared out of band. | Collapsed daemons only reach `discovered`. Not needed for the move itself. |

## Steps

Each step lists its check. Stop at the first failed check and use the rollback.

1. **Release in hand.** v0.8.2 published, signed, and `weftos-release.json` verifies.
   Check: `gh release view v0.8.2` lists the assets and the signature file.
2. **Upgrade PG to v0.8.2 (Terraform).** `terraform_data.pg_weftos_release` runs
   `weaver update --no-restart` as `weftos` (verifies the release signature against the
   pinned key), checks `weaver --version` is 0.8.2, restarts `weftos.service`. Applied in
   the same saved plan as step 4.
   Check: `weaver --version` is 0.8.2; `weftos.service` active; the project child restarts
   on demand; `ss -ltn` still shows only `100.90.170.87:9489`.
3. **Archive the coordinator state to PG.** From the Mac, as `weavelogic` on PG:
   `rsync -a --no-owner --no-group --no-perms <client workspace>/cognitum/weave-coordinator/.weftos/runtime/
   …/_archive/weave-coordinator-20261005/runtime/` excluding `kernel.sock`, then a
   `--checksum --dry-run` pass. Keys are copied as files into a `weavelogic`-owned
   `0700` archive and never printed.
   Check: checksum pass reports 0 files to transfer; record it in
   `docs/research/pg-source-intake.md`.
4. **Gateway on PG (Terraform).** `terraform_data.pg_weftos_gateway` in
   `environments/main.tf` writes `~/.weftos/gateway.json` (host per D5, port 18789, CORS
   for the Mac console origin) and a `weftos-gateway.service` user unit running
   `weft gateway -c ~/.weftos/gateway.json --runtime ~/.weftos/run --dangerously-plain-http`,
   then enables and starts it.
   `terraform plan -out=tfplan`, show it, **owner approves the exact plan**, apply.
   Check: unit active; `curl http://100.90.170.87:18789/health` from the Mac answers;
   nothing listens on `192.168.1.20`.
5. **Console token.** On PG as `weftos`: `weft token issue --read-only`; store it on the
   Mac as `~/.weftos/pg-console-readonly.token` (0600). Point the console at
   `http://100.90.170.87:18789`.
   Check: the console's Network tab lists PG, its project child and the Mac.
6. **Stop the Mac coordinator and its gateway.** `bin/weaver kernel stop` from
   `<client workspace>/cognitum/weave-coordinator` (not `kernel restart`: card `ba258d78`), stop the
   `weft gateway` process.
   Check: no listener on `:9489` or `:18789` on the Mac; `kernel.pid` gone.
7. **Start the Mac as a member.**
   - `cp` the coordinator's `node.key` to `~/.weftos/run/node.key` (0600) per D2.
   - Rewrite `~/.weftos/weave.toml`: `seed_peers = ["100.90.170.87:9489"]`,
     `listen_addr = "127.0.0.1:9489"` (the Mac does not need to accept inbound peers),
     `noise = true`.
   - `weaver kernel start --profile user` (v0.8.2), then the launchd unit
     (`weaver service unit --kind launchd`, `launchctl bootstrap`).
   Check: `weaver kernel status --profile user` shows node `c12684d4…`, roles
   `machine, user`, runtime `~/.weftos/run`; PG's `weaver fleet status` shows the Mac
   active.
8. **Record and close.** Update this plan's status, the handoff migration table, the PG
   plan doc and the board. The Mac coordinator directory stays as is (no deletes); remove
   it only on a later explicit decision.

## Rollback

- Before step 6: nothing on the Mac changed. Stop `weftos-gateway.service` on PG (or
  revert the Terraform resource with a reviewed plan).
- After step 6: `weaver kernel stop --profile user` on the Mac, remove
  `~/.weftos/run/node.key` only if it was copied in step 7, restart the coordinator with
  `nohup bin/weaver kernel start --foreground` in its directory and restart its gateway
  with `bin/weft gateway -c .weftos/gateway.json --runtime …/.weftos/runtime`. Its runtime
  was never modified.

## Out of scope

- Machine mesh service and verified admission on either machine (D6).
- Client projects under `<client workspace>/` (each needs its own PG home and UID first).
- Cog Host on PG (held: explicit bind landed in `6eeb99ed8`, unit template in
  `deploy/photo-gallery/`, not deployed).
- Retiring or deleting anything on the Mac.
