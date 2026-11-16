//! Training outside the gym: runs, matches, hikes, one-off or weekly ("football every Tuesday"). Code
//! only stores them and shows them where planning looks (`get_week`, the Sunday wake); what an activity
//! means for the plan is the agent's call. Their minutes count toward cardio goals.
use jiff::civil::Date;
use jiff::tz::TimeZone;
use serde_json::{Value, json};

use crate::db::models::{Activity, Intensity};
use crate::db::{Connection, new_id};
use crate::domain::calendar::week_of;
use crate::domain::core::mutate;
use crate::domain::profile::get_profile;
use crate::error::Result;
use crate::time::Ts;

/// What `log` and `update` set; `None` keeps the current value on update.
#[derive(Clone, Debug, Default)]
pub struct Fields {
    pub kind: Option<String>,
    pub at: Option<Ts>,
    pub minutes: Option<i64>,
    pub intensity: Option<Intensity>,
    pub notes: Option<String>,
    pub weekly: Option<bool>,
    /// Weekly: the last day it happens.
    pub until: Option<Date>,
}

impl Fields {
    /// The idempotency intent: what a retry must repeat.
    fn intent(&self) -> Value {
        json!({"kind": self.kind, "at": self.at.map(Ts::db), "minutes": self.minutes, "intensity": self.intensity,
            "notes": self.notes, "weekly": self.weekly, "until": self.until.map(|d| d.to_string())})
    }
}

/// One occurrence of an activity in a week.
pub struct Occurrence {
    pub activity: Activity,
    pub at: Ts,
}

impl Occurrence {
    /// "Tue Oct 20 19:00 football 90 min, hard, weekly" for the Sunday wake.
    pub fn line(&self, tz: &TimeZone) -> String {
        let a = &self.activity;
        let weekly = if a.weekly { ", weekly" } else { "" };
        let at = self.at.local(tz).strftime("%a %b %-d %H:%M");
        format!("{at} {} {} min, {}{weekly}", a.kind, a.minutes, a.intensity)
    }

    pub fn data(&self) -> Value {
        let a = &self.activity;
        json!({"id": a.id, "kind": a.kind, "at": self.at, "minutes": a.minutes, "intensity": a.intensity,
            "notes": a.notes, "weekly": a.weekly})
    }
}

/// When `activity` happens in `week` (Monday, owner-local): a one-off once in its own week, a weekly one
/// at its local weekday and time from its first week until `until_at`.
fn occurs(activity: &Activity, week: Date, tz: &TimeZone) -> Result<Option<Ts>> {
    let first = activity.started_at.local(tz);
    if !activity.weekly {
        return Ok((week_of(activity.started_at, tz) == week).then_some(activity.started_at));
    }
    if week < week_of(activity.started_at, tz) {
        return Ok(None);
    }
    let day = week.checked_add(jiff::Span::new().days(first.weekday().to_monday_zero_offset() as i64))?;
    let at = Ts(day.to_datetime(first.time()).to_zoned(tz.clone())?.timestamp());
    Ok((activity.until_at.is_none_or(|u| at < u)).then_some(at))
}

/// Activities in `week`, in time order. Removed ones are left out.
pub fn in_week(db: &Connection, week: Date, tz: &TimeZone) -> Result<Vec<Occurrence>> {
    let mut found = Vec::new();
    for activity in Activity::select(db, "WHERE removed_at IS NULL", [])? {
        if let Some(at) = occurs(&activity, week, tz)? {
            found.push(Occurrence { activity, at });
        }
    }
    found.sort_by_key(|o| o.at);
    Ok(found)
}

fn validate(row: &Activity) -> Result<()> {
    if row.kind.trim().is_empty() {
        crate::bail!("INVALID_INPUT", "Say what the activity was (kind), e.g. \"run 8k\"");
    }
    if !(1..=24 * 60).contains(&row.minutes) {
        crate::bail!("INVALID_INPUT", "minutes must be 1–1440");
    }
    Ok(())
}

fn apply(row: &mut Activity, fields: &Fields, tz: &TimeZone) -> Result<()> {
    if let Some(kind) = &fields.kind {
        row.kind = kind.trim().to_string();
    }
    row.started_at = fields.at.unwrap_or(row.started_at);
    row.minutes = fields.minutes.unwrap_or(row.minutes);
    row.intensity = fields.intensity.unwrap_or(row.intensity);
    if fields.notes.is_some() {
        row.notes = fields.notes.clone();
    }
    row.weekly = fields.weekly.unwrap_or(row.weekly);
    if let Some(until) = fields.until {
        row.until_at = Some(Ts(until.tomorrow()?.to_zoned(tz.clone())?.timestamp()));
    }
    validate(row)
}

/// Log an activity the owner did or will do; `kind` and `minutes` are required.
pub fn log(db: &Connection, fields: Fields, now: Ts, request_id: &str) -> Result<Value> {
    mutate(db, "activity.logged", request_id, fields.intent(), now, || {
        let tz = get_profile(db)?.tz();
        let (Some(_), Some(_)) = (&fields.kind, fields.minutes) else {
            crate::bail!("ARGUMENT_REQUIRED", "Logging an activity needs kind and minutes");
        };
        let mut row = Activity {
            id: new_id(),
            kind: String::new(),
            started_at: now,
            minutes: 0,
            intensity: Intensity::Moderate,
            notes: None,
            weekly: false,
            until_at: None,
            removed_at: None,
            created_at: now,
        };
        apply(&mut row, &fields, &tz)?;
        row.insert(db)?;
        Ok(json!({"activity": row}))
    })
}

fn load(db: &Connection, id: &str) -> Result<Activity> {
    match Activity::get(db, id)?.filter(|a| a.removed_at.is_none()) {
        Some(row) => Ok(row),
        None => crate::bail!("ACTIVITY_NOT_FOUND", "No activity {id}; get_week lists a week's activities"),
    }
}

/// Change a logged activity, e.g. end a weekly one (`until`) or fix its minutes.
pub fn update(db: &Connection, id: &str, fields: Fields, now: Ts, request_id: &str) -> Result<Value> {
    let intent = json!({"id": id, "fields": fields.intent()});
    mutate(db, "activity.updated", request_id, intent, now, || {
        let mut row = load(db, id)?;
        apply(&mut row, &fields, &get_profile(db)?.tz())?;
        row.update(db)?;
        Ok(json!({"activity": row}))
    })
}

/// Remove an activity logged by mistake: it no longer counts anywhere (a weekly one stops with `until`).
pub fn remove(db: &Connection, id: &str, now: Ts, request_id: &str) -> Result<Value> {
    mutate(db, "activity.removed", request_id, json!({"id": id}), now, || {
        let mut row = load(db, id)?;
        row.removed_at = Some(now);
        row.update(db)?;
        Ok(json!({"removed": row.id}))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn week(day: &str) -> Date {
        day.parse().unwrap()
    }

    #[test]
    fn weekly_activities_repeat_at_local_time_until_they_end() {
        let db = crate::db::test::db();
        let tz = get_profile(&db).unwrap().tz();
        let tuesday = Ts::parse("2026-10-20T19:00:00", Some(&tz)).unwrap();
        let football = Fields {
            kind: Some("football".into()),
            at: Some(tuesday),
            minutes: Some(90),
            weekly: Some(true),
            ..Fields::default()
        };
        let id = log(&db, football, tuesday, "r1").unwrap()["data"]["activity"]["id"].as_str().unwrap().to_string();
        let sunday = Ts::parse("2026-10-25T19:00:00", Some(&tz)).unwrap();
        log(
            &db,
            Fields { kind: Some("run 8k".into()), at: Some(sunday), minutes: Some(45), ..Fields::default() },
            tuesday,
            "r2",
        )
        .unwrap();
        let days = |w: &str| -> Vec<String> {
            in_week(&db, week(w), &tz)
                .unwrap()
                .iter()
                .map(|o| o.at.local(&tz).strftime("%a %d %H:%M").to_string())
                .collect()
        };
        assert!(days("2026-10-12").is_empty());
        assert_eq!(days("2026-10-19"), ["Tue 20 19:00", "Sun 25 19:00"]);
        // After the switch from summer time the wall-clock time stays.
        assert_eq!(days("2026-11-02"), ["Tue 03 19:00"]);
        // Last day Monday the 16th: that week's match is off.
        update(&db, &id, Fields { until: Some(week("2026-11-16")), ..Fields::default() }, tuesday, "r3").unwrap();
        assert_eq!(days("2026-11-09"), ["Tue 10 19:00"]);
        assert!(days("2026-11-16").is_empty());
    }
}
