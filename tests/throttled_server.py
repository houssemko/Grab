"""Throttled single-file HTTP server for Grab's lifecycle test.

Serves one file slowly (~160KB/s) so pause() always lands mid-transfer.
Usage: throttled_server.py PORT FILE
"""
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

with open(sys.argv[2], "rb") as f:
    DATA = f.read()


class H(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Length", str(len(DATA)))
        self.send_header("Content-Type", "application/octet-stream")
        self.end_headers()
        try:
            for i in range(0, len(DATA), 8192):
                self.wfile.write(DATA[i : i + 8192])
                self.wfile.flush()
                time.sleep(0.05)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def log_message(self, *a):
        pass


HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
