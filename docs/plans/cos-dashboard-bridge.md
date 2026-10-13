# Chief of staff on the hosted dashboard: bridge contract

Status: Accepted, 2026-10-13. Owner request (via Grok): a tabbed `/phone` page
with **Phone**, **Mentra** and **Weaver** tabs. The hosted dashboard cannot
reach the Mac's cos.v1 API and must never hold an endpoint bearer, so the Mac
runs a bridge that relays through the dashboard with its existing node
credential.

```
phone page ──insert request──▶ Supabase ◀──POST /api/nodes/cos/sync── weftos-cos bridge (Mac)
           ◀─read turns/status─         ──pending requests──▶           │ loopback, no token
                                                                        ▼
                                                              cos.v1 127.0.0.1:18300
```

## Sync route (dashboard)

`POST /api/nodes/cos/sync`, authenticated exactly like `POST /api/nodes/heartbeat`
(node bearer token). The body is at most 256 KB.

Request:

```json
{
  "turns":   [{"turn_id": "…", "role": "user|assistant", "endpoint": "mentra", "text": "…", "ts": "RFC3339"}],
  "status":  {"agent": {…}, "mentra": {"listen": {…}, "speak": {…}}, "updated_at": "RFC3339"},
  "results": [{"id": "<request uuid>", "status": "accepted|failed", "turn_id": "…", "error": "…"}]
}
```

- `turns`: final messages from `GET /v1/cos/turns`, newest last, at most 50
  per call. The route upserts them on `(node_id, turn_id, role)` and keeps the
  newest 500 per node.
- `status`: replaces the node's status row. `agent` holds whatever cos.v1
  exposes: state (idle, thinking, speaking), model, current task, and healthy.
  `mentra.listen` and `mentra.speak` hold the two status files below, verbatim
  apart from size limits.
- `results`: outcomes of requests returned by an earlier call.

Response:

```json
{"requests": [{"id": "uuid", "endpoint": "mentra|dashboard", "text": "…", "requested_by": "uuid", "created_at": "RFC3339"}]}
```

- The route returns up to 10 queued requests for this node and marks them
  `claimed` atomically. A request claimed but not resolved within 2 minutes
  goes back to `queued`, at most 3 times, and then `failed`.

## Request from the page

An RPC `request_cos_turn(p_node uuid, p_endpoint text, p_text text)`:

- callable only by a signed-in member of the node's workspace;
- `p_endpoint` must be `mentra` or `dashboard`;
- `p_text` is 1–2000 characters, after trimming;
- at most 30 requests per user per minute.

The page never talks to the Mac directly and never handles a token.

## Bridge (Mac)

`weftos-cos bridge`. It reads the dashboard URL and node token file the same way
the weaver dashboard reporter does (`[dashboard]` in `~/.weftos/weave.toml`, the
token file path). It syncs every 2 s, backing off to 30 s on errors. For each
request it posts `{"text", "source": "typed", "endpoint"}` to the local cos.v1
`POST /v1/cos/utterance` over loopback, so no bearer is needed. It never calls
`/v1/cos/ptt`.

- Flags: `--agent` (default `http://127.0.0.1:18300`), `--interval-ms` (2000),
  `--once`, `--dry-run`. Start it with `scripts/build.sh cos-bridge`.
- Turns already delivered are tracked by `(turn_id, role)`; a sync that fails
  re-sends them. Turn text is cut at 2000 characters and each status file is
  limited to 32 KB (larger: `{"error": …}`), so a sync stays under the body limit.
  A missing status file is `null`; an unreadable or corrupt one is `{"error": …}`.
- `status.agent` is the body of the agent's read-only `GET /v1/cos/status`
  (`state`, `reason`, `task`, `model`, `lanes_running`) plus `healthy` from
  `/healthz`; with the agent down it is just `{"healthy": false}`.
- A request with an unknown endpoint or empty/over-long text is not sent to the
  agent and is reported `failed`.
- `speak` follows the endpoint's own config. `mentra` replies are spoken on
  the glasses by the Mac speak watcher. `dashboard` turns are never spoken.
- The phone page does no text-to-speech.

## Glasses status files (written by the Mac listen/speak scripts)

Each file is written atomically: temp file, then rename. Staleness: the page
shows "not running" when `updated_at` is older than 30 s.

`~/.weftos/run/mentra/listen.status.json`

```json
{"armed": true, "pid": 1, "updated_at": "…", "floor_rms": 150, "voiced_min": 0.15,
 "last_take": {"name": "…", "at": "…", "rms": 0, "voiced": 0.0, "outcome": "skipped|posted|empty|error",
               "reason": "…", "transcript": "…", "turn_id": "…"},
 "recent": [/* last 20 takes, same shape */]}
```

`~/.weftos/run/mentra/speak.status.json`

```json
{"up": true, "pid": 1, "updated_at": "…",
 "last_play": {"turn_id": "…", "at": "…", "chars": 0, "ok": true, "error": null},
 "recent": [/* last 20 plays */]}
```

The listen file is rewritten after every take. The speak file is rewritten
after every play and at least every 15 s.

## Addendum: push-to-talk and the listening toggle

The owner made push-to-talk and a listening on/off toggle must-haves on both
tabs. Neither uses `POST /v1/cos/ptt`, which captures the Mac microphone.

### Request kinds

`cos_requests` gains `kind` and `payload jsonb`. The sync and pending
responses carry `{"id", "kind", "endpoint", "text", "payload", ...}`:

| kind | payload | from | bridge does |
|---|---|---|---|
| `utterance` | — (`text`, `endpoint`) | typed line, either tab | POST `/v1/cos/utterance` |
| `listen` | `{"on": bool}` | the toggle (one state for both tabs) | writes `listening` in the control file |
| `ptt_mentra` | `{"hold_id": uuid, "phase": "down"\|"up"}` | Mentra hold button | writes `ptt` in the control file; the glasses loop captures that hold and posts endpoint `mentra` |
| `audio` | `{"hold_id": uuid, "bytes": n, "ms": n}` | Weaver hold button (phone mic) | downloads the WAV, transcribes on the Mac, posts endpoint `dashboard`, `source: "voice"` |

The RPC splits into `request_cos_turn(p_node, p_endpoint, p_text)` and
`request_cos_control(p_node, p_kind, p_payload)` (kinds `listen` and
`ptt_mentra`). Each has the same membership check and validates its payload.
The rate limit for controls is 120 per user per minute.

### Low-latency delivery

`GET /api/nodes/cos/pending?wait=25` uses the same node auth. It holds for up to
`wait` seconds (cap 25) and returns as soon as any request is queued for the
node, with the same claim semantics and response shape as sync. The bridge
keeps one long-poll open at all times and still syncs turns and status every
2 s. A press reaches the Mac in well under a second.

### Phone audio (Weaver push-to-talk)

- The page records with WebAudio and encodes 16 kHz mono PCM16 WAV
  client-side, up to 15 s (≈ 480 KB).
- It uploads to a private Supabase Storage bucket `cos-audio` at
  `<workspace>/<node>/<hold_id>.wav`. Members of the workspace can insert but
  cannot read others' files.
- It then calls `request_cos_audio(p_node, p_hold_id, p_bytes, p_ms)`.
- The bridge fetches the bytes with `GET /api/nodes/cos/audio/<request id>`
  (node auth, streams the object), transcribes in-process (the `onnx` build),
  posts the text as endpoint `dashboard` with `source: "voice"`, and reports
  the transcript in `results[].text`.
- The route deletes the object once the result is reported. Unclaimed audio
  older than 10 minutes is deleted.

### Mac control file (bridge writes, glasses listen loop reads)

`~/.weftos/run/mentra/control.json` is written atomically (temp file, then
rename). The loop polls it every 200 ms:

```json
{"listening": true, "updated_at": "RFC3339",
 "ptt": {"hold_id": "uuid", "phase": "down|up", "at": "RFC3339"} }
```

- `listening: false`: the loop starts no continuous RECORD.
- `ptt.phase` down: the loop pauses continuous takes, starts one capture
  (cap 15 s), and stops it on up for that `hold_id` or at the cap. It then
  pulls the take, transcribes it and posts it as endpoint `mentra`. The voiced
  gate is relaxed for a deliberate hold.
- The file missing means `listening: true` and no hold, which matches today.
- The loop reports `listening` and the current `ptt` in listen.status.json, so
  the page shows the live state.
