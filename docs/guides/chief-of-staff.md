# Chief of staff: running it for real

The chief of staff (`weftos-cos`) is the agent behind the console. It answers from
live data (the board, project history, the weaver daemon), speaks replies, keeps
notes in an installation brain, and exposes the frozen conversation API
(`docs/plans/adr-118-cos-contract.md` section 3, `cos.v1`) to the console window,
the pad and the glasses bridge.

## Start it (one command)

```bash
scripts/build.sh cos-up
```

This builds `weftos-cos` with push-to-talk and local speech-to-text (`mic,onnx`),
starts the local model server if it is down (detached, log in
`~/.config/weftos/cos/model.log`), loads the model, then serves on
`127.0.0.1:18300`. Ctrl-C stops the agent and the model server it started. The
console window is a second terminal:

```bash
scripts/build.sh cos-host
crates/weftos-cos-console/target/release/cos-console-host --window   # hold space to talk
```

Pieces, if you want them separately:

```bash
target/debug/weftos-cos model status      # local server up or down, cloud keys present, brain, unbound tools
target/debug/weftos-cos model start       # start + warm the local model (detached)
target/debug/weftos-cos model stop        # stop the one this host started (by its recorded pid)
target/debug/weftos-cos serve --mic       # without --start-model the model must already be up
target/debug/weftos-cos transcribe x.wav  # check local STT without a microphone
```

## Models

| Role | Default | Why |
|---|---|---|
| quick turns | `local/mlx-community/GLM-4.7-Flash-4bit`, served by `~/llm/bin/serve` on `:8081` | right tool call every time against the full tool set; about 3 s for a tool round on this Mac |
| planning turns | the keyed cloud provider's frontier model (Anthropic Sonnet, else OpenRouter), else the quick model | planning cues or complexity at or above 0.3 |
| fallback chain | the fast model of every keyed cloud provider, in order | a dead or unloaded local server, an account out of credit |

The status line always shows the model that is answering; a retry on the fallback sends a
new `status` whose task ends `(fallback)`. A key being present is not proof it works: the
chain exists so an account out of credit does not end the turn. Qwen3-4B
(`local/mlx-community/Qwen3-4B-Instruct-2507-4bit`) is faster but named tools in prose about
half the time against the full tool set; use it for plain chat only.

The local server is the llm lab's (`~/llm`, the controller for local models on this Mac). The
first request after the server starts loads the weights (about 45 s); `model start` does that
for you so the first spoken turn is not the slow one.

## Config (`~/.config/weftos/cos.json`, every field optional)

```json
{
  "models": {"quick": "local/mlx-community/GLM-4.7-Flash-4bit", "plan": null, "fallback": null,
             "local_api_base": "http://127.0.0.1:8081/v1", "serve_command": null},
  "harness": {"project_id": "<the dashboard project UUID>"},
  "voice": {"tts": {"engine": "auto", "voice": null, "rate": 200}, "device": null},
  "brain": {"installation": "weftos://local/installations/<id>"}
}
```

- `fallback: []` turns the cloud fallback off. `serve_command` replaces the llm lab command.
- Credentials: the harness token is the 0600 file the dashboard harness uses
  (`~/.config/weftos/board-token`, or `harness.token_file`). The agent never prints it, and
  events and tool logs are redacted.

## Voice

- Push to talk: hold the button (space in the console window, `POST /v1/cos/ptt`). Capture is
  the Mac's default input; speech-to-text is local Parakeet (`~/.weftos/models/parakeet`).
  Raw audio is dropped after transcription.
- Spoken replies: the final message of a voice turn is read by `say` (the default on macOS).
  `voice.tts.engine: "command"` takes any argv that reads the text on stdin and plays it,
  which is the seam for Kokoro, Pocket TTS or Orpheus once a wrapper script exists. A new
  button press cuts the reply off. `"none"` is silent (the glasses speak for themselves).

## Tools

Read: `projects.activity`, `board.read`, `card.current`, `nodes.health`, `mesh.routes`.
Keep: `notes.record` (project history, the card, and the brain), `board.propose` (a staged
manifest for the steward). Change shared state, so each waits for the owner's "yes":
`board.claim`, `board.note`, `work.kickoff`. Screen: the `ui.*` tools.

## Brain and notes (ADR-121 decision 9)

`notes.record` also writes the installation's RVF brain
(`~/.weftos/brains/chief-of-staff/<installation>.rvf`). Each entry carries
`agents/schemas/brain-metadata.schema.json` metadata: `kind`, `summary`, `why`, `where`, the
installation, the package digest (read from `agents/catalog.json`), author, time, `scope:
installation`. A note the schema refuses (no reason, an absolute machine path) is refused
before anything is written anywhere.

## Checks

```bash
scripts/build.sh test weftos-cos          # unit + API + contract + the recorded end-to-end session
WEFTOS_COS_LIVE=1 cargo test -p weftos-cos --test live_tools live_smoke   # read-only, real board
COS_UPDATE_GOLDEN=1 cargo test -p weftos-cos --test e2e_session           # after an intended change
```
