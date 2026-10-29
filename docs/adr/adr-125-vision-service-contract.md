# ADR-125: Vision service contract: `svc.vision.*` kinds and `vision.v1`

- **Status**: Proposed (draft, 2026-10-28). Not accepted until the owner says so. The owner
  asked for the build to start on it (2026-10-28), so cards that depend on it may build against
  this draft; a change on acceptance is a `vision.v1` edit before anything is released.
- **Date**: 2026-10-28
- **Deciders**: owner
- **Card**: `835ab524` (Vision 1), epic `30e898d7`
- **Builds on**: ADR-122 (cog data plane: service calls, section 12; blobs, section 13; mesh
  topics as exposures, section 14), ADR-101 (inference workload kind, `infer.<role>`),
  ADR-120 (exposures: announce, see, switch off, log), ADR-099 (governed placement, node
  facts), ADR-100 (cog workload kind), ADR-104 (sensor guides ship with cogs).
- **Relates-To**: `contracts/cogs/README.md` (schema registry), `infer.v1` and
  `detections.v1` there, the eikon media archivist (`~/llm/eikon`, not in this repo).
- **Prior decisions**: the decision-record store could not be consulted when this was
  written (2026-10-28, Ruflo unavailable), so earlier recorded decisions on this topic are
  unknown here. The ADRs above were read instead.

## Context

The eikon archivist on the owner's Mac reads images through Apple Vision (a Swift helper:
text recognition, image classification, barcodes, face rectangles, a feature print and its
distance) and through MLX models served on the Mac (Qwen3-VL, Molmo pointing, SAM 3.1
segmentation) with SigLIP-2 for zero-shot labels and embeddings. All of it is Mac-only and
none of it goes through WeftOS: another node cannot use it, and eikon cannot use the Venq's
NPU or CPU.

The owner wants these exposed through WeftOS (2026-10-28): a program asks its local daemon
for vision work, WeftOS picks a provider (Apple Vision on a Mac, the NPU or CPU cogs on the
Venq), places and governs it, and carries the call across the mesh.

The data plane already has what a vision call needs, node-local: `svc.*` service calls with a
deadline and per-caller limits (ADR-122 section 12), and blobs for anything over the 64 KiB
body cap (section 13). What is missing is a shared contract, so that a client written against
one provider works against another, and so that results from different providers are never
mixed where they cannot be compared.

## Decision

### 1. Kinds

A vision service is a cogbus service (ADR-122 section 12). Its topic kind names the task;
the source id names the provider instance, as for `svc.infer/<node>`:

| Kind | Task | First providers |
|---|---|---|
| `svc.vision.ocr` | text in the image, as lines with boxes | apple-vision (card 2), Venq `vision-ocr` (RapidOCR ONNX, card 3) |
| `svc.vision.codes` | barcodes and QR codes, decoded | apple-vision, Venq `vision-codes` (OpenCV) |
| `svc.vision.faces` | face count and boxes; **never identity** | apple-vision, Venq `vision-faces` (OpenCV YuNet) |
| `svc.vision.labels` | scores for labels; the caller's list (zero-shot) or the provider's own taxonomy | apple-vision (own taxonomy), Venq `vision-embed` / SigLIP (caller list) |
| `svc.vision.embed` | one image embedding of a named kind (section 4) | apple-vision (`apple-featureprint-r2`), Venq `vision-embed` (`siglip2-so400m-p16-384`) |
| `svc.vision.point` | points for a phrase | Mac `mlx-point` (Molmo, card 10) |
| `svc.vision.segment` | boxes, scores and masks for a text prompt | Mac `mlx-segment` (SAM 3.1, card 11) |

The kind is the task; the request repeats it in `task` so a body is self-describing in logs
and fixtures, and a provider refuses a body whose `task` does not match the kind it serves
(`bad_request`). One cog may serve several kinds (apple-vision serves five), each a separate
`serve` with its own exposure; or one kind (the Venq cogs are single-purpose).

**Not a vision kind.** Image chat (describe this image, answer a question about it) is
inference, not a vision task: it stays on `svc.infer` with `infer.v1` (`image_blob`) on the
node, and on the ADR-101 inference proxy as a role (`infer.vision`, card 5) for OpenAI-style
clients. A second schema for the same model call would split one queue in two. The Venq's VLM
(Qwen3-VL-4B under GenieX) has one owner, `npu-infer`; no vision cog opens GenieX.

Depth estimation is out of scope here (a later card); it would be `svc.vision.depth` with a
map blob in the reply, in the same shape as a segment mask.

### 2. `vision.v1`: one schema, request and reply

`vision.v1` is a registered schema id (`contracts/cogs`). Providers list each kind they serve
in `[[outputs]]` (class `reading`, schema `vision.v1`) and callers list it in `[[inputs]]`,
as ADR-122 section 12 requires. The request and the reply are both `vision.v1`; the reply
vectors are the `reply-valid-*` / `reply-invalid-*` files, as for `sdr.v1`. Like `infer.v1`,
neither carries the common envelope: the broker stamps caller, call id and clock.

**Request** (every task):

| Field | Type | Rule |
|---|---|---|
| `task` | string | `ocr`, `codes`, `faces`, `labels`, `embed`, `point` or `segment`; must match the kind |
| `image` | `blob.v1` | the image, held by the broker (ADR-122 section 13). Exactly one of `image` and `image_b64` |
| `image_b64`, `image_mime` | string | a small inline image, at most 40 K base64 characters (so the body stays under the 64 KiB reading cap with room for the rest); both or neither |
| `max_results` | u32 | optional, 1..=256: most lines, codes, faces, labels, points or instances returned |
| `min_score` | f64 | optional, 0..=1: drop results scored below it |

The image media type, in `image.media_type` or `image_mime`, is one of `image/png`,
`image/jpeg`, `image/webp`, `image/heic`, `image/tiff`, `image/bmp`. A blob image is at most
32 MiB. A provider that cannot decode a listed type answers `service_error` naming it
(`unsupported_media`), so the resolver (card 7) can try another; HEIC on the Venq is the
expected case.

**Per task** (only these fields, beside the common ones):

| Task | Field | Type | Rule |
|---|---|---|---|
| `ocr` | `languages` | array of string | optional, at most 8 BCP-47 tags (`en`, `en-US`), each at most 16 chars; the provider's default when absent |
| `labels` | `labels` | array of string | optional, 1..=64 entries, each 1..=64 chars: score these (zero-shot). Absent: the provider's own taxonomy, and the reply says which (`taxonomy`) |
| `embed` | `embedding_kind` | string | optional, a registered kind (section 4): the kind the caller needs. A provider that does not produce it refuses (`service_error`, `wrong_embedding_kind`); it never substitutes another |
| `point` | `phrase` | string | required, 1..=256 chars |
| `segment` | `prompt` | string | required, 1..=256 chars |
| `segment` | `mask_format` | string | optional, `png` (default) or `rle` |

A field that belongs to another task is refused, so a caller that misspells the task does not
get a silently different call.

**Reply** (every task):

| Field | Type | Rule |
|---|---|---|
| `task` | string | as the request |
| `provider` | object | `{model, version}`: the model or framework that answered (`apple-vision`, `rapidocr-onnx`) and its version, each 1..=128 chars |
| `width`, `height` | u32 | 1..=65535: the decoded image size the provider read, so a caller can check boxes against it |
| `took_ms` | u32 | time the provider spent, queueing excluded |

Geometry is normalised to the image, origin top-left, as `detections.v1`: a box is
`[x, y, w, h]` with every value in 0..=1 and `x + w <= 1`, `y + h <= 1`; a point is `[x, y]`
in 0..=1. A provider whose framework uses a bottom-left origin (Apple Vision) flips `y` before
replying.

| Task | Field | Type | Rule |
|---|---|---|---|
| `ocr` | `lines` | array | at most 1024 of `{text (1..=1024 chars), confidence (0..=1), bbox}` |
| `codes` | `codes` | array | at most 256 of `{symbology, payload (at most 4096 chars), bbox}`; `symbology` is `[a-z0-9_-]`, 1..=32 chars (`qr`, `ean13`, `code128`, `datamatrix`, `pdf417`, `aztec`) |
| `faces` | `count` | u32 | the number of faces found, at least the number of `boxes` (`max_results` may cut the boxes) |
| `faces` | `boxes` | array | at most 256 of `{bbox, score (0..=1)}`. **No identity, no landmarks, no attributes, no embedding of a face.** A field that could identify a person is not in `v1`, and a provider must not add one under an unknown key |
| `labels` | `labels` | array | at most 256 of `{label (1..=64 chars), score (0..=1)}`, highest first |
| `labels` | `taxonomy` | string | present when the request named no labels: the provider's taxonomy (`apple-vision-classify-r1`), 1..=64 chars |
| `embed` | `embedding` | object | section 4 |
| `point` | `phrase` | string | the phrase answered |
| `point` | `points` | array | at most 256 `[x, y]` |
| `point` | `raw` | string | the model's own text, at most 4096 chars, for audit |
| `segment` | `instances` | array | at most 256 of `{bbox, score, mask}`; `mask` is a `blob.v1` (`image/png` for `png`, `application/x-weftos-rle` for `rle`) of the mask at the image size |

Masks are blobs because a 1-bit mask of a phone photo is larger than the whole reply cap.
The reply hands the caller read access to each mask by carrying its handle (ADR-122 section
13); across the mesh the caller fetches only the handles its reply carried (card 4).
`application/x-weftos-rle` is row-major, run lengths of alternating 0 and 1 starting with 0,
as unsigned LEB128 varints, preceded by `width` and `height` as varints.

### 3. Limits and deadlines

- **Deadline.** The call's own `timeout_ms` (ADR-122 section 12) is the deadline; `vision.v1`
  does not repeat it. Providers that load a model on first call (Molmo, SAM) count the load
  against it, and a caller that may hit a cold model gives a longer timeout (the cog guide
  states the cold time).
- **Caller caps.** ADR-122's per-caller and per-service pending limits hold (16 and 64). A
  provider with one model in memory (point, segment) runs one call at a time and queues the
  rest within those limits.
- **Pixel cap.** A provider reads the image header first and refuses a decoded size over
  64 megapixels or 16384 on a side (`service_error`, `image_too_large`) before it decodes the
  pixels, so a small, highly compressed file cannot exhaust memory. The 32 MiB blob cap
  bounds the bytes; this bounds the pixels.
- **Body caps.** Every request and reply is at most 64 KiB as JSON; the reply caps in section
  2 keep a typical reply under it. A provider that would exceed it truncates to `max_results`
  and, failing that, answers `service_error` (`too_many_results`).

### 4. Embedding kinds

An embedding is only comparable with another of the same kind: the same weights, the same
preprocessing, the same output layer. The reply's `embedding` object says which:

| Field | Type | Rule |
|---|---|---|
| `kind` | string | a registered embedding kind |
| `dim` | u32 | must equal the kind's dimension |
| `distance` | string | must equal the kind's distance: `cosine` or `euclidean` |
| `vector` | array of f32 | `dim` finite values, inline. Exactly one of `vector` and `vector_blob` |
| `vector_blob` | `blob.v1` | for a kind too large to inline: `application/x-weftos-f32le`, size `dim * 4` |

Registered kinds (`vision.v1` validates `dim` and `distance` against this table):

| Kind | What it is | Dim | Distance |
|---|---|---|---|
| `apple-featureprint-r2` | `VNGenerateImageFeaturePrintRequest` revision 2 (macOS 14+), the observation's float data in order | 768 | `euclidean` |
| `siglip2-so400m-p16-384` | `google/siglip2-so400m-patch16-384` image tower, pooled output, L2-normalised | 1152 | `cosine` |

The revision 2 dimension was measured on this Mac (2026-10-28: 768 `float` elements, 3072
bytes; revision 1 is 2048 and would be its own kind). A provider that runs the same weights
on another runtime (SigLIP-2 on CPU through ONNX on the Venq, on MPS through PyTorch on the
Mac) uses the same kind only if its vectors agree with the reference to cosine similarity at
least 0.99 on the conformance images the cog guide names; a quantised or otherwise different
model is a new kind. Adding a kind is a `vision.v1` change: a row here, a row in the
validator, and a vector.

Clients keep the kind with every stored vector. A distance between two kinds is undefined: a
client refuses it (eikon's `distances` raises) rather than returning a number, and a corpus
pins its kind, so the resolver (card 7) sends an embed call for a SigLIP corpus only to a
SigLIP provider, never to Apple.

### 5. What each piece reuses

| Need | Reused from | Change here |
|---|---|---|
| call, reply, deadline, pending limits | ADR-122 section 12 (`serve`, `call`, `reply`) | none |
| images and masks | ADR-122 section 13 (`blob.v1`, `put_blob`, `blob_get`, hand-on by travelling) | none |
| image chat | `svc.infer` with `infer.v1` (the `npu-infer` cog); ADR-101 proxy role for OpenAI clients | none; card 5 adds the role |
| across the mesh | ADR-122 section 14 exposures (`weaver expose`, local wins, activity log) | card 4 extends exposures from topics to `svc.*` |
| non-cog callers (eikon) | the daemon's loopback surfaces | card 6 |
| provider choice | ADR-099 node facts and placement | card 7: `apple_vision`, `mlx`, the Venq cogs' kinds |
| guides | ADR-104 | each vision cog ships `guide/` with measured timings |

### 6. Privacy

Vision calls see whatever is in the image. The contract keeps three lines:

- **Faces are counted and boxed, never identified.** `v1` has no field for identity, a face
  embedding or face landmarks, and section 2 forbids adding one under an unknown key. An
  identity service would be a separate ADR with its own governance.
- **Images stay where they are put.** An image is a blob on the caller's node; across the
  mesh it travels only to the provider of an exposed service the caller called (card 4), and
  the provider's reply is the only thing that comes back. Providers do not keep images or
  results after the reply, beyond the blob TTL of their reply's own masks.
- **Licences go with the model.** A provider records its model's licence in its `cog.toml`;
  gated weights (SAM 3.1) are fetched on the node by the operator and never packed into the
  signed artifact (`redistributable = false`, card 11).

## Consequences

- One client can call any provider of a kind, on the node or, after card 4, across the mesh,
  and check its result against the same fixtures.
- Apple feature prints and SigLIP vectors can no longer be mixed silently: every vector
  carries its kind, and the validator checks `dim` and `distance` against it.
- Adding a task (depth, captions as a non-chat task) is a new kind and a `vision.v1` change in
  one commit, as for any schema in the registry.
- A provider that cannot decode a media type is found by the resolver at call time
  (`unsupported_media`), not declared ahead of it. If that proves noisy, a later revision can
  add accepted media types to the service advert.
- The pixel cap pushes decoding of very large images (panoramas, scans) to the client, which
  must downscale before the call.

## Alternatives considered

- **One schema per kind** (`vision.ocr.v1`, ...). Seven ids, seven sets of vectors, and the
  shared fields (image, provider, size, geometry) repeated in each. One schema with a `task`
  discriminator keeps the shared rules in one place; a breaking change to one task would
  still need `vision.v2` for all, which is acceptable for a contract this young.
- **Images inline only.** Simple, but a phone photo is several megabytes and the reading cap
  is 64 KiB; blobs already exist for this (ADR-122 section 13).
- **Image chat as `svc.vision.chat`.** Rejected in section 1: the VLM already has a service
  (`svc.infer`) and an OpenAI-compatible role (ADR-101); a second path would give the NPU two
  queues.
- **A bare float vector for embeddings.** It is what the Swift helper and SigLIP return, and
  it is how the two get mixed. The kind costs a few bytes per reply.
- **Face identity behind a flag.** Not in a general vision contract; see section 6.

## Open questions for acceptance

1. The kind names. The card used `apple-featureprint-v1` and `siglip2-so400m-384`; this
   draft uses `apple-featureprint-r2` (it is Vision's revision 2, and revision 1 is a different
   vector) and `siglip2-so400m-p16-384` (Google also publishes a patch-14 384 model).
2. The caps: 32 MiB per image blob, 64 megapixels and 16384 per side decoded, 256 results.
3. Whether `labels` with no list (the provider's own taxonomy) stays in `v1`, or every labels
   call must name its labels.
