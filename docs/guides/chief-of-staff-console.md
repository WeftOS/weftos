# The chief-of-staff console on the Mac

ADR-118 C3. The console screen (`weftos-cos-console`), the agent
(`weftos-cos`) and push-to-talk run as two processes on the Mac: the agent
owns the microphone and the models; the console host draws the screen in
an SDL window and talks to the agent over its loopback API. The space bar
is the push button. The pad comes later (C4); nothing here depends on it.

## Build

```bash
scripts/build.sh cos --features mic,onnx   # agent with mic capture and parakeet STT
scripts/build.sh cos-host                  # console host with the SDL2 window, plus tests
```

`cos-host` needs SDL2 from Homebrew (`brew install sdl2`); it is found
through pkg-config. Without `onnx` the agent accepts the button but the
STT backend reports its model absent and nothing is transcribed; typed
input still works.

## Run

```text
terminal 1:  weft cos serve --mic            # or target/debug/weftos-cos serve --mic
terminal 2:  crates/weftos-cos-console/target/release/cos-console-host --window
```

- **Hold space to talk.** The avatar listens while the key is down; on
  release the agent transcribes and the transcript appears as your line.
  The agent then thinks and streams its answer.
- **Enter opens a typed line** (development only, in the window): type,
  Enter sends it as a `typed` utterance, Escape cancels. While the line is
  open the space bar types a space. `--no-typing` turns this off.
- **Touch is the mouse.** Drag the log to scroll, tap a survey button,
  toggle a checklist item, swipe the page right to close it.
- **Models:** the status strip shows the model in use. Quick turns go to
  `models.quick`, planning turns to `models.plan`
  (`~/.config/weftos/cos.json`); see `crates/weftos-cos/src/config.rs`.

Flags: `--agent http://127.0.0.1:18300` (the agent's bind), `--scale 2`
(window zoom), `--sim --seconds N` (headless smoke run without SDL).

## What you see when the agent is down

The avatar shows `BLOCKED` with the reason `agent offline` and the status
line reads `reconnecting`; the host retries the event stream with backoff
(250 ms doubling to 4 s). When the stream comes back the avatar returns to
idle and the last status is restored. If the stream drops mid-answer the
line on screen stays as it was and the next tokens start a new line.

## How the pieces connect

```text
space bar ── SDL window ──▶ ptt.down/up sensor ──▶ host ──▶ POST /v1/cos/ptt
                                                  ◀── {posted: {text}} ◀── mic + STT (agent)
host ◀── GET /v1/cos/events (SSE): state, status, token, message, layout,
         checklist, steps, ask, page
tap on a guide view ──▶ host ──▶ POST /v1/cos/answer
Enter + typing ──▶ host ──▶ POST /v1/cos/utterance {source: typed}
```

The API is `docs/plans/adr-118-cos-contract.md` §3. The host links only
`weftos-cos-console` and `weftos-display-client`, never the agent crate.

## Tests

`scripts/build.sh cos-host` runs, headless:

- the host against a fake agent on real loopback sockets
  (`crates/weftos-cos-console/tests/host.rs`): tokens stream into the log,
  an `ask` shows the survey and a tap posts the answer body, button edges
  post push-to-talk and the transcript lands, typing posts a typed
  utterance, an unreachable agent shows Blocked and recovers, a dropped
  stream ends the line cleanly;
- the agent's `POST /v1/cos/ptt` route (`crates/weftos-cos/tests/ptt_api.rs`);
- the window key mapping (`crates/weftos-display-client/src/keys.rs`).

## Later

- The pad (C4): the same host draws through the leaf display endpoint; the
  button is the pad's GPIO sensor; typing stays off there.
- Spoken replies and rendered markdown pages (C5).
- `Back` on a survey is drawn but not yet sent; the agent has no kind for it.
