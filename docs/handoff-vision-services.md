# Handoff: vision services through WeftOS (deferred 2026-10-29)

Epic card `30e898d7`. The owner deferred the remaining vision work on 2026-10-29: images for the
current client are analysed locally on the Mac. This page records what landed, what is staged,
and how to resume stage 1 (image chat from a Mac answered by the Venq's VLM through WeftOS).

## What landed on `target-0.8.5`

- **ADR-125** (Proposed), the vision service contract: `svc.vision.*` kinds and one `vision.v1`
  request/reply schema in `contracts/cogs` and `clawft-types` `cog_topics`, with golden vectors.
  It refuses unknown fields, refuses bodies over 64 KiB, and pins embedding kinds.
- **Inference roles backed by a cogbus service** (ADR-101 update). `inference.json` roles take
  `svc: {topic, model, project}`. `weaver infer status` and `weaver infer expose` exist.
  Governance sees the caller (`local:<ip>`, `mesh:<node>`) and the carrier (the proxy). Image
  blobs are released after each call, and a full store answers 429.
- **Advert timing**: received `infer.<role>` adverts expire by receive time, and a stopped
  provider withdraws its advert at once. The defaults were slowed on 2026-10-29 to a 60 s announce
  and a 180 s TTL, set by `advert_secs` and `advert_ttl_secs`.
- **macOS native cogs** (ADR-100 update): the `aarch64-macos` key and the
  `aarch64-macos-native` route. Only thin arm64 Mach-O executables are accepted. They run as the
  daemon user. `pack` checks each binary against its key.
- A `pulldown-cmark` 0.13.5 bump (RUSTSEC-2026-0414; the gate's `cargo audit`).

The stage-1 landing commit is `bb937eab6`. The advert defaults landed after it.

## Staged, not deployed

- **Venq daemon**: runs the dev weaver built from the stage-1 branch (`0.8.4 (ff18de75)`).
  The rc.15 binary is kept beside it as `~/.local/bin/weaver.rc15`. To roll back, move it over
  `weaver`, then run `systemctl --user restart weftos.service`. weftos-sd and venq-camview were
  unaffected by the restart.
- **npu-infer 0.1.1 package**: dev-signed with the owner-operator key.
  - Location: on PG under the weftos user at `~/vision-stage1/npu-infer-0.1.1-pkg`, with a local
    copy in the worktrees volume (`npu-infer-0.1.1-pkg`). Package id `de69c082…`.
  - It is built from weftos-cogs branch `fix/npu-infer-existing-listener`, which adds the
    listener handling and the `[mesh_limits]` table that pack requires.
- **v0.8.4 weaver client on PG**: weftos user, `~/vision-stage1/client084/` (release
  `clawft-weave-x86_64-unknown-linux-gnu`, sha256 checked against the release). It is used only as
  a client, because PG's installed rc.15 CLI has no `workload place --arg`.
- **PG's WeftOS project child kernel**: stopped again. Start it with
  `weaver kernel start --profile user --project weftos` before placing.

## Resuming stage 1

Preconditions on the Venq:

1. Nothing else drives GenieX. The eikon bridges on the Mac, which tunnelled to the Venq's GenieX
   on loopback :18181, were stopped at the owner's request on 2026-10-29 ("not the right way to be
   doing this"). Vision access goes through WeftOS, so they should not come back. The GenieX
   process they used was still running, idle, at handoff, without a parent npu-infer.
2. MemAvailable is at least 10 GiB. The planner reserves the cog's `[resources].ram_mb` (10240)
   against MemAvailable. It refused at 3.8 GiB while a model was resident through the bridge. The
   baseline with weftos-sd and venq-camview resident is about 11.2 GiB.
3. If the orphaned GenieX is still listening and idle, the launcher replaces it: it sends SIGTERM,
   then SIGKILL after 10 s, but only when the process is a GenieX of the same uid with parent pid 1
   serving that port. Otherwise the load is refused with the port and pid named.

Place, from PG as the weftos user, against the project child kernel:

```sh
WEFTOS_RUNTIME_DIR=~/.weftos/run/<weftos project id> \
  ~/vision-stage1/client084/<dir>/weaver workload place ~/vision-stage1/npu-infer-0.1.1-pkg \
  --pin <venq node id> --mode listener \
  --arg=--geniex-bin --arg=<venq home>/.local/bin/geniex \
  --arg=--geniex-home --arg=<venq home> \
  --arg=--budget-mib --arg=7000
```

`--geniex-bin` and `--geniex-home` are needed because the cog sandbox has no `HOME`: the first
placement exited 2 with "GenieX wrapper not found at ./.local/bin/geniex". `budget_mib` 7000
keeps qwen3-vl-4b (estimated 6000 MiB) as the only resident model.

Then:

1. Confirm `svc.infer/venq-npu` is served (cogbus activity log on the Venq).
2. Venq `~/.weftos/run/inference.json`:
   `{"roles":[{"role":"vision","proxy_port":18391,"svc":{"topic":"svc.infer/venq-npu","model":"qwen3-vl-4b","project":"<weftos project id>"}}],"mesh":{"expose":["vision"],"serve_peers":{"vision":["<mac node id>"]}}}`.
   Restart weftos.service only if the file is not reloaded, and record the weftos-sd and
   venq-camview PIDs before and after.
3. Mac: install the stage-1 weaver, write
   `{"mesh":{"remote_nodes":{"vision":["<venq node id>"]}}}` and run
   `weaver kernel start --profile user`.
4. Send one chat completion with a generated PNG (text, a QR code, shapes; never client media)
   to the Mac's `127.0.0.1:18391`.
5. Abort rule for the first calls: if MemAvailable falls below 1 GiB, the kernel swaps hard, or
   weftos-sd restarts, stop npu-infer through the daemon (`weaver workload stop`) and record RSS
   and MemAvailable.

## Open items

- **MeshId risk**: the Venq reports no MeshId ("learned on pair"). If Mac to Venq forwarding is
  refused at admission, that is a pairing or access change for the owner.
- **ADR-125 acceptance** by the owner. Its open questions cover the kind names, the caps, labels
  without a list, and refusing unknown fields.
- **Two unpushed weftos-cogs branches**, which need pushing, PRs and an owner merge, then a WeftOS
  pin bump:
  - `feat/vision-venq-cogs` (`aba8e36`): vision-ocr, vision-codes, vision-faces and vision-embed;
  - `fix/npu-infer-existing-listener` (`f6380f5`).
- **vision-embed memory**: SigLIP2 so400m declares 10240 MiB of address space, and with npu-infer
  that does not fit the Venq's 14 GiB. This is an owner decision: a resident-memory amendment, or
  a smaller model.
- **Cards**: `081139ca` (nested_instances flake; it also fails at base) and `3c2f398a` (deferred
  review items: newest-wins flapping, untested timeout and error release paths, a final chunk
  without a handle, no runtime vision.v1 enforcement, BlobHandle unknown fields).
- **Not started**: cards 4, 6, 7, 8, 10 and 11 (svc over the mesh, client access, provider
  resolution, eikon backend, Molmo, SAM), and the Windows guide for card 9. Cards 2 and 3 still
  need live acceptance: an apple-vision cog that uses the macOS route, and placing the Venq cogs.
