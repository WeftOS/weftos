# ADR-118: The chief-of-staff console: a voice-driven agent on a green vector display

- **Status**: Proposed (2026-10-10; owner request)
- **Deciders**: owner
- **Builds on**: ADR-117 (display endpoints: objects, bindings, grants; this console is its
  first app and replaces the "command center" sketch in ADR-117 §8), `vector-leaf-display.md`,
  ADR-112 (agent teams: define, apply, run), the voice stack (`clawft-voice-talk`,
  `clawft-voice-onnx`, `clawft-voice-tts`, `clawft-channels` talk mode; ADR-058..061 voice and
  ECC design), the dashboard harness API, ADR-114 `weftos://` names.

## Context

The owner wants the edge pad to start with a completely new interface:
- a green-screen vector look;
- a chat in which the user only **speaks**, with speech-to-text driven by a hardware push
  button wired separately, not drawn on screen;
- a visual area for an avatar, which can later expand to full pages of rendered markdown and
  SVG;
- a status area;
- a chat log the user can scroll back through;
- behind it, a **real agent acting as the user's chief of staff**, able to use all WeftOS
  models, skills, helpers and techniques, with more built as needed.

## Decision

### 1. Screen layout (800×480, landscape)

| Region | Box | Contents |
|---|---|---|
| System strip | 0,0 → 800×24 | Firmware-owned (ADR-117 §5): app name, mesh, clock |
| Visual panel | 0,24 → 300×456 | The avatar now; page mode later |
| Status | 300,24 → 500×64 | Agent state, current task, model, running lanes, push-to-talk indicator |
| Chat log | 300,88 → 500×392 | Transcribed user lines and agent replies, newest at the bottom, scrolled by dragging |

- **Page mode:** the visual panel expands to 0,24 → 800×456 and shows rendered markdown with
  SVG figures, scrolled by dragging. The page is laid out on the host and sent as scene objects
  (text runs, rules, vector paths). A swipe or a voice command returns to the split.
- **Theme:** phosphor green `#33FF66` on black; dim green `#127A36` for history and
  secondary text; amber `#FFB000` only for warnings and blocked states; 1 px vector strokes;
  the built-in monospaced font. No bitmaps in the UI. Pixels appear only as image and video
  media objects (ADR-117 §6).
- **Avatar:** a vector wireframe head with four states (idle, listening, thinking, speaking),
  animated with scene tweens and driven by the agent's state stream.

### 2. Voice only, with a hardware push button

- **The button:** a momentary button on a pad GPIO pin, wired by the owner. The pad publishes
  it as a sensor (ADR-117 §4): `ptt.down` and `ptt.up`, debounced.
- **Audio source:** pluggable. v1 uses the **Mac's microphone**: the console host records while
  the button is held. Optionally, an I²S MEMS mic on the pad streams 16 kHz audio over the leaf
  channel while the button is held.
- **Speech-to-text:** the existing voice stack's STT, run locally (`clawft-voice-onnx` and the
  llm lab). The transcript becomes the user's chat line and the agent's input. Raw audio is not
  stored by default.
- **No keyboard:** there is no on-screen keyboard or typing path on the pad. The Mac window
  version may accept typed input for development.

### 3. The chief-of-staff agent

- **Package:** a WeftOS agent package, `agents/chief-of-staff` (AGENT.md, skills, tools, evals),
  in the owner's own team alongside Stew, Doc/Gus, Liber and Mo. It carries method only; facts
  stay in the projects (ADR-112).
- **Abilities:**
  - read and summarise project activity (board, project history, agent and team messages,
    machine and route health);
  - plan and prioritise;
  - **kick off work** (start a team or lane on a card; approve pending steps);
  - record notes as project history (`kind: note`);
  - delegate: board writes go through Stew, docs through Doc/Gus, memory through Liber, and
    purpose checks to Mo.
- **Models:** routed per task through WeftOS model routing: local llm-lab models for quick
  turns, frontier models for planning and reasoning. The status area shows the model in use.
- **Safety:** the agent asks before anything outward-facing, destructive or production-touching.
  It never handles the owner's credentials. Every tool call is recorded.
- **Conversation API:** the console host talks to the agent through a small API, in the lane
  contract: user utterances in; state, tokens, messages, tool activity and page documents out.

### 4. Where it runs

The console host app runs on a WeftOS node and draws on the pad through the display endpoint.
The same app opens a desktop window on the Mac (`weftos-leaf-canvas` / `weftos-leaf-sim`), so
it's usable before the pad joins the mesh. v1 runs on the Mac, where the microphone and local
models are. Moving the agent to PG is a later step.

## Phases

| Phase | Delivers | Done when |
|---|---|---|
| C1 | Display endpoint D1 (ADR-117): sessions, scene ops, object events, client library; on the simulator | the console scene renders in the simulator from a client session |
| C2 | Console screen: layout, theme, avatar states, status, scrollable chat log, page-mode stub | a recorded conversation replays correctly in the simulator and a Mac window |
| C3 | Chief-of-staff agent v1 with the conversation API, push-to-talk and Mac-mic STT | the owner holds the button, speaks, sees the transcript, and gets an answer that uses project data |
| C4 | On the pad: leaf join, the button on GPIO, display grant | the same conversation works on the pad |
| C5 | Page mode with rendered markdown and SVG; optional pad mic and spoken replies | the agent shows a full-page plan on request |

## Open questions for the owner

1. **Microphone:** v1 uses the Mac mic. Should a mic be wired to the pad?
2. **Spoken replies:** text only for now, or speak through a speaker on the pad or the Mac?
3. **Where it runs:** the Mac for v1, then PG?
