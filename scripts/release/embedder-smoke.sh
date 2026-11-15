#!/usr/bin/env bash
# Real-embedder smoke for release binaries and the Docker image (ADR-132).
#
# Proves a built `weft` embeds with the REAL nomic-embed-text-v1.5 model: each
# reference text must come back 768-wide with cosine >= 0.999 against a vector
# recorded from the native ONNX Runtime build, and two paraphrased queries must
# rank the right skill first. A binary with no embedder fails at the first
# text with "semantic search unavailable". Hashing is never accepted: the
# reported model must be nomic-embed-text-v1.5.
#
# Usage:
#   embedder-smoke.sh reference <weft> <model-dir> <out.json>   # record vectors (native ort build)
#   embedder-smoke.sh binary    <weft> <model-dir> <ref.json>   # check a built binary
#   embedder-smoke.sh image     <image> <model-dir> <ref.json>  # check an image (CONTAINER=docker|podman)
#
# <model-dir> holds onnx/model.onnx + tokenizer.json. The binary runs with a
# throwaway HOME and WEFTOS_EMBED_MODEL_DIR; the image gets the bundle mounted
# read-only at its default location (/home/weft/.weftos/models/...) and no
# network. Needs python3 on the host.
set -euo pipefail

MODE="${1:?usage: embedder-smoke.sh reference|binary|image <weft|image> <model-dir> <json>}"
TARGET="${2:?missing weft binary or image}"
MODEL="${3:?missing model dir}"
REF="${4:?missing reference json}"
CONTAINER="${CONTAINER:-docker}"

[ -f "$MODEL/onnx/model.onnx" ] && [ -f "$MODEL/tokenizer.json" ] \
  || { echo "embedder-smoke: $MODEL lacks onnx/model.onnx or tokenizer.json" >&2; exit 2; }
MODEL="$(cd "$MODEL" && pwd)"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# One `weft memory embed` call; prints its JSON line. Args: text [--query].
embed() {
  case "$MODE" in
    image)
      "$CONTAINER" run --rm --network none \
        -v "$MODEL:/home/weft/.weftos/models/nomic-embed-text-v1.5:ro" \
        --entrypoint /usr/local/bin/weft "$TARGET" memory embed "$@"
      ;;
    *)
      env -u OPENAI_API_KEY -u WEFTOS_EMBEDDING_MODE HOME="$WORK/home" \
        WEFTOS_EMBED_MODEL_DIR="$MODEL" "$TARGET" memory embed "$@"
      ;;
  esac
}
mkdir -p "$WORK/home"

# Reference texts (lane, text) and the recall set: the descriptors and two
# queries of the core router recall test, both top-1 under nomic.
TEXTS=(
  "document|The borrow checker rejected my Rust code with a lifetime error."
  "query|why does the borrow checker complain"
  "document|WeftOS keeps memory, governance and a chain of decisions, and runs on laptops, servers and small boards; the weaver daemon supervises the kernel services."
)
DOCS=(
  "rust-debug: rust compile error fix"
  "python-debug: python traceback investigation"
  "write-poem: compose a creative short poem"
  "plan-trip: plan a vacation itinerary"
  "explain-graph: explain a knowledge graph"
)
QUERIES=(
  "2|write some verses about autumn leaves"
  "1|my snake script crashed with a stack trace"
)

: > "$WORK/texts.jsonl"
for entry in "${TEXTS[@]}"; do
  lane="${entry%%|*}"; text="${entry#*|}"
  flag=(); [ "$lane" = query ] && flag=(--query)
  if ! out="$(embed "$text" ${flag[@]+"${flag[@]}"} 2>"$WORK/err")"; then
    echo "FAIL: weft memory embed exited non-zero for $lane text:" >&2
    cat "$WORK/err" >&2
    exit 1
  fi
  printf '%s\t%s\t%s\n' "$lane" "$text" "$out" >> "$WORK/texts.jsonl"
done

if [ "$MODE" = reference ]; then
  python3 -I - "$WORK/texts.jsonl" "$REF" "$MODEL/onnx/model.onnx" <<'PY'
import hashlib, json, sys
rows = [l.rstrip("\n").split("\t", 2) for l in open(sys.argv[1])]
h = hashlib.sha256(open(sys.argv[3], "rb").read()).hexdigest()
out = {"model": "nomic-embed-text-v1.5", "model_sha256": h, "texts": []}
for lane, text, js in rows:
    o = json.loads(js)
    assert o["model"] == "nomic-embed-text-v1.5" and o["dims"] == 768, o["model"]
    out["engine"] = o.get("engine")
    out["texts"].append({"lane": lane, "text": text, "vector": o["vector"]})
json.dump(out, open(sys.argv[2], "w"))
print(f"wrote {len(rows)} reference vectors ({out['engine']}, model sha256 {h[:12]}) to {sys.argv[2]}")
PY
  exit 0
fi

: > "$WORK/docs.jsonl"
for d in "${DOCS[@]}"; do embed "$d" >> "$WORK/docs.jsonl"; done
: > "$WORK/queries.jsonl"
for q in "${QUERIES[@]}"; do
  printf '%s\t%s\n' "${q%%|*}" "$(embed "${q#*|}" --query)" >> "$WORK/queries.jsonl"
done

python3 -I - "$WORK" "$REF" "$MODEL/onnx/model.onnx" <<'PY'
import hashlib, json, sys
work, ref_path, onnx = sys.argv[1:4]
ref = json.load(open(ref_path))
h = hashlib.sha256(open(onnx, "rb").read()).hexdigest()
if h != ref["model_sha256"]:
    sys.exit(f"FAIL: model sha256 {h} differs from the reference's {ref['model_sha256']}")
dot = lambda a, b: sum(x * y for x, y in zip(a, b))
ok = True
rows = [l.rstrip("\n").split("\t", 2) for l in open(f"{work}/texts.jsonl")]
for (lane, text, js), r in zip(rows, ref["texts"]):
    o = json.loads(js)
    if o["model"] != "nomic-embed-text-v1.5" or o["dims"] != 768:
        sys.exit(f"FAIL: got model {o['model']!r} dims {o['dims']}, want nomic-embed-text-v1.5 / 768")
    c = dot(o["vector"], r["vector"])
    good = c >= 0.999
    ok &= good
    print(f"{'ok  ' if good else 'FAIL'} cosine {c:.6f} vs {ref.get('engine')} reference ({o['engine']}, {lane}): {text[:60]}")
docs = [json.loads(l)["vector"] for l in open(f"{work}/docs.jsonl")]
for line in open(f"{work}/queries.jsonl"):
    want, js = line.rstrip("\n").split("\t", 1)
    q = json.loads(js)["vector"]
    scores = [dot(q, d) for d in docs]
    best = max(range(len(docs)), key=scores.__getitem__)
    good = best == int(want)
    ok &= good
    print(f"{'ok  ' if good else 'FAIL'} recall: top-1 doc {best} (want {want}), scores {[round(s, 4) for s in scores]}")
if not ok:
    sys.exit("FAIL: embedder smoke")
print("PASS: real nomic-embed-text-v1.5 embedder")
PY
