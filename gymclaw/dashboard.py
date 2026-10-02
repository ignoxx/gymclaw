"""Local read-only visual demo. Serves explicit assets, never repository/private files."""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
from urllib.parse import urlsplit

from gymclaw.demo import build_demo_snapshot

ASSETS = {"/": ("index.html", "text/html"), "/styles.css": ("styles.css", "text/css"), "/app.js": ("app.js", "text/javascript")}


def demo_handler(snapshot: dict):
    payload = json.dumps(snapshot, allow_nan=False).encode()
    root = Path(__file__).with_name("web")
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            route = urlsplit(self.path).path
            if route == "/api/demo":
                body, mime = payload, "application/json"
            elif route in ASSETS:
                filename, mime = ASSETS[route]
                body = (root / filename).read_bytes()
            else:
                self.send_error(404)
                return
            self.send_response(200)
            self.send_header("Content-Type", mime + "; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.send_header("Content-Security-Policy", "default-src 'self'; style-src 'self'; script-src 'self'; connect-src 'self'; img-src 'self'; frame-ancestors 'none'")
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            self.send_error(405, "Read-only demo")

        def log_message(self, format, *args):
            pass
    return Handler


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=8765)
    args = parser.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), demo_handler(build_demo_snapshot()))
    print(f"GymClaw offline demo: http://127.0.0.1:{args.port} — synthetic data only", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
