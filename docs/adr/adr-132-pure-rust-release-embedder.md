# ADR-132: A pure-Rust engine (tract) runs the release embedder on the cargo-dist archives and the Docker image

- **Status**: Proposed (2026-11-13). Implemented on branch `w2/dist-embedder`; not landed. Needs the owner's diff-gate approval rows (new dependency tree, `tract-linalg`'s build script) and merge call.
- **Date**: 2026-11-13
- **Deciders**: owner
- **Owner direction**: 2026-11-13 00:38 ET: hold the release until the cargo-dist archives and the Docker (musl) image ship a real embedder, real models only, with tests. Addendum: "Embedded we have in ruvector tools": use the embedder ruvector already has.
- **Prior decisions**: the decision store, consulted 2026-11-13, reported its decisions category unavailable, so prior history on this topic is unknown.
- **Builds on**: the EF line (`w2/ef-real-embedders`): nomic-embed-text-v1.5 as the real embedder, no silent hash fallback, ort 2.0.0-rc.15 / ONNX Runtime 1.31.
- **Changes**: `scripts/build.sh native` no longer skips the embedder on musl and x86_64-apple-darwin; it uses tract there. The ort path on the four ort triples is unchanged.

## Context

`ort` links a prebuilt ONNX Runtime that its build script downloads. The list of
prebuilt runtimes is `ort-sys-2.0.0-rc.15/build/download/dist.tsv`:

| Triple | Rows in dist.tsv (rc.15) |
|---|---|
| x86_64-unknown-linux-gnu | none, cuda13+tensorrt+nvrtx, migraphx, nvrtx, openvino, webgpu |
| aarch64-unknown-linux-gnu | none, cuda13+tensorrt+nvrtx |
| aarch64-apple-darwin | coreml, coreml+webgpu |
| x86_64-pc-windows-msvc / aarch64-pc-windows-msvc | directml variants / directml |
| x86_64-unknown-linux-musl, aarch64-unknown-linux-musl, x86_64-apple-darwin | absent |

The cargo-dist targets (`[workspace.metadata.dist] targets`) include both musl
triples and x86_64-apple-darwin, and the Docker image builds a static musl
binary on `rust:1.93-alpine`. Before this change the dist packages enabled no
embedder feature on any target and the Docker build used default features, so
every archive and the image answered recall with "semantic search unavailable".

## Options

| Option | What it takes | Evidence | Verdict |
|---|---|---|---|
| (a) `ort` `load-dynamic`: link nothing, `dlopen` libonnxruntime at run time | ship a `.so`/`.dylib` beside the binary and in the image | ort rc.15 `load-dynamic = ["std", "preload-dylibs", "ort-sys/disable-linking"]`. A static musl executable cannot `dlopen` (musl supports dynamic loading only in dynamically linked programs), and Microsoft publishes no musl ONNX Runtime, so a glibc `.so` would need a dynamic musl binary plus gcompat. That gives up the static binary and adds a native library to every archive | Rejected |
| (b) build ONNX Runtime from source for both musl triples (and Intel mac), link with `ORT_LIB_LOCATION` | a C++/CMake build of ONNX Runtime per target per release | not built here; it is a multi-hour CI addition per target with its own toolchain upkeep, and nothing in-tree builds it today | Rejected (cost) |
| (c) move the image to a glibc base (debian-slim / distroless-cc) and use the prebuilt linux-gnu runtime | a different base image | fixes the image only, not the musl and Intel-mac archives; changes a product property (Alpine static image, ~30 to 40 MB) | Not needed; would be an owner call |
| (d) a pure-Rust engine running the same `.onnx` file | a new dependency tree | tract-onnx 0.23.10 loads nomic-embed-text-v1.5 (sha256 `147d5aa8...52965`) and matches ort to cosine >= 0.9999993 (table below) | **Chosen** |
| (e) drop x86_64-apple-darwin from dist | an owner call | not needed: (d) builds for it | Not taken |
| (f) ruvector's embedder, `ruvector-embed-core` 2.4.0 `TractEmbedder` (owner addendum) | the crate from crates.io | it IS option (d)'s engine (tract-onnx 0.23). As published it cannot load nomic: `optimize: Failed analyse for node #138 "/embeddings/word_embeddings/Gather"`, root cause `Impossible to unify Sym(batch_size) with Val(1)`: it pins every input to a concrete `[1, seq]`, while HF exports carry symbolic `batch_size`/`sequence_length` in their value_info. It also optimises one plan per distinct sequence length (weights repacked per plan) and pins tokenizers 0.20 (WeftOS is on 0.21) | Engine adopted, crate not used; gap reported |

### Engine parity (nomic-embed-text-v1.5, six texts, 7 to 130 tokens)

| Pair | Min cosine | Max abs component difference |
|---|---|---|
| tract aarch64-apple-darwin vs ort aarch64-apple-darwin | 0.9999996 | 1.97e-7 |
| tract x86_64-apple-darwin (run under Rosetta 2) vs ort aarch64-apple-darwin | 0.9999993 | |
| weft musl (cargo-dist archive) and the Docker image vs the native ort reference | 1.000000 (6 decimals) | |

Speed on the same Mac (one thread for tract, four intra-op threads for ort):
19 tokens 26 ms against 56 ms; 130 tokens 98 ms against 82 ms. tract builds one
plan for every length in 0.3 to 1.8 s at load.

## Decision

1. `clawft-core` gets one engine-neutral runner, `embeddings::onnx_graph::OnnxGraph`:
   ONNX Runtime under `onnx-embeddings`, tract under `tract-embeddings` (ort
   wins if both are on). The core `OnnxEmbedder` (memory, tools, CoS brain,
   the daemon's recall) and the kernel `OnnxEmbeddingProvider` (session tier,
   `shared.embed`) both run on it. Tokenisation, masked mean pooling, L2
   normalisation and the nomic task prefixes are unchanged.
2. The tract runner declares the inputs with the graph's own symbol names
   (`batch_size`, `sequence_length`), so one optimised plan serves every
   sequence length.
3. `tract-embeddings` is the embedder of every cargo-dist archive
   (`features = ["tract-embeddings"]` in `[package.metadata.dist]` of
   `clawft-cli` and `clawft-weave`; dist then builds each package on its own)
   and of the Docker image. `scripts/build.sh native` keeps ort on its four
   triples and uses tract on every other native triple.
4. The model still ships separately (`~/.weftos/models/nomic-embed-text-v1.5`
   or `WEFTOS_EMBED_MODEL_DIR`); the image documents a read-only mount at that
   path.
5. `weft memory embed <text> [--query]` prints `{model, engine, dims, lane,
   vector}`; `scripts/release/embedder-smoke.sh` checks a binary or image
   against reference vectors recorded from the native ort build.

## Consequences

- One code path for every release artifact that embeds; the only per-target
  difference is the engine, and its vectors agree to six decimals.
- New dependency tree: tract-onnx 0.23.10 and its 8 tract crates plus about 25
  small crates (prost 0.14, num-bigint, itertools 0.15, ...). `tract-linalg`
  has a build script that compiles assembly kernels with `cc`. Both need
  diff-gate approval rows.
- The Qwen3 provider stays ort-only; on tract builds it reports itself
  unavailable, as on any build without ort.
- Not covered: the `weftos` and `weft-gui-egui` dist binaries do not embed, so
  they are unaffected.
- ruvector-embed-core could carry the same symbolic-dims fix (reuse the model's
  symbol names, one plan); that is an upstream change in a ruvnet repository and
  is not made here.

- The Docker image grows from 76.2 MB to 118 MB (weft and weaver both carry
  the engine). A musl `weft` loading the model peaks at 1.1 GB RSS.

## Verification

`scripts/release/embedder-smoke.sh` against reference vectors recorded from the
native ort build (`tests/fixtures/embedder/nomic-v1.5-reference.json`): three
texts at cosine >= 0.999 and two paraphrased queries ranked top-1.

| Artifact | Before (no feature) | After |
|---|---|---|
| Docker image (podman build of `Dockerfile`, x86_64) | FAIL: "semantic search unavailable ... no local embedding engine" | PASS, cosine 1.000000 x3, recall top-1 x2 |
| cargo-dist `dist build --target x86_64-unknown-linux-musl` (dist 0.31.0), `weft` from the clawft-cli archive (static-pie) | FAIL, same message | PASS, same numbers |
| native ort `weft` (`scripts/build.sh native`, linux-gnu) | | PASS (it is the reference) |

Both "after" runs were on a pre-AVX Xeon (SSE4.2 only), where the native ort
binary dies with SIGILL at start-up; tract picks its kernels at run time. The
real-model recall tests of core, kernel, cos and tools pass on tract with no
skips. Not run: aarch64 Linux (musl archive, arm64 image), the full
x86_64-apple-darwin and Windows dist builds.
