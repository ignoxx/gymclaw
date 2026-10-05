import json
import os
import subprocess
import sys
import tempfile
import time

def tool(cwd, env, *args):
    # What scripts/gymclaw-tool runs, minus its repo .venv (CI has none).
    result = subprocess.run([sys.executable, "-m", "gymclaw.warm", *args], capture_output=True, text=True, cwd=cwd, env=env)
    return result.returncode, json.loads(result.stdout)


def test_warm_worker_matches_direct_calls(tmp_path):
    # AF_UNIX paths are capped at ~104 chars, too short for pytest's tmp_path on macOS.
    with tempfile.TemporaryDirectory(dir="/tmp") as short:
        sock = f"{short}/w.sock"
        worker = subprocess.Popen([sys.executable, "-m", "gymclaw.warm", "--serve", sock], stderr=subprocess.DEVNULL)
        try:
            while not os.path.exists(sock):
                time.sleep(0.05)
            # Relative DB URL: the worker must use the caller's cwd and environment.
            env = os.environ | {"GYMCLAW_DB_URL": "sqlite:///state.db", "GYMCLAW_WARM_SOCKET": sock}
            assert tool(tmp_path, env, "db", "init") == (0, {"ok": True, "data": {"initialized": True}, "events": [], "user_message_hint": None})
            assert tool(tmp_path, env, "profile", "update", "--data", '{"prep_minutes":25}')[1]["data"]["prep_minutes"] == 25
            code, error = tool(tmp_path, env, "crowd", "nope")
            assert code == 1 and error["error"]["code"] == "INVALID_INPUT"
        finally:
            worker.terminate()
            worker.wait()
        # Worker gone: same call runs in-process against the same state.
        assert tool(tmp_path, env, "profile", "get")[1]["data"]["prep_minutes"] == 25
