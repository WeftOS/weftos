# Ventuno Q: App Lab brick spike (own or share the MCU)

Date: 2026-10-25. Card `venq-cog-brick-spike`. Decides decision 3 of
[`docs/plans/ventuno-q-cog-plan.md`](../plans/ventuno-q-cog-plan.md).

**Result: WeftOS can own the MCU.** Bricks never touch the MCU or the router.
The only thing that overwrites a WeftOS sketch is starting an App Lab app that
ships its own sketch, so WeftOS uses bricks without such apps: brick-only App
Lab apps work, and running the brick's model server directly works better.

Board software at the time: arduino-app-cli 0.14.0 (assets 0.13.1), App Lab
0.11.0, arduino-router 0.11.0, arduino-cli 1.5.1, core `arduino:zephyr` 1.0.0,
`Arduino_RouterBridge` 0.4.3, GenieX 0.9.3.

## How App Lab is built

An App Lab app has up to three parts:

| Part | What it is | Touches the MCU? |
|---|---|---|
| Python app | `python-apps-base` container; mounts all of `/dev`, the router socket, LEDs, pipewire; runs as uid 1000 with the router group (gid 127) added | through the router, if its code calls `Bridge` |
| Bricks | Docker services from `assets/<ver>/compose/arduino/<brick>/brick_compose[.ventunoq].yaml`; the vision and audio ones are Edge Impulse model servers (`ei-qnn-models-runner`) on loopback, with `/dev/fastrpc-cdsp` and `/dev/dma_heap/system` passed in | **no**: no router socket, no MCU device |
| Sketch (optional) | Arduino sketch, compiled and uploaded with the bundled arduino-cli (`remoteocd`, sketch slot at `0x80F0000`, separate from the MCU bootloader) | **yes**: flashed on app start |

Only one App Lab app runs at a time. No default app was set, so nothing starts
at boot.

## What was tested

A minimal sketch (`weftos_ping` returns 42, `weftos_calls` counts) was built and
uploaded with plain `arduino-cli upload --fqbn arduino:zephyr:ventunoq`, outside
App Lab, and probed over `/var/run/arduino-router.sock` with a small msgpack-RPC
client.

| Question | Test | Result |
|---|---|---|
| Does a non-App-Lab sketch work with the router? | upload probe, call it | yes: 42, round trip 1.5–5 ms |
| Can two router clients coexist? | two persistent connections, 200 calls each, interleaved | yes: 400/400, no errors, counter values unique (MCU serialises), p50 2.8 ms, p95 3.8 ms |
| Does a brick need the MCU or router? | read compose files; run object detection | no: compose mounts neither; detection ran with the probe still answering |
| Brick-only app start/stop vs our sketch | start and stop an app with only `arduino:object_detection`, no sketch | sketch untouched on start and on stop |
| App with a sketch vs our sketch | start the `blink-with-ui` example | **reflashed** on start ("sketch updated"), our methods gone; after stop its own methods were gone too |
| App Lab restart | `systemctl restart arduino-app-cli` | sketch untouched |
| Reboot | full reboot | sketch untouched; WeftOS daemon and `/data` came back on their own |
| Brick and GenieX on the NPU together | detection loop running while GenieX generates, including GenieX loading a model and switching models | both work: detection 160–270 ms/frame, GenieX 74 tokens in about 3.7 s warm, model loads 7–12 s |
| Brick without App Lab | run `ei-qnn-models-runner` with plain Docker, `POST /api/image` | works: about 100 ms per HTTP round trip after warm-up (NPU inference 3–4 ms), same detections as through App Lab (bus 0.90, person 0.88/0.81/0.36) |

Earlier the same day GenieX returned `SDKError(Model loading failed)` for every
request. It was not caused by the brick: the failure stayed with the brick
stopped, and a fresh GenieX process worked, including while the brick ran. The
old process had been up about 13 hours. GenieX also only starts through its
`~/.local/bin/geniex` wrapper (the bare binary cannot find `libgeniex.so`).

## Brick model-server API

The brick's Python (`arduino/app_internal/core/ei.py` in `python-apps-base`)
calls the runner over HTTP on port 1337 inside the compose network (published
on loopback):

- `GET /api/info`: model and project metadata (health check).
- `POST /api/image`: multipart image upload; returns `result.bounding_boxes`
  (`label`, `value`, box) or classifications, plus `timing`.
- `POST /api/features`: JSON `{"features": [...]}` for non-image models
  (audio, motion; keyword spotting uses this path).

## Decision

Decision 3 is **own**: one generic WeftOS sketch on the MCU, with `mcu-bridge`
as its router client. Conditions:

1. WeftOS never starts an App Lab app that ships a sketch on a node where it
   owns the MCU. The `mcu-bridge` cog checks on start that its own methods
   answer and reports `mcu: foreign-sketch` if they don't.
2. Bricks are used by running their model servers directly as WeftOS-managed
   containers (preferred), or through brick-only App Lab apps. A `vision-detect`
   cog posts frames to `/api/image` and publishes `detections.v1`.
3. The NPU cannot be owned by one service while bricks run: the EI runners open
   `/dev/fastrpc-cdsp` themselves. Plan section 4.5 changes from "one owner"
   to "one scheduler": `npu-infer` owns GenieX and starts and stops the EI
   runners it needs, and is the only thing that does.
4. `npu-infer` health-checks GenieX and restarts it through its wrapper when a
   model load fails.
5. The router socket needs group 127 (`arduino-router`); `mcu-bridge` gets that
   group and nothing else on the MCU side.

## Not tested, and open

- **App Lab and router updates**: none was pending. They ship as apt packages
  (`arduino-app-cli`, `arduino-router`, `arduino-app-lab`). Hold
  `arduino-router`, or rerun these probes after every board update, since a
  router protocol change would break `mcu-bridge`.
- **Licences**: `models-list.yaml` records sources (Qualcomm AI Hub; Edge
  Impulse builds of YOLOX, MobileNetV2 and others) but no licences. The `.eim`
  packages fall under Edge Impulse's terms. Running them on our own board is
  fine; nothing is redistributed until each model is cleared.

## Follow-up: USB camera and microphone (2026-10-25)

Hardware: an Innomaker U20CAM (1080p, day/night, UVC) with a built-in USB microphone, on
a USB 3 port. Test audio came from the Mac's speaker (`say -v Samantha`) next to the
board, so the reference text is known.

| Test | Result |
|---|---|
| Node facts | `media.camera.usb` count 1 (`video0`; the UVC metadata node not counted); `media.audio.capture` lists the board codec and the camera's microphone |
| Capture | `gst-launch-1.0 v4l2src ! image/jpeg,width=1280,height=720` works; wide-angle lens |
| Object detection on live frames | YOLOX-nano on the NPU via the EI runner directly: 60 of 60 frames found the person (score ≥ 0.5); NPU inference median 7 ms; 7.6 frames/s end to end with one `curl` per frame (process start dominates; a persistent client is faster) |
| Microphone level | 16 kHz mono; speech RMS 7,000–12,000 against a floor near 500 (about 25 dB) at full gain |
| Speech to text | Parakeet TDT 0.6B int8 (CPU, 8 threads) transcribed all eight phrases word for word; 32 s of audio in 3.9 s (8.3× real time) |
| Keyword spotting ("Hey Arduino", NPU) | 1 s windows, 250 ms step, threshold 0.6: all 3 wake phrases caught (peak 1.0), but 3 false triggers on other speech ("Turn on the kitchen light", "The quick brown fox…", "Sensor readings arrive…") at 0.9–1.0; 25 ms per window |

The wake-word model is not usable alone with this voice: recall was perfect, but it fires
on ordinary speech at full confidence. Use it as the first stage only, and confirm with
ASR on the following audio (the transcript must contain the wake phrase), the same
two-stage gate as the voice VAD. Captured frames and the recording were deleted after the
test.

## State left on the board

The probe sketch is on the MCU (it registers only `weftos_ping` and
`weftos_calls`). The brick-only test app is in `~/ArduinoApps/weftos-spike-od`
for the keyword-spotting follow-up. Spike files are in `/data/build/spike`.
GenieX was restarted and serves on loopback as before.
