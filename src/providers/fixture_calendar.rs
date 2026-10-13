//! Explicit in-memory demo calendar, never used as evidence of live Google behavior (port of `fixture_calendar.py`).
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::calendar::{CalendarEvent, CalendarProvider, SyncBatch, already_applied, body_session_id, conflict, parse_event, sync_token_expired};
use crate::bail;
use crate::error::Result;

/// A calendar fixture file (`--fixture path.json`).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CalendarFixture {
    #[serde(default = "demo_calendar")]
    pub calendar_id: String,
    #[serde(default = "berlin")]
    pub timezone: String,
    #[serde(default)]
    pub events: Vec<Value>,
    #[serde(default = "yes")]
    pub full: bool,
}

fn demo_calendar() -> String {
    "demo-calendar".into()
}
fn berlin() -> String {
    "Europe/Berlin".into()
}
fn yes() -> bool {
    true
}

/// Which provider call a test saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    Create(String),
    Update(String),
    Delete(String),
}

/// In-memory calendar. Every write appends to `changes`; sync tokens are `fixture:<change count>`.
pub struct FixtureCalendarProvider {
    pub calendar_id: String,
    pub timezone: String,
    /// Insertion-ordered, like Python's dict.
    events: Vec<(String, Value)>,
    pub changes: Vec<Value>,
    pub calls: Vec<Call>,
    pub fail_sync: bool,
    pub expire_token: bool,
    pub batch_override: Option<SyncBatch>,
}

impl Default for FixtureCalendarProvider {
    fn default() -> Self {
        FixtureCalendarProvider::new("demo-calendar", "Europe/Berlin")
    }
}

impl FixtureCalendarProvider {
    pub fn new(calendar_id: &str, timezone: &str) -> FixtureCalendarProvider {
        FixtureCalendarProvider {
            calendar_id: calendar_id.into(),
            timezone: timezone.into(),
            events: vec![],
            changes: vec![],
            calls: vec![],
            fail_sync: false,
            expire_token: false,
            batch_override: None,
        }
    }

    /// A provider that always returns the fixture's events as one batch, with a token derived from its content.
    pub fn from_fixture(fixture: &CalendarFixture) -> Result<FixtureCalendarProvider> {
        let mut provider = FixtureCalendarProvider::new(&fixture.calendar_id, &fixture.timezone);
        for event in &fixture.events {
            provider.set(event.clone());
        }
        let digest = hex::encode(Sha256::digest(super::python_json(&serde_json::to_value(fixture)?).as_bytes()));
        provider.batch_override = Some(SyncBatch {
            events: fixture.events.iter().map(|e| parse_event(e, &fixture.timezone)).collect::<Result<_>>()?,
            next_sync_token: format!("fixture:{digest}"),
            timezone: fixture.timezone.clone(),
            full: fixture.full,
        });
        Ok(provider)
    }

    fn raw(&self, event_id: &str) -> Option<&Value> {
        self.events.iter().find(|(id, _)| id == event_id).map(|(_, v)| v)
    }

    fn set(&mut self, body: Value) {
        let id = body["id"].as_str().unwrap_or_default().to_string();
        match self.events.iter_mut().find(|(k, _)| *k == id) {
            Some(slot) => slot.1 = body,
            None => self.events.push((id, body)),
        }
    }

    /// Store `raw` as a remote change (new etag) and return it parsed.
    pub fn put(&mut self, raw: Value) -> Result<CalendarEvent> {
        let mut body = raw;
        body["etag"] = json!(format!("\"fixture-{}\"", self.changes.len() + 1));
        self.set(body.clone());
        self.changes.push(body.clone());
        parse_event(&body, &self.timezone)
    }

    /// Remote edit of an existing event: `changes` are merged over it.
    pub fn edit(&mut self, event_id: &str, changes: Value) -> Result<CalendarEvent> {
        let merged = merge(self.raw(event_id).cloned().unwrap_or_else(|| json!({})), &changes);
        self.put(merged)
    }

    /// Remote deletion (Google keeps a cancelled stub).
    pub fn cancel(&mut self, event_id: &str) -> Result<CalendarEvent> {
        self.put(json!({"id": event_id, "status": "cancelled"}))
    }
}

/// Python `a | b` on dicts.
fn merge(mut base: Value, changes: &Value) -> Value {
    if let (Some(base), Some(changes)) = (base.as_object_mut(), changes.as_object()) {
        for (k, v) in changes {
            base.insert(k.clone(), v.clone());
        }
    }
    base
}

impl CalendarProvider for FixtureCalendarProvider {
    fn calendar_id(&self) -> &str {
        &self.calendar_id
    }

    fn source(&self) -> &'static str {
        "fixture"
    }

    fn incremental_sync(&mut self, sync_token: Option<&str>) -> Result<SyncBatch> {
        if self.fail_sync {
            bail!("CALENDAR_UNAVAILABLE", "Fixture sync intentionally unavailable");
        }
        if self.expire_token && sync_token.is_some() {
            self.expire_token = false;
            return Err(sync_token_expired());
        }
        if let Some(batch) = &self.batch_override {
            return Ok(batch.clone());
        }
        let raws: Vec<&Value> = match sync_token {
            None => self.events.iter().map(|(_, v)| v).collect(),
            Some(token) => {
                let previous: usize = token.split(':').nth(1).and_then(|n| n.parse().ok()).unwrap_or(0);
                self.changes.iter().skip(previous).collect()
            }
        };
        Ok(SyncBatch {
            events: raws.into_iter().map(|e| parse_event(e, &self.timezone)).collect::<Result<_>>()?,
            next_sync_token: format!("fixture:{}", self.changes.len()),
            timezone: self.timezone.clone(),
            full: sync_token.is_none(),
        })
    }

    fn get_event(&mut self, event_id: &str) -> Result<Option<CalendarEvent>> {
        self.raw(event_id).map(|raw| parse_event(raw, &self.timezone)).transpose()
    }

    fn create_gym_event(&mut self, body: &Value) -> Result<CalendarEvent> {
        let id = body["id"].as_str().unwrap_or_default().to_string();
        self.calls.push(Call::Create(id.clone()));
        if let Some(existing) = self.get_event(&id)? {
            if existing.cancelled() {
                bail!("CALENDAR_EVENT_DELETED", "Fixture event deleted");
            }
            if !existing.managed || existing.session_id.as_deref() != body_session_id(body) {
                bail!("CALENDAR_NOT_OWNED", "Fixture ownership mismatch");
            }
            return Ok(existing);
        }
        self.put(body.clone())
    }

    fn update_gym_event(&mut self, event_id: &str, body: &Value, etag: &str) -> Result<CalendarEvent> {
        self.calls.push(Call::Update(event_id.into()));
        let event = match self.get_event(event_id)? {
            Some(event) if !event.cancelled() => event,
            _ => bail!("CALENDAR_EVENT_DELETED", "Fixture event deleted"),
        };
        if !event.managed || event.session_id.as_deref() != body_session_id(body) {
            bail!("CALENDAR_NOT_OWNED", "Fixture ownership mismatch");
        }
        if event.etag.as_deref() != Some(etag) {
            if already_applied(&event, body, &self.timezone)? {
                return Ok(event);
            }
            return Err(conflict());
        }
        let merged = merge(self.raw(event_id).cloned().unwrap_or_else(|| json!({})), body);
        self.put(merged)
    }

    fn delete_gym_event(&mut self, event_id: &str, etag: &str, session_id: &str) -> Result<()> {
        self.calls.push(Call::Delete(event_id.into()));
        let event = match self.get_event(event_id)? {
            Some(event) if !event.cancelled() => event,
            _ => return Ok(()),
        };
        if !event.managed || event.session_id.as_deref() != Some(session_id) {
            bail!("CALENDAR_NOT_OWNED", "Fixture ownership mismatch");
        }
        if event.etag.as_deref() != Some(etag) {
            return Err(conflict());
        }
        self.cancel(event_id)?;
        Ok(())
    }
}
