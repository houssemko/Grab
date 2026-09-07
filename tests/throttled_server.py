"""Throttled single-file HTTP server for Grab's lifecycle test.

Serves one file slowly (~160KB/s) so pause() always lands mid-transfer.
Honors Range requests with 206 (exercises real resume); logs each
request's range (or "full") for assertions.
Usage: throttled_server.py PORT FILE RANGELOG
"""
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

with open(sys.argv[2], "rb") as f:
    DATA = f.read()
RANGELOG = sys.argv[3]


class H(BaseHTTPRequestHandler):
    def do_GET(self):
        requested = self.headers.get("Range")
        with open(RANGELOG, "a") as log:
            log.write((requested or "full") + "\n")
        start = 0
        if requested and requested.startswith("bytes="):
            try:
                start = int(requested[len("bytes="):].split("-")[0])
            except ValueError:
                start = 0
        if start >= len(DATA):
            self.send_response(416)
            self.send_header("Content-Range", f"bytes */{len(DATA)}")
            self.end_headers()
            return
        body = DATA[start:]
        if start > 0:
            self.send_response(206)
            self.send_header(
                "Content-Range", f"bytes {start}-{len(DATA) - 1}/{len(DATA)}"
            )
        else:
            self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Content-Type", "application/octet-stream")
        self.end_headers()
        try:
            for i in range(0, len(body), 8192):
                self.wfile.write(body[i : i + 8192])
                self.wfile.flush()
                time.sleep(0.05)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def log_message(self, *a):
        pass


HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
