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
  "brain": {"installation": "weftos://local/installations/<id>"},
  "serve": {"bind": null},
  "endpoints": {"mentra": {"speak": true}}
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
  button press cuts the reply off. `"none"` silences the Mac (every endpoint's replies read
  by the host process), so for the glasses use an endpoint instead (next section).

## Remote front ends (the glasses, a second device)

Each front end is an endpoint with its own id; the Mac's own microphone, button and console are
`host`. A remote front end does its own speech-to-text and speaks its own replies:

1. Connect to `GET /v1/cos/events` first, then `POST /v1/cos/utterance` with
   `{"text": ..., "source": "voice", "endpoint": "mentra"}`.
2. The final `message` of that turn comes back with `"endpoint": "mentra"` and, when the
   endpoint speaks, `"speak": true`. The Mac does not read it aloud; the Mac still reads the
   `host` turns. `ptt` is the Mac's microphone and is not available to remote endpoints.
3. To keep one endpoint quiet without touching the Mac or another endpoint, give it
   `"endpoints": {"mentra": {"speak": false}}` in `cos.json`; its replies then
   arrive without `speak`. (The older `tts.engine: "none"` still works and means the same.)

Reaching the Mac from another machine, two ways:

- **Tailnet listener.** Put `"serve": {"bind": "tailnet"}` in `cos.json` (or
  `"tailnet:PORT"`). The host then also listens on its Tailscale IPv4 (found with
  `tailscale ip -4`, which must be in 100.64.0.0/10; anything else is refused, and it never
  listens on `0.0.0.0`). Make a token per endpoint with `weftos-cos endpoint add mentra`: it
  writes a 0600 file under `~/.config/weftos/cos/endpoints/` and prints only the path. The
  remote client sends `Authorization: Bearer <contents of that file>` on every request. The
  local console on `127.0.0.1:18300` needs no token. Delete the file to revoke; `weftos-cos
  endpoint list` shows who has one.
- **ADR-116 router.** Leave the API on loopback and let the router forward only
  `/v1/cos/utterance`, `/v1/cos/answer`, `/v1/cos/events` and `/healthz` from the allowed
  peers (never `/v1/cos/ptt`). The router owns who may connect; endpoint ids still come from
  the request body.

A remote endpoint sees the whole event stream, not just its own turns.

## Dashboard bridge (the phone page)

The hosted dashboard cannot reach the Mac's loopback API and holds no endpoint
bearer, so `weftos-cos bridge` relays through the dashboard with the node's
existing credential (contract: `docs/plans/cos-dashboard-bridge.md`). Every 2 s
it POSTs `/api/nodes/cos/sync` with the new final turns (at most 50 per call, each
sent once), a status snapshot (the agent's state, task and model from
`GET /v1/cos/status`, plus the glasses `listen.status.json` and `speak.status.json`
under `~/.weftos/run/mentra/`; a missing file is `null`, an unreadable one is
`{"error": ...}`) and the outcomes of earlier requests. Each queued request the
dashboard returns is posted to `POST /v1/cos/utterance` as a `typed` turn over
loopback, and reported `accepted` (with its `turn_id`) or `failed` (with the
error) in the next sync. It never calls `/v1/cos/ptt`.

It reads `url` and `token_file` from `[dashboard]` in `~/.weftos/weave.toml`, the
same section the weaver reporter uses (https, or http to loopback; the token file
must be a regular 0600 file). The token is read on each sync and never logged.

```bash
scripts/build.sh cos-bridge                     # weftos-cos bridge, from ~/.weftos/bin/weftos-cos
weftos-cos bridge --dry-run                     # print the sync body (text cut to 80 chars, no token), post nothing
weftos-cos bridge --once                        # a single sync, exit non-zero if it failed
weftos-cos bridge --agent http://127.0.0.1:18300 --interval-ms 2000
```

On a dashboard error it backs off to 30 s and logs one line per state change; a
401 or 403 logs "node token rejected". The agent must be running (`serve`); with
it down, turns are not sent and the status shows `healthy: false`.

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
