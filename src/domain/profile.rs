//! The owner's training constraints (one row). Changes are validated as a whole profile.
use jiff::civil::Time;
use jiff::tz::TimeZone;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::db::Connection;
use crate::db::Json;
use crate::db::models::UserProfile;
use crate::error::{Error, Result};
use crate::time::{Ts, zone};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Profile {
    pub timezone: String,
    pub weekly_min_sessions: i64,
    pub weekly_target_sessions: i64,
    pub weekly_max_sessions: i64,
    /// Monday = 0.
    pub weekdays_allowed: Vec<i64>,
    pub minimum_full_rest_days_between_sessions: i64,
    #[serde(with = "local_time")]
    #[schemars(with = "String")]
    pub earliest_workout_start: Time,
    #[serde(with = "local_time")]
    #[schemars(with = "String")]
    pub latest_workout_finish: Time,
    pub preferred_workout_minutes: i64,
    pub minimum_workout_minutes: i64,
    pub prep_minutes: i64,
    pub commute_to_gym_minutes: i64,
    pub commute_home_minutes: i64,
    pub warmup_policy: String,
    pub default_compound_rest_seconds: i64,
    pub default_accessory_rest_seconds: i64,
    pub crowd_preference_weight: f64,
    pub calendar_preference_weight: f64,
    pub recovery_weight: f64,
    /// Calendar event location: calendar apps derive travel time / time-to-leave from it.
    pub gym_address: Option<String>,
}

impl Default for Profile {
    fn default() -> Self {
        Profile {
            timezone: "Europe/Berlin".into(),
            weekly_min_sessions: 2,
            weekly_target_sessions: 3,
            weekly_max_sessions: 3,
            weekdays_allowed: vec![0, 1, 2, 3, 4],
            minimum_full_rest_days_between_sessions: 1,
            earliest_workout_start: Time::constant(7, 0, 0, 0),
            latest_workout_finish: Time::constant(22, 0, 0, 0),
            preferred_workout_minutes: 70,
            minimum_workout_minutes: 45,
            prep_minutes: 15,
            commute_to_gym_minutes: 20,
            commute_home_minutes: 20,
            warmup_policy: "first_primary_exercise_one_warmup_set".into(),
            default_compound_rest_seconds: 150,
            default_accessory_rest_seconds: 90,
            crowd_preference_weight: 1.0,
            calendar_preference_weight: 1.0,
            recovery_weight: 1.0,
            gym_address: None,
        }
    }
}

impl Profile {
    /// One rest duration for every exercise. The stored compound/template settings are
    /// retained for legacy data; `rest --remember` still updates both profile defaults.
    pub fn rest_seconds(&self) -> i64 {
        self.default_accessory_rest_seconds
    }

    pub fn tz(&self) -> TimeZone {
        zone(&self.timezone).unwrap_or(TimeZone::UTC)
    }

    pub fn validate(&self) -> Result<()> {
        let fail = |m: &str| Err(Error::domain("INVALID_PROFILE", m.to_string()));
        zone(&self.timezone)?;
        let counts = [self.weekly_min_sessions, self.weekly_target_sessions, self.minimum_full_rest_days_between_sessions, self.prep_minutes, self.commute_to_gym_minutes, self.commute_home_minutes];
        if counts.iter().any(|v| *v < 0) || !(0..=7).contains(&self.weekly_max_sessions) {
            return fail("Counts and minutes can't be negative; weekly maximum is 0–7");
        }
        if [self.preferred_workout_minutes, self.minimum_workout_minutes, self.default_compound_rest_seconds, self.default_accessory_rest_seconds].iter().any(|v| *v <= 0) {
            return fail("Durations must be positive");
        }
        if [self.crowd_preference_weight, self.calendar_preference_weight, self.recovery_weight].iter().any(|w| !w.is_finite() || *w < 0.0) {
            return fail("Weights must be finite and nonnegative");
        }
        if self.weekdays_allowed.iter().any(|d| !(0..=6).contains(d)) {
            return fail("Weekdays are 0 (Monday) to 6");
        }
        if !(self.weekly_min_sessions <= self.weekly_target_sessions && self.weekly_target_sessions <= self.weekly_max_sessions) {
            return fail("Weekly minimum <= target <= maximum required");
        }
        if self.minimum_workout_minutes > self.preferred_workout_minutes {
            return fail("Minimum duration exceeds preferred duration");
        }
        if self.earliest_workout_start >= self.latest_workout_finish {
            return fail("Workout bounds must fit within one local day");
        }
        let mut days = self.weekdays_allowed.clone();
        days.sort();
        days.dedup();
        if days.len() != self.weekdays_allowed.len() {
            return fail("Duplicate allowed weekdays");
        }
        if self.warmup_policy != "first_primary_exercise_one_warmup_set" {
            return fail("Unknown warmup policy");
        }
        if let Some(address) = &self.gym_address
            && !(3..=200).contains(&address.chars().count())
        {
            return fail("Gym address must be 3–200 characters");
        }
        Ok(())
    }

    fn from_row(row: &UserProfile) -> Result<Profile> {
        let time = |s: &str| local_time::parse(s).map_err(|e| Error::domain("INVALID_PROFILE", e));
        Ok(Profile {
            timezone: row.timezone.clone(),
            weekly_min_sessions: row.weekly_min_sessions,
            weekly_target_sessions: row.weekly_target_sessions,
            weekly_max_sessions: row.weekly_max_sessions,
            weekdays_allowed: row.weekdays_allowed.0.clone(),
            minimum_full_rest_days_between_sessions: row.minimum_full_rest_days_between_sessions,
            earliest_workout_start: time(&row.earliest_workout_start)?,
            latest_workout_finish: time(&row.latest_workout_finish)?,
            preferred_workout_minutes: row.preferred_workout_minutes,
            minimum_workout_minutes: row.minimum_workout_minutes,
            prep_minutes: row.prep_minutes,
            commute_to_gym_minutes: row.commute_to_gym_minutes,
            commute_home_minutes: row.commute_home_minutes,
            warmup_policy: row.warmup_policy.clone(),
            default_compound_rest_seconds: row.default_compound_rest_seconds,
            default_accessory_rest_seconds: row.default_accessory_rest_seconds,
            crowd_preference_weight: row.crowd_preference_weight,
            calendar_preference_weight: row.calendar_preference_weight,
            recovery_weight: row.recovery_weight,
            gym_address: row.gym_address.clone(),
        })
    }

    fn write(&self, row: &mut UserProfile) {
        row.timezone = self.timezone.clone();
        row.weekly_min_sessions = self.weekly_min_sessions;
        row.weekly_target_sessions = self.weekly_target_sessions;
        row.weekly_max_sessions = self.weekly_max_sessions;
        row.weekdays_allowed = Json(self.weekdays_allowed.clone());
        row.minimum_full_rest_days_between_sessions = self.minimum_full_rest_days_between_sessions;
        row.earliest_workout_start = local_time::format(self.earliest_workout_start);
        row.latest_workout_finish = local_time::format(self.latest_workout_finish);
        row.preferred_workout_minutes = self.preferred_workout_minutes;
        row.minimum_workout_minutes = self.minimum_workout_minutes;
        row.prep_minutes = self.prep_minutes;
        row.commute_to_gym_minutes = self.commute_to_gym_minutes;
        row.commute_home_minutes = self.commute_home_minutes;
        row.warmup_policy = self.warmup_policy.clone();
        row.default_compound_rest_seconds = self.default_compound_rest_seconds;
        row.default_accessory_rest_seconds = self.default_accessory_rest_seconds;
        row.crowd_preference_weight = self.crowd_preference_weight;
        row.calendar_preference_weight = self.calendar_preference_weight;
        row.recovery_weight = self.recovery_weight;
        row.gym_address = self.gym_address.clone();
    }
}

/// The profile, created with defaults (timezone from `GYMCLAW_TIMEZONE`) on first use.
pub fn get_profile(db: &Connection) -> Result<Profile> {
    if let Some(row) = UserProfile::get(db, 1)? {
        return Profile::from_row(&row);
    }
    let profile = Profile { timezone: std::env::var("GYMCLAW_TIMEZONE").unwrap_or_else(|_| "Europe/Berlin".into()), ..Profile::default() };
    let now = Ts::now();
    let mut row = UserProfile {
        id: 1, timezone: String::new(), weekly_min_sessions: 0, weekly_target_sessions: 0, weekly_max_sessions: 0, weekdays_allowed: Json(vec![]),
        minimum_full_rest_days_between_sessions: 0, earliest_workout_start: String::new(), latest_workout_finish: String::new(),
        preferred_workout_minutes: 0, minimum_workout_minutes: 0, prep_minutes: 0, commute_to_gym_minutes: 0, commute_home_minutes: 0,
        warmup_policy: String::new(), default_compound_rest_seconds: 0, default_accessory_rest_seconds: 0, crowd_preference_weight: 0.0,
        calendar_preference_weight: 0.0, recovery_weight: 0.0, gym_address: None, created_at: now, updated_at: now,
    };
    profile.write(&mut row);
    row.insert(db)?;
    Ok(profile)
}

/// Apply `changes` (a JSON object of profile fields) and validate the result as a whole.
pub fn update_profile(db: &Connection, changes: &Map<String, Value>) -> Result<Profile> {
    let mut merged = serde_json::to_value(get_profile(db)?)?;
    let object = merged.as_object_mut().expect("profile is an object");
    for (key, value) in changes {
        object.insert(key.clone(), value.clone());
    }
    let profile: Profile = serde_json::from_value(merged).map_err(|e| Error::domain("INVALID_PROFILE", e.to_string()))?;
    profile.validate()?;
    let mut row = UserProfile::get(db, 1)?.expect("created by get_profile");
    profile.write(&mut row);
    row.updated_at = Ts::now();
    row.update(db)?;
    Ok(profile)
}

/// Local wall-clock times as Python's `time.isoformat()`: "07:00:00" (input also takes "07:00").
pub mod local_time {
    use jiff::civil::Time;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn parse(text: &str) -> Result<Time, String> {
        let text = text.trim();
        let full = if text.len() == 5 { format!("{text}:00") } else { text.to_string() };
        full.parse::<Time>().map_err(|_| format!("Not a local time (HH:MM): {text}"))
    }

    pub fn format(time: Time) -> String {
        time.strftime("%H:%M:%S").to_string()
    }

    pub fn serialize<S: Serializer>(time: &Time, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format(*time))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Time, D::Error> {
        parse(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
