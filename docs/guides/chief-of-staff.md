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
  "endpoints": {"mentra": {"speak": true}},
  "mcp_servers": []
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

## Tools

Read: `projects.activity`, `board.read`, `card.current`, `nodes.health`, `mesh.routes`.
Keep: `notes.record` (project history, the card, and the brain), `board.propose` (a staged
manifest for the steward). Change shared state, so each waits for the owner's "yes":
`board.claim`, `board.note`, `work.kickoff`. Screen: the `ui.*` tools. Anything else comes from
the configured MCP servers (below), gated as configured.

## Adding capabilities with MCP servers

New abilities (flights, travel, code hosting, mail, a project's own tools) come from
configuring MCP servers, not from new Rust. Each `mcp_servers` entry in `cos.json` is one
server; its allow-listed tools appear to the model as `<id>.<tool>` (on the wire
`<id>_<tool>`) with the server's own JSON schema. The same shape is what the planned cloud
chief of staff keeps per user.

```json
{
  "mcp_servers": [
    {
      "id": "local",
      "transport": {"type": "stdio", "command": "target/debug/weftos-cos-mcp-echo"},
      "allow": ["echo", "time.*"],
      "gate": "open",
      "use_for": "checking the time"
    }
  ]
}
```

That server is the one to try first: `weftos-cos-mcp-echo` is built with the crate, needs no
account, and offers `echo`, `time.now`, `note.append` and `big`. Allow `note.append` too
(leave its gate at the default) to watch a write wait for your "yes".

Fields:

- `id`: lowercase letters, digits, `-`, `_`. The tool namespace.
- `transport`: `{"type": "stdio", "command", "args", "env"}` or `{"type": "http", "url"}`
  (streamable HTTP; answers as JSON or SSE). `env` lists variable **names** to pass to a
  stdio server; the values come from the environment, never from this file.
- `auth` (http only): `{"bearer_env": "NAME"}` or `{"bearer_file": "/path"}`, a variable
  name or a mode-0600 file. A token is never inline, never logged and scrubbed from
  errors and results. A bearer is refused over plain `http://` except to localhost.
- `allow`: tool names or `*` globs to expose. **Empty (the default) exposes nothing**, so a
  new server does nothing until you pick its tools.
- `gate`: `confirm` (the default; the call waits for the owner's "yes") or `open`
  (read-only tools). `tool_gates` overrides per tool or glob; an exact name beats a glob.
- `use_for`: one sentence telling the model when to use the server.
- `connect_timeout_secs` (8), `call_timeout_secs` (30), `max_output_bytes` (8000, longer
  results are cut with a "+N more bytes" note), `retry_secs` (30).

Behaviour: the host connects at start and lists tools. A server that is down is reported
(`not bound: mcp:<id>: ...` at start, "unavailable" in the model's notes) and retried at
turn starts and whenever one of its tools is called; it never stops the host. The model sees
at most `mcp_max_tools_per_turn` (default 12) MCP tools per turn, chosen by keyword match
with your request when there are more; every allowed tool stays callable. Prefer narrow
`allow` lists: a long tool list hurts the local model.

### Provided servers (the catalog)

A publisher such as WeaveLogic can ship a catalog of vetted servers. You switch entries on
by id instead of writing them out:

```json
{
  "mcp_catalog": {"path": "~/.config/weftos/cos/mcp-catalog.json", "pinned_key": null, "require_signature": false},
  "mcp_enable": [{"id": "travel", "allow": ["search_*"], "confirm": [], "auth": {"bearer_env": "MY_TRAVEL_TOKEN"}}]
}
```

A catalog entry carries `id`, `name`, `publisher`, `authorized`, `transport`, `auth`
(`{"kind": "none"}`, `{"kind": "bearer_env", "env": "NAME"}`; `oauth` is recognised but cannot
be enabled yet), a default `allow` list, `gate`, `tool_gates` and `use_for`. In `mcp_enable`,
`allow` can only **narrow** the catalog's allow-list (a tool must match both), `confirm`
forces tools to wait for your yes, and `auth` points the credential at your own variable or
file. Nothing in `mcp_enable` can widen an allow-list or open a gate.

Trust: servers you write in `mcp_servers` are **unvetted** and default to `confirm`; catalog
servers are reported as `authorized by <publisher>` (or "not marked authorized") at start,
with `signature verified` or `unsigned`. The origin is set by the host and cannot be claimed
in `cos.json`. If the catalog has a `signature` (Ed25519, hex, over `weftos-mcp-catalog-v1\n`
plus the document's canonical JSON without the signature), it is checked against
`mcp_catalog.pinned_key` (or the host's built-in key, empty until the publisher issues one);
a signature that does not verify, or a signed catalog with no key to check it, refuses the
whole catalog. Unsigned catalogs load unless `require_signature` is true, so turn that on
once your publisher signs.

Templates (not active; fill in the real URL, command and variable names, and keep the
values in the environment or a 0600 file):

```json
{"id": "travel", "transport": {"type": "http", "url": "https://<travel-mcp-host>/mcp"},
 "auth": {"bearer_env": "TRAVEL_MCP_TOKEN"}, "allow": ["search_*"], "gate": "open",
 "tool_gates": {"book_*": "confirm"}, "use_for": "flights, hotels and trip plans"}

{"id": "github", "transport": {"type": "stdio", "command": "<github-mcp-server>", "args": ["stdio"], "env": ["GITHUB_TOKEN"]},
 "allow": ["get_*", "list_*", "search_*", "add_issue_comment"], "gate": "open",
 "tool_gates": {"add_*": "confirm"}, "use_for": "issues, pull requests and code in our repositories"}
```

To add a flights server: get an MCP endpoint and an API token from the flights provider,
store the token in a 0600 file or an environment variable the host runs with, add an entry
like the `travel` template with `allow` limited to the search tools, restart `weftos-cos
serve`, and check the start report for `mcp:travel`.

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
