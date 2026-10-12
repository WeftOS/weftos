# The chief-of-staff console on the edge pad: hardware-in-the-loop

ADR-117 / ADR-118. The console host draws on the CrowPanel edge pad through
the daemon's display endpoint, and the pad's touch and push-to-talk button
come back as events. Everything below the hardware is already tested against
an emulated pad (`scripts/build.sh leaf-display`); this guide is for the first
run on the real one. Design: `docs/design/display-session-transport.md`.

`scripts/edgepad-console-hil.sh` does the provisioning, checks and launch.
It never flashes anything and never touches USB.

## What you need

- The pad on USB power, on the same Wi-Fi as this Mac (2.4 GHz).
- `weaver` and a daemon that has booted at least once with `[kernel.mesh]`
  enabled (that writes `<runtime>/node.key`, which `prepare` reads), and for
  the Mac window nothing else. For the console's brain: the agent
  (`weft cos serve`, see `docs/guides/chief-of-staff-console.md`); without it
  the console draws and shows BLOCKED `agent offline`.
- For the firmware build: Docker and the `weftos-leaf-build:idf532-cache`
  image (arm64 `espressif/idf-rust` with ESP-IDF 5.3.2 cached).
- The owner's `wifi_secrets.rs` for the pad (gitignored). The build uses the
  copy inside the image, or `EDGEPAD_WIFI_SECRETS=<path>`.

## Steps

1. **Provision and enrol the pad's identity.**

   ```bash
   scripts/edgepad-console-hil.sh prepare
   ```

   This prints this machine's node and user identities, issues the pad's
   certificate (`weaver leaf provision`, keys under `~/.weftos/edgepad-hil/`,
   mode 0700, never in the repo), and enrols it in
   `<runtime>/leaf/registry`. The daemon installs its leaf gate **only if that
   directory exists when it boots**, so the script creates it and tells you
   when the daemon needs a restart.

2. **Let the daemon listen for the pad on the LAN.** In the daemon's config:
   `~/.weftos/weave.toml` for the user daemon; a daemon started with
   `--config <file>` takes the same keys as JSON under `kernel.mesh`.

   ```toml
   [kernel.mesh]
   enabled = true
   listen_addr = "127.0.0.1:9489"        # main mesh stays on loopback
   leaf_listen_addr = "192.0.2.10"       # this Mac's LAN IP; leaf TCP :9491, discovery udp :9490
   ```

   Restart the daemon (`weaver kernel restart`). `weaver kernel status` must print
   `Leaf bind:  tcp <LAN ip>:9491 / discovery udp <LAN ip>:9490`. A wildcard
   address needs `leaf_listen_any = true` and is refused otherwise.

3. **Rehearse without the pad** (optional, recommended once):

   ```bash
   scripts/build.sh cos-host                       # console host (release, SDL2 window)
   (cd crates/weftos-display-client && cargo build --features wire-leaf --bin leaf-emulator)
   scripts/edgepad-console-hil.sh rehearse
   ```

   This was run against a real `weaver` daemon (collapsed mesh, leaf listener
   on loopback) on 2026-10-12 and passed: discovery, signed join, a 5 s console
   session, clean teardown.

   An emulated pad with its own throwaway identity (`leaf-rehearsal`; the
   daemon keeps a replay floor per leaf, so it must not share the real pad's)
   joins through real discovery, and the console host opens a session on it
   for 5 s. If this passes, the daemon config, registry and host are right and
   any later failure is the pad's.

4. **Build the pad image** (does not flash):

   ```bash
   EDGEPAD_PROVISION_DIR=~/.weftos/edgepad-hil/leaf-prov scripts/build.sh edgepad-fw
   ```

   Output: `target/edgepad-fw/edgepad-merged.bin` (4 MB, app 64% of its slot).
   Without `EDGEPAD_PROVISION_DIR` the image boots the display but never
   joins the mesh.

5. **Flash it** (a deliberate step, by the person at the pad): follow
   "Flashing" in `crates/clawft-edge-pad-idf/README.md`. Verify the port by the
   board's MAC (`esptool chip-id`), not by its port name: the CH340 re-enumerates and another Espressif device may sit on a neighbouring port.
   Back up the flash first. Write at 230400 baud; if `write-flash` reports
   "Invalid head of packet", retry.

6. **Check the join.** Power-cycle the pad, then:

   ```bash
   scripts/edgepad-console-hil.sh check        # uses: weaver leaf status --leaf-id <id>
   ```

7. **Run the console on the pad.**

   ```bash
   scripts/edgepad-console-hil.sh run          # cos-console-host --leaf <id>
   ```

## What to look for

Serial log (115200 baud on the CH340), in order:

| Line | Meaning |
|---|---|
| `[mesh] certified leaf id '<id>'` | the identity in the image is the one you enrolled |
| `[mesh] connected to <ip>:9491` | discovery found the daemon's leaf listener |
| `[mesh] subscribed` / boot text "subscribed -- waiting for pushes" | the signed subscribe was ACKed |
| `[scene] SESSION ops=N render=...` | a session's ops are arriving; the boot text disappears |
| `[ptt] ptt.down` / `ptt.up` | the IO44 button is seen |

On screen: the green console (avatar, status strip, chat area). Hold the
button: the status strip's PTT dot lights and the avatar listens. Tap a survey
button or drag the log: the host reacts. Stop the host: the pad clears the
console and shows the boot text again. `Ctrl-C` and a killed host both end
the session within a minute.

## If it does not work

| Symptom | Likely cause |
|---|---|
| `check`: no `Leaf bind:` line | gate not installed: registry dir missing at boot, mesh not enabled, or no persisted node key |
| `check`: bind is on 127.0.0.1 | `leaf_listen_addr` not set; the pad cannot reach loopback |
| pad logs `discovery failed` | different Wi-Fi or a firewall blocking UDP 9490; discovery is a broadcast |
| daemon logs `leaf frame refused` (wrong sequence) | the replay floor. A pad whose NVS was erased (a full merged-image flash can do it) restarts at sequence 1, behind the daemon's floor. Re-provision the leaf (`rm -r ~/.weftos/edgepad-hil/leaf-prov`, then `prepare`, rebuild, reflash), or, to keep the identity, delete `<runtime>/leaf/floors/<leaf id>.json` (your call: it lowers the replay floor for this one pad) |
| daemon logs `leaf frame refused` (not enrolled) | the image carries a different identity than the one enrolled |
| `cos-console-host`: `display busy: held by ...` | another app holds the screen |
| `cos-console-host`: `... is not connected` | the pad is not subscribed (see `check`) |
| touch lags | every pointer move is a signed, journaled frame; a fast drag backpressures. Known, see the design doc |

## Known gaps

No ADR-108 grant chains yet (any `Write` caller may open a session; `proof` is
carried, not verified), no queueing of a second app, no system strip. The
firmware builds and links but has not run on the pad; this guide is the first
run. Report what the serial log says at each step.
