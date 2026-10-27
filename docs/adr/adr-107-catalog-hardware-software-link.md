# ADR-107: The hardware catalog links modules to cogs

- **Status**: Accepted (2026-10-03, owner feedback: "I cannot see how to install this sensor, or if we have a cog installed, or if one is available")
- **Successor**: [COG-107](https://github.com/WeftOS/weftos-cogs/blob/v0.1.4/docs/decisions/COG-107-catalog-link.md) in WeftOS/weftos-cogs (Cogs extraction, 2026-10-27). Cogs owns catalog joins, Sensor Explorer, the Manager panels and the firmware and docs metadata. The Cogs product crates it names now live in that repository without the `weftos-` prefix. This record stays as written.
- **Date**: 2026-10-03
- **Deciders**: Owner / platform
- **Depends-On**: ADR-104 (sensor guides ship with sensor cogs), ADR-105 (cog sources), COG-009 (appliance console)
- **First implementation**: `crates/weftos-cog-market` (`hw.rs`: `Module::cogs`, `firmware`, `docs`, `seen_in`); `crates/weftos-cog-manager` (`sensor_link.rs`, `sensor_detail.rs`)

## Context

The appliance console had three tabs that never met. The Catalog tab described a module (the HLK-LD2450 card showed a summary, price and spec rows), the Cogs tab managed cogs, and the Sensors tab rendered a running cog's guide. Nothing said that the `ld2450-radar` cog drives the LD2450, whether it was available, installed or running, how to wire the sensor, or where its firmware and docs were.

## Decision

1. **One source of truth, on the hardware side.** Each catalog `Module` carries `cogs: [<cog id>]`, the ids of the cogs that drive it (the cog's own `[cog].id`). A supporting board lists every cog that needs it (`ads1115-board` lists `sen0213-ecg` and `sound-detect`). The reverse lookup, `HwCatalog::modules_for_cog`, is computed, never stored, so the two directions cannot drift. An empty list means no cog yet.
2. **Optional module facts live beside it.**
   - `firmware`: `version`, `read_with`, `read_config_key`, `update`, `url`, `notes`. Empty strings mean unknown, never a guess. `read_with` says how a cog reads the version off the module (ld2450: `query_firmware`); the update field only points at the vendor route. WeftOS does not update module firmware.
   - `docs`: `[{label, url}]`, where `url` is an `http(s)` link or a repo-relative / `repo: path` reference shown as text.
   - `seen_in`: the source documents the entry was drawn from (already in the JSON, now typed).
3. **Availability and state are never stored in the catalog.** They come from the marketplace catalog the Cogs tab already loads (WeaveLogic signed, Cognitum) and the host's `/status` (installed, running, stopped, refused by the licence gate). The join is `sensor_link::cog_view`.
4. **The module card is the detail panel.** Expanding it shows Software, Setup, Firmware, Docs and, for running cogs, Stats.
   - Software: the cog, the sources offering it, install state, and Install / Start / Stop / Configure / Open guide.
   - Setup: catalog pins, bus enablement steps, power, and the cog's own guide wiring when it is being served.
   - Firmware: the facts above and whether the cog can read the version.
   - Docs: datasheet, the Sensors-tab guide (deep link), curated links, buy link, catalog source.
   - Stats: host supervision facts plus the cog's own `/status` output line on its export port.
   The actions call the same client methods the Cogs tab uses (`install`, `lifecycle`). The host has no config-write endpoint, so Configure opens the cog's guide at its config-keys page rather than writing anything.
5. **Cross-links both ways.** The Sensors tab and the Cogs tab (running rows and marketplace rows) link to the hardware a cog needs, and open that module's card in the Catalog. The Catalog list badges each module `cog available`, `installed` or `running`.
6. **Works with no host and in WASM.** With no host the panel shows availability only and Install is disabled with a reason. All logic is in egui-independent functions; the cog's export `/status` is fetched with `ehttp` like the guide.
7. **Mesh-wide, not one device.** The panel and badges show where a cog is on the mesh: per node, the version, state, restarts, uptime, last output and output-log size. The console never polls peers. It asks the connected host for `GET /mesh/cogs`, and the host reads its own supervisor and asks each online tailnet peer's `/status` (literal tailnet IPs only, bounded, cached 5 s; a peer that is not a cog host is listed as unreachable with the reason). With no answer, or a host that predates the route, the view is the connected host alone and says `this host only (<why>)`. The daemon's placement layer (`workload.list`) is not consulted yet: `weftos-cog-host` has no daemon link, and the daemon RPC is not browser-reachable. **Queue depth does not exist**: cogs post straight to the store. The only backlog a host holds is each cog's output log, so `/status` gains `log_bytes` and `log_age_s` (from `host.log`) and the panel labels them as the output log, not a queue.
8. **Guide first, then node, check, install, verify.** A user cannot hook a sensor up without reading its guide, so the flow is: (1) read the full hook-up guide, (2) pick the node, (3) pre-install check on that node, (4) install, (5) post-install check.
   - Guides ship with the catalog: `scripts/bundle-cog-guides.py` bundles each sensor cog's `guide/` folder into `crates/weftos-cog-market/catalog/guides/<cog>.json` (the same JSON a cog serves at `/guide`), embedded by `guides.rs`. The Sensors tab lists every bundled guide, and the module card's Setup section uses its wiring rows, with no host, install or running cog. Images are not bundled: the Raspberry Pi header photo three guides carried has no recorded source or licence, so the bundles leave it out and each guide's own drawn header diagram and the pinout.xyz link stand in. The guides sit behind the `guides` cargo feature of `weftos-cog-market`, which only the console enables. A running cog's own `/guide` on its export port is only a fallback for a cog with no bundled guide, with a human message when it cannot be reached. The console flags when it is pointed at 127.0.0.1.
   - Pre-install facts come from a new read-only, token-guarded `GET /hw/buses` on the node's cog-host (`introspect.rs`): arch, UART / I2C / USB-serial device nodes that exist, whether the kernel command line puts the console on the UART (unknown when unreadable), and the enabled cogs. The console turns them into pass / fail / warn / verify-by-hand rows with the fix (`sensor_install.rs`). It only claims what the host reported: I2C addresses, wiring and licence are "verify by hand" rows.
   - Post-install reads `GET /cogs/<id>/last`, the cog's last output line cut down to health fields (`health`, `source.verified`, `reasons`, frame rate; never target positions), so `no_source` versus a found sensor shows even when the cog's own export is bound to loopback. The route needs the host token and sends no wildcard CORS: health, reasons and quality can reveal a person's state (an ECG's lead-off or rhythm quality). `/mesh/cogs` is guarded the same way. `GET /cogs/<id>/guide` serves a `guide.json` shipped inside an installed package, when one exists.
   - Checks and install run through the node's own cog-host. Picking another mesh node offers "Switch the console to <url>". Nodes are picked by address, never by name, only tailnet addresses (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`) are offered, and the current host's token is not sent to the new node: tokens are kept per host and the new node's is asked for.
9. **No cog yet** is stated plainly, with how to request one (the dashboard board) and how to build one (the `sensor-cog` skill).

## Consequences

- Adding a cog for a module is a one-line catalog edit plus the cog; the console picks it up. `scripts/consolidate-catalog.py` unions `cogs` when it merges duplicate modules.
- `HwCatalog::validate` rejects malformed or duplicated cog ids, empty doc links and a firmware `read_config_key` with no `read_with`. It cannot check that a cog id exists, because the cogs live in other repos; an unlisted cog shows as `cog (not published)`.
- The host reports each running cog's listening ports in `/status` (`export_ports`, read from `/proc`, token holders only), and the console prefers the cog's declared port among them. The built-in id-to-port map (`sensor_link::default_export_port`) is only a fallback for hosts that predate it and for cogs that are not running.
- Cogs that are not hardware-bound (`bridge`, `catalog`, `sensor-ota-push`) have no module and no panel.
