//! The agent-writable layer above the core (#92). The agent adapts to the owner without a code change
//! by writing three kinds of extension, all built on the existing tools and never around them:
//!
//! - playbook: steps for a request that keeps coming back ("swap day"); name and one line in every
//!   prompt, the steps read on demand with `sql_query`;
//! - collection: its own log (creatine, shoulder pain) as `agent_data` rows;
//! - trigger: a daily/weekly local time, one local date and time, a set time before each planned
//!   session, or a finished workout, that wakes the agent with an instruction, through the same
//!   decision-event path as every other background wake (`extension.trigger`). A trigger runs no logic
//!   of its own: the agent decides, and may stay silent. A one-off trigger turns itself off once its
//!   time has passed.
//!
//! Every change appends an `extension_version` row and can be restored. The owner lists and turns off
//! anything with `/extensions` or on the dashboard. Requests no tool covers go to `capability_gap`
//! for review, never to an automatic fix.
use std::fmt::Write;

use jiff::SignedDuration;
use jiff::civil::{Date, DateTime, Time, Weekday};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::db::models::{AgentEvent, PlannedSession, WorkoutSession, WorkoutTemplate};
use crate::db::{Connection, Json};
use crate::error::Result;
use crate::time::Ts;
use crate::{sql_enum, table};

sql_enum!(ExtensionKind { Playbook = "playbook", Collection = "collection", Trigger = "trigger" });
sql_enum!(Author { Agent = "agent", Owner = "owner" });
sql_enum!(GapSource { Agent = "agent", StepCap = "step_cap", Deadline = "deadline", RepeatedCall = "repeated_call" });

/// The wake event a trigger raises.
pub const TRIGGER_EVENT: &str = "extension.trigger";
/// How late a scheduled trigger may still fire (after a restart or pause); later, that slot is skipped.
const GRACE: SignedDuration = SignedDuration::from_hours(1);
/// Trigger wakes per local day across all triggers: a runaway trigger can't flood the owner.
pub const DAILY_FIRES: i64 = 8;
/// Enabled extensions at once. They go into every prompt, so the agent has to choose.
pub const LIMIT: usize = 20;

/// What an extension does. The variant is its kind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Spec {
    /// Steps for a request that keeps coming back, e.g. "swap day".
    Playbook {
        /// One line: when it applies. Shown in every prompt.
        summary: String,
        /// The steps in order, naming the tools to use.
        steps: Vec<String>,
    },
    /// Your own log, e.g. creatine taken. Rows go in with data_add; read them with sql_query on agent_data.
    Collection {
        /// One line: what a row means and its data fields, e.g. "creatine taken; data {grams}".
        summary: String,
    },
    /// Wakes you with the instruction. You decide then; when nothing needs saying, stay silent.
    Trigger {
        when: When,
        /// What to check and when to message, e.g. "If agent_data has no creatine row today, ask whether they took it."
        instruction: String,
    },
}

// Flat on the wire so its schema stays small; `slot` gives the typed form.
/// When it fires, in the owner's local time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct When {
    pub on: Repeat,
    /// Local "HH:MM"; daily, weekly and once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// Weekly only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub day: Option<Day>,
    /// Once only: the local date, "YYYY-MM-DD".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// Before_session only: minutes before the session starts (1–1440).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minutes: Option<i64>,
    /// Before_session only: just sessions of this workout (template id); every session when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workout: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    Daily,
    Weekly,
    /// One local date and time ("remind me tomorrow at 9"); turns itself off afterwards.
    Once,
    /// `minutes` before each planned session (or each of one `workout`).
    BeforeSession,
    /// Once after each finished workout.
    WorkoutFinished,
}

/// A validated `When`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Slot {
    Daily(Time),
    Weekly(Weekday, Time),
    Once(DateTime),
    BeforeSession { minutes: i64, workout: Option<String> },
    WorkoutFinished,
}

impl When {
    fn new(on: Repeat) -> Self {
        When { on, at: None, day: None, date: None, minutes: None, workout: None }
    }

    pub fn daily(at: &str) -> Self {
        When { at: Some(at.into()), ..When::new(Repeat::Daily) }
    }

    pub fn once(date: &str, at: &str) -> Self {
        When { date: Some(date.into()), at: Some(at.into()), ..When::new(Repeat::Once) }
    }

    pub fn before_session(minutes: i64, workout: Option<&str>) -> Self {
        When { minutes: Some(minutes), workout: workout.map(String::from), ..When::new(Repeat::BeforeSession) }
    }

    pub fn workout_finished() -> Self {
        When::new(Repeat::WorkoutFinished)
    }

    /// Refuses a schedule that is missing its time or day.
    pub fn slot(&self) -> Result<Slot> {
        let at = || match &self.at {
            Some(at) => clock(at),
            None => crate::bail!("INVALID_EXTENSION", "A {:?} trigger needs `at` (local HH:MM)", self.on),
        };
        Ok(match self.on {
            Repeat::Daily => Slot::Daily(at()?),
            Repeat::Weekly => match self.day {
                Some(day) => Slot::Weekly(day.weekday(), at()?),
                None => crate::bail!("INVALID_EXTENSION", "A weekly trigger needs `day` (mon…sun)"),
            },
            Repeat::Once => {
                let Some(date) = &self.date else {
                    crate::bail!("INVALID_EXTENSION", "A once trigger needs `date` (local YYYY-MM-DD) and `at`")
                };
                let Ok(date) = date.trim().parse::<Date>() else {
                    crate::bail!("INVALID_EXTENSION", "Trigger date must be local YYYY-MM-DD, got {date:?}")
                };
                Slot::Once(date.to_datetime(at()?))
            }
            Repeat::BeforeSession => match self.minutes {
                Some(minutes @ 1..=1440) => Slot::BeforeSession { minutes, workout: self.workout.clone() },
                _ => crate::bail!("INVALID_EXTENSION", "A before_session trigger needs `minutes` (1–1440)"),
            },
            Repeat::WorkoutFinished => Slot::WorkoutFinished,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Day {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Day {
    fn weekday(self) -> Weekday {
        match self {
            Day::Mon => Weekday::Monday,
            Day::Tue => Weekday::Tuesday,
            Day::Wed => Weekday::Wednesday,
            Day::Thu => Weekday::Thursday,
            Day::Fri => Weekday::Friday,
            Day::Sat => Weekday::Saturday,
            Day::Sun => Weekday::Sunday,
        }
    }
}

impl Spec {
    pub fn kind(&self) -> ExtensionKind {
        match self {
            Spec::Playbook { .. } => ExtensionKind::Playbook,
            Spec::Collection { .. } => ExtensionKind::Collection,
            Spec::Trigger { .. } => ExtensionKind::Trigger,
        }
    }

    /// One line for the prompt, `/extensions` and the dashboard.
    pub fn summary(&self) -> String {
        match self {
            Spec::Playbook { summary, .. } | Spec::Collection { summary } => summary.clone(),
            Spec::Trigger { when, instruction } => {
                let at = when.at.as_deref().unwrap_or("?");
                let when = match when.on {
                    Repeat::Daily => format!("daily {at}"),
                    Repeat::Weekly => format!("{} {at}", when.day.map_or("?".into(), |d| format!("{d:?}"))),
                    Repeat::Once => format!("once {} {at}", when.date.as_deref().unwrap_or("?")),
                    Repeat::BeforeSession => format!(
                        "{} min before each {} session",
                        when.minutes.unwrap_or_default(),
                        when.workout.as_deref().unwrap_or("planned")
                    ),
                    Repeat::WorkoutFinished => "after each workout".into(),
                };
                format!("{when}: {}", short(instruction, 120))
            }
        }
    }

    /// Refuses what would cost the prompt too much or never fire.
    fn check(&self) -> Result<()> {
        let line = |text: &str, max: usize, what: &str| -> Result<()> {
            let n = text.trim().chars().count();
            if n == 0 || n > max {
                crate::bail!("INVALID_EXTENSION", "{what} must be 1–{max} characters (is {n})");
            }
            Ok(())
        };
        match self {
            Spec::Playbook { summary, steps } => {
                line(summary, 160, "summary")?;
                if steps.is_empty() || steps.len() > 12 {
                    crate::bail!("INVALID_EXTENSION", "A playbook has 1–12 steps");
                }
                steps.iter().try_for_each(|s| line(s, 300, "each step"))
            }
            Spec::Collection { summary } => line(summary, 160, "summary"),
            Spec::Trigger { when, instruction } => {
                line(instruction, 500, "instruction")?;
                when.slot().map(drop)
            }
        }
    }
}

/// A local "HH:MM".
fn clock(at: &str) -> Result<Time> {
    at.trim().parse::<Time>().map_err(|_| {
        crate::error::Error::domain("INVALID_EXTENSION", format!("Trigger time must be local HH:MM, got {at:?}"))
    })
}

fn short(text: &str, max: usize) -> String {
    let text = text.trim().replace('\n', " ");
    if text.chars().count() <= max { text } else { format!("{}…", text.chars().take(max).collect::<String>()) }
}

table!(Extension = "extension" {
    /// None on insert: SQLite assigns it.
    id: Option<i64>,
    kind: ExtensionKind,
    name: String,
    spec_json: Json<Spec>,
    enabled: bool,
    version: i64,
    created_at: Ts,
    updated_at: Ts,
});

table!(ExtensionVersion = "extension_version" {
    id: Option<i64>,
    extension_id: i64,
    version: i64,
    spec_json: Json<Spec>,
    enabled: bool,
    author: Author,
    created_at: Ts,
});

table!(AgentData = "agent_data" {
    id: Option<i64>,
    collection: String,
    at: Ts,
    data_json: Json<Value>,
    created_at: Ts,
});

table!(CapabilityGap = "capability_gap" {
    id: Option<i64>,
    at: Ts,
    source: GapSource,
    asked: String,
    tried: String,
    why: String,
    run_id: Option<String>,
});

impl Extension {
    fn id(&self) -> i64 {
        self.id.unwrap_or_default()
    }
}

/// Creates the extension, or saves a new version of the one with this kind and name. A spent one-off
/// trigger (auto-disabled after firing) is re-enabled when saved with a new future time; one the
/// owner turned off stays off.
pub fn save(db: &Connection, name: &str, spec: Spec, author: Author, now: Ts) -> Result<Extension> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 40 {
        crate::bail!("INVALID_EXTENSION", "Name must be 1–40 characters");
    }
    spec.check()?;
    if let Spec::Trigger { when, .. } = &spec {
        match when.slot()? {
            Slot::Once(at) => {
                let tz = crate::domain::profile::get_profile(db)?.tz();
                if Ts(at.to_zoned(tz)?.timestamp()) <= now {
                    crate::bail!("INVALID_EXTENSION", "{at} has passed (owner-local); pick a later date or time");
                }
            }
            Slot::BeforeSession { workout: Some(id), .. } if WorkoutTemplate::get(db, id.as_str())?.is_none() => {
                crate::bail!(
                    "INVALID_EXTENSION",
                    "No workout template {id:?}; pass a template id or leave `workout` out"
                )
            }
            _ => {}
        }
    }
    let kind = spec.kind();
    let mut ext = match Extension::first(db, "WHERE kind = ?1 AND name = ?2", (kind, name))? {
        Some(ext) if *ext.spec_json == spec => return Ok(ext),
        Some(mut ext) => {
            ext.spec_json = Json(spec);
            // A one-off that fired auto-disables itself (Author::Agent). Saving a new future time
            // re-enables it, but one the owner turned off stays off. We find the version that
            // actually turned it off: the first disabled version after the last enabled one.
            if !ext.enabled
                && let Spec::Trigger { when, .. } = &*ext.spec_json
                && matches!(when.slot(), Ok(Slot::Once(_)))
            {
                let turned_off: Option<Author> = db.query_row(
                    "SELECT author FROM extension_version                      WHERE extension_id = ?1 AND NOT enabled                        AND version > COALESCE((SELECT MAX(version) FROM extension_version WHERE extension_id = ?1 AND enabled), 0)                      ORDER BY version ASC LIMIT 1",
                    [ext.id()],
                    |r| r.get(0),
                ).ok();
                if turned_off == Some(Author::Agent) {
                    check_room(db)?;
                    ext.enabled = true;
                }
            }
            ext
        }
        None => {
            check_room(db)?;
            let ext = Extension {
                id: None,
                kind,
                name: name.into(),
                spec_json: Json(spec),
                enabled: true,
                version: 0,
                created_at: now,
                updated_at: now,
            };
            ext.insert(db)?;
            Extension { id: Some(db.last_insert_rowid()), ..ext }
        }
    };
    commit(db, &mut ext, author, now)?;
    Ok(ext)
}

/// Turns an extension on or off.
pub fn set_enabled(db: &Connection, id: i64, enabled: bool, author: Author, now: Ts) -> Result<Extension> {
    let mut ext = get(db, id)?;
    if ext.enabled == enabled {
        return Ok(ext);
    }
    if enabled {
        check_room(db)?;
    }
    ext.enabled = enabled;
    commit(db, &mut ext, author, now)?;
    Ok(ext)
}

/// Rolls an extension back to what it did at `version`, as a new version (history is never rewritten).
pub fn restore(db: &Connection, id: i64, version: i64, author: Author, now: Ts) -> Result<Extension> {
    let mut ext = get(db, id)?;
    let Some(old) = ExtensionVersion::first(db, "WHERE extension_id = ?1 AND version = ?2", [id, version])? else {
        crate::bail!("VERSION_NOT_FOUND", "Extension {id} has versions 1–{}", ext.version)
    };
    if ext.spec_json == old.spec_json {
        return Ok(ext);
    }
    ext.spec_json = old.spec_json;
    commit(db, &mut ext, author, now)?;
    Ok(ext)
}

fn get(db: &Connection, id: i64) -> Result<Extension> {
    match Extension::get(db, id)? {
        Some(ext) => Ok(ext),
        None => crate::bail!("EXTENSION_NOT_FOUND", "No extension {id}"),
    }
}

fn check_room(db: &Connection) -> Result<()> {
    if enabled(db)?.len() >= LIMIT {
        crate::bail!(
            "EXTENSION_LIMIT",
            "{LIMIT} extensions are on, the most the prompt holds. Turn off or merge one first (extension_edit)."
        );
    }
    Ok(())
}

/// Saves the changed extension as its next version, with a copy in `extension_version`.
fn commit(db: &Connection, ext: &mut Extension, author: Author, now: Ts) -> Result<()> {
    ext.version += 1;
    ext.updated_at = now;
    ext.update(db)?;
    ExtensionVersion {
        id: None,
        extension_id: ext.id(),
        version: ext.version,
        spec_json: ext.spec_json.clone(),
        enabled: ext.enabled,
        author,
        created_at: now,
    }
    .insert(db)
}

/// Enabled extensions, by kind then name.
pub fn enabled(db: &Connection) -> Result<Vec<Extension>> {
    Extension::select(db, "WHERE enabled ORDER BY kind, name", [])
}

/// Logs a row into an enabled collection.
pub fn add_data(db: &Connection, collection: &str, at: Ts, data: Value, now: Ts) -> Result<i64> {
    let found = Extension::first(db, "WHERE kind = 'collection' AND name = ?1", [collection])?;
    match found {
        Some(c) if c.enabled => {}
        Some(_) => crate::bail!(
            "COLLECTION_OFF",
            "Collection {collection} is turned off; ask the owner before logging into it"
        ),
        None => crate::bail!(
            "COLLECTION_NOT_FOUND",
            "No collection {collection}. Create it with extension_save (kind collection) first."
        ),
    }
    if data.to_string().len() > 2000 {
        crate::bail!("DATA_TOO_LARGE", "One row holds at most 2000 characters of data");
    }
    AgentData { id: None, collection: collection.into(), at, data_json: Json(data), created_at: now }.insert(db)?;
    Ok(db.last_insert_rowid())
}

/// The prompt section: what the agent built, one line each. None when it built nothing.
pub fn prompt(db: &Connection) -> Result<Option<String>> {
    let all = enabled(db)?;
    if all.is_empty() {
        return Ok(None);
    }
    let mut out = String::from("\n\n## Your extensions (extension_save/extension_edit; ids in brackets)");
    for (kind, title) in [
        (ExtensionKind::Playbook, "Playbooks (follow the steps: sql_query extension.spec_json by id)"),
        (ExtensionKind::Collection, "Collections (rows in agent_data by collection)"),
        (ExtensionKind::Trigger, "Triggers"),
    ] {
        let lines: Vec<String> = all
            .iter()
            .filter(|e| e.kind == kind)
            .map(|e| format!("- [{}] {}: {}", e.id(), e.name, e.spec_json.summary()))
            .collect();
        if !lines.is_empty() {
            write!(out, "\n{title}:\n{}", lines.join("\n")).unwrap();
        }
    }
    Ok(Some(out))
}

/// Raises an `extension.trigger` wake for every enabled trigger whose slot has come: at most one per
/// trigger and slot (a local day or week, or one finished workout), none for a slot more than `GRACE`
/// ago or before its latest save, and at most `DAILY_FIRES` per local day. Returns how many fired.
pub fn fire_due(db: &Connection, now: Ts) -> Result<usize> {
    let tz = crate::domain::profile::get_profile(db)?.tz();
    let local = now.local(&tz);
    let midnight = Ts(local.date().to_zoned(tz.clone())?.timestamp());
    let mut fired_today: i64 = db.query_row(
        "SELECT count(*) FROM agent_event WHERE type = 'extension.trigger' AND created_at >= ?1",
        [midnight],
        |r| r.get(0),
    )?;
    let mut fired = 0;
    for trigger in Extension::select(db, "WHERE enabled AND kind = 'trigger' ORDER BY id", [])? {
        let Spec::Trigger { when, instruction } = &*trigger.spec_json else { continue };
        // (slot key, extra payload) of each slot that is due now.
        let mut due: Vec<(String, Value)> = vec![];
        // A slot that passed before the latest save (a time moved earlier, switched back on) waits for next time.
        let since = trigger.updated_at.max(now - GRACE);
        // A one-off whose time has passed, fired or missed: it turns off after this tick.
        let mut spent = false;
        let slot = when.slot()?;
        match &slot {
            Slot::Daily(at) | Slot::Weekly(_, at) => {
                let day_ok = match slot {
                    Slot::Weekly(day, _) => local.weekday() == day,
                    _ => true,
                };
                let slot = Ts(local.date().to_datetime(*at).to_zoned(tz.clone())?.timestamp());
                if day_ok && since <= slot && slot <= now {
                    due.push((local.date().to_string(), json!({})));
                }
            }
            Slot::Once(at) => {
                let slot = Ts(at.to_zoned(tz.clone())?.timestamp());
                if since <= slot && slot <= now {
                    due.push((at.to_string(), json!({})));
                }
                // Only switch off after the event is actually emitted (below), not when the daily
                // cap blocks it. If the grace period has passed without firing, give up.
                spent = slot + GRACE < now;
            }
            Slot::BeforeSession { minutes, workout } => {
                let lead = SignedDuration::from_mins(*minutes);
                // Only fire for sessions that haven't started yet: planned_start_at > now.
                for s in PlannedSession::select(
                    db,
                    "WHERE status IN ('TENTATIVE', 'COMMITTED') AND planned_start_at > ?1 AND planned_start_at <= ?2 AND planned_start_at > ?3 ORDER BY planned_start_at",
                    (since + lead, now + lead, now),
                )? {
                    if workout.as_ref().is_some_and(|w| s.workout_template_id.as_ref() != Some(w)) {
                        continue;
                    }
                    // Keyed by start too: a session moved later fires again before its new time.
                    let key = format!("{}@{}", s.id, s.planned_start_at.0.as_second());
                    due.push((key, json!({"session_id": s.id, "starts_at": s.planned_start_at})));
                }
            }
            Slot::WorkoutFinished => {
                for w in WorkoutSession::select(
                    db,
                    "WHERE status = 'PLAN_UPDATED' AND completed_at > ?1 AND completed_at <= ?2 ORDER BY completed_at",
                    [since, now],
                )? {
                    due.push((w.id.clone(), json!({"workout_id": w.id})));
                }
            }
        }
        for (slot_key, extra) in due {
            let key = format!("trigger:{}:{slot_key}", trigger.id());
            if fired_today >= DAILY_FIRES {
                return Ok(fired);
            }
            if AgentEvent::first(db, "WHERE correlation_id = ?1", [&key])?.is_some() {
                continue;
            }
            let mut payload = json!({"trigger_id": trigger.id(), "trigger": trigger.name, "instruction": instruction});
            if let (Some(p), Value::Object(extra)) = (payload.as_object_mut(), extra) {
                p.extend(extra);
            }
            let mut event = crate::domain::core::emit(db, TRIGGER_EVENT, now, payload)?;
            event.correlation_id = Some(key);
            event.update(db)?;
            fired_today += 1;
            fired += 1;
            if matches!(&slot, Slot::Once(_)) {
                spent = true;
            }
        }
        if spent {
            set_enabled(db, trigger.id(), false, Author::Agent, now)?;
        }
    }
    Ok(fired)
}

/// Records a request no tool could handle.
pub fn log_gap(
    db: &Connection,
    source: GapSource,
    asked: &str,
    tried: &str,
    why: &str,
    run_id: Option<&str>,
    now: Ts,
) -> Result<i64> {
    CapabilityGap {
        id: None,
        at: now,
        source,
        asked: short(asked, 500),
        tried: short(tried, 500),
        why: short(why, 500),
        run_id: run_id.map(String::from),
    }
    .insert(db)?;
    Ok(db.last_insert_rowid())
}

/// The gap of a turn that ran out of room: what it was answering (the newest owner message or
/// background prompt) and how far it got. Its tool calls are in the turn log under `run_id`.
pub fn log_turn_gap(db: &Connection, source: GapSource, run_id: &str, calls: usize, now: Ts) -> Result<i64> {
    let asked: String = db
        .query_row(
            "SELECT text FROM chat_message WHERE role = 'owner' OR meta_json ->> 'kind' = 'agent_event' ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap_or_default();
    let why = match source {
        GapSource::StepCap => "hit the step cap before finishing",
        GapSource::Deadline => "ran past the turn deadline",
        GapSource::RepeatedCall => "kept repeating the same call",
        GapSource::Agent => "logged by the agent",
    };
    log_gap(db, source, &asked, &format!("{calls} tool calls (turn log run {run_id})"), why, Some(run_id), now)
}

/// The wake that asks the agent to learn from one of its turns (`reflect`).
pub const REFLECT_EVENT: &str = "agent.reflect";
/// Tool calls from which an owner turn counts as hard enough to be worth a playbook.
pub const REFLECT_AFTER: usize = 6;
/// Reflection wakes per 24 h: each is a model turn, and most hard turns teach nothing new.
const REFLECTS_PER_DAY: i64 = 3;

/// Why a turn is worth learning from.
pub enum Lesson<'a> {
    /// It took many tool calls or ran out of room; `tools` are the calls it made, in order.
    Hard { forced: Option<&'a str>, tools: &'a [String], asked: &'a str },
}

/// Wakes the agent, once the chat is quiet, to turn a hard turn into a playbook or note,
/// so the same request goes right next time. Hermes-style self-written skills, through the same
/// extension layer. At most `REFLECTS_PER_DAY`; returns whether it raised one.
pub fn reflect(db: &Connection, run_id: &str, lesson: Lesson, now: Ts) -> Result<bool> {
    let recent: i64 = db.query_row(
        "SELECT count(*) FROM agent_event WHERE type = 'agent.reflect' AND created_at > ?1",
        [now - SignedDuration::from_hours(24)],
        |r| r.get(0),
    )?;
    if recent >= REFLECTS_PER_DAY {
        return Ok(false);
    }
    let Lesson::Hard { asked, .. } = &lesson;
    let asked = asked.to_string();
    let payload = match lesson {
        Lesson::Hard { forced, tools, .. } => json!({
            "run_id": run_id, "asked": short(&asked, 500), "forced": forced, "tools": tools,
            "instruction": "That turn took many tool calls. If the owner is likely to ask this again, save or update a playbook \
                with the steps that worked (extension_save), so next time it takes a few. If a missing fact sent you the long \
                way, save a learned note instead. If neither would help, do nothing. Don't message the owner about this.",
        }),
    };
    let before = crate::domain::events::mark(db)?;
    crate::domain::events::request(db, REFLECT_EVENT, payload, now)?;
    Ok(crate::domain::events::mark(db)? > before)
}

/// `/extensions`: everything the agent built, on or off, and how to turn one off. No model.
pub fn overview(db: &Connection) -> Result<String> {
    let all = Extension::select(db, "ORDER BY kind, name", [])?;
    if all.is_empty() {
        return Ok("The agent hasn't built any extensions yet.".into());
    }
    let mut lines = vec!["**Extensions** · `/extensions off <id>` turns one off, `on <id>` back on".to_string()];
    for e in &all {
        let rows = if e.kind == ExtensionKind::Collection {
            let n: i64 =
                db.query_row("SELECT count(*) FROM agent_data WHERE collection = ?1", [&e.name], |r| r.get(0))?;
            format!(" ({n} rows)")
        } else {
            String::new()
        };
        lines.push(format!(
            "{} [{}] {} **{}**{rows}: {}",
            if e.enabled { "✅" } else { "⏸" },
            e.id(),
            e.kind,
            e.name,
            short(&e.spec_json.summary(), 100)
        ));
    }
    let gaps: i64 = db.query_row("SELECT count(*) FROM capability_gap", [], |r| r.get(0))?;
    if gaps > 0 {
        lines.push(format!("{gaps} capability gaps logged (dashboard → Memory)."));
    }
    Ok(lines.join("\n"))
}

/// `/extensions off <id>` / `/extensions on <id>` from the owner.
pub fn owner_switch(db: &Connection, args: &str, now: Ts) -> Result<String> {
    let mut words = args.split_whitespace();
    let (Some(action), Some(Ok(id))) = (words.next(), words.next().map(str::parse::<i64>)) else {
        return overview(db);
    };
    let on = match action {
        "on" => true,
        "off" => false,
        _ => return overview(db),
    };
    let ext = set_enabled(db, id, on, Author::Owner, now)?;
    Ok(format!("{} {} **{}** is {}.", if on { "✅" } else { "⏸" }, ext.kind, ext.name, if on { "on" } else { "off" }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{ChatMessage, ChatRole};
    use crate::tools::Ctx;

    fn at(text: &str) -> Ts {
        Ts::parse(text, None).unwrap()
    }

    /// Runs a tool as the agent (`tg-…`), a background turn (`event-…`) or the dashboard.
    fn call(db: &Connection, request_id: &str, now: Ts, tool: &str, args: Value) -> Result<Value> {
        let dir = std::env::temp_dir();
        let mut cx = Ctx { db, now, request_id: request_id.into(), data_dir: &dir, effects: vec![] };
        crate::tools::call(&mut cx, tool, args)
    }

    fn wakes(db: &Connection) -> Vec<Value> {
        crate::domain::events::wake_pending(db).unwrap().into_iter().filter(|e| e["type"] == TRIGGER_EVENT).collect()
    }

    /// The issue's example: "track my creatine and nag me if I forget". A collection the agent logs
    /// into and reads with sql_query, and a 09:00 trigger that wakes it once a day with its own
    /// instruction (the agent decides whether to ask; nothing is sent by code).
    #[test]
    fn creatine_log_and_daily_nag() {
        let db = crate::db::test::db();
        // Sunday 15 Nov, Berlin is UTC+1: the owner asks at 10:00, after today's 09:00 slot.
        let asked = at("2026-11-15T09:00:00Z");
        let collection =
            json!({"name": "creatine", "spec": {"kind": "collection", "summary": "creatine taken; data {grams}"}});
        call(&db, "tg-1-1", asked, "extension_save", collection).unwrap();
        let instruction = "If agent_data has no creatine row today, ask whether they took it; otherwise stay silent.";
        let trigger = json!({"name": "creatine nag", "spec": {"kind": "trigger", "when": {"on": "daily", "at": "09:00"}, "instruction": instruction}});
        call(&db, "tg-1-2", asked, "extension_save", trigger).unwrap();
        call(
            &db,
            "tg-1-3",
            asked,
            "data_add",
            json!({"collection": "creatine", "data": {"grams": 5}, "at": "2026-11-15T08:30"}),
        )
        .unwrap();
        let rows = call(
            &db,
            "tg-1-4",
            asked,
            "sql_query",
            json!({"sql": "SELECT data_json ->> 'grams' FROM agent_data WHERE collection = 'creatine'"}),
        )
        .unwrap();
        assert_eq!(rows["rows"], json!([[5]]));
        // A slot that passed before the trigger existed never fires.
        assert_eq!(fire_due(&db, asked.mins(5)).unwrap(), 0);
        // Monday: nothing at 08:59, one wake at 09:00, none again in the same slot.
        assert_eq!(fire_due(&db, at("2026-11-16T07:59:00Z")).unwrap(), 0);
        assert_eq!(fire_due(&db, at("2026-11-16T08:00:30Z")).unwrap(), 1);
        assert_eq!(fire_due(&db, at("2026-11-16T08:30:00Z")).unwrap(), 0);
        let fired = wakes(&db);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0]["payload"]["instruction"], instruction);
        // Down from 08:00 to 11:00 on Tuesday: a slot more than an hour old is skipped, not sent late.
        assert_eq!(fire_due(&db, at("2026-11-17T10:00:00Z")).unwrap(), 0);
        // The prompt lists both in one line each.
        let prompt = prompt(&db).unwrap().unwrap();
        assert!(
            prompt.contains("creatine: creatine taken; data {grams}")
                && prompt.contains("creatine nag: daily 09:00: If agent_data"),
            "{prompt}"
        );
    }

    /// Every change is a version; a rollback restores an old one as a new version. The owner's off
    /// switch stops the trigger and hides it from the prompt, and no background turn turns it back on.
    #[test]
    fn versions_roll_back_and_the_owner_switch_holds() {
        let db = crate::db::test::db();
        let now = at("2026-11-15T09:00:00Z");
        let spec = |instruction: &str| Spec::Trigger { when: When::daily("07:00"), instruction: instruction.into() };
        let first = save(&db, "weigh-in", spec("Ask for today's weight"), Author::Agent, now).unwrap();
        let id = first.id.unwrap();
        save(&db, "weigh-in", spec("Ask for today's weight"), Author::Agent, now).unwrap();
        save(&db, "weigh-in", spec("Ask only on Sundays"), Author::Agent, now).unwrap();
        let restored =
            call(&db, "dashboard:x", now, "extension_edit", json!({"id": id, "restore_version": 1})).unwrap();
        assert_eq!(restored["version"], 3, "an unchanged save is no version");
        assert_eq!(*get(&db, id).unwrap().spec_json, spec("Ask for today's weight"));
        let authors: Vec<Author> =
            ExtensionVersion::select(&db, "ORDER BY version", []).unwrap().into_iter().map(|v| v.author).collect();
        assert_eq!(authors, [Author::Agent, Author::Agent, Author::Owner]);

        owner_switch(&db, &format!("off {id}"), now).unwrap();
        assert_eq!(fire_due(&db, at("2026-11-16T06:00:00Z")).unwrap(), 0);
        assert!(prompt(&db).unwrap().is_none());
        // Saving it again keeps it off.
        save(&db, "weigh-in", spec("Ask every day"), Author::Agent, now).unwrap();
        assert!(!get(&db, id).unwrap().enabled);
        let woken = call(&db, "event-b-1", now, "extension_edit", json!({"id": id, "enabled": true})).unwrap_err();
        assert_eq!(woken.code(), "OWNER_REQUIRED");
        call(&db, "tg-9-1", now, "extension_edit", json!({"id": id, "enabled": true})).unwrap();
        assert!(get(&db, id).unwrap().enabled);
        // Background wakes never carry the owner's permission for gated tools either.
        crate::db::test::authority().insert(&db).unwrap();
        let resume = call(&db, "event-b-2", now, "runtime_resume", json!({"owner_asked": true})).unwrap_err();
        assert_eq!(resume.code(), "RUNTIME_NOT_APPROVED");
    }

    /// A finished workout wakes its trigger once, with the workout; all triggers together wake the
    /// agent at most `DAILY_FIRES` times a local day.
    #[test]
    fn workout_triggers_fire_once_and_the_day_is_capped() {
        let db = crate::db::test::db();
        let now = at("2026-11-15T09:00:00Z");
        let after = Spec::Trigger { when: When::workout_finished(), instruction: "Ask how the shoulder felt".into() };
        save(&db, "shoulder", after, Author::Agent, now).unwrap();
        db.execute(
            "INSERT INTO workout_session (id, status, started_at, completed_at, waited_for_equipment_count, template_snapshot) VALUES ('w1', 'PLAN_UPDATED', ?1, ?2, 0, '{}')",
            [now.mins(50), now.mins(55)],
        )
        .unwrap();
        assert_eq!(fire_due(&db, now.mins(56)).unwrap(), 1);
        assert_eq!(fire_due(&db, now.mins(57)).unwrap(), 0);
        assert_eq!(wakes(&db)[0]["payload"]["workout_id"], "w1");

        for i in 0..DAILY_FIRES {
            let spec = Spec::Trigger { when: When::daily("12:00"), instruction: format!("check {i}") };
            save(&db, &format!("t{i}"), spec, Author::Agent, now).unwrap();
        }
        assert_eq!(fire_due(&db, at("2026-11-15T11:00:00Z")).unwrap() as i64, DAILY_FIRES - 1);
        assert_eq!(wakes(&db).len() as i64, DAILY_FIRES);
    }

    /// "Remind me tomorrow at 9": one wake at that local time, then the trigger turns itself off and
    /// leaves the prompt. A time already past is refused rather than silently never firing.
    #[test]
    fn once_triggers_fire_once_then_turn_off() {
        let db = crate::db::test::db();
        let now = at("2026-11-15T09:00:00Z");
        let chalk = |date: &str| Spec::Trigger {
            when: When::once(date, "09:00"),
            instruction: "Remind them to buy chalk".into(),
        };
        assert_eq!(
            save(&db, "chalk", chalk("2026-11-15"), Author::Agent, now).unwrap_err().code(),
            "INVALID_EXTENSION"
        );
        let id = save(&db, "chalk", chalk("2026-11-16"), Author::Agent, now).unwrap().id.unwrap();
        assert!(prompt(&db).unwrap().unwrap().contains("chalk: once 2026-11-16 09:00: Remind"));
        assert_eq!(fire_due(&db, at("2026-11-16T07:59:00Z")).unwrap(), 0);
        assert_eq!(fire_due(&db, at("2026-11-16T08:00:30Z")).unwrap(), 1);
        assert!(!get(&db, id).unwrap().enabled && prompt(&db).unwrap().is_none());
        assert_eq!(fire_due(&db, at("2026-11-16T08:01:00Z")).unwrap(), 0);
        // Missed while the bot was down: off without a stale wake.
        let late = save(&db, "chalk2", chalk("2026-11-17"), Author::Agent, now).unwrap().id.unwrap();
        assert_eq!(fire_due(&db, at("2026-11-17T12:00:00Z")).unwrap(), 0);
        assert!(!get(&db, late).unwrap().enabled);
        assert_eq!(wakes(&db).len(), 1);
    }

    /// "Before leg day, remind me to bring the belt": one wake per session of that workout, the set
    /// minutes before it starts; a session moved later wakes it again before its new time.
    #[test]
    fn before_session_triggers_follow_the_planned_sessions() {
        let db = crate::db::test::with_short_template();
        let now = at("2026-11-15T09:00:00Z");
        let belt = |workout: &str| Spec::Trigger {
            when: When::before_session(60, Some(workout)),
            instruction: "Remind them to bring the belt".into(),
        };
        assert_eq!(save(&db, "belt", belt("legs"), Author::Agent, now).unwrap_err().code(), "INVALID_EXTENSION");
        save(&db, "belt", belt("short"), Author::Agent, now).unwrap();
        let session = |template: &str, start: &str| {
            let mut s = PlannedSession::new("2026-11-16".into(), Some(template.into()), false);
            s.planned_start_at = at(start);
            s.insert(&db).unwrap();
            s
        };
        // Monday 12:00 and 18:00 local (UTC+1); the 18:00 one is another workout.
        let mut monday = session("short", "2026-11-16T11:00:00Z");
        session("other", "2026-11-16T17:00:00Z");
        assert_eq!(fire_due(&db, at("2026-11-16T09:59:00Z")).unwrap(), 0);
        assert_eq!(fire_due(&db, at("2026-11-16T10:00:30Z")).unwrap(), 1);
        assert_eq!(fire_due(&db, at("2026-11-16T10:05:00Z")).unwrap(), 0);
        assert_eq!(wakes(&db)[0]["payload"]["session_id"], monday.id);
        monday.planned_start_at = at("2026-11-16T13:00:00Z");
        monday.update(&db).unwrap();
        assert_eq!(fire_due(&db, at("2026-11-16T12:00:30Z")).unwrap(), 1);
        assert_eq!(fire_due(&db, at("2026-11-16T16:00:30Z")).unwrap(), 0);
        assert!(prompt(&db).unwrap().unwrap().contains("belt: 60 min before each short session"));
    }

    /// A hard turn wakes the agent to save a playbook, but only once the chat is quiet: it is never an
    /// open issue of an owner turn. A 👎 names what the owner asked before that reply. Capped per day.
    #[test]
    fn hard_turns_wake_a_reflection() {
        let db = crate::db::test::db();
        let now = at("2026-11-15T09:00:00Z");
        let say = |role, text: &str| {
            ChatMessage {
                id: None,
                at: now,
                role,
                text: text.into(),
                telegram_message_id: Some(1),
                meta_json: Json(json!({})),
            }
            .insert(&db)
            .unwrap();
            db.last_insert_rowid()
        };
        say(ChatRole::Owner, "swap my whole week to evenings");
        say(ChatRole::Assistant, "Done, all evenings.");
        say(ChatRole::Owner, "thanks");
        let tools = ["get_week".to_string(), "session_update (failed)".into()];
        assert!(
            reflect(
                &db,
                "r1",
                Lesson::Hard { forced: None, tools: &tools, asked: "swap my whole week to evenings" },
                now
            )
            .unwrap()
        );
        // A 👎 is now routed as an owner message, not a reflection (see bot.rs::reaction).
        let woken = |run: &str| {
            let events = crate::domain::events::wake_pending(&db).unwrap();
            events.into_iter().find(|e| e["type"] == REFLECT_EVENT && e["payload"]["run_id"] == run).unwrap()["payload"]
                .clone()
        };
        assert_eq!(woken("r1")["tools"], json!(tools));
        assert_eq!(woken("r1")["asked"], "swap my whole week to evenings");
        assert!(crate::domain::events::open_issues(&db).unwrap().is_empty());
        let hard = |asked| Lesson::Hard { forced: None, tools: &[], asked };
        assert!(reflect(&db, "r3", hard("another request"), now).unwrap());
        assert!(reflect(&db, "r4", hard("yet another"), now).unwrap());
        // 3 per day cap: the 4th is dropped.
        assert!(!reflect(&db, "r5", hard("one more"), now).unwrap());
    }

    #[test]
    fn trigger_times_must_be_local_clock_times() {
        let bad = Spec::Trigger { when: When::daily("9am"), instruction: "x".into() };
        assert_eq!(
            save(&crate::db::test::db(), "x", bad, Author::Agent, Ts::now()).unwrap_err().code(),
            "INVALID_EXTENSION"
        );
    }
}
