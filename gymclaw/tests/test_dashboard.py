import http.client
import json
from threading import Thread
from http.server import ThreadingHTTPServer

from gymclaw.dashboard import demo_handler
from gymclaw.demo import build_demo_snapshot


def test_demo_uses_real_domain_with_explicit_isolation():
    data = build_demo_snapshot()
    assert data["demo"] and data["live_database_accessed"] is False
    assert len(data["days"]) == 7 and data["days"][0]["today"]
    assert data["days"][0]["session"]["status"] == "STARTED"
    workout = data["workout"]
    assert workout["active"]["set_number"] == 2 and workout["rest_until"] and workout["last_set"] == "11 × 60 kg"
    assert all(e["svg_url"] for e in workout["exercises"])
    assert data["next"]["template"] and all(e["svg_url"] for e in data["next"]["exercises"])
    assert data["crowd"]["readings"] and len(data["crowd"]["typical"]) >= 2
    # Weeks of finished demo workouts feed history and insights.
    assert len(data["history"]) >= 10 and data["insights"]["progress"] and data["insights"]["heatmap"]
    assert data["crowd"]["health"]["status"] == "ok"


def test_demo_http_serves_only_public_assets_and_rejects_writes():
    # Loopback fixture only; no external provider/model requests.
    server = ThreadingHTTPServer(("127.0.0.1", 0), demo_handler({"demo": True}))
    thread = Thread(target=server.serve_forever, daemon=True)
    thread.start()
    client = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=3)
    try:
        client.request("GET", "/api/demo")
        response = client.getresponse()
        assert response.status == 200 and json.loads(response.read())["demo"]
        client.request("GET", "/")
        response = client.getresponse()
        assert response.status == 200 and b"gymclaw" in response.read()
        assert "frame-ancestors 'none'" in response.getheader("Content-Security-Policy")
        for path in ("/../README.md", "/data/private.sqlite", "/unknown"):
            client.request("GET", path)
            response = client.getresponse()
            assert response.status == 404
            response.read()
        client.request("POST", "/api/demo", body="{}")
        response = client.getresponse()
        assert response.status == 405
        response.read()
    finally:
        client.close()
        server.shutdown()
        server.server_close()
        thread.join(timeout=3)
