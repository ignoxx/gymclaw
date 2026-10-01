import json
from collections import deque
from datetime import datetime, timedelta, timezone

import pytest
import requests
from google.oauth2.credentials import Credentials
from sqlalchemy.orm import Session

from gymclaw.db import initialize, make_engine
from gymclaw.models import CalendarCredential
from gymclaw.providers.calendar import CalendarConflict, SyncTokenExpired, parse_event
from gymclaw.providers.google_auth import SCOPES, authenticate, bind_calendar, load_credentials
from gymclaw.providers.google_calendar import GoogleCalendarProvider
from gymclaw.services.errors import DomainError


def response(status=200, body=None):
    res = requests.Response()
    res.status_code = status
    res._content = json.dumps(body or {}).encode()
    return res


class Transport:
    def __init__(self, *responses):
        self.responses = deque(responses)
        self.calls = []

    def request(self, method, url, **kwargs):
        self.calls.append((method, url, {k: dict(v) if isinstance(v, dict) else v for k, v in kwargs.items()}))
        res = self.responses.popleft()
        if isinstance(res, Exception):
            raise res
        return res


def raw_event(event_id="owned", session_id="s", **changes):
    event = {"id": event_id, "etag": '"v1"', "summary": "Gym", "start": {"dateTime": "2026-10-14T20:00:00+02:00"}, "end": {"dateTime": "2026-10-14T21:10:00+02:00"}, "extendedProperties": {"private": {"gymclawManaged": "true", "gymclawSessionId": session_id, "gymclawPlanRevision": "1"}}}
    return event | changes


def test_pagination_keeps_sync_params():
    transport = Transport(response(body={"timeZone": "Europe/Berlin", "items": [raw_event()], "nextPageToken": "page"}), response(body={"items": [], "nextSyncToken": "new"}))
    provider = GoogleCalendarProvider("only-this@group.calendar.google.com", transport)
    batch = provider.incremental_sync("old")
    assert len(batch.events) == 1 and batch.next_sync_token == "new" and not batch.full
    first, second = [c[2]["params"] for c in transport.calls]
    assert first == {"showDeleted": "true", "singleEvents": "false", "maxResults": 2500, "syncToken": "old"}
    assert second == first | {"pageToken": "page"}
    assert all("only-this%40group.calendar.google.com" in c[1] for c in transport.calls)
    assert not {"timeMin", "timeMax", "orderBy"} & first.keys()


def test_expired_token_and_safe_errors():
    with pytest.raises(SyncTokenExpired):
        GoogleCalendarProvider("calendar", Transport(response(410))).incremental_sync("old")
    with pytest.raises(DomainError) as error:
        GoogleCalendarProvider("calendar", Transport(requests.ConnectionError("SECRET_URL_AND_TOKEN"))).incremental_sync(None)
    assert "SECRET" not in str(error.value)


def test_all_day_exclusive_end_and_minimal_deletion():
    event = parse_event({"id": "travel", "start": {"date": "2026-10-12"}, "end": {"date": "2026-10-19"}, "description": "unnecessary private data", "attendees": [{"email": "private@example.com"}]}, "Europe/Berlin")
    assert event.start == datetime(2026, 10, 11, 22, tzinfo=timezone.utc)
    assert event.end == datetime(2026, 10, 18, 22, tzinfo=timezone.utc)
    assert "description" not in event.raw and "attendees" not in event.raw
    assert parse_event({"id": "deleted", "status": "cancelled"}, "Europe/Berlin").start is None


def test_create_retry_verifies_ownership_and_does_not_duplicate():
    transport = Transport(response(409), response(body=raw_event()))
    provider = GoogleCalendarProvider("calendar", transport)
    assert provider.create_gym_event(raw_event()).id == "owned"
    assert [c[0] for c in transport.calls] == ["POST", "GET"]
    with pytest.raises(DomainError, match="ownership"):
        GoogleCalendarProvider("calendar", Transport(response(409), response(body=raw_event(session_id="other")))).create_gym_event(raw_event())


def test_update_guard_and_if_match():
    transport = Transport(response(body=raw_event()), response(body=raw_event(etag='"v2"')))
    provider = GoogleCalendarProvider("calendar", transport)
    assert provider.update_gym_event("owned", raw_event(), etag='"v1"').etag == '"v2"'
    assert transport.calls[1][0] == "PATCH"
    assert transport.calls[1][2]["headers"] == {"If-Match": '"v1"'}
    with pytest.raises(CalendarConflict):
        GoogleCalendarProvider("calendar", Transport(response(body=raw_event(etag='"v2"', start={"dateTime": "2026-10-14T20:30:00+02:00"})))).update_gym_event("owned", raw_event(), etag='"v1"')
    with pytest.raises(DomainError, match="ownership"):
        GoogleCalendarProvider("calendar", Transport(response(body=raw_event(session_id="other")))).delete_gym_event("owned", etag='"v1"', session_id="s")


def test_oauth_private_sqlite_and_calendar_binding(tmp_path, monkeypatch):
    path = tmp_path / "private.db"
    engine = make_engine(f"sqlite:///{path}")
    initialize(engine)
    client = tmp_path / "client.json"
    client.write_text('{"installed": {"client_id": "fixture-client"}}')
    credentials = Credentials(token="fixture-access", refresh_token="fixture-refresh", client_id="fixture-client", client_secret="fixture-secret", token_uri="https://oauth2.googleapis.com/token", scopes=SCOPES, expiry=datetime.now(timezone.utc).replace(tzinfo=None) + timedelta(hours=1))

    class Flow:
        def run_local_server(self, **kwargs):
            assert kwargs["authorization_prompt_message"] is None
            assert kwargs["host"] == "127.0.0.1"
            assert kwargs["access_type"] == "offline"
            return credentials

    monkeypatch.setattr("gymclaw.providers.google_auth.InstalledAppFlow.from_client_config", lambda *args, **kwargs: Flow())
    with Session(engine) as db, db.begin():
        result = authenticate(db, "dedicated", client_path=client)
        assert result["calendar_events_changed"] is False
        assert "fixture-secret" not in json.dumps(result)
        assert path.stat().st_mode & 0o777 == 0o600
    with Session(engine) as db:
        assert db.get(CalendarCredential, 1).credentials_json["refresh_token"] == "fixture-refresh"
        assert load_credentials(db).token == "fixture-access"
        with pytest.raises(DomainError, match="another calendar"):
            bind_calendar(db, "different", "google")
        with pytest.raises(DomainError):
            bind_calendar(db, "dedicated", "fixture")
    engine.dispose()
