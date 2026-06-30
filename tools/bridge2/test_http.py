from __future__ import annotations

import os
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
from clockp_bridge2.http import get_json


class JSONHandler(BaseHTTPRequestHandler):
    def do_GET(self) -> None:
        body = b'{"ok":true}\n'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format: str, *_args: object) -> None:
        return


class HTTPTests(unittest.TestCase):
    def test_helper_requests_ignore_system_proxy(self) -> None:
        server = ThreadingHTTPServer(("127.0.0.1", 0), JSONHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(server.shutdown)
        self.addCleanup(server.server_close)
        url = f"http://127.0.0.1:{server.server_port}/healthz"
        env = {
            "HTTP_PROXY": "http://127.0.0.1:1",
            "HTTPS_PROXY": "http://127.0.0.1:1",
            "http_proxy": "http://127.0.0.1:1",
            "https_proxy": "http://127.0.0.1:1",
        }
        with mock.patch.dict(os.environ, env, clear=False):
            self.assertEqual(get_json(url, timeout=2.0), {"ok": True})


if __name__ == "__main__":
    unittest.main()
