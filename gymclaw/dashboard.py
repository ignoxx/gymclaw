"""Local read-only visual demo. Serves explicit assets, never repository/private files."""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import shutil
import subprocess
from pathlib import Path
from urllib.parse import urlsplit

from gymclaw.demo import build_demo_snapshot
from gymclaw.services.illustrations import public_assets

ASSETS = {"/": ("index.html", "text/html"), "/styles.css": ("styles.css", "text/css"), "/app.js": ("app.js", "text/javascript")}


def live_snapshot() -> dict:
    """Consume private app output in memory, never inspect/copy host or sandbox DB files."""
    from gymclaw.providers.openclaw import cli_json
    launcher = Path.home() / ".local/bin/nemoclaw"
    binary = str(launcher) if launcher.is_file() else shutil.which("nemoclaw")
    if not binary:
        raise RuntimeError("NemoClaw unavailable")
    result = subprocess.run([binary, "gymclaw", "exec", "--timeout", "12", "--workdir",
        "/sandbox/.openclaw/workspace/gymclaw", "--", ".venv/bin/python", "-m", "gymclaw.dashboard_state"],
        capture_output=True, text=True, timeout=20)
    if result.returncode:
        raise RuntimeError("Sandbox state unavailable")
    value = cli_json(result.stdout)
    if value.get("demo") is not False:
        raise RuntimeError("Live state unavailable")
    return value


def demo_handler(snapshot: dict | None, *, live: bool = False):
    payload = json.dumps(snapshot, allow_nan=False).encode() if snapshot is not None else None
    root = Path(__file__).with_name("web")
    illustrations = public_assets()
    fonts = {"/fonts/BarlowCondensed-SemiBold.ttf": root / "fonts/BarlowCondensed-SemiBold.ttf",
             "/fonts/DM-Sans.ttf": root / "fonts/DM-Sans.ttf"}
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            route = urlsplit(self.path).path
            if live and (self.headers.get("Host") not in {f"127.0.0.1:{self.server.server_port}", f"localhost:{self.server.server_port}"}
                         or self.headers.get("Sec-Fetch-Site") == "cross-site"):
                self.send_error(403)
                return
            if route == "/api/state" or (route == "/api/demo" and not live):
                if live:
                    try:
                        body = json.dumps(live_snapshot(), allow_nan=False).encode()
                    except Exception:
                        self.send_error(503, "Live state unavailable")
                        return
                else:
                    body = payload
                mime = "application/json"
            elif route in illustrations:
                body, mime = illustrations[route].read_bytes(), "image/svg+xml"
            elif route in fonts:
                body, mime = fonts[route].read_bytes(), "font/ttf"
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
            self.send_header("Cross-Origin-Resource-Policy", "same-origin")
            self.send_header("Content-Security-Policy", "default-src 'self'; style-src 'self'; script-src 'self'; connect-src 'self'; img-src 'self'; font-src 'self'; frame-ancestors 'none'")
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            self.send_error(405, "Read-only demo")

        def log_message(self, format, *args):
            pass
    return Handler


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int)
    parser.add_argument("--live", action="store_true", help="Read authoritative sandbox state, no writes or additional poller")
    args = parser.parse_args()
    port = args.port or (8766 if args.live else 8765)
    server = ThreadingHTTPServer(("127.0.0.1", port), demo_handler(None if args.live else build_demo_snapshot(), live=args.live))
    mode = "read-only sandbox state" if args.live else "synthetic demo"
    print(f"GymClaw: http://127.0.0.1:{port}, {mode}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
