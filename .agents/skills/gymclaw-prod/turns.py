"""Format a GymClaw turn log (JSONL on stdin) as a readable timeline. Used by `prod turns`."""
import json
import os
import sys
from datetime import datetime
from zoneinfo import ZoneInfo

tz, show_all = ZoneInfo(os.environ["TZ_LOCAL"]), os.environ["ALL"] == "1"
beat = set()  # runs triggered by the heartbeat
for line in sys.stdin:
    e = json.loads(line)
    ev, run = e.get("ev"), e.get("run")
    if e.get("trigger") == "heartbeat":
        beat.add(run)
    if (not show_all and run in beat) or ev in ("llm_input", "llm_output"):
        continue
    t = datetime.fromisoformat(e["ts"].replace("Z", "+00:00")).astimezone(tz).strftime("%H:%M:%S")
    g = e.get
    if ev == "inbound":
        s = f"<< {g('text', '')}"
    elif ev == "outbound":
        s = f">> {g('text', '')}" + ("" if g("ok", True) else "  [FAILED]")
    elif ev == "tool":
        s = f"   tool {g('name')} {g('ms')}ms ok={g('ok')} {json.dumps(g('args'), ensure_ascii=False)[:200]}"
    elif ev == "model_call":
        s = f"   model {g('ms')}ms ttfb={g('ttfbMs')}ms {g('outcome')}"
    elif ev == "run_end":
        s = f"   run_end {g('ms')}ms ok={g('ok')} trigger={g('trigger')} reply={str(g('reply', ''))[:120]!r}"
    elif ev == "fast":
        s = f"   fast {g('flow')}/{g('action')} {g('ms')}ms ok={g('ok')}"
    else:
        s = f"   {ev} " + json.dumps({k: v for k, v in e.items() if k not in ("ts", "ev")}, ensure_ascii=False)[:200]
    print(t, s)
