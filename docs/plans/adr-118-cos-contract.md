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

## 3. Agent conversation API, version `cos.v1` (frozen 2026-10-12)

C owns it; the console host, the pad lane and the Mentra glasses bridge use it. It is an
in-process Rust trait (`weftos_cos::Conversation`) with a local HTTP/SSE shell over it. The
shell is the contract for every front end.

**Binding.** Loopback only, default `127.0.0.1:18300`; a non-loopback bind is refused. No
authentication (loopback is the boundary). JSON bodies are UTF-8 with
`Content-Type: application/json`. Errors are `{"error": "<text>"}`, with secrets redacted.

**Versioning.** `GET /healthz` answers `{"ok": true, "api": "cos.v1"}`. Within `cos.v1` changes
are additive only: new optional request fields, new optional event fields, new event kinds.
A client MUST ignore event kinds and fields it does not know. Anything else (a removed or
renamed field, a changed meaning, a new required field) is `cos.v2` under `/v2/cos/...`, served
beside v1 for at least one release. `tests/contract_v1.rs` fails when this section and the
code disagree.

### 3.1 Routes

| Route | Request body | Success | Refusals |
|---|---|---|---|
| `POST /v1/cos/utterance` | `{"text": string, "source": "voice"\|"typed"\|"touch"}`; `text` is 1 to 8000 characters after trimming | `202 {"turn_id": string}` | `400` empty or longer text, unknown `source`, malformed JSON |
| `POST /v1/cos/answer` | `{"kind": "answer"\|"toggle"\|"step"\|"skip"\|"back", "id": string, "value": any}`; `value` defaults to `null` | `202 {"turn_id": string}` | `400` unknown or stale `id`, a `value` the kind cannot use, malformed JSON |
| `POST /v1/cos/ptt` | `{"down": bool}` | down: `202 {"active": true}`; up: `202 {"active": false, "posted": {"text": string, "turn_id": string}}` or `202 {"active": false, "nothing": "<reason>"}` | `503` the agent runs without `--mic`; `500` capture or STT failed; `400` malformed JSON |
| `GET /v1/cos/events` | none | `200 text/event-stream` (see 3.2) | none |
| `GET /healthz` | none | `200 {"ok": true, "api": "cos.v1"}` | none |

- **`turn_id`** is an opaque string (32 hex characters today). The events of the turn carry it.
- **`answer` values.** `answer`: the 1-based choice number of the `ask` with that `id`.
  `toggle`: the checklist item id, or `{"item_id": string, "done": bool}`. `step`: `"next"`,
  `"back"`, or a 1-based step number. `skip`: ignored. `back`: ignored; it re-asks the
  previous survey question (question 1 re-shows question 1). An `answer` or `skip` becomes a
  `touch` user turn; `toggle`, `step` and `back` change the view and re-emit it without a model
  turn (the returned `turn_id` then has no events of its own).
- **`ptt`** is the push-to-talk button. `down` starts capture (and silences a reply being
  spoken); `up` stops it, transcribes locally, and posts the transcript as a `voice` utterance.
  The reply tells the host what was posted so it can show the transcript. Raw audio is
  discarded after transcription (section 4).

### 3.2 Event stream

`GET /v1/cos/events` is Server-Sent Events. Each event is one `data:` line holding one JSON
object whose `kind` names its shape; lines starting with `:` are keep-alives. There is no
replay: a client sees events emitted after it connected, so connect before posting. A
subscriber that falls too far behind skips events instead of stalling the agent. The
objects, with one example of each (these examples are executable: the contract test parses
every block below and requires it to round-trip through the code's event type):

| `kind` | Fields | Meaning |
|---|---|---|
| `state` | `state`: `idle`\|`listening`\|`thinking`\|`speaking`\|`blocked`; `reason` only with `blocked` | the avatar state |
| `status` | `task`: string; `model`: string; `lanes_running`: number | the status line. `model` is the model that is answering this turn (`provider/model`), `none` for a confirmation turn; a retry on the fallback model sends a new `status` with `task` ending ` (fallback)` |
| `token` | `turn_id`; `text` | a streamed piece of the reply. Optional to render; the `message` is authoritative |
| `message` | `turn_id`; `text`; `final`: bool | the reply. `final: true` ends the turn's text; every turn that has a reply sends exactly one |
| `tool` | `name`: dotted id; `summary`: string; `status`: `running`\|`ok`\|`error`\|`refused`\|`awaiting_confirmation`\|`cancelled` | a tool call. Never carries arguments or credentials |
| `page` | `turn_id`; `markdown` | a full page for the `page` layout |
| `layout` | `layout`: `split`\|`page`\|`guide` | which layout to show |
| `checklist` | `id`; `title`; `items`: `[{id, text, done}]` | sent whole on every change |
| `steps` | `id`; `title`; `steps`: `[string]`; `current`: 0-based number | numbered instructions |
| `ask` | `id`; `question`; `explain`; `choices`: `[{n, caption}]` (2 to 6, `n` from 1); `index`; `total` | one survey question |

```json cos-event
{"kind": "state", "state": "thinking"}
```
```json cos-event
{"kind": "state", "state": "blocked", "reason": "model: connection refused"}
```
```json cos-event
{"kind": "status", "task": "typed turn", "model": "local/mlx-community/GLM-4.7-Flash-4bit", "lanes_running": 0}
```
```json cos-event
{"kind": "token", "turn_id": "0b3f7c1e5a9d4e2f8c6a1d7b3e9f5a20", "text": "Two of "}
```
```json cos-event
{"kind": "message", "turn_id": "0b3f7c1e5a9d4e2f8c6a1d7b3e9f5a20", "text": "Two of three nodes are up.", "final": true}
```
```json cos-event
{"kind": "tool", "name": "nodes.health", "summary": "2/3 nodes healthy", "status": "ok"}
```
```json cos-event
{"kind": "page", "turn_id": "0b3f7c1e5a9d4e2f8c6a1d7b3e9f5a20", "markdown": "# Today\n\n- ship the pad button"}
```
```json cos-event
{"kind": "layout", "layout": "guide"}
```
```json cos-event
{"kind": "checklist", "id": "day", "title": "Today", "items": [{"id": "a", "text": "Review the PR", "done": false}]}
```
```json cos-event
{"kind": "steps", "id": "flash", "title": "Flash the pad", "steps": ["Plug it in", "Run the flasher"], "current": 0}
```
```json cos-event
{"kind": "ask", "id": "q1", "question": "Which first?", "explain": "Pick one.", "choices": [{"n": 1, "caption": "Pad"}, {"n": 2, "caption": "Glasses"}], "index": 1, "total": 2}
```

**Order within a turn.** `state: thinking`, then `status`; then any number of `token`, `tool`
and guide events (`layout`, `checklist`, `steps`, `ask`, `page`) in the order they happen;
then `state: speaking` and the final `message`; then `state: idle`. A failed turn sends a
final `message` ("I hit a problem: ...") and then `state: blocked` with the reason. A tool
that needs the owner's yes sends `tool` with `awaiting_confirmation` and a final `message`
holding the question; the owner's next utterance (`yes`, or anything else to cancel) is a new
turn, its `tool` events end `ok`, `error` or `cancelled`. After a survey `ask` the turn ends;
the owner's tap or spoken number comes back as the next utterance (`source: touch`).

**Who speaks.** The agent host reads the final `message` of a `voice` turn aloud on the Mac
(section 4). A front end that has its own speaker (the glasses) renders `message` itself and
may switch the host's speech off with `voice.tts.engine: "none"`.

```json cos-request
{"text": "where are we", "source": "typed"}
```
```json cos-request
{"kind": "answer", "id": "q1", "value": 2}
```
```json cos-request
{"down": true}
```

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
- 2026-10-12 (cosbrain): §3 frozen as `cos.v1`: exact routes, payloads and events, `GET /healthz` carries `"api": "cos.v1"`, executable examples (`tests/contract_v1.rs`). The `status` event reports the model that is answering, including a fallback. The agent host speaks `voice` replies itself (local TTS, `voice.tts` in `cos.json`).
