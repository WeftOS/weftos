# ADR-118: contract between the console, display and agent lanes (2026-10-10)

Three lanes build the chief-of-staff console at once:
- (A) display endpoint D1, which is the session transport;
- (B) the console screen;
- (C) the chief-of-staff agent and voice.

This file fixes where they meet. Change it only with the lead's agreement.

## 1. Display session (A provides, B uses)

Crate `weftos-display-client`:

```rust
let mut s = DisplaySession::open(target, grant)?;   // target: leaf endpoint or a LeafSim
s.apply(&[SceneOp ...])?;                           // weftos-leaf-scene ops, batched
let ev: Option<ObjectEvent> = s.poll_event();       // Tap{node}, Drag{node, dx, dy}, Sensor{name, value}
s.size(); s.capabilities();
```

- **Events:** `ObjectEvent::Sensor` carries leaf sensor events. `ptt.down` / `ptt.up` come from
  the pad's button; on the simulator they come from a key (space bar).
- **Backends:** `LeafSim` (in process, for tests), a desktop-window backend
  (`weftos-leaf-canvas` or the simulator's window), and the real leaf over the certified-leaf
  channel, which arrives later and is not needed for C1 or C2.

## 2. Console scene (B owns)

- **Layout and theme:** ADR-118 §1, exactly.
- **Node ranges, so lanes never collide:**

| Range | Contents |
|---|---|
| 1–99 | Chrome (frames, labels) |
| 100–199 | Avatar |
| 200–299 | Status |
| 1000–4095 | Chat lines, recycled |

- **API to the host:**

```rust
let mut c = Console::new(&mut session);
c.set_agent_state(AgentState::Listening | Thinking | Speaking | Idle | Blocked(reason));
c.set_status(StatusLine { task, model, lanes_running, ptt_active });
c.push_user_line(text); c.push_agent_text(delta_or_full, msg_id);
c.show_page(PageDoc) /* stub in C2: title plus plain paragraphs */; c.hide_page();
c.on_event(ev);                                      // scroll by drag, swipe to close the page
```

## 3. Agent conversation API (C owns, the console host uses)

The interface is an in-process Rust trait, with a local HTTP/SSE server for the Mac window and
other front ends:

```text
POST /v1/cos/utterance {text, source: "voice"|"typed"} -> 202 {turn_id}
GET  /v1/cos/events (SSE) -> {kind: "state", state}
                            {kind: "token", turn_id, text}
                            {kind: "message", turn_id, text, final: true}
                            {kind: "tool", name, summary, status}      // no arguments with secrets
                            {kind: "page", turn_id, markdown}           // for page mode (C5)
                            {kind: "status", task, model, lanes_running}
```

- **Binding:** loopback only, `127.0.0.1:18300`.

## 4. Voice (C owns)

- **Flow:** `ptt.down` starts capture from the Mac's microphone through the existing voice
  stack; `ptt.up` stops it, runs STT and posts the utterance with `source: voice`.
- **Feedback:** the console shows `ptt_active` while the button is held, and the transcript as
  the user's line.
- **Retention:** raw audio is discarded after transcription unless a debug flag is set.
