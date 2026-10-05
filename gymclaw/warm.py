"""Warm worker behind scripts/gymclaw-tool.

Importing GymClaw's dependencies (SQLAlchemy, Alembic, Google auth, Pillow) takes ~1.3s on the VPS,
far longer than most commands. `python -m gymclaw.warm --serve SOCKET` imports them once and forks
a child per call. `python -m gymclaw.warm ARGS...` sends the call to that worker when
GYMCLAW_WARM_SOCKET points at one and runs the CLI in-process otherwise. A call keeps the
caller's argv, cwd and environment, so both paths behave the same.

Stdlib only at the top: the client must start in ~50ms.
"""
import json
import os
import socket
import sys


def run(argv: list[str]) -> tuple[int, str, str]:
    """Run the CLI with captured output. Returns (exit code, stdout, stderr)."""
    import contextlib
    import io
    import traceback

    from gymclaw.cli import main

    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        try:
            code = main(argv)
        except SystemExit as exit:  # `--help`
            code = exit.code if isinstance(exit.code, int) else (0 if exit.code is None else 1)
        except BaseException:
            traceback.print_exc()
            code = 1
    return code, out.getvalue(), err.getvalue()


def serve(path: str):
    import importlib
    import pkgutil
    import socketserver
    import time

    import gymclaw.providers
    import gymclaw.services

    for package in (gymclaw.services, gymclaw.providers):
        for module in pkgutil.iter_modules(package.__path__, f"{package.__name__}."):
            importlib.import_module(module.name)
    importlib.import_module("gymclaw.cli")
    for name in ("PIL.Image", "PIL.ImageDraw", "PIL.ImageFont"):
        importlib.import_module(name)

    class Handler(socketserver.StreamRequestHandler):
        # Runs in the forked child, so state never leaks between calls.
        def handle(self):
            request = json.loads(self.rfile.read())
            os.environ.clear()
            os.environ.update(request["env"])
            time.tzset()
            os.chdir(request["cwd"])
            sys.argv = ["gymclaw-tool", *request["argv"]]
            code, out, err = run(request["argv"])
            self.wfile.write(json.dumps({"code": code, "out": out, "err": err}).encode())

    class Server(socketserver.ForkingMixIn, socketserver.UnixStreamServer):
        pass

    if os.path.exists(path):
        os.unlink(path)
    os.umask(0o077)
    with Server(path, Handler) as server:
        print(f"gymclaw warm worker on {path}", file=sys.stderr, flush=True)
        server.serve_forever()


def call(path: str, argv: list[str]) -> tuple[int, str, str] | None:
    """Send one call to the worker. None means no worker is listening, so the caller runs it itself."""
    request = json.dumps({"argv": argv, "cwd": os.getcwd(), "env": dict(os.environ)}).encode()
    with socket.socket(socket.AF_UNIX) as conn:
        try:
            conn.connect(path)
        except (FileNotFoundError, ConnectionRefusedError):
            return None
        conn.sendall(request)
        conn.shutdown(socket.SHUT_WR)
        reply = b"".join(iter(lambda: conn.recv(1 << 16), b""))
    if not reply:
        # The worker may have died mid-call after writing. Never rerun: report it instead.
        failed = {"ok": False, "error": {"code": "CLI_FAILED", "message": "GymClaw worker dropped the call; check state before retrying"}}
        return 1, json.dumps(failed) + "\n", ""
    result = json.loads(reply)
    return result["code"], result["out"], result["err"]


def main() -> int:
    argv = sys.argv[1:]
    if argv[:1] == ["--serve"]:
        serve(argv[1])
        return 0
    path = os.environ.get("GYMCLAW_WARM_SOCKET")
    result = call(path, argv) if path else None
    if result is None:
        from gymclaw.cli import main as cli

        return cli(argv)
    code, out, err = result
    sys.stdout.write(out)
    sys.stderr.write(err)
    return code


if __name__ == "__main__":
    sys.exit(main())
