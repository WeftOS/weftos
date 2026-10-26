# Cog topic schemas

Registered schema ids for `[[outputs]]` and `[[inputs]]` in a cog's `cog.toml`
(Ventuno cog plan, section 4.2). The id list and typed messages live in
`clawft-types` (`cog_topics`); `weaver workload pack` rejects a declaration that
names an id not listed here.

| Path | What it is |
|------|------------|
| `vectors/<schema>/valid-*.json` | Messages a validator must accept. Each must also round-trip through the typed message unchanged |
| `vectors/<schema>/invalid-*.json` | Messages a validator must reject |

`spatial.evidence.v1` is defined by [`contracts/sensors`](../sensors/README.md)
(ADR-111) and has no vectors here.

## Common fields

Every message is one JSON object carrying these, beside the schema's own fields.
Unknown fields are ignored, so new optional fields stay in `v1`; changing the
meaning of a field needs `v2`.

| Field | Type | Rule |
|-------|------|------|
| `source_id` | string | The installed sensor, `[a-z0-9][a-z0-9._-]*`, at most 64 chars. Stable if the part moves node |
| `node_id` | string | `hex(SHA-256(pubkey)[..16])`, 32 lower-case hex chars |
| `t_ns` | u64 | Capture clock, nanoseconds. Values above 2^53 lose precision in JavaScript consumers |
| `seq` | u64 | Per-source counter |

Floats are JSON numbers read as 64-bit. Where a table says "f32", the producer
holds that precision; the JSON is not rounded.

## Schemas

Size classes: **reading** (at most 4 KB, one broker message), **stream** (sustained
ordered chunks, each chunk is one message with a header and base64 payload),
**blob** (bytes in the content store, the message is a handle).

### `senml.v1` (reading)

A SenML pack inside the cog envelope. Units and ranges per name come from
`contracts/sensors/quantities.v1.json`.

| Field | Type | Rule |
|-------|------|------|
| `pack` | array | 1..=64 records |
| `pack[].n` | string | name, 1..=64 chars |
| `pack[].u` | string | optional unit |
| `pack[].v` / `vs` / `vb` | number / string / bool | exactly one per record |

### `spatial.evidence.v1`

Registered, defined in `contracts/sensors`. See ADR-111.

### `pcm.s16le.v1` (stream)

| Field | Type | Rule |
|-------|------|------|
| `rate_hz` | u32 | 8000..=192000 |
| `channels` | u8 | 1..=8, interleaved |
| `frames` | u32 | samples per channel, 1..=1048576 |
| `data_b64` | string | standard padded base64 of `frames * channels * 2` bytes, signed 16-bit little-endian |

### `video.frame-ref.v1` (blob handle)

Raw frames stay on the node. The handle names bytes in its content store.

| Field | Type | Rule |
|-------|------|------|
| `hash` | string | BLAKE3 of the frame, 64 lower-case hex |
| `width`, `height` | u32 | 1..=16384 |
| `pixel_format` | string | `nv12`, `yuyv`, `rgb24`, `bgr24`, `gray8`, `mjpeg` |
| `ttl_ms` | u32 | 1..=3600000, how long the owner keeps the bytes |

### `detections.v1` (reading)

| Field | Type | Rule |
|-------|------|------|
| `frame_hash` | string | BLAKE3 hex of the `video.frame-ref.v1` frame |
| `model` | string | optional |
| `boxes` | array | at most 256 |
| `boxes[].label` | string | 1..=64 chars |
| `boxes[].score` | f64 | 0..=1 |
| `boxes[].bbox` | `[x, y, w, h]` | normalized to the frame, top-left origin; the box stays inside it |

### `can.frame.v1` (reading)

| Field | Type | Rule |
|-------|------|------|
| `id` | u32 | at most 0x7FF, or 0x1FFFFFFF when `ext` |
| `ext` | bool | 29-bit id |
| `fd` | bool | CAN FD frame |
| `dlc` | u8 | 0..=8 classic; 0..=15 FD (9..=15 map to 12, 16, 20, 24, 32, 48, 64 bytes) |
| `data` | string | lower-case hex, exactly the length `dlc` implies |

### `iq.cs16.v1` (stream)

| Field | Type | Rule |
|-------|------|------|
| `center_hz` | u64 | tuned frequency |
| `sample_rate_hz` | u32 | complex samples per second |
| `samples` | u32 | complex samples in the chunk, 1..=4194304 |
| `data_b64` | string | base64 of `samples * 4` bytes: I then Q, signed 16-bit little-endian |

Optional `v1` extension fields (readers ignore what they do not know; a producer that tracks loss sets them):

| Field | Type | Meaning |
|-------|------|---------|
| `bandwidth_hz` | u32 | analog bandwidth |
| `segment` | u32 | counts dwells; a new segment starts at every retune |
| `sample_index` | u64 | index of the first sample on the segment's sample clock; lost samples are counted in it, so a gap is a jump |
| `lost` | u32 | samples lost immediately before this chunk |
| `discontinuity`, `band_changed`, `all_zero` | bool | present and true when set: samples were lost just before; the tuning changed; every sample is zero (a dead receive path) |
| `gap_samples` | u64 | gap records only |

A **gap record** is a message with `samples` 0, an empty `data_b64` and `gap_samples` set to the number lost. It is not a sample chunk, so the validator rejects it (`is_gap_record` recognises it) and a reader that validates chunks skips it; the next chunk's `lost` carries the same number.

### `spectrum.psd.v1` (reading)

| Field | Type | Rule |
|-------|------|------|
| `center_hz` | u64 | centre of the sweep |
| `span_hz` | u64 | total span the bins cover, above 0 |
| `bins` | array of f32 dB | 1..=4096 values, each -300..=100, low frequency first |

### `transcript.v1` (reading)

| Field | Type | Rule |
|-------|------|------|
| `text` | string | 1..=4096 chars |
| `lang` | string | `en`, `en-US` style, at most 16 chars |
| `start_ns`, `end_ns` | u64 | capture clock; `end_ns >= start_ns` |
| `confidence` | f64 | 0..=1 |

### `telemetry.v1` (reading)

| Field | Type | Rule |
|-------|------|------|
| `items` | array | 1..=64 |
| `items[].name` | string | `[a-z][a-z0-9_.]*`, at most 64 chars |
| `items[].value` | f64 | finite |
| `items[].unit` | string | at most 16 chars, empty for a bare count |

### `store.vector.v1` (reading)

Feature vectors for the placing project's store (ADR-122 section 9). The body is
the ingest bridge's `POST /api/v1/store/ingest` body; the broker's built-in store
sink takes every `store.vector/*` topic and writes it to the project that placed
the publishing cog, never another project's store.

| Field | Type | Rule |
|-------|------|------|
| `vectors` | array | 1..=256 `[id, [8 values]]` pairs |
| `vectors[][0]` | u64 | caller-chosen id, unique per instance |
| `vectors[][1]` | 8 x f32 | finite, within the f32 range |
| `dedup` | bool | optional, default false: skip ids or bit-identical values the instance already stored |
| `metrics` | object | optional, at most 32 `name: number` entries (`[a-z][a-z0-9_.]*`), carried to subscribers, not stored |

### `infer.v1` (service call request)

The body of a call on `svc.infer/<node>` (ADR-122 section 12). It carries no common
fields: the broker stamps the caller, call id and clock on the call. The reply body is
the service's own (the NPU service returns `text`, `usage` and timings).

| Field | Type | Rule |
|-------|------|------|
| `model` | string | a model id of the service's catalog, 1..=128 chars |
| `priority` | u8 | 0 (background) ..= 9 (interactive), default 4 |
| `text` | string | the prompt, at most 32 KiB; one of `text`, `image_b64` or `image_blob` is required |
| `system` | string | optional system prompt, at most 32 KiB |
| `image_b64`, `image_mime` | string | one small inline image (at most 40 K chars of base64), `image/png` or `image/jpeg`; both or neither |
| `image_blob` | `blob.v1` | optional, instead of `image_b64`: a PNG or JPEG of at most 4 MiB held by the broker (ADR-122 section 13). The handle travelling in the call lets the service read it |
| `max_tokens` | u32 | optional, 1..=4096 |

### `blob.v1` (handle)

A handle to bytes in the node's content store (ADR-122 section 3, B5). It is a reading body
on its own, or nested anywhere in a reading, a service call or a service reply: a payload
over the 64 KiB body limit travels as a handle and the receiver fetches the bytes from the
broker (`blob_get`). It carries no common fields.

| Field | Type | Rule |
|-------|------|------|
| `hash` | string | BLAKE3 of the bytes, 64 lower-case hex |
| `size` | u64 | at least 1 |
| `media_type` | string | `type/subtype`, lower-case, e.g. `image/png` |
| `expires_ms` | u64 | Unix milliseconds after which the store may drop the bytes |

## Using it

- **Producers**: emit messages that pass every `valid-*` vector's rules and fail the
  `invalid-*` ones in your own tests, so no hardware is needed.
- **Adding a schema**: add the id to `SCHEMA_IDS`, a typed message with `validate()`,
  a section above and vectors, in one commit.

### `sdr.v1` (service call on `svc.sdr/<node>`)

Request body, no envelope (the broker stamps caller, call id and clock): `{"method": "...", ...fields}`. A band is `centre_hz` 70 MHz..=6 GHz, `rate_sps` 520833..=61440000, `bw_hz` 200 kHz..=56 MHz, `gain_db` a number or `"auto"`.

| `method` | Fields | Effect |
|----------|--------|--------|
| `tune` | `centre_hz`; `rate_sps`, `bw_hz`, `gain_db`, `dwell_ms` optional | the caller's own single-band plan; replaces its previous `tune` |
| `set_rate`, `set_bw`, `set_gain` | `rate_sps` / `bw_hz` / `gain_db` | change that field of the caller's `tune` band |
| `plan` | `id` (1..=48 chars; `default`, `tune:*`, `lease:*` reserved), `bands` (1..=32 of `centre_hz`, `rate_sps`, `dwell_ms`, optional `bw_hz`, `gain_db`) | a band plan that takes turns with the others |
| `plan_remove` | `id` | remove one of the caller's plans |
| `lease` | `centre_hz`, `secs` 1..=3600; `rate_sps`, `bw_hz`, `gain_db` optional | exclusive use of the tuner; other callers' tuning is refused |
| `release` | none | give the lease back |
| `status` | none | the service's status document (free-form JSON) |
| `tx_config` | `attenuation_db` (at most 0), `centre_hz`, `rate_sps`, all optional | transmit setup; a receive-only service always refuses it |

Reply body on success: `{"ok": true}` plus, by method, `plan` (string), `bands` (u32), `lease` (u64), `secs` (u64) and `uncalibrated` (bool, `tune`: the band is outside the part's rated range). `status` replies with the status document. A failed call fails with a message instead of an `ok: false` body. Reply vectors are the `reply-valid-*.json` files.
