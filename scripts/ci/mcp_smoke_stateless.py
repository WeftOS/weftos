#!/usr/bin/env python3
"""Stateless (MCP 2026-07-28) smoke test for `weft mcp-server` over stdio.

No `initialize`: probes `server/discover`, then sends `tools/list` and
`tools/call` with the protocol version in `_meta`, and checks that an
unsupported version is refused with the supported list. Companion to
mcp_smoke.py, which keeps covering the 2025-06-18 handshake path.

Exit code: 0 on success, 1 on any failed assertion.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time

BIN = os.environ.get("WEFT_BIN", "target/debug/weft")
TIMEOUT_SECS = 30
VERSION = "2026-07-28"
META_VERSION = "io.modelcontextprotocol/protocolVersion"


def send(proc: subprocess.Popen, msg_id: int, method: str, params: dict) -> None:
    line = json.dumps({"jsonrpc": "2.0", "id": msg_id, "method": method, "params": params})
    assert proc.stdin is not None
    proc.stdin.write((line + "\n").encode("utf-8"))
    proc.stdin.flush()


def recv(proc: subprocess.Popen) -> dict:
    assert proc.stdout is not None
    deadline = time.monotonic() + TIMEOUT_SECS
    while time.monotonic() < deadline:
        raw = proc.stdout.readline()
        if not raw:
            raise RuntimeError("mcp-server closed stdout before responding")
        try:
            obj = json.loads(raw.decode("utf-8", errors="replace").strip())
        except json.JSONDecodeError:
            continue
        if isinstance(obj, dict) and ("result" in obj or "error" in obj):
            return obj
    raise TimeoutError(f"mcp-server timed out after {TIMEOUT_SECS}s")


def result_of(resp: dict, label: str) -> dict:
    assert "error" not in resp, f"{label}: server returned error: {resp['error']}"
    return resp["result"]


def meta() -> dict:
    return {"_meta": {META_VERSION: VERSION}}


def main() -> int:
    if not os.path.isfile(BIN) or not os.access(BIN, os.X_OK):
        print(f"FAIL: WEFT_BIN={BIN} is not an executable file", file=sys.stderr)
        return 1
    proc = subprocess.Popen(
        [BIN, "mcp-server"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env={**os.environ, "RUST_LOG": "warn"},
    )
    try:
        # 1. server/discover, with no prior handshake.
        send(proc, 1, "server/discover", meta())
        disc = result_of(recv(proc), "server/discover")
        assert VERSION in disc["supportedVersions"], f"discover: {disc}"
        assert disc["resultType"] == "complete", f"discover: {disc}"
        assert isinstance(disc["capabilities"], dict), f"discover: {disc}"
        print(f"[stateless] discover OK; versions={disc['supportedVersions']}")

        # 2. tools/list, deterministic order, cache fields.
        send(proc, 2, "tools/list", meta())
        listed = result_of(recv(proc), "tools/list")
        names = [t["name"] for t in listed["tools"]]
        assert names, "tools/list: no tools"
        assert names == sorted(names), f"tools/list: not sorted: {names[:5]}"
        assert listed["resultType"] == "complete", f"tools/list: {listed}"
        assert "ttlMs" in listed and "cacheScope" in listed, f"tools/list: {listed}"
        print(f"[stateless] tools/list OK; {len(names)} tools")

        # 3. tools/call against a tool that needs no setup.
        target = next((n for n in names if n.endswith("read_file")), names[0])
        args = {"path": "Cargo.toml"} if target.endswith("read_file") else {}
        send(proc, 3, "tools/call", {"name": target, "arguments": args, **meta()})
        called = result_of(recv(proc), f"tools/call {target}")
        assert called["resultType"] == "complete", f"tools/call: {called}"
        assert isinstance(called["content"], list), f"tools/call: {called}"
        print(f"[stateless] tools/call {target} OK")

        # 4. Unsupported version is refused, not served.
        send(proc, 4, "tools/list", {"_meta": {META_VERSION: "1900-01-01"}})
        err = recv(proc).get("error")
        assert err and err["code"] == -32022, f"unsupported version: {err}"
        assert VERSION in err["data"]["supported"], f"unsupported version: {err}"
        print("[stateless] unsupported version refused OK")

        print("[stateless] PASS")
        return 0
    except (AssertionError, KeyError, TimeoutError, RuntimeError) as e:
        print(f"FAIL: {e!r}", file=sys.stderr)
        if proc.stderr is not None:
            proc.kill()
            sys.stderr.write(proc.stderr.read().decode("utf-8", errors="replace"))
        return 1
    finally:
        try:
            proc.stdin.close()  # type: ignore[union-attr]
            proc.terminate()
            proc.wait(timeout=5)
        except Exception:
            proc.kill()


if __name__ == "__main__":
    sys.exit(main())
