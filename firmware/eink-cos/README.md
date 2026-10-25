# eink-cos: chief-of-staff glance panel (ADR-119 S4)

Waveshare 2.13" HAT Rev 2.1 (glass V4, GxEPD2_213_GDEY0213B74, 250x122) on an
ESP32-S3-N16R8. Shows agent state, current task, next step, one alert (inverted
bar) and the model, readable with the main screen off.

Wiring: CS 10, DC 9, RST 8, BUSY 13, SCK 12, MOSI 11 (no MISO; GPIO13 is BUSY).

## Protocol

Newline-delimited JSON at 115200 over USB serial, one object per line:

    {"v":1,"state":"idle|listening|thinking|speaking|blocked","task":"...","next":"...","alert":"...","model":"...","ts":"HH:MM"}

An image line draws idle art with a full refresh:

    {"v":1,"img":"<base64 of 3904 bytes>","w":250,"h":122,"caption":"the loom at dusk","ts":"HH:MM"}

The bitmap is 1 bit per pixel, MSB first, 32 bytes per row (6 pad bits), 122
rows, `1` = ink. Only 250x122 with exactly 3904 decoded bytes is accepted. The
bottom 12 px are overdrawn with a caption strip and the clock. The next text
line whose content changed (or that carries `"redraw":true`) does a full
refresh back to status; an unchanged heartbeat leaves the art up. The receive
buffer is heap-allocated (about 5.5 KB).

Malformed lines, unknown states and `v` other than 1 are ignored. Strings are
ASCII (anything else is shown as `?`) and truncated with `...`. The panel
redraws when state/task/next/alert/model change (partial refresh), does a full
refresh every 6 partials, on a large text change or after 30 minutes, and powers the booster off (no hibernate: a wake-up reset drops the previous-image RAM and partial updates then overlay old text). With
no valid line for 5 minutes it shows "NO LINK". The clock only repaints on the
next real change; the bridge sends a heartbeat every 60 s so the link stays up.

## Build and flash

    arduino-cli compile --fqbn esp32:esp32:esp32s3:CDCOnBoot=cdc,FlashSize=16M firmware/eink-cos
    arduino-cli upload -p /dev/cu.usbmodemXXXX --fqbn esp32:esp32:esp32s3:CDCOnBoot=cdc,FlashSize=16M firmware/eink-cos

Needs the esp32 core 3.3.x and the GxEPD2 and Adafruit GFX libraries.

## Bridge (Mac)

    scripts/build.sh cos
    nohup target/debug/weftos-cos glance-serial --port /dev/cu.usbmodemXXXX [--url http://127.0.0.1:18300] > glance.log 2>&1 &

It follows `GET /v1/cos/events` on the local cos agent, reconnects when the
agent restarts (the panel shows `blocked: agent link down` meanwhile), reopens
the port when the board resets, and writes a line only when the content changes
(2 s debounce). Source: `crates/weftos-cos/src/glance.rs`.

## Idle art

With `art.url` set in the CoS config (`--config`, default `cos.json`), the
bridge asks an image service for a woodcut-style weaving scene after 15 minutes
with no agent activity and no live card, at most once an hour (the last time is
kept in `glance.art.json` beside the status file, so restarts do not repeat it).
Any event or card takes it down. `art.token_file` (default
`~/.config/weftos/sd-art.token`) holds the bearer token and must be mode 600.
Failures fall back to status and retry after 10 minutes. The `art` block of the
glance status file shows `off|waiting|generating|showing`. Source:
`crates/weftos-cos/src/glance_art.rs`.
