"""Serve the built dashboard with hand-made data, without a taskr daemon."""
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

root = Path(__file__).resolve().parents[2]


class Showcase(SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(root / "web/dist"), **kwargs)

    def do_GET(self):
        if self.path == "/api/state":
            body = (root / "docs/screenshots/state.json").read_bytes()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        else:
            super().do_GET()


ThreadingHTTPServer(("127.0.0.1", 7799), Showcase).serve_forever()
