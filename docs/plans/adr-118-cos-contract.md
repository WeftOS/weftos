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
| 300–499 | Page layout |
| 500–999 | Guide views (header 500–559, rows from 560 in strides of 10) |
| 1000–4095 | Chat lines, recycled (4092–4095: scroll thumb and unread marker) |

- **API to the host:**

```rust
let mut c = Console::new(&mut session);
c.set_agent_state(AgentState::Listening | Thinking | Speaking | Idle | Blocked(reason));
c.set_status(StatusLine { task, model, lanes_running, ptt_active });
c.push_user_line(text); c.push_agent_text(delta_or_full, msg_id);
c.set_layout(Layout::Split | Layout::Page | Layout::Guide);
c.show_page(PageDoc) /* example page in C2: title, headings, paragraphs, a list */; c.hide_page();
c.show_checklist(Checklist { id, title, items: Vec<{id, text, done}> }); c.set_item(id, item_id, done);
c.show_steps(Steps { id, title, steps: Vec<String>, current });
c.ask(Survey { id, question, explain, choices: Vec<{n: u8, caption}>, index, total });  // 2..=6 choices
// guide events come back from c.on_event(..) -> Option<GuideAnswer>:
//   Answer{survey_id, choice_n} | Toggle{checklist_id, item_id, done} | Step{steps_id, dir} | Skip{survey_id} | Back{survey_id}
c.on_event(ev);                                      // scroll by drag, swipe to close the page
```

## 3. Agent conversation API (C owns, the console host uses)

The interface is an in-process Rust trait, with a local HTTP/SSE server for the Mac window and
other front ends:

```text
POST /v1/cos/utterance {text, source: "voice"|"typed"|"touch"} -> 202 {turn_id}
POST /v1/cos/answer {kind: "answer"|"toggle"|"step"|"skip"|"back", id, value} -> 202  // guide interactions; back re-asks the previous survey question (Q1: re-shows Q1)
GET  /v1/cos/events (SSE) -> {kind: "state", state}
                            {kind: "token", turn_id, text}
                            {kind: "message", turn_id, text, final: true}
                            {kind: "tool", name, summary, status}      // no arguments with secrets
                            {kind: "page", turn_id, markdown}           // page layout (example page now; markdown in C5)
                            {kind: "layout", layout: "split"|"page"|"guide"}
                            {kind: "checklist", id, title, items: [{id, text, done}]}
                            {kind: "steps", id, title, steps: [..], current}
                            {kind: "ask", id, question, explain, choices: [{n, caption}], index, total}
                            {kind: "status", task, model, lanes_running}
```

- **Binding:** loopback only, `127.0.0.1:18300`.

## 4. Voice (C owns)

- **Flow:** `ptt.down` starts capture from the Mac's microphone through the existing voice
  stack; `ptt.up` stops it, runs STT and posts the utterance with `source: voice`.
- **Feedback:** the console shows `ptt_active` while the button is held, and the transcript as
  the user's line.
- **Retention:** raw audio is discarded after transcription unless a debug flag is set.

## Changes

- 2026-10-11: console methods return `Result<(), S::Error>`; `Console` holds `&mut S`, reached through `session()`; `ObjectEvent` may add `Release{node}` (end of gesture, for swipes); `tick(now_ms)` drives avatar tweens; node range 300–999 assigned to page mode and guide views.
- 2026-10-11: page 300–499 and guide 500–999 (replacing 300–999); `GuideAnswer::Back{survey_id}` added; gestures end on a tap or a 250 ms pause in dragging (the client has no pointer-up event); `PageDoc` blocks are Heading, Paragraph, Bullets and Rule.
- 2026-10-11 (host): `POST /v1/cos/ptt {down: bool}` added to §3 (202 `{active}` on down; 202 `{active: false, posted: {text, turn_id}}` or `{nothing: reason}` on up; 503 when the agent runs without `--mic`). The console host (`cos-console-host`, in `weftos-cos-console`) talks to the agent over HTTP/SSE only and shows the voice transcript from the `up` reply. The desktop window reports keys as sensors `key.char` / `key.named` (display-client `keys`), used for the host's development typing line; the pad never sends them. `GuideAnswer::Back` is not posted (no agent kind for it yet).
