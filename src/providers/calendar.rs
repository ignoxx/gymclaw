//! Calendar provider contract and minimal normalized Google event payloads (port of `providers/calendar.py`).
use serde::Serialize;
use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::time::{Ts, zone};

/// `error.code()` when a sync token is no longer accepted; the caller restarts with a full sync.
pub const SYNC_TOKEN_EXPIRED: &str = "CALENDAR_SYNC_TOKEN_EXPIRED";
/// `error.code()` when the remote event changed under us (etag mismatch / HTTP 412).
pub const WRITE_CONFLICT: &str = "CALENDAR_WRITE_CONFLICT";

pub fn sync_token_expired() -> Error {
    Error::domain(SYNC_TOKEN_EXPIRED, "Calendar requires fresh synchronization")
}

pub fn conflict() -> Error {
    Error::domain(WRITE_CONFLICT, "Calendar event changed remotely; sync before retry")
}

/// A Google Calendar event, reduced to what GymClaw stores. `raw` keeps only the whitelisted keys
/// (no descriptions, attendees, …) and is what goes into `calendar_event_snapshot.raw_json`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CalendarEvent {
    pub id: String,
    pub etag: Option<String>,
    pub title: String,
    /// Google status: "confirmed", "tentative" or "cancelled".
    pub status: String,
    /// None only for cancelled events.
    pub start: Option<Ts>,
    pub end: Option<Ts>,
    pub updated: Option<Ts>,
    pub ical_uid: Option<String>,
    pub managed: bool,
    pub session_id: Option<String>,
    pub revision: Option<i64>,
    pub raw: Map<String, Value>,
}

impl CalendarEvent {
    pub fn cancelled(&self) -> bool {
        self.status == "cancelled"
    }
}

/// One `incremental_sync` result: every changed event since `sync_token` (all events when `full`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SyncBatch {
    pub events: Vec<CalendarEvent>,
    pub next_sync_token: String,
    pub timezone: String,
    pub full: bool,
}

/// A calendar GymClaw syncs from and writes its gym sessions to. Implemented by
/// `GoogleCalendarProvider` (live) and `FixtureCalendarProvider` (tests/demos).
/// Bodies are Google event resources (`calendar_write.body_json`).
pub trait CalendarProvider {
    fn calendar_id(&self) -> &str;
    /// "google" or "fixture" (`calendar_sync_state.source`).
    fn source(&self) -> &'static str;
    /// Changes since `sync_token`, or everything when None. Fails with `SYNC_TOKEN_EXPIRED` when the token is stale.
    fn incremental_sync(&mut self, sync_token: Option<&str>) -> Result<SyncBatch>;
    fn get_event(&mut self, event_id: &str) -> Result<Option<CalendarEvent>>;
    /// Idempotent on `body["id"]`: an existing event we own is returned instead of duplicated.
    fn create_gym_event(&mut self, body: &Value) -> Result<CalendarEvent>;
    /// Patch with If-Match `etag`. A lost response for an already applied patch is acknowledged, not a conflict.
    fn update_gym_event(&mut self, event_id: &str, body: &Value, etag: &str) -> Result<CalendarEvent>;
    /// Delete with If-Match `etag`; already gone is fine.
    fn delete_gym_event(&mut self, event_id: &str, etag: &str, session_id: &str) -> Result<()>;
}

fn invalid(message: &str) -> Error {
    Error::domain("CALENDAR_INVALID_RESPONSE", message.to_string())
}

/// Google `{"dateTime": …}` / `{"date": …}` (+ optional `timeZone`) to an instant. Naive date-times
/// and all-day dates are read in the event's or else the calendar's zone.
pub fn parse_time(value: &Value, calendar_timezone: &str) -> Result<Ts> {
    let tz = zone(value.get("timeZone").and_then(Value::as_str).unwrap_or(calendar_timezone))?;
    if let Some(stamp) = value.get("dateTime") {
        return Ts::parse(stamp.as_str().ok_or_else(|| invalid("Calendar event dateTime must be text"))?, Some(&tz));
    }
    if let Some(day) = value.get("date") {
        let day: jiff::civil::Date = day.as_str().ok_or_else(|| invalid("Calendar event date must be text"))?.trim().parse()?;
        return Ok(Ts(day.to_zoned(tz)?.timestamp()));
    }
    Err(invalid("Calendar event time must contain dateTime or date"))
}

const KEEP: [&str; 12] = [
    "id",
    "etag",
    "summary",
    "start",
    "end",
    "status",
    "updated",
    "iCalUID",
    "transparency",
    "recurrence",
    "recurringEventId",
    "originalStartTime",
];
const PRIVATE_KEEP: [&str; 3] = ["gymclawManaged", "gymclawSessionId", "gymclawPlanRevision"];

/// `extendedProperties.private` of a Google event resource.
pub fn private_props(raw: &Value) -> Option<&Map<String, Value>> {
    raw.get("extendedProperties")?.get("private")?.as_object()
}

/// Python `int(value)` for the plan revision; anything unparsable is None.
fn revision(value: &Value) -> Option<i64> {
    match value {
        Value::String(s) => s.trim().replace('_', "").parse().ok(),
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)),
        _ => None,
    }
}

/// Normalize a Google event resource. Cancelled events carry no times.
pub fn parse_event(raw: &Value, calendar_timezone: &str) -> Result<CalendarEvent> {
    let text = |key: &str| raw.get(key).and_then(Value::as_str).map(str::to_string);
    let private = private_props(raw);
    let private_text = |key: &str| private.and_then(|p| p.get(key)).and_then(Value::as_str);
    let mut keep: Map<String, Value> = KEEP.iter().filter_map(|k| raw.get(*k).map(|v| (k.to_string(), v.clone()))).collect();
    if let Some(private) = private.filter(|p| !p.is_empty()) {
        let kept: Map<String, Value> = private
            .iter()
            .filter(|(k, _)| PRIVATE_KEEP.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        keep.insert("extendedProperties".into(), serde_json::json!({"private": kept}));
    }
    let status = text("status").unwrap_or_else(|| "confirmed".into());
    let cancelled = status == "cancelled";
    let time = |key: &str| -> Result<Option<Ts>> {
        match raw.get(key) {
            Some(value) if !cancelled => parse_time(value, calendar_timezone).map(Some),
            _ => Ok(None),
        }
    };
    let (start, end) = (time("start")?, time("end")?);
    if !cancelled && !matches!((start, end), (Some(s), Some(e)) if e > s) {
        return Err(invalid("Active calendar event requires ordered start/end"));
    }
    Ok(CalendarEvent {
        id: text("id").ok_or_else(|| invalid("Calendar event without id"))?,
        etag: text("etag"),
        title: text("summary").unwrap_or_default(),
        status,
        start,
        end,
        updated: text("updated").filter(|u| !u.is_empty()).map(|u| Ts::parse(&u, None)).transpose()?,
        ical_uid: text("iCalUID"),
        managed: private_text("gymclawManaged") == Some("true"),
        session_id: private_text("gymclawSessionId").map(str::to_string),
        revision: private.and_then(|p| p.get("gymclawPlanRevision")).and_then(revision),
        raw: keep,
    })
}

/// The remote event already carries `body`'s plan revision and times: our earlier patch landed and
/// only its response was lost.
pub(crate) fn already_applied(event: &CalendarEvent, body: &Value, calendar_timezone: &str) -> Result<bool> {
    let wanted = private_props(body).and_then(|p| p.get("gymclawPlanRevision"));
    let have = event.revision.map_or("None".to_string(), |r| r.to_string());
    if wanted.and_then(Value::as_str) != Some(have.as_str()) {
        return Ok(false);
    }
    for (key, current) in [("start", event.start), ("end", event.end)] {
        if let Some(value) = body.get(key)
            && current != Some(parse_time(value, calendar_timezone)?)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `extendedProperties.private.gymclawSessionId` of a write body.
pub(crate) fn body_session_id(body: &Value) -> Option<&str> {
    private_props(body)?.get("gymclawSessionId")?.as_str()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn all_day_exclusive_end_and_minimal_raw() {
        let event = parse_event(
            &json!({"id": "travel", "start": {"date": "2026-10-12"}, "end": {"date": "2026-10-19"}, "description": "unnecessary private data",
                "attendees": [{"email": "private@example.com"}], "extendedProperties": {"private": {"gymclawPlanRevision": "3", "other": "x"}}}),
            "Europe/Berlin",
        )
        .unwrap();
        assert_eq!(event.start.unwrap().iso(), "2026-10-11T22:00:00+00:00");
        assert_eq!(event.end.unwrap().iso(), "2026-10-18T22:00:00+00:00");
        assert_eq!(event.revision, Some(3));
        assert_eq!(
            Value::Object(event.raw),
            json!({"id": "travel", "start": {"date": "2026-10-12"}, "end": {"date": "2026-10-19"},
            "extendedProperties": {"private": {"gymclawPlanRevision": "3"}}})
        );
        assert_eq!(
            parse_event(&json!({"id": "deleted", "status": "cancelled", "start": {"date": "x"}}), "Europe/Berlin")
                .unwrap()
                .start,
            None
        );
    }
}
