import pytest


@pytest.fixture(autouse=True)
def block_live_http(monkeypatch):
    """Tests must inject providers/transports; never call Google or other live APIs."""
    def blocked(*args, **kwargs):
        raise RuntimeError("Live HTTP disabled in tests")
    monkeypatch.setattr("requests.sessions.Session.request", blocked)
