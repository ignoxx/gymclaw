"""Desktop OAuth; private SQLite storage, never secrets in JSON tool results."""
import json
import logging
from pathlib import Path

from google.auth.transport.requests import Request
from google.oauth2.credentials import Credentials
from google_auth_oauthlib.flow import InstalledAppFlow
from sqlalchemy.engine import make_url
from sqlalchemy.orm import Session

from gymclaw.models import CalendarCredential, CalendarSyncState
from gymclaw.services.errors import DomainError

SCOPES = ("https://www.googleapis.com/auth/calendar.events.owned",)
DEFAULT_CLIENT = Path.home() / ".config/gymclaw/google-client.json"


def bind_calendar(db: Session, calendar_id: str, source: str) -> CalendarSyncState:
    if not calendar_id or calendar_id == "primary":
        raise DomainError("CALENDAR_ID_REQUIRED", "Specify dedicated calendar ID; primary is not an implicit fallback")
    state = db.get(CalendarSyncState, 1)
    if state and (state.calendar_id != calendar_id or state.source != source):
        raise DomainError("CALENDAR_SCOPE_MISMATCH", "DB is bound to another calendar/provider; use a separate DB")
    if state is None:
        state = CalendarSyncState(id=1, calendar_id=calendar_id, source=source)
        db.add(state)
        db.flush()
    return state


def private_storage(db: Session):
    database = make_url(str(db.get_bind().url)).database
    if not database or database == ":memory:":
        raise DomainError("AUTH_STORAGE_REQUIRED", "OAuth requires a persistent private SQLite DB")
    Path(database).chmod(0o600)


def authenticate(db: Session, calendar_id: str, *, client_path: Path = DEFAULT_CLIENT) -> dict:
    bind_calendar(db, calendar_id, "google")
    private_storage(db)
    try:
        client = json.loads(client_path.expanduser().read_text())
        if "installed" not in client:
            raise ValueError("Desktop client required")
        flow = InstalledAppFlow.from_client_config(client, scopes=SCOPES)
        # Callback URLs contain authorization codes; silence library HTTP logging.
        logger = logging.getLogger("google_auth_oauthlib.flow")
        was_disabled = logger.disabled
        logger.disabled = True
        try:
            credentials = flow.run_local_server(host="127.0.0.1", port=0, authorization_prompt_message=None, success_message="GymClaw authorized. Close this tab and return to terminal.", timeout_seconds=180, access_type="offline", prompt="consent", include_granted_scopes="false")
        finally:
            logger.disabled = was_disabled
        if not credentials.refresh_token or not credentials.has_scopes(SCOPES) or credentials.granted_scopes is not None and not set(SCOPES) <= set(credentials.granted_scopes):
            raise ValueError("Offline authorization missing")
        row = db.get(CalendarCredential, 1)
        if row is None:
            row = CalendarCredential(id=1)
            db.add(row)
        row.credentials_json = json.loads(credentials.to_json())
        db.flush()
    except Exception as error:
        # OAuth failures can contain codes, tokens, URLs or client secrets.
        raise DomainError("CALENDAR_AUTH_FAILED", "Google authorization failed. Check desktop client, test-user access and browser consent; retry.") from error
    return {"authorized": True, "calendar_id": calendar_id, "scopes": list(SCOPES), "storage": "private_sqlite", "calendar_events_changed": False}


def load_credentials(db: Session) -> Credentials:
    row = db.get(CalendarCredential, 1)
    if row is None:
        raise DomainError("CALENDAR_AUTH_REQUIRED", "Run calendar auth before Google sync")
    private_storage(db)
    try:
        credentials = Credentials.from_authorized_user_info(row.credentials_json)
        if not credentials.has_scopes(SCOPES):
            raise ValueError("Scopes missing")
        if not credentials.valid:
            if not credentials.refresh_token:
                raise ValueError("Refresh token missing")
            credentials.refresh(Request())
            row.credentials_json = json.loads(credentials.to_json())
            db.flush()
    except Exception as error:
        raise DomainError("CALENDAR_AUTH_REQUIRED", "Google credentials expired or invalid; run calendar auth again") from error
    return credentials
