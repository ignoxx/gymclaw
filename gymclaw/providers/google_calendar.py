"""Google Calendar v3 REST adapter. No implicit primary calendar or live fixture calls.

Sync uses stable list params and pages without time filters. Recurring masters and
exceptions are expanded locally; syncToken cannot be combined with timeMin/timeMax.
"""
from collections.abc import Callable
from typing import Protocol
from urllib.parse import quote

import requests
from google.auth.transport.requests import AuthorizedSession
from sqlalchemy.orm import Session

from gymclaw.models import CalendarCredential, CalendarSyncState
from gymclaw.providers.calendar import CalendarConflict, CalendarEvent, SyncBatch, SyncTokenExpired, parse_event, parse_time
from gymclaw.providers.google_auth import bind_calendar, load_credentials
from gymclaw.services.errors import DomainError


class Transport(Protocol):
    def request(self, method: str, url: str, **kwargs) -> requests.Response: ...


class GoogleCalendarProvider:
    source = "google"

    def __init__(self, calendar_id: str, transport: Transport, *, timezone: str = "Europe/Berlin", persist_credentials: Callable[[], None] | None = None):
        if not calendar_id or calendar_id == "primary":
            raise DomainError("CALENDAR_ID_REQUIRED", "Dedicated calendar ID required")
        self.calendar_id = calendar_id
        self.transport = transport
        self.timezone = timezone
        self.persist_credentials = persist_credentials
        self.url = f"https://www.googleapis.com/calendar/v3/calendars/{quote(calendar_id, safe='')}/events"

    def _request(self, method: str, suffix: str = "", **kwargs) -> requests.Response:
        try:
            response = self.transport.request(method, self.url + suffix, timeout=30, **kwargs)
            if self.persist_credentials:
                self.persist_credentials()
            return response
        except Exception as error:
            # Do not expose URL/query params, sync tokens or credentials in failures.
            raise DomainError("CALENDAR_UNAVAILABLE", "Calendar request failed; sync/retry before further changes.") from error

    @staticmethod
    def _check(response):
        if response.status_code == 412:
            raise CalendarConflict()
        if response.status_code == 401:
            raise DomainError("CALENDAR_AUTH_REQUIRED", "Google authorization expired; run calendar auth")
        if response.status_code >= 400:
            raise DomainError("CALENDAR_UNAVAILABLE", f"Google Calendar request failed (HTTP {response.status_code}); retry later")

    def incremental_sync(self, sync_token: str | None) -> SyncBatch:
        params = {"showDeleted": "true", "singleEvents": "false", "maxResults": 2500}
        if sync_token:
            params["syncToken"] = sync_token
        events = []
        seen_pages = set()
        while True:
            response = self._request("GET", params=params)
            if response.status_code == 410:
                raise SyncTokenExpired()
            self._check(response)
            raw = response.json()
            self.timezone = raw.get("timeZone", self.timezone)
            events.extend(parse_event(e, self.timezone) for e in raw.get("items", []))
            page = raw.get("nextPageToken")
            if not page:
                token = raw.get("nextSyncToken")
                if not token:
                    raise DomainError("CALENDAR_INVALID_RESPONSE", "Google sync response missing nextSyncToken")
                return SyncBatch(events=tuple(events), next_sync_token=token, timezone=self.timezone, full=sync_token is None)
            if page in seen_pages:
                raise DomainError("CALENDAR_INVALID_RESPONSE", "Google sync repeated page token")
            seen_pages.add(page)
            params["pageToken"] = page

    def get_event(self, event_id: str) -> CalendarEvent | None:
        response = self._request("GET", "/" + quote(event_id, safe=""))
        if response.status_code in {404, 410}:
            return None
        self._check(response)
        return parse_event(response.json(), self.timezone)

    @staticmethod
    def _owned(event: CalendarEvent, session_id: str | None):
        if not event.managed or not session_id or event.session_id != session_id:
            raise DomainError("CALENDAR_NOT_OWNED", "Refusing to modify event without matching GymClaw ownership")

    def create_gym_event(self, body: dict) -> CalendarEvent:
        intended = parse_event(body, self.timezone)
        self._owned(intended, intended.session_id)
        response = self._request("POST", json=body)
        if response.status_code == 409:
            existing = self.get_event(body["id"])
            if existing is None or existing.status == "cancelled":
                raise DomainError("CALENDAR_EVENT_DELETED", "Deleted event ID will not be recreated")
            self._owned(existing, intended.session_id)
            return existing
        self._check(response)
        return parse_event(response.json(), self.timezone)

    def update_gym_event(self, event_id: str, body: dict, *, etag: str) -> CalendarEvent:
        event = self.get_event(event_id)
        if event is None or event.status == "cancelled":
            raise DomainError("CALENDAR_EVENT_DELETED", "Event was deleted; sync before updating")
        session_id = body.get("extendedProperties", {}).get("private", {}).get("gymclawSessionId")
        self._owned(event, session_id)
        if event.etag != etag:
            private = body.get("extendedProperties", {}).get("private", {})
            already_applied = str(event.revision) == private.get("gymclawPlanRevision") and all(key not in body or getattr(event, key) == parse_time(body[key], self.timezone) for key in ("start", "end"))
            if already_applied:
                return event  # Response lost after successful patch: acknowledge, don't duplicate.
            raise CalendarConflict()
        response = self._request("PATCH", "/" + quote(event_id, safe=""), json={k: v for k, v in body.items() if k != "id"}, headers={"If-Match": etag})
        self._check(response)
        return parse_event(response.json(), self.timezone)

    def delete_gym_event(self, event_id: str, *, etag: str, session_id: str) -> None:
        event = self.get_event(event_id)
        if event is None or event.status == "cancelled":
            return
        self._owned(event, session_id)
        if event.etag != etag:
            raise CalendarConflict()
        response = self._request("DELETE", "/" + quote(event_id, safe=""), headers={"If-Match": etag})
        if response.status_code not in {404, 410}:
            self._check(response)


def google_provider(db: Session, calendar_id: str | None = None) -> GoogleCalendarProvider:
    state = db.get(CalendarSyncState, 1)
    if calendar_id is None:
        if state is None:
            raise DomainError("CALENDAR_ID_REQUIRED", "Authenticate dedicated calendar first")
        calendar_id = state.calendar_id
    state = bind_calendar(db, calendar_id, "google")
    credentials = load_credentials(db)

    def persist():
        import json
        db.get(CalendarCredential, 1).credentials_json = json.loads(credentials.to_json())
        db.flush()

    return GoogleCalendarProvider(calendar_id, AuthorizedSession(credentials), timezone=state.timezone, persist_credentials=persist)
