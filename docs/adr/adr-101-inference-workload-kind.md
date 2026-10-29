# ADR-101: The inference workload kind and migration of local model hosting

- **Status**: Accepted (2026-09-29; decisions settled 2026-09-29, implementation tracked on cards mesh-placement-01..23)
- **Date**: 2026-09-28
- **Deciders**: Platform / ops. Open questions settled 2026-09-29 (defaults accepted by user); status is Accepted (2026-09-29) and the decisions below are settled, with implementation tracked on the cards named in Status.
- **Depends-On**: ADR-099 (governed workload placement)
- **Relates-To**: ADR-060 (local Hermes serving and KV management), ADR-018 (Hermes as clawft-llm provider), ADR-100, `~/llm` ADR-0004 (open capability vocabulary), 0016 (local model serving), 0022 (model storage tiers), 0025 (ruflo local models across hosts; title only, not read)
- **Amends**: how ADR-060 / ADR-018 endpoints are configured (static URLs become placement-resolved). Does not change their model, KV, or provider-protocol decisions.

## Context

Requirement: "this really should be the same way local inference is hosted long term", meaning model servers become governed, placed workloads with accelerators as placement targets.

### How local inference is hosted today

**Serving side, `~/llm` (the model lab; `USING_LOCAL_MODELS.md` is its map):**
- `bin/serve-llamacpp <model.gguf> [--draft] [--kv] [--ctx] [--port 8090] [--host]` wraps `llama-server` (OpenAI-compatible, port 8090 default, speculative decoding, KV quantization, Metal on this Mac). This is the Hermes recipe from ADR-060.
- `bin/serve` wraps `mlx_lm.server` (ports 8081-8085, 8092 in the roster).
- Ollama (`:11434`, MLX backend on Apple Silicon since 0.19) for managed models and those `mlx_lm` cannot load; TTS model `orpheus-tts` is served by Ollama.
- `bin/queue roster` prints the measured roster: role, backend, model, serve command (for example `coder-daily` = Qwen3-Coder-Next-4bit on `:8081`, `planner` on `:8082`, `swarm-micro` on `:8085`, `embed`, `vlm`). The source is `docs/models/queue.yaml`.
- Weights live on a removable drive `/Volumes/ai-models` by default, with a small "hot tier" promoted to internal storage (`bin/modelstore`, ADR-0022, `docs/models/storage-tiers.yaml`); Ollama's store moves with `OLLAMA_MODELS`. A detached drive leaves dangling links.
- Concurrency rules are manual: co-residency limits are written in docs ("do not co-reside" planner and Next-coder) and memory is a live constraint (ADR-0016).
- I observed on 2026-09-28: no listener on `:8090`; Ollama on `:11434` up with no model loaded. This machine is an M5 Max, 128 GB unified memory.

**Consumer side, clawft:**
- Constants: `DEFAULT_LOCAL_LLM_SERVICE_URL = http://127.0.0.1:8090`, `DEFAULT_LOCAL_LLM_API_BASE = .../v1`, model `hermes-4.3-36b` (`clawft-types/src/config/local_llm.rs:24-39`); duplicate `DEFAULT_LLM_SERVICE_URL` (`clawft-service-llm/src/lib.rs:72`); `HERMES_SERVING_BASE_URL` and Ollama `http://localhost:11434/v1` (`clawft-llm/src/local_provider.rs:35-50`); builtin providers `local` (prefix `local/`) and `ollama` (`clawft-llm/src/config.rs:133-150`).
- `ProviderRouter::route` picks a provider by model-name prefix; each provider holds a fixed `base_url` (`clawft-llm/src/router.rs:80-110`).
- Resolution order env, then `[kernel.llm]`, then ADR-060 defaults, shared by daemon and CLI (`clawft-core/src/local_llm_bridge.rs`).
- The daemon registers the LLM client as kernel service `llm` with lifecycle flag, health check (`/health` on llama-server) and a chain-anchored contract (`clawft-weave/src/llm_service.rs`).
- Other hardcoded consumers: Orpheus TTS URL `http://127.0.0.1:11434/api/generate` (`clawft-voice-tts/src/orpheus.rs:34`, `clawft-voice-talk/src/tts.rs:77`), the voice loop bound to `:8090` (`clawft-voice-talk/src/llm.rs:24`).
- Per repo memory notes, `.clawft/config.json` can override `weave.toml`, and a bare model name resolves to the openai provider; a migration must not break those precedences.

**Nothing in the kernel today knows about GPUs, NPUs, or model formats** (grep for Metal / CoreML / CUDA / Hailo / Coral in `clawft-kernel` and `clawft-types` finds none).

So today a model server is a hand-started process bound to a fixed local port, described in `~/llm` docs, reached by fixed URLs. It cannot move to a better node, fail over, be governed as a workload, or be found by remote consumers.

## Decision

An inference endpoint is a **placed workload** (`kind = "inference"`) on the ADR-099 layer with a **stable mesh address**. The provider router resolves it through the placement layer instead of a hardcoded `api_base`. Existing constants remain as fallbacks so nothing breaks while the migration proceeds.

### 1. Inference workload spec

```
InferenceSpec {
  role: "coder-daily",                 // stable name consumers use; maps to ~/llm roster roles
  model: ModelRef { manifest: <hash>, name: "Qwen3-Coder-Next-4bit", format: "mlx" | "gguf" | ... },
  runtime: "infer.mlx-lm" | "infer.llamacpp" | "infer.ollama" | ...,   // capability ids, not code
  serve: { ctx, kv_quant, draft_model?, sampling defaults, chat_template_args },
  memory: { weights_bytes, kv_budget_bytes },
  excludes: ["role:planner"],          // co-residency
  latency_class: "interactive" | "batch",
  affinity: { sticky: true },          // warm KV state
  api: "openai-v1",
}
```

The current `bin/queue roster` rows (role, backend, model, serve args, port) translate one to one; importing `queue.yaml` produces specs. `~/llm` remains the model lab and catalog; it stops being the process launcher of record once the adapter can do that (migration step 3).

### 2. Requirements derived

- `runtime.infer.<x>` present (or a generic `runtime.native` plus the server binary), and a **format** capability the runtime supports for this model (`format.gguf` for llama.cpp, `format.mlx` for mlx-lm, `format.coreml` / `format.onnx` / `format.tflite` / `format.hef` for NPU / TPU runtimes);
- an accelerator: `accel.gpu.metal` on this Mac, `accel.gpu.cuda`, `accel.gpu.rocm`, `accel.gpu.vulkan`, `accel.npu.*`, `accel.tpu.*`, `accel.tsu.*`, or none (CPU fallback, explicit only). Attribute predicates on precision (`int4`, `mxfp4`, `fp16`) and supported formats;
- **memory**: `weights_bytes + kv_budget_bytes` against `mem.unified` free (Apple) or `mem.vram` free (discrete GPU). On unified memory this is one pool; the placer must subtract already-placed inference and other workloads from the same pool and honor `excludes`;
- trust tier and provenance from policy.

**Locality is a preference for inference, not a hard filter:** where the model shards already are (`model.present`), where the storage tier is mounted (`store.tier.external.mounted`), and proximity to the heaviest consumer (voice loop latency). A cold external drive counts as absent.

### 3. Model artifacts

Follow ADR-099 section 6, plus:
- **Model manifest**: per-shard `{path, size, blake3}` (safetensors shards, one GGUF file, MLX quant directories), optional tokenizer / chat-template hashes, source (HF repo and revision, Ollama tag), and an operator signature over the manifest.
- Upstream signing is rare, so trust is **operator attestation of a hash manifest**, established at adoption ("trust on first use by an operator", pinned thereafter). A model file whose hash later differs is refused.
- **Adopt in place**: hash the HF cache / `mlx-quants` / Ollama blobs where they lie and register them as artifacts; do not copy. Weights of 20-60 GB (for example the 45 GB Qwen3-Coder-Next 4-bit) are never re-shipped when a node already holds them.
- **Transfer only when needed and allowed**: policy `allow_weight_transfer` with a size ceiling; otherwise place where the bytes are or return `Unplaceable` with the reason. Transfers are chunked and resumable, and the placer records the fetch-versus-relocate choice.
- Storage-tier facts come from probes (drive mounted, free space), because the removable drive can vanish; the workload becomes `Degraded` or `Lost` (not silently broken) if shards disappear under it.

**Implementation status (card mesh-placement-17, 2026-10-03):** the model manifest, adopt-in-place, lazy re-hash, `model.present` / `store.tier.external` advertising, the fetch-versus-relocate decision and the sharing gate are implemented in `clawft-kernel::model_manifest` (`weaver model adopt | list | verify | explain`). Choices made there:
- The manifest is a `kind = "model"` envelope (same signing domain and `TrustAnchors` as workload packages); `redistributable` defaults to false and is part of the signed statement. Weights enter the artifact exchange only through `model_manifest::sharing::seed_model`, which needs the opt-in and the exchange's `RedistributionPolicy` (grant origin `OptIn` or `NotFlagged`, never Cognitum). Seeding is the one place weights are duplicated on disk; adoption never copies.
- `model.present` carries a `shards` list: one `model:<package id>` marker (complete and available only) plus the shard BLAKE3 hashes. A partial holding or a detached drive is `Degraded`, so the placer stops preferring it. Paths and drive labels are never advertised.
- The placer integration is a soft `locality_preference` (or hard `model_present_requirement`); the fetch decision (`decide`) takes a `TransferPolicy` (`allow_weight_transfer` default off, optional `max_bytes`, free-space headroom) and names the reason when it returns `Unplaceable`.
- Inference adapters (card 18) consume `ModelRegistry::resolve`, which lazily verifies and returns the adopted file paths, or `NotReady` (refused, degraded, detached).

**Hardening (review round 1, 2026-10-03):**
- *Containment.* Adoption and every later check resolve each file and require it to lie under the canonical model root (an HF snapshot may also reach the sibling `blobs/` of the same repo cache). Symlinked directories are refused, an escaping file link refuses the scan or, after adoption, the model (state `Refused`, nothing advertised), and links are re-checked on every check and immediately before seeding. Residual: a path can still change between the check and the adapter opening it.
- *Trust at use.* `ModelRegistry::resolve(id)` keeps its signature, but now verifies against the registry's `ModelTrust` (set with `with_trust` / `set_trust`): the attestation must verify against the pinned anchors now, the package, signers and shard hashes must not be revoked, and the file list must equal the signed body. A registry without trust refuses to resolve. `open` rejects a registry whose entries are structurally inconsistent (key is not the package id, file list differs from the body).
- *Lazy window.* The first `resolve` of a model in a process hashes every byte (minutes for tens of GB); later resolves re-hash only files whose size or mtime changed. A file replaced with the same size and same mtime after the first resolve is therefore not noticed until the next process start or an explicit `CheckMode::Full` check. Adapters can rely on: bytes matched the attestation at first resolve in this process; any later change that alters size or mtime is caught; adapters that must close the window run a full check before loading.
- *Tokenizer and template.* The signed body carries their relative paths with the hashes; `attach` checks them on disk and `resolve().tokenizer` is the verified path.
- *Privacy.* The `shards` list advertises the BLAKE3 of every shard, which fingerprints which models a node holds to every peer. A per-model opt-out hides a model from facts (`weaver model advertise <model> off`, `adopt --no-advertise`); it stays usable locally. A refused or revoked model advertises an empty, `Degraded` `model.present` (there is no `Unavailable` capability state).

### 4. Adapters

`infer.llamacpp`, `infer.mlx-lm`, `infer.ollama` (each `provides()` its runtime, format, and accelerator capabilities after a probe such as `llama-server --version`, Metal availability, or `ollama` reachable). Each supports:
- **Adopted** mode: register and health-check a server that is already running (today's state), without controlling it. This is the first migration step and needs no behavior change in `~/llm`.
- **Managed** mode: start the server through the native (or container) adapter with the spec's serve args, health-probe `/health` and `/v1/models`, restart with backoff, drain and unload on request. Ollama is special: it manages its own load / unload, so the adapter drives its API and reports state rather than owning the process.
- Health means: process up, `/health` ok, model listed, and (managed) a tiny completion succeeds. `Degraded("loading model")` while a 503 is returned, matching `llm_service.rs`.
- Accelerator-specific inference runners (Hailo HEF, Coral TFLite, RKNN, Qualcomm, TSU samplers) are **deferred** until the hardware exists; they implement the same trait and add capability ids.

**Implementation status (card mesh-placement-18, 2026-10-03):** `clawft-kernel::workload_runtime::infer` implements all three adapters as one `InferRuntime` over `WorkloadRuntime`. Choices made there:
- An inference workload is `VerifiedWorkload::inference(InferenceSpec)` (`kind = "inference"`, new `WorkloadSource::Inference`). Governance sees package trust `operator_attested`, so a permit for inference must set `min_package_trust = operator_attested` explicitly; nothing is signed or fetched.
- **Adopted** adapters have id `infer.<x>.adopted` and only ever send reads (`GET /health`, `/v1/models`, Ollama `/api/version|tags|ps`). `start` confirms the server answers, `stop` is refused as `Unsupported`, `unload` forgets the registration, and a server that goes away reports `Exited` and is not restarted. Because the lab starts its servers with `--host 0.0.0.0`, an adopted instance reports `network_exposure = Lan` instead of assuming loopback.
- **Managed** adapters have id `infer.<x>`. llama.cpp and mlx-lm run the configured launcher under the native `Supervised` process supervision (cleared environment, own process group, output capped) with argv built from the spec and the model that `ModelRegistry::resolve` returned; the spec never names a weights path. Hosts are loopback only and `extra_args` cannot set `--host`, `--port`, `-m`, `--draft`, `--ctx` or `--kv`. Admission refuses a port something already listens on (adopt it instead), a model that is `NotReady`, and a format the runtime cannot load. Only the process the adapter spawned is ever signalled. `status` maps process up with nothing listening to `Degraded("starting...")`, HTTP 503 to `Degraded("loading model")`, a model the server does not list to `Degraded`, and a dead process to `Exited`. `reconcile` restarts a server that died while it should run, with doubling backoff and a restart budget; `restart` and `deep_health` (one real token) are separate calls.
- **Ollama** is driven through its API only: load is an empty `/api/generate` with `keep_alive`, stop is `keep_alive: 0`, state is `/api/ps`. The adapter never starts Ollama, never pulls (a model not in `/api/tags` is an admission refusal) and never deletes.
- `provides()` is empty until `probe_capabilities()` has checked the runtime (HTTP reachability, or an executable launcher plus an optional version command), then `runtime.infer.<x>`, `format.*` and, on Apple silicon, `accel.gpu.metal` at `probed` provenance.
- The API card 19 (stable address and proxy) builds on: `endpoint(&handle)`, `health(&handle)` (`ServerReport`), `spec_of`, `status`, `reconcile`, `restart`, `deep_health`.
- **Review round 1 hardening:**
  - `extra_args` is an allowlist (exact flag spellings with arity, per runtime; Ollama takes none). Anything else, including `--hos`, `-md`, `--ctx-size`, `--lora*`, `--api-key`, `--ssl-*`, `--path`, `*-file`, `--k=v` forms and entries containing whitespace, is refused at validation.
  - A managed server's binding is verified, not assumed: after start, `status` and `reconcile` connect to every non-loopback local address on the instance's port. If it answers, the process is stopped, the instance records why, `reconcile` returns `StoppedExposed` (the caller chains it) and the instance will not start again until reloaded.
  - Adopted instances carry package trust `adopted_unverified` (new `PackageTrust` value, ordered just above `unsigned`, so no signed or attested minimum is satisfied by it). A permit for them must name it; a permit for attested model weights does not cover them. Adopted mode is observe-only.
  - `admit` is a read-only self-check that the host calls before the gate, so a spec's loopback port is probed with GETs before any permit is consulted. `InferConfig::allowed_ports` bounds which ports that can be; capabilities count a server only when it is up or loading.
  - The supervisor remembers the process group, kills it when the leader is reaped, and signals it on drop, so a launcher that forks leaves nothing behind. A caller-supplied `PATH` overrides the supervisor's default; `ManagedConfig` forwards the daemon's `PATH` and `HOME` by default.
  - Ollama stop and unload unload the model from Ollama's memory even when another client loaded it; Ollama has no per-client ownership.
  - The instances lock is not held across network calls or process termination; `deep_health` uses a 300 s timeout for cold models.
- Not done here: wiring instances into the daemon's `weaver workload list`, a `WorkloadKind` that turns an `InferenceSpec` into placement requirements, and non-loopback binds (a chained Permit, section 7).

### 5. Stable mesh address and provider resolution

Each running inference instance is announced as a `ServiceAdvertisement` named `infer.<role>` (`mesh_service_adv.rs:16`) with metadata (model hash, api, node id, port, load). Consumers resolve `infer.<role>` through a `PlacementResolver`:

- **Compatibility proxy (recommended primary path).** Each node that has consumers runs a **loopback endpoint proxy**: a stable local listener (for example `127.0.0.1:8090`) that forwards OpenAI-compatible HTTP to the current placed instance, locally when it is on this node and over the Noise mesh otherwise. The advertised address never changes when the workload moves. Existing clients, including `~/llm` tools, Grok's `local-*` aliases, the voice loop, Orpheus TTS, and any `apiBase` configuration, keep working unchanged.
- **Direct resolution (for clawft internals).** `ProviderRouter` gains a `resolve_base_url(role)` hook consulted per request with a short TTL cache and invalidation on `MeshPeerEvent` / service-advertisement change. The `local` and `ollama` builtin providers keep their prefixes (`local/`, `ollama/`), which now map to roles; `base_url` becomes a fallback used when resolution fails or the layer is disabled.
- Precedence is unchanged: env, then `[kernel.llm]`, then placement, then the ADR-060 constants. An explicit `LLM_SERVICE_URL` still wins (operator escape hatch).
- Service `llm` in the kernel (`llm_service.rs`) keeps its lifecycle flag and chain contract and gains the resolved endpoint as its source of truth.

**Implementation status (card mesh-placement-19, 2026-10-03):** `clawft-kernel::infer_proxy` (the table, proxy and mesh forwarding) and `clawft-llm::placement` (the router hook). Choices made there:

- **Table.** `PlacementTable` maps a role to a local instance or an admitted peer. Local instances come from the card-18 adapter through `sync_local` (`reconcile`, `health`, `endpoint`); a role is served only while its server answers. Remote instances come from `infer.<role>` `ServiceAdvertisement`s, accepted only from peers the `MeshDialer` reports as admitted, and admission is re-checked at every `resolve`, so a revoked peer stops resolving at once. The local instance wins over a remote one (sticky, warm KV). A generation counter and a `watch` channel report every change.
- **Advertisement.** `advertisement(role)` builds `infer.<role>` with metadata `role`, `node_id`, `port`, `api`, `runtime`, `load` and `model`. It exists only for roles the operator exposed to the mesh (`expose_to_mesh`, audited), because advertising an unexposed role would only send peers to a refusal.
- **Proxy.** `InferProxy` is one loopback listener per role, one request per connection (`Connection: close`), hand-rolled HTTP/1.1 with a strict parser. It forwards only GET and POST on an allowlist (`/v1/*`, `/health`, Ollama's inference and read-only endpoints; `/api/pull`, `/api/delete` and the other management paths are refused). The destination is never taken from the request: an absolute-form target, a `//host` path, a non-loopback `Host` (DNS rebinding), a cross-origin `Origin`, chunked request bodies and duplicate or malformed `Content-Length` are refused. Requests and responses are bounded in size and time (`ProxyLimits`), and a streamed response is relayed as it arrives.
- **Port safety.** The bind address must be loopback. A port something already answers on (on `127.0.0.1`, which also catches a wildcard listener) is never bound over: `OccupiedPolicy::Refuse` fails, `OccupiedPolicy::Adopt` leaves the existing server as the address for the card-18 `Adopted` mode. The socket is bound without `SO_REUSEADDR`, so a wildcard listener also blocks the bind instead of being shadowed. Both outcomes are audited.
- **Mesh forwarding.** New frame types `InferRequest` (0x10) and `InferResponse` (0x11). The consumer dials only the node the table resolved, through the `MeshDialer`, which is the single authority on admission and on the stream being authenticated. The server serves a request only from a verified peer, only for a role exposed to the mesh, and only from a local instance, never onward, so a request cannot be bounced across nodes. The client's `Authorization` is never sent over the mesh. Everything decoded from the wire is revalidated as a client request would be, and responses are capped and checked for ordering.
- **Router hook.** `ProviderRouter::with_placement(resolver, ttl, &[(provider, role)])` makes the named providers follow a `PlacementResolver` through a TTL cache (`invalidate_placement`, `invalidate_all_placement`); a resolved URL is used only if it is plain `http` to a loopback host, and the configured `base_url` is the fallback. A failed request to a placed endpoint drops the cached answer and retries once on the configured endpoint. For a remote role the table resolves to this node's loopback proxy, so `clawft-llm` never speaks the mesh.
- **Precedence.** Env, then `[kernel.llm]` and `[providers.local]`, then placement, then the ADR-060 constants. `clawft_core::local_llm_bridge::local_placement_allowed` is true only when none of the first two chose the endpoint, and a provider left out of the role list is never placed.
- **Review round (2026-10-03):**
  - Placement trust: `ingest_advertisement(sender, ad)` takes the id the connection was verified as and requires `ad.node_id == sender`; the sender must hold an enforced-admission grant (`admitted`, class `Node`, `trust_scope`, not merely observed), re-checked at every resolve; and the consumer must have allowlisted the sender for the role (`allow_remote_node`, audited, default deny: the counterpart of `expose_to_mesh`, and the governed decision to send prompts to that node). Adverts expire by receive time (`with_advert_ttl`, default 60 s, refreshed by repeats); the peer's own timestamp is not trusted. **Staleness and supersede rule (2026-10-29):** announcements go out every 20 s (`DEFAULT_ADVERT`), so the 60 s TTL is three missed announcements. `resolve` ignores an advert older than the TTL and, among live adverts for a role, picks the one heard most recently, so a placement that moved to another node supersedes the node that went quiet. The table holds one entry per (role, node): a newer advert replaces the older, and a withdrawal removes it at once. Expired entries are pruned on each ingest of the role and on each announce round (`prune_expired`). Freshness uses a replaceable receive-time clock (`with_clock`) so tests do not sleep. A svc-backed role is deregistered when its provider stops serving, which drops its advert and queues a withdrawal to peers.
  - Only statuses 200 to 599 are accepted from a peer or a local server; a 1xx or 101 becomes a 502, so a peer cannot inject a forged response. `clawft-llm` providers no longer follow redirects.
  - The serving side serves peers only `/v1/chat/completions`, `/v1/completions`, `/v1/embeddings` and `/v1/models` (not `/api/ps`, `/api/show` or Ollama management), pins `model` to what the server knows the instance as, removes `keep_alive`, and refuses a POST body that is not a JSON object. `mlx_lm.server` fetches any repository named in `model`, so MLX instances get its `default_model` placeholder. A `ServeGate` bounds concurrent requests per peer and per node.
  - Peers are told a generic reason only (`refused`, `role not served here`, `request not accepted`, `timeout`, `upstream error`); detail goes to the audit trail. The internal instance port is not advertised.
  - The local client's `Authorization` is forwarded to the local instance (including Adopted servers, which may run with `--api-key`) and never over the mesh.
  - Serving requires the peer's grant to qualify (admitted under enforce, class `Node`, trusted scope, not merely observed) and the peer to be on the role's serve allowlist (`allow_mesh_peer`, audited, default deny; exposure says the role may leave loopback, the allowlist says to whom). The serve gate's slots are taken before the frame is read, so a peer over its limit cannot make this node buffer a frame per stream. Forwarded bodies keep only an allowlist of OpenAI body keys per path (`mesh_policy`); `adapters`, `draft_model`, `keep_alive`, `options`, `grammar`, slot controls and unknown keys are dropped. The list follows the request fields `mlx_lm.server`, Ollama's native API and llama-server read beyond the OpenAI schema, as best documented; it was not checked against a server binary here.
  - Forwarding the local client's `Authorization` to a local instance, Adopted ones included, relies on adopt-as-trust-decision: adopting a server on a loopback port is the operator's decision to trust that process with the credentials local clients send to this address.
  - Fallback retries only connect errors, timeouts and 502, 503, 504; a 4xx or a quota 429 is the server's real answer.
  - The loopback probe covers `127.0.0.1` and `[::1]` for either bind family; accept errors back off; a head with a bare LF is refused. `mesh_framing::read_frame` now strips and checks the length prefix `write_frame` adds.
- **Daemon wiring (2026-10-03):** `clawft-weave::infer_wire` (config `<runtime>/inference.json`, off by default; Adopted roles only), `infer_rpc` (`infer.status` Read, `infer.expose` and `infer.allow` Admin), `clawft-kernel::infer_proxy::hub` and `clawft-core::placement_hook`. The mesh path is the existing control-topic mechanism (`mesh.infer` on `MeshRuntime`), so the hub receives each message with the connection's own `PeerCtx`: `InferPeer.grant` is `PeerCtx::grant()` (admitted, class, trust scope as admission recorded them) and the sender of an advert is `ctx.peer_id`, never a payload field. The hub learns a peer's standing from traffic (a `hello` on connect and on each announcement) and only dials a peer it holds a qualifying grant for. In service mode no hub is built: the daemon's `MeshSink` treats deliveries as unverified (the service vouches for the source node, not this process), so there is no grant to serve or trust a remote node on; local placement and the proxy still work, and the mesh verbs refuse with that reason. Carrying the grant from the service to the daemon is a follow-up that needs a mesh-local protocol change. Forwarded requests are bounded at 2 MiB (hex inside a control message) and responses are chunked; a faster dedicated data stream is a later optimisation. Managed roles (launching servers through the governance gate) and the `weaver` verbs for the `infer.*` RPCs are not done.
- **First slice of the migration (card mesh-placement-20, 2026-10-03):**
  - `workload_runtime::infer::roster` imports the model lab's `queue.yaml` (read only; the caller names the file; tests use a fixture copy) into `InferenceSpec`s: memory from `ram_gb`, port and flavor from `backend` (first of "a or b"), sticky by default, latency class from an operator overlay or the role text, `excludes` from the overlay. Unsupported backends and aliases are reported with a reason.
  - `workload_runtime::infer::residency` is the unified-memory budget and co-residency rule: one `ResidencyLedger`, shared through `ManagedConfig`, reserved when a managed instance starts (before anything is spawned) and released on stop or unload. A violation is `AdmissionRefused("unplaceable: ...")`; through `WorkloadHost` it is chained as `workload.refuse` with the reason.
  - The daemon (`infer_cfg`, `infer_wire`, `infer_managed`, `infer_expose`) adds `managed` roles: the adapter starts and stops the server through the configured launcher (`serve_programs`, never guessed), weights from the model registry, every step gated by the workload gate and chained, never autostarted unless asked; `infer.start` and `infer.stop` (Admin).
  - Listening beyond loopback (section 7): only a role's proxy, never the model server, and only with a bearer token (mode 0600 file, 32+ characters, checked before any body is read, never forwarded or recorded) and a permit from the workload gate for `network: lan` (default deny, chained). `InferProxy::start_exposed` needs an `ExposurePermit` that only a `Permit` decision can make.
  - Consumers: `clawft_types::placement::roles` is the process-wide role resolver. The kernel `llm` service follows the `local` role when no URL or config chose its endpoint (and swaps its client when the role moves), and the Orpheus TTS follows a role named `orpheus-tts` when it uses its default endpoint. A role served on this node resolves to its server directly (no proxy hop), so the local latency path is unchanged.
  - Review round: the ledger is released on every path that stops holding memory (give-up, stopped-exposed, failed Ollama load, and only after the process exited on stop) and the daemon notices a role that stopped running; the exposed proxy is a second listener beside the token-free loopback one (own pool, per-client cap, never the registered proxy port); the token file is opened with `O_NOFOLLOW` and checked on the descriptor, with its directories checked; an `ExposurePermit` can only be made by `ExposurePermit::ask`, which runs the gate check; Ollama unloads only a model this adapter loaded; the launcher and roster inputs are checked (owner and mode, regular file, size); role resolution accepts loopback http only; native-API roles (the TTS) resolve only to servers on this node. The exposed bearer is cleartext on the LAN: `network: lan` is for trusted segments until TLS lands.
  - Second review round: a give-up or a stopped-exposed outcome sets the role to not wanted and it waits for an explicit `infer.start` (the restart budget is reset only there, never by an automatic drive or by `start_instance`), so a crashing server is not looped and an exposed one is not put back; listener and server ports are checked for collisions across all roles (Ollama roles may share a server); an Ollama load is marked ours from the moment it is requested, so a cut-short load is still unloaded (Ollama has no ownership: a model another client evicted and reloaded may still be unloaded at stop).
  - Out of scope for this slice, written up on the card: failover to a second node with a KV-cold mark (needs a second inference-capable node), and ARM verification (done on the Pi by the operator).
- Not done here: second-node failover and KV-cold marking, the service-mode grant hand-off, and `weaver` verbs for the `infer.*` RPCs. The `InferRequest` and `InferResponse` frame types are the representation inside an exchange; on the wire to a peer they travel as `mesh.infer` control messages.

### 6. Stickiness, KV, and latency

ADR-060 records that llama.cpp KV slot save/restore is prefix-only and context-shift is off, so an agent loop's speed depends on a warm KV cache on one server. Therefore inference declares `affinity.sticky = true`: no automatic migration on load; migration only on node loss, operator request, or the current node failing constraints, and each migration is chained with a "KV cold" note. Voice (`latency_class = interactive`) adds a hard RTT bound between consumer and server (proposal 30 ms) so a voice consumer is never placed against a remote-LAN server by score alone.

### 7. Governance

Actions from ADR-099 (`workload.place`, `.load`, `.start`, `.unload`, `.migrate`) with effect context including: accelerator use, memory reserved, model source and manifest hash, whether weights are transferred, and consumers exposed to (loopback only, LAN, mesh). Exposure beyond loopback is a separate governed decision, because a `--host 0.0.0.0` model server is currently open to the LAN without authentication (as `~/llm` uses to let Apple containers reach it).

### 8. Migration plan (end state: an inference endpoint is a placed workload)

1. **Observe**: register the currently running servers on this Mac (`:8090`, `:8081...`, `:11434`) as `Adopted` inference workloads with capabilities probed, appearing in `weaver workload list` and the chain. No behavior change.
2. **Describe**: import `~/llm/docs/models/queue.yaml` roster into `InferenceSpec`s and model manifests (adopt in place, operator-signed).
3. **Manage**: switch `Adopted` to `Managed` per role, with the adapter launching `serve-llamacpp` / `serve` / Ollama; `~/llm` scripts remain the underlying commands, invoked through the adapter.
4. **Resolve**: stand up the loopback proxy and `PlacementResolver`; move `orpheus`, voice loop, `clawft-llm` providers, and `clawft-service-llm` onto it; leave constants as fallback.
5. **Distribute**: add a second inference-capable node (for example a Linux GPU box or another Mac) and demonstrate role placement, failover, and weights locality.
6. Only then consider remote GPU / NPU / TPU / TSU nodes, once such hardware and adapters exist.

## Consequences

Positive: local inference gains governance, health, failover, memory-budget enforcement instead of hand-kept rules, and the ability to use a better node; consumers stop caring where a model runs; accelerator support is a vocabulary entry plus an adapter.

Cost: model-manifest and large-artifact machinery; a proxy and resolver; import of the `~/llm` roster; care around sticky KV; a security decision about network exposure.

Risks: a placement bug takes local inference down (mitigation: `Adopted` first, constants as fallback, opt-in per role); unified-memory accounting errors causing OOM (mitigation: reserve with margin, admission self-check, measure); operator confusion between `~/llm` and the layer (mitigation: `~/llm` stays the lab, and only the launcher-of-record role moves).

## Decisions (Decided 2026-09-29, defaults accepted by user)

1. **Inference clients**: keep the loopback proxy as the compatibility strategy; existing clients are unchanged. Rationale: zero client churn, stable address across moves.
2. **`~/llm`**: stays a separate repo. The adapter calls its roster (`docs/models/queue.yaml`, `bin/queue`) and serve scripts (`bin/serve-llamacpp`, `bin/serve`) rather than porting them. Rationale: it is the model lab and catalog; only the launcher-of-record role moves.
3. **Model trust**: operator attestation over a hash manifest (section 3). Rationale: upstream rarely signs weights.
4. **Weight transfer**: no hard ceiling in v1; adopt-in-place is allowed on removable drives, and a detached drive marks the workload Degraded. Rationale: ships value early; drive loss is visible, not silent.
5. **`0.0.0.0` bindings**: permitted only with authentication, via an explicit chained governance Permit (section 7). Rationale: today's unauthenticated LAN exposure should become a recorded decision.
6. **First slice**: `coder-daily`, the Hermes `local/` role, and Orpheus TTS. Rationale: these are the consumers actually wired in clawft and the voice path.
7. **Vocabulary changes** go through the governance path (ADR-099 decision 6).
8. **Measured throughput**: `perf.infer.tok_s{model}` measured capabilities feed scoring for `interactive` roles (ADR-099 section 2).

## Updated 2026-10-28: roles backed by a cogbus service (infer.vision)

Status is unchanged (Accepted). Section 5's proxy gains a second kind of
local backend: besides a loopback HTTP server, a role can be served by a
provider of the `infer.v1` service on this node's cog data plane (ADR-122
section 12), for example the Venq's `npu-infer` cog (`svc.infer/venq-npu`,
Qwen3-VL-4B).

```json
{ "roles": [ { "role": "vision", "proxy_port": 18391,
               "svc": { "topic": "svc.infer/venq-npu", "model": "qwen3-vl-4b" } } ],
  "mesh": { "expose": ["vision"], "serve_peers": { "vision": ["<mac node id>"] } } }
```

- The placement table resolves such a role to `Target::Svc{topic, model}`.
  It is registered only while the topic has a provider and withdrawn when the
  provider goes, so the `infer.<role>` advertisement follows the NPU cog.
  Exposure, the serve allowlist, the ADR-120 `infer.<role>` exposure, the
  activity log and `infer.expose` / `weaver infer expose` behave exactly as
  for an HTTP-served role; a role that is not exposed (or a peer not on its
  allowlist) is refused before any call is made.
- The proxy translates, it does not forward. `POST /v1/chat/completions` is
  turned into `infer.v1`: system messages become `system`, the last user
  message's text parts become `text`, `max_tokens` / `max_completion_tokens`
  become `max_tokens` clamped to 1..=4096 (512 when absent). Earlier turns and
  assistant/tool messages are not sent (`infer.v1` is single-shot). The reply
  `text` becomes `choices[0].message.content`, `usage` is mapped when the
  service reports token counts, and `model` is the role's model. `GET
  /v1/models` lists that model. Completions and embeddings are 501.
- One image per request, as a `data:image/png|jpeg;base64,` URL, at most 4 MiB
  decoded and checked against the PNG/JPEG signature. `http(s)` image URLs are
  refused (the daemon does not fetch what a client names), as are other types
  and more than one image. A data URI is `data:<mime>[;param=value]*;base64,<data>`:
  the mime is the first segment, a `base64` parameter is required, other
  parameters (`charset`) are ignored. Images are taken from the **last user
  message only**; an image in an earlier turn is ignored, not refused (the
  history is not sent). The image is put as a broker blob owned by the proxy
  and sent as `image_blob`, because a service call body is capped at 64 KiB.
  The content store has no delete, so the blob gets the shortest TTL that
  outlives the call (the 120 s call timeout plus 30 s) and expires on its own;
  a failed upload drops the proxy's open uploads. `stream: true` is a 400: the
  service answers once.
- A full content store, too many open uploads or the broker's pending-call cap
  is a **429 with `Retry-After`** (`ProxyError::Busy`), not a 502 or 503. One
  caller (`local:<addr>` or `mesh:<node>`) may have at most 2 image calls in
  flight; a third is a 429.
- The proxy is a broker principal of its own (`infer-proxy`, an in-process
  session; no socket and no token handed out). Its calls are governed as any
  `cogbus.call` and `cogbus.blob.put`, count in the bus activity under that
  identity, and obey the broker's pending-call caps. If the provider is in a
  project, `svc.project` gives the proxy's instance that project and bus policy
  must grant it calls across projects; otherwise the call is refused. The proxy
  has one bus identity, so every svc role must name the same `svc.project`
  (or none); roles that differ are not served (stated in the role's reason).
  **Limit:** the broker's governance request carries a single principal (the
  instance) and has no on-behalf-of field, so governance and the bus activity
  see `infer-proxy`, not the real caller; carrying it needs a change to the
  broker request type. The originating client is instead written to the
  proxy's audit trail with every call (`infer.svc.call`: role, caller, topic,
  image or not, size, outcome, never the prompt), and rejected requests (400,
  413) are recorded the same way with the caller, status and reason. For
  peers it is also in the ADR-120 activity log. The per-caller image cap above
  is what bounds one caller's use of the shared identity.
- Over the mesh the forwarded request is capped at 2 MiB (`max_mesh_request_body`),
  after base64 inflation (4/3) and JSON framing. An image therefore has to be
  about 1.5 MiB or less to cross the mesh; downscale on the client. A local
  client of the Venq itself may send up to the 4 MiB image limit (8 MiB body).
- Service-mode daemons build no hub (ADR-101 section 7 and ADR-103), so remote
  callers are user daemons until the grant hand-off lands.
