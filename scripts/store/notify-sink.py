#!/usr/bin/env python3
"""Loopback webhook sink for RustFS bucket notifications (test helper, ADR-126 S2).

Listens on 127.0.0.1:<port>, answers 200 to every POST, and appends the body to <file> as one JSON
line (compacted; a body that is not JSON is written as a JSON string). The L1 follower relay test
(`crates/weftos-store/tests/s2_follower_rustfs.rs`) reads that file as the site subscriber's feed.
Loopback only; stop it with Ctrl-C or kill. Never point a production RustFS at it.

  scripts/store/notify-sink.py 19411 /path/to/events.jsonl
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


def main() -> None:
    port, path = int(sys.argv[1]), sys.argv[2]

    class H(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            body = self.rfile.read(int(self.headers.get("content-length", "0")))
            try:
                line = json.dumps(json.loads(body), separators=(",", ":"))
            except ValueError:
                line = json.dumps(body.decode("utf-8", "replace"))
            with open(path, "a", encoding="utf-8") as f:
                f.write(line + "\n")
            self.send_response(200)
            self.send_header("content-length", "0")
            self.end_headers()

        def log_message(self, *a) -> None:  # quiet
            pass

    HTTPServer(("127.0.0.1", port), H).serve_forever()


if __name__ == "__main__":
    main()
