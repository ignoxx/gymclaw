//! UTC instants stored the way SQLAlchemy's `UTCDateTime` does: naive UTC text with microseconds.
use std::fmt;

use jiff::{SignedDuration, Timestamp, Zoned, civil, tz::TimeZone};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ts(pub Timestamp);

impl Ts {
    pub fn now() -> Ts {
        Ts(Timestamp::now())
    }

    /// RFC 3339 with an offset. Naive input is the owner's local time when `local` is given, else an error.
    pub fn parse(text: &str, local: Option<&TimeZone>) -> Result<Ts> {
        let text = text.trim();
        if let Ok(ts) = text.parse::<Timestamp>() {
            return Ok(Ts(ts));
        }
        let naive = text
            .parse::<civil::DateTime>()
            .or_else(|_| text.parse::<civil::Date>().map(|d| d.to_datetime(civil::Time::midnight())))
            .map_err(|_| Error::domain("INVALID_TIME", format!("Not a timestamp: {text}")))?;
        let zone = local.ok_or_else(|| Error::domain("INVALID_TIME", "Timestamp must include timezone"))?;
        Ok(Ts(naive.to_zoned(zone.clone())?.timestamp()))
    }

    pub fn from_db(text: &str) -> Option<Ts> {
        let naive: civil::DateTime = text.trim().replace(' ', "T").parse().ok()?;
        Some(Ts(naive.to_zoned(TimeZone::UTC).ok()?.timestamp()))
    }

    /// `2026-10-12 12:07:18.231715`, the SQLAlchemy SQLite storage format.
    pub fn db(self) -> String {
        self.0.strftime("%Y-%m-%d %H:%M:%S%.6f").to_string()
    }

    /// Python `isoformat()` of an aware UTC datetime: `2026-10-14T10:30:00+00:00` (micros only when set).
    pub fn iso(self) -> String {
        iso(&self.0.to_zoned(TimeZone::UTC))
    }

    pub fn local(self, zone: &TimeZone) -> Zoned {
        self.0.to_zoned(zone.clone())
    }

    /// `ts.mins(-15)`: 15 minutes earlier.
    pub fn mins(self, n: i64) -> Ts {
        self + SignedDuration::from_mins(n)
    }
}

impl std::ops::Add<SignedDuration> for Ts {
    type Output = Ts;
    fn add(self, d: SignedDuration) -> Ts {
        Ts(self.0.checked_add(d).expect("timestamp in range"))
    }
}

impl std::ops::Sub<SignedDuration> for Ts {
    type Output = Ts;
    fn sub(self, d: SignedDuration) -> Ts {
        Ts(self.0.checked_sub(d).expect("timestamp in range"))
    }
}

/// `later - earlier`.
impl std::ops::Sub<Ts> for Ts {
    type Output = SignedDuration;
    fn sub(self, earlier: Ts) -> SignedDuration {
        self.0.duration_since(earlier.0)
    }
}

/// Python `isoformat()` of an aware datetime in any zone: `2026-10-16T12:00:00+02:00`.
pub fn iso(z: &Zoned) -> String {
    let micros = z.subsec_nanosecond() / 1000;
    let base = z.strftime("%Y-%m-%dT%H:%M:%S").to_string();
    let offset = z.strftime("%:z").to_string();
    if micros == 0 { format!("{base}{offset}") } else { format!("{base}.{micros:06}{offset}") }
}

pub fn zone(name: &str) -> Result<TimeZone> {
    TimeZone::get(name).map_err(|_| Error::domain("INVALID_TIMEZONE", format!("Unknown timezone: {name}")))
}

impl fmt::Display for Ts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.iso())
    }
}

impl ToSql for Ts {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.db()))
    }
}

impl FromSql for Ts {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        Ts::from_db(text).ok_or_else(|| FromSqlError::Other(format!("bad timestamp {text:?}").into()))
    }
}

impl Serialize for Ts {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.iso())
    }
}

impl<'de> Deserialize<'de> for Ts {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        Ts::parse(&text, None).map_err(serde::de::Error::custom)
    }
}

impl schemars::JsonSchema for Ts {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Timestamp".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type": "string", "description": "ISO 8601 timestamp; without an offset it is the owner's local time"})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_python_formats() {
        let ts = Ts::from_db("2026-10-12 12:07:18.231715").unwrap();
        assert_eq!(ts.db(), "2026-10-12 12:07:18.231715");
        assert_eq!(ts.iso(), "2026-10-12T12:07:18.231715+00:00");
        let berlin = zone("Europe/Berlin").unwrap();
        let local = Ts::parse("2026-10-16T12:00", Some(&berlin)).unwrap();
        assert_eq!(local.iso(), "2026-10-16T10:00:00+00:00");
        assert_eq!(iso(&local.local(&berlin)), "2026-10-16T12:00:00+02:00");
    }
}
