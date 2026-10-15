//! RFC 5545 recurrence sets (RRULE, RDATE, EXDATE), expanded on the wall clock the way Python's
//! `dateutil` + `recurring-ical-events` do. Shared by the ICS feed (`providers::ics`) and the
//! Google recurring-master expansion in `domain::calendar_busy` (`occurrences`).
//!
//! Semantics worth knowing:
//! - Rules step in local wall-clock time, so a 19:00 Europe/Berlin series stays at 19:00 across DST.
//! - A wall time that doesn't exist or happens twice resolves like Python's `fold=0`
//!   (jiff's "compatible" disambiguation).
//! - UNTIL without `Z` on a zoned series is read as UTC, and a date UNTIL as UTC midnight
//!   (recurring-ical-events' repair of rules `dateutil` rejects). EXDATE matches by UTC or local time;
//!   a date-valued EXDATE removes every instance on that date.
use std::cmp::Ordering;
use std::collections::HashSet;

use jiff::civil::{self, Date, DateTime, Weekday};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp, ToSpan};

use crate::error::{Error, Result};
use crate::time::Ts;

/// An iCalendar time value: a date, a floating (zone-less) wall time, or a zoned time.
#[derive(Clone, Debug, PartialEq)]
pub enum When {
    Date(Date),
    Floating(DateTime),
    /// Wall time in a zone. Kept as wall time (like a Python aware datetime), so a time that
    /// doesn't exist on a DST day still adds up on the wall clock the way Python does.
    Zoned(DateTime, TimeZone),
}

impl When {
    /// Parse `20261019`, `20261019T190000`, `20261019T170000Z`; `tzid` applies to local date-times.
    /// Unknown TZIDs fall back to floating, like Python's `icalendar`.
    pub fn parse(value: &str, tzid: Option<&str>, date: bool) -> Result<When> {
        let value = value.trim();
        let bad = || invalid(format!("bad date/time {value:?}"));
        let digits = |range: std::ops::Range<usize>| -> Result<i32> {
            let part = value.get(range).ok_or_else(bad)?;
            if !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad());
            }
            part.parse().map_err(|_| bad())
        };
        let day = civil::Date::new(digits(0..4)? as i16, digits(4..6)? as i8, digits(6..8)? as i8).map_err(|_| bad())?;
        if value.len() == 8 {
            return Ok(When::Date(day));
        }
        if date || value.as_bytes().get(8) != Some(&b'T') || !(value.len() == 15 || value.len() == 16 && value.ends_with(['Z', 'z'])) {
            return Err(bad());
        }
        let time = civil::Time::new(digits(9..11)? as i8, digits(11..13)? as i8, digits(13..15)?.min(59) as i8, 0).map_err(|_| bad())?;
        let local = day.to_datetime(time);
        if value.len() == 16 {
            return Ok(When::Zoned(local, TimeZone::UTC));
        }
        match tzid.and_then(zone_for_tzid) {
            Some(tz) => Ok(When::Zoned(local, tz)),
            None => Ok(When::Floating(local)),
        }
    }

    /// Wall-clock time (dates at midnight).
    pub fn civil(&self) -> DateTime {
        match self {
            When::Date(d) => d.to_datetime(civil::Time::midnight()),
            When::Floating(dt) => *dt,
            When::Zoned(dt, _) => *dt,
        }
    }

    fn tz(&self) -> Option<&TimeZone> {
        match self {
            When::Zoned(_, tz) => Some(tz),
            _ => None,
        }
    }

    /// The instant: dates and floating times are read in `zone` (Python `ics_feed._aware`).
    pub fn to_ts(&self, zone: &TimeZone) -> Result<Ts> {
        Ok(Ts(match self {
            When::Zoned(dt, tz) => instant(*dt, tz)?,
            other => other.civil().to_zoned(zone.clone())?.timestamp(),
        }))
    }

    /// Python `convert_to_datetime(value, tz)`: dates become midnight, naive times get `tz`,
    /// zoned times lose their zone when `tz` is None.
    fn with_tz(&self, tz: Option<&TimeZone>) -> Result<When> {
        Ok(match (self, tz) {
            (When::Zoned(..), Some(_)) => self.clone(),
            (When::Zoned(dt, _), None) => When::Floating(*dt),
            (_, Some(tz)) => When::Zoned(self.civil(), tz.clone()),
            (_, None) => When::Floating(self.civil()),
        })
    }

    /// Python `to_recurrence_ids`: UTC and local wall time for zoned values, the wall time otherwise.
    fn ids(&self) -> Vec<DateTime> {
        match self {
            When::Zoned(dt, tz) => match instant(*dt, tz) {
                Ok(at) => vec![TimeZone::UTC.to_datetime(at), *dt],
                Err(_) => vec![*dt],
            },
            other => vec![other.civil()],
        }
    }

    fn add(&self, d: SignedDuration) -> Result<When> {
        Ok(match self {
            When::Date(day) => When::Date(day.checked_add((d.as_secs().div_euclid(86_400)).days())?),
            When::Floating(dt) => When::Floating(dt.checked_add(d)?),
            // Python aware arithmetic: add on the wall clock, keep the zone.
            When::Zoned(dt, tz) => When::Zoned(dt.checked_add(d)?, tz.clone()),
        })
    }
}

/// Python `make_comparable` + `<`: dates compare as dates; otherwise both become datetimes in the
/// first zone either one has, and compare as instants (or wall times when neither has a zone).
pub fn compare(a: &When, b: &When) -> Result<Ordering> {
    if let (When::Date(x), When::Date(y)) = (a, b) {
        return Ok(x.cmp(y));
    }
    let tz = a.tz().or(b.tz()).cloned();
    Ok(match (a.with_tz(tz.as_ref())?, b.with_tz(tz.as_ref())?) {
        (When::Zoned(x, xz), When::Zoned(y, yz)) => instant(x, &xz)?.cmp(&instant(y, &yz)?),
        (x, y) => x.civil().cmp(&y.civil()),
    })
}

/// Python `end - start` after `make_comparable`: wall-clock difference within one zone, absolute across zones.
pub fn difference(end: &When, start: &When) -> Result<SignedDuration> {
    let tz = start.tz().or(end.tz()).cloned();
    Ok(match (end.with_tz(tz.as_ref())?, start.with_tz(tz.as_ref())?) {
        (When::Zoned(e, ez), When::Zoned(s, sz)) if ez.iana_name() != sz.iana_name() || ez.iana_name().is_none() => {
            instant(e, &ez)?.duration_since(instant(s, &sz)?)
        }
        (e, s) => e.civil().duration_since(s.civil()),
    })
}

/// `TZID` → zone. Vendor prefixes (`/mozilla.org/20050126_1/Europe/Berlin`) are stripped.
fn zone_for_tzid(tzid: &str) -> Option<TimeZone> {
    let tzid = tzid.trim().trim_matches('"');
    let parts: Vec<&str> = tzid.split('/').collect();
    (0..parts.len()).find_map(|i| TimeZone::get(&parts[i..].join("/")).ok())
}

/// Wall time → instant; gaps and folds resolve like Python's `fold=0`.
pub fn instant(dt: DateTime, tz: &TimeZone) -> Result<Timestamp> {
    Ok(dt.to_zoned(tz.clone())?.timestamp())
}

fn invalid(detail: impl Into<String>) -> Error {
    Error::domain(
        "CALENDAR_RECURRENCE_INVALID",
        format!("Calendar recurrence could not be expanded; plan unchanged ({})", detail.into()),
    )
}

/// One iCalendar content line: `NAME;PARAM=V;...:value`, name and param keys upper-cased.
pub(crate) struct ContentLine<'a> {
    pub name: String,
    pub params: Vec<(String, &'a str)>,
    pub value: &'a str,
}

impl<'a> ContentLine<'a> {
    pub fn parse(line: &'a str) -> Option<ContentLine<'a>> {
        // The value starts at the first ':' outside a quoted parameter value.
        let mut quoted = false;
        let colon = line
            .char_indices()
            .find(|&(_, c)| {
                if c == '"' {
                    quoted = !quoted;
                }
                c == ':' && !quoted
            })?
            .0;
        let mut head = line[..colon].split(';');
        let name = head.next()?.trim().to_ascii_uppercase();
        let params = head
            .filter_map(|p| p.split_once('='))
            .map(|(k, v)| (k.trim().to_ascii_uppercase(), v.trim().trim_matches('"')))
            .collect();
        Some(ContentLine {
            name,
            params,
            value: &line[colon + 1..],
        })
    }

    pub fn param(&self, key: &str) -> Option<&'a str> {
        self.params.iter().find(|(k, _)| k == key).map(|(_, v)| *v)
    }

    /// Comma-separated date/date-time values (DTSTART, EXDATE, RDATE, RECURRENCE-ID).
    pub fn times(&self) -> Result<Vec<When>> {
        let date = self.param("VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE"));
        self.value
            .split(',')
            .filter(|v| !v.trim().is_empty())
            .map(|v| When::parse(v, self.param("TZID"), date))
            .collect()
    }
}

/// RDATE value: an instant, or a PERIOD with its own length.
#[derive(Clone, Debug)]
pub enum RDate {
    At(When),
    Period(When, SignedDuration),
}

/// RFC 5545 DURATION (`PT1H`, `-P1D`, `P2W`) as seconds.
pub fn parse_duration(text: &str) -> Result<SignedDuration> {
    let text = text.trim().to_ascii_uppercase();
    let bad = || invalid(format!("bad duration {text:?}"));
    let (sign, rest) = match text.strip_prefix('-') {
        Some(r) => (-1, r),
        None => (1, text.strip_prefix('+').unwrap_or(&text)),
    };
    let rest = rest.strip_prefix('P').ok_or_else(bad)?;
    let (mut secs, mut number, mut in_time, mut any) = (0i64, String::new(), false, false);
    for c in rest.chars() {
        match c {
            '0'..='9' => number.push(c),
            'T' if number.is_empty() => in_time = true,
            'W' | 'D' | 'H' | 'M' | 'S' => {
                let n: i64 = number.parse().map_err(|_| bad())?;
                number.clear();
                any = true;
                secs += n * match (c, in_time) {
                    ('W', false) => 604_800,
                    ('D', false) => 86_400,
                    ('H', true) => 3_600,
                    ('M', true) => 60,
                    ('S', true) => 1,
                    _ => return Err(bad()),
                };
            }
            _ => return Err(bad()),
        }
    }
    if !any || !number.is_empty() {
        return Err(bad());
    }
    Ok(SignedDuration::from_secs(sign * secs))
}

/// A recurrence set anchored at its first instance.
pub struct Series {
    start: When,
    rules: Vec<(Rule, Option<When>)>,
    rdates: Vec<RDate>,
    exdate_ids: HashSet<DateTime>,
    exdate_days: HashSet<Date>,
    /// RFC 5545/recurring-ical-events: DTSTART is always an instance. `dateutil` (Google masters) doesn't add it.
    include_start: bool,
}

/// One generated instance; `length` is set for RDATE periods.
#[derive(Clone, Debug)]
pub struct Instance {
    pub start: When,
    pub length: Option<SignedDuration>,
}

impl Instance {
    /// Python `to_recurrence_ids` of this instance's start.
    pub fn ids(&self) -> Vec<DateTime> {
        self.start.ids()
    }
}

impl Series {
    /// `end` only helps pick the series zone (Python `make_all_dates_comparable`).
    pub fn new(start: &When, end: Option<&When>, rrules: &[&str], rdates: Vec<RDate>, exdates: &[When], include_start: bool) -> Result<Series> {
        let rdate_starts = rdates.iter().map(|r| match r {
            RDate::At(w) | RDate::Period(w, _) => w,
        });
        let everything: Vec<&When> = std::iter::once(start).chain(end).chain(exdates).chain(rdate_starts).collect();
        let all_dates = everything.iter().all(|w| matches!(w, When::Date(_)));
        let tz = everything.iter().find_map(|w| w.tz()).cloned();
        let tz = tz.as_ref();
        let first = start.with_tz(tz)?;
        let mut rules = Vec::new();
        for text in rrules {
            let rule = Rule::parse(text, first.civil())?;
            let until = rule.until.as_ref().map(|u| repair_until(u, &first, all_dates)).transpose()?;
            rules.push((rule, until));
        }
        let rdates = rdates
            .into_iter()
            .map(|r| {
                Ok(match r {
                    RDate::At(w) => RDate::At(w.with_tz(tz)?),
                    RDate::Period(w, d) => RDate::Period(w.with_tz(tz)?, d),
                })
            })
            .collect::<Result<_>>()?;
        let mut exdate_ids = HashSet::new();
        let mut exdate_days = HashSet::new();
        for exdate in exdates {
            exdate_ids.extend(exdate.ids());
            if let When::Date(d) = exdate {
                exdate_days.insert(*d);
            }
        }
        Ok(Series {
            start: first,
            rules,
            rdates,
            exdate_ids,
            exdate_days,
            include_start,
        })
    }

    /// Whether `ids` (Python recurrence ids) hit an EXDATE.
    pub fn excludes(&self, ids: &[DateTime]) -> bool {
        ids.iter().any(|id| self.exdate_ids.contains(id))
    }

    /// Instances with `lower <= start < upper`, sorted, EXDATEs removed. More than `limit` is an error.
    pub fn between(&self, lower: &When, upper: &When, limit: usize) -> Result<Vec<Instance>> {
        let tz = self.start.tz();
        let horizon = upper.with_tz(tz)?.civil().checked_add(2.days())?;
        let from = lower.with_tz(tz)?.civil().checked_sub(2.days())?;
        let wrap = |dt: DateTime| -> Result<When> {
            Ok(match tz {
                Some(tz) => When::Zoned(dt, tz.clone()),
                None => When::Floating(dt),
            })
        };
        let mut found: Vec<Instance> = Vec::new();
        let mut keep = |instance: Instance| -> Result<()> {
            if compare(&instance.start, lower)? != Ordering::Less && compare(&instance.start, upper)? == Ordering::Less {
                found.push(instance);
            }
            Ok(())
        };
        let mut last_until: Option<&When> = None;
        for (rule, until) in &self.rules {
            for dt in rule.iter(self.start.civil(), from, horizon) {
                let start = wrap(dt)?;
                if let Some(until) = until
                    && compare(&start, until)? == Ordering::Greater
                {
                    break;
                }
                keep(Instance { start, length: None })?;
            }
            match (until, last_until) {
                (Some(u), Some(last)) if compare(u, last)? != Ordering::Greater => {}
                (Some(u), _) => last_until = Some(u),
                _ => {}
            }
        }
        let start_counts = match last_until {
            Some(last) => compare(&self.start, last)? != Ordering::Greater,
            None => true,
        };
        if self.include_start && start_counts {
            keep(Instance {
                start: self.start.clone(),
                length: None,
            })?;
        }
        for rdate in &self.rdates {
            keep(match rdate {
                RDate::At(w) => Instance {
                    start: w.clone(),
                    length: None,
                },
                RDate::Period(w, d) => Instance {
                    start: w.clone(),
                    length: Some(*d),
                },
            })?;
        }
        let mut seen = HashSet::new();
        let mut result = Vec::new();
        found.sort_by(|a, b| compare(&a.start, &b.start).unwrap_or(Ordering::Equal));
        for instance in found {
            let ids = instance.ids();
            if !seen.insert(ids[0]) || self.excludes(&ids) || self.exdate_days.contains(&instance.start.civil().date()) {
                continue;
            }
            if result.len() == limit {
                crate::bail!("CALENDAR_RECURRENCE_TOO_LARGE", "Too many recurring blockers; simplify calendar recurrence");
            }
            result.push(instance);
        }
        Ok(result)
    }

    /// Instance end: start + series length on the wall clock (Python aware `+ timedelta`).
    pub fn end_of(instance: &Instance, length: SignedDuration) -> Result<When> {
        instance.start.add(instance.length.unwrap_or(length))
    }
}

/// recurring-ical-events' UNTIL repair, so rules `dateutil` rejects still expand.
fn repair_until(until: &When, start: &When, all_dates: bool) -> Result<When> {
    Ok(match (until, start) {
        (u, _) if all_dates => When::Date(u.civil().date()),
        (When::Zoned(dt, _), When::Floating(_)) => When::Floating(*dt),
        (When::Date(_) | When::Floating(_), When::Zoned(..)) => When::Zoned(until.civil(), TimeZone::UTC),
        (u, _) => u.clone(),
    })
}

/// Recurrence starts of `lines` (`RRULE:…`, `EXDATE;TZID=…:…`, `RDATE…`, as Google's `recurrence`
/// array has them) with `lower <= start < upper`, sorted. `dtstart` is the first instance: zoned for
/// timed events, `When::Date` for all-day ones. Like `dateutil.rrulestr(forceset=True)`, DTSTART only
/// counts when the rule produces it. More than `limit` instances → CALENDAR_RECURRENCE_TOO_LARGE;
/// unparsable lines → CALENDAR_RECURRENCE_INVALID.
pub fn occurrences(lines: &[impl AsRef<str>], dtstart: &When, lower: &When, upper: &When, limit: usize) -> Result<Vec<When>> {
    let mut rrules = Vec::new();
    let mut rdates = Vec::new();
    let mut exdates = Vec::new();
    for line in lines {
        let line = line.as_ref().trim();
        let parsed = ContentLine::parse(line).ok_or_else(|| invalid(format!("bad line {line:?}")))?;
        match parsed.name.as_str() {
            "RRULE" => rrules.push(parsed.value),
            "EXDATE" => exdates.extend(parsed.times()?),
            "RDATE" => rdates.extend(rdate_values(&parsed)?),
            other => return Err(invalid(format!("unsupported property {other}"))),
        }
    }
    let series = Series::new(dtstart, None, &rrules, rdates, &exdates, false)?;
    Ok(series.between(lower, upper, limit)?.into_iter().map(|i| i.start).collect())
}

/// RDATE values, including `VALUE=PERIOD` (`start/end` or `start/duration`).
pub(crate) fn rdate_values(line: &ContentLine<'_>) -> Result<Vec<RDate>> {
    if !line.param("VALUE").is_some_and(|v| v.eq_ignore_ascii_case("PERIOD")) {
        return Ok(line.times()?.into_iter().map(RDate::At).collect());
    }
    line.value
        .split(',')
        .map(|period| {
            let (a, b) = period.split_once('/').ok_or_else(|| invalid("bad period"))?;
            let start = When::parse(a, line.param("TZID"), false)?;
            let length = if b.trim_start().starts_with(['P', 'p', '-', '+']) {
                parse_duration(b)?
            } else {
                difference(&When::parse(b, line.param("TZID"), false)?, &start)?
            };
            Ok(RDate::Period(start, length))
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Freq {
    Yearly,
    Monthly,
    Weekly,
    Daily,
    Hourly,
    Minutely,
    Secondly,
}

/// A parsed RRULE with `dateutil`'s defaults filled in from DTSTART.
#[derive(Clone, Debug)]
struct Rule {
    freq: Freq,
    interval: i64,
    count: Option<u32>,
    until: Option<When>,
    by_month: Vec<i8>,
    by_week_no: Vec<i8>,
    by_year_day: Vec<i16>,
    by_month_day: Vec<i8>,
    /// Plain weekdays, and (n, weekday) for "nth weekday of the month/year".
    by_weekday: Vec<Weekday>,
    by_nth_weekday: Vec<(i8, Weekday)>,
    by_hour: Vec<i8>,
    by_minute: Vec<i8>,
    by_second: Vec<i8>,
    by_set_pos: Vec<i32>,
    week_start: Weekday,
}

fn weekday(code: &str) -> Option<Weekday> {
    Some(match code {
        "MO" => Weekday::Monday,
        "TU" => Weekday::Tuesday,
        "WE" => Weekday::Wednesday,
        "TH" => Weekday::Thursday,
        "FR" => Weekday::Friday,
        "SA" => Weekday::Saturday,
        "SU" => Weekday::Sunday,
        _ => return None,
    })
}

impl Rule {
    fn parse(text: &str, start: DateTime) -> Result<Rule> {
        let text = text.trim();
        let text = if text.len() >= 6 && text[..6].eq_ignore_ascii_case("RRULE:") {
            &text[6..]
        } else {
            text
        };
        let mut freq = None;
        let mut rule = Rule {
            freq: Freq::Daily,
            interval: 1,
            count: None,
            until: None,
            by_month: vec![],
            by_week_no: vec![],
            by_year_day: vec![],
            by_month_day: vec![],
            by_weekday: vec![],
            by_nth_weekday: vec![],
            by_hour: vec![],
            by_minute: vec![],
            by_second: vec![],
            by_set_pos: vec![],
            week_start: Weekday::Monday,
        };
        fn list<T: std::str::FromStr + PartialOrd + Default>(value: &str, range: std::ops::RangeInclusive<T>, nonzero: bool) -> Result<Vec<T>> {
            value
                .split(',')
                .map(|v| {
                    let n: T = v.trim().parse().map_err(|_| invalid(format!("bad number {v:?}")))?;
                    if !range.contains(&n) || nonzero && n == T::default() {
                        return Err(invalid(format!("out of range {v:?}")));
                    }
                    Ok(n)
                })
                .collect()
        }
        for part in text.split(';').filter(|p| !p.trim().is_empty()) {
            let (key, value) = part.split_once('=').ok_or_else(|| invalid(format!("bad rule part {part:?}")))?;
            let value = value.trim().to_ascii_uppercase();
            match key.trim().to_ascii_uppercase().as_str() {
                "FREQ" => {
                    freq = Some(match value.as_str() {
                        "YEARLY" => Freq::Yearly,
                        "MONTHLY" => Freq::Monthly,
                        "WEEKLY" => Freq::Weekly,
                        "DAILY" => Freq::Daily,
                        "HOURLY" => Freq::Hourly,
                        "MINUTELY" => Freq::Minutely,
                        "SECONDLY" => Freq::Secondly,
                        _ => return Err(invalid(format!("bad FREQ {value}"))),
                    })
                }
                "INTERVAL" => rule.interval = list(&value, 1..=i64::from(u16::MAX), true)?[0],
                // recurring-ical-events drops negative counts (issue 128).
                "COUNT" => {
                    rule.count = if value.starts_with('-') {
                        None
                    } else {
                        Some(list(&value, 0..=u32::MAX, false)?[0])
                    }
                }
                "UNTIL" => rule.until = Some(When::parse(&value, None, false)?),
                "BYMONTH" => rule.by_month = list(&value, 1..=12, true)?,
                "BYWEEKNO" => rule.by_week_no = list(&value, -53..=53, true)?,
                "BYYEARDAY" => rule.by_year_day = list(&value, -366..=366, true)?,
                "BYMONTHDAY" => rule.by_month_day = list(&value, -31..=31, true)?,
                "BYHOUR" => rule.by_hour = list(&value, 0..=23, false)?,
                "BYMINUTE" => rule.by_minute = list(&value, 0..=59, false)?,
                "BYSECOND" => rule.by_second = list(&value, 0..=59, false)?,
                "BYSETPOS" => rule.by_set_pos = list(&value, -366..=366, true)?,
                "WKST" => rule.week_start = weekday(&value).ok_or_else(|| invalid(format!("bad WKST {value}")))?,
                "BYDAY" | "BYWEEKDAY" => {
                    for day in value.split(',') {
                        let day = day.trim();
                        let split = day.len().checked_sub(2).ok_or_else(|| invalid(format!("bad BYDAY {day}")))?;
                        let wd = weekday(&day[split..]).ok_or_else(|| invalid(format!("bad BYDAY {day}")))?;
                        match &day[..split] {
                            "" => rule.by_weekday.push(wd),
                            n => rule.by_nth_weekday.push((list(n, -53..=53, true)?[0], wd)),
                        }
                    }
                }
                other => return Err(invalid(format!("unknown rule part {other}"))),
            }
        }
        rule.freq = freq.ok_or_else(|| invalid("FREQ missing"))?;
        // dateutil: nth weekdays only mean something for MONTHLY/YEARLY.
        if rule.freq > Freq::Monthly {
            let plain: Vec<Weekday> = rule.by_nth_weekday.drain(..).map(|(_, wd)| wd).collect();
            rule.by_weekday.extend(plain);
        }
        let no_day_rules = rule.by_week_no.is_empty()
            && rule.by_year_day.is_empty()
            && rule.by_month_day.is_empty()
            && rule.by_weekday.is_empty()
            && rule.by_nth_weekday.is_empty();
        if no_day_rules {
            match rule.freq {
                Freq::Yearly => {
                    if rule.by_month.is_empty() {
                        rule.by_month = vec![start.month()];
                    }
                    rule.by_month_day = vec![start.day()];
                }
                Freq::Monthly => rule.by_month_day = vec![start.day()],
                Freq::Weekly => rule.by_weekday = vec![start.weekday()],
                _ => {}
            }
        }
        if rule.by_hour.is_empty() && rule.freq < Freq::Hourly {
            rule.by_hour = vec![start.hour()];
        }
        if rule.by_minute.is_empty() && rule.freq < Freq::Minutely {
            rule.by_minute = vec![start.minute()];
        }
        if rule.by_second.is_empty() && rule.freq < Freq::Secondly {
            rule.by_second = vec![start.second()];
        }
        for list in [&mut rule.by_hour, &mut rule.by_minute, &mut rule.by_second] {
            list.sort_unstable();
            list.dedup();
        }
        Ok(rule)
    }

    /// Wall-clock starts from `start`, skipping whole periods before `from` when COUNT allows,
    /// and stopping after `horizon`.
    fn iter(&self, start: DateTime, from: DateTime, horizon: DateTime) -> impl Iterator<Item = DateTime> + '_ {
        let first_period = if self.count.is_none() {
            self.periods_between(start, from).saturating_sub(1).max(0)
        } else {
            0
        };
        let mut period = first_period;
        let mut emitted = 0u32;
        let mut pending = std::collections::VecDeque::new();
        std::iter::from_fn(move || {
            loop {
                if self.count.is_some_and(|c| emitted >= c) {
                    return None;
                }
                if let Some(dt) = pending.pop_front() {
                    emitted += 1;
                    return Some(dt);
                }
                let (days, anchor) = self.period(start, period)?;
                if anchor > horizon {
                    return None;
                }
                period += 1;
                let mut set = self.candidates(&days, anchor);
                if !self.by_set_pos.is_empty() {
                    let n = set.len() as i32;
                    let mut picked: Vec<DateTime> = self
                        .by_set_pos
                        .iter()
                        .filter_map(|&p| {
                            let i = if p > 0 { p - 1 } else { n + p };
                            (0..n).contains(&i).then(|| set[i as usize])
                        })
                        .collect();
                    picked.sort();
                    picked.dedup();
                    set = picked;
                }
                pending.extend(set.into_iter().filter(|dt| *dt >= start));
            }
        })
    }

    /// Whole periods (of `interval` units) from `start` to `to`.
    fn periods_between(&self, start: DateTime, to: DateTime) -> i64 {
        if to <= start {
            return 0;
        }
        let secs = to.duration_since(start).as_secs();
        let units = match self.freq {
            Freq::Yearly => i64::from(to.year() - start.year()),
            Freq::Monthly => i64::from(to.year() - start.year()) * 12 + i64::from(to.month() - start.month()),
            Freq::Weekly => secs / 604_800,
            Freq::Daily => secs / 86_400,
            Freq::Hourly => secs / 3_600,
            Freq::Minutely => secs / 60,
            Freq::Secondly => secs,
        };
        units / self.interval
    }

    /// Days of period `k` and its first moment. None past year 9999.
    fn period(&self, start: DateTime, k: i64) -> Option<(Vec<Date>, DateTime)> {
        let step = k.checked_mul(self.interval)?;
        let midnight = |d: Date| d.to_datetime(civil::Time::midnight());
        let days_from = |first: Date, n: i64| -> Option<Vec<Date>> { (0..n).map(|i| first.checked_add(i.days()).ok()).collect() };
        match self.freq {
            Freq::Yearly => {
                let year = i64::from(start.year()) + step;
                let first = Date::new(i16::try_from(year).ok().filter(|y| *y <= 9999)?, 1, 1).ok()?;
                Some((days_from(first, i64::from(first.days_in_year()))?, midnight(first)))
            }
            Freq::Monthly => {
                let index = i64::from(start.year()) * 12 + i64::from(start.month() - 1) + step;
                let year = i16::try_from(index.div_euclid(12)).ok().filter(|y| *y <= 9999)?;
                let first = Date::new(year, (index.rem_euclid(12) + 1) as i8, 1).ok()?;
                Some((days_from(first, i64::from(first.days_in_month()))?, midnight(first)))
            }
            Freq::Weekly => {
                let back = (start.weekday().to_monday_zero_offset() - self.week_start.to_monday_zero_offset()).rem_euclid(7);
                let first = start
                    .date()
                    .checked_sub(i64::from(back).days())
                    .ok()?
                    .checked_add(step.checked_mul(7)?.days())
                    .ok()?;
                Some((days_from(first, 7)?, midnight(first)))
            }
            Freq::Daily => {
                let day = start.date().checked_add(step.days()).ok()?;
                Some((vec![day], midnight(day)))
            }
            Freq::Hourly | Freq::Minutely | Freq::Secondly => {
                let unit = match self.freq {
                    Freq::Hourly => 3_600,
                    Freq::Minutely => 60,
                    _ => 1,
                };
                let truncated = match self.freq {
                    Freq::Hourly => start.with().minute(0).second(0).subsec_nanosecond(0).build().ok()?,
                    Freq::Minutely => start.with().second(0).subsec_nanosecond(0).build().ok()?,
                    _ => start.with().subsec_nanosecond(0).build().ok()?,
                };
                let at = truncated.checked_add(SignedDuration::from_secs(step.checked_mul(unit)?)).ok()?;
                Some((vec![at.date()], at))
            }
        }
    }

    /// The period's date-times that pass every BY* filter, sorted.
    fn candidates(&self, days: &[Date], anchor: DateTime) -> Vec<DateTime> {
        let times: Vec<civil::Time> = match self.freq {
            Freq::Hourly | Freq::Minutely | Freq::Secondly => {
                let fixed_hour = anchor.hour();
                let fixed_minute = (self.freq >= Freq::Minutely).then(|| anchor.minute());
                let fixed_second = (self.freq == Freq::Secondly).then(|| anchor.second());
                let ok = |list: &[i8], v: i8| list.is_empty() || list.contains(&v);
                if !ok(&self.by_hour, fixed_hour)
                    || fixed_minute.is_some_and(|m| !ok(&self.by_minute, m))
                    || fixed_second.is_some_and(|s| !ok(&self.by_second, s))
                {
                    return vec![];
                }
                let minutes = fixed_minute.map_or(self.by_minute.clone(), |m| vec![m]);
                let seconds = fixed_second.map_or(self.by_second.clone(), |s| vec![s]);
                product(&[fixed_hour], &minutes, &seconds)
            }
            _ => product(&self.by_hour, &self.by_minute, &self.by_second),
        };
        days.iter()
            .filter(|d| self.day_matches(**d))
            .flat_map(|d| times.iter().map(move |t| d.to_datetime(*t)))
            .collect()
    }

    fn day_matches(&self, day: Date) -> bool {
        let month_day = day.day();
        let month_len = day.days_in_month();
        let year_day = day.day_of_year();
        let year_len = day.days_in_year();
        if !self.by_month.is_empty() && !self.by_month.contains(&day.month()) {
            return false;
        }
        if !self.by_month_day.is_empty() && !self.by_month_day.iter().any(|&m| m == month_day || m == month_day - month_len - 1) {
            return false;
        }
        if !self.by_year_day.is_empty() && !self.by_year_day.iter().any(|&y| y == year_day || y == year_day - year_len - 1) {
            return false;
        }
        if !self.by_week_no.is_empty() && self.freq == Freq::Yearly {
            let (number, total) = week_number(day, self.week_start);
            if !self.by_week_no.iter().any(|&w| i16::from(w) == number || i16::from(w) == number - total - 1) {
                return false;
            }
        }
        if self.by_weekday.is_empty() && self.by_nth_weekday.is_empty() {
            return true;
        }
        if self.by_weekday.contains(&day.weekday()) {
            return true;
        }
        // nth weekday within the month (MONTHLY, or YEARLY with BYMONTH) or within the year.
        let in_month = self.freq == Freq::Monthly || !self.by_month.is_empty();
        let (index, len) = if in_month {
            (i16::from(month_day), i16::from(month_len))
        } else {
            (year_day, year_len)
        };
        let (forward, backward) = ((index - 1) / 7 + 1, -((len - index) / 7 + 1));
        self.by_nth_weekday
            .iter()
            .any(|&(n, wd)| wd == day.weekday() && (i16::from(n) == forward || i16::from(n) == backward))
    }
}

fn product(hours: &[i8], minutes: &[i8], seconds: &[i8]) -> Vec<civil::Time> {
    let mut out = Vec::new();
    for &h in hours {
        for &m in minutes {
            for &s in seconds {
                out.push(civil::Time::new(h, m, s, 0).expect("validated ranges"));
            }
        }
    }
    out
}

/// Week number of `day` (week 1 holds Jan 4, weeks start on `week_start`) and that year's week count.
fn week_number(day: Date, week_start: Weekday) -> (i16, i16) {
    let week_one = |year: i16| -> Date {
        let jan4 = Date::new(year, 1, 4).expect("valid date");
        let back = (jan4.weekday().to_monday_zero_offset() - week_start.to_monday_zero_offset()).rem_euclid(7);
        jan4.checked_sub(i64::from(back).days()).expect("in range")
    };
    let mut year = day.year();
    if day >= week_one(year + 1) {
        year += 1;
    } else if day < week_one(year) {
        year -= 1;
    }
    let first = week_one(year);
    let weeks = |from: Date, to: Date| (to.since(from).expect("in range").get_days() / 7) as i16;
    (weeks(first, day) + 1, weeks(first, week_one(year + 1)))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    /// Google `recurrence` arrays against Python's `dateutil.rrulestr(forceset=True).xafter(...)`
    /// (`testdata/ics/recurrence.json`, generated from Python before its removal).
    #[test]
    fn matches_dateutil() {
        let cases: Vec<Value> = serde_json::from_str(include_str!("testdata/ics/recurrence.json")).unwrap();
        let berlin = TimeZone::get("Europe/Berlin").unwrap();
        for case in cases {
            let when = |key: &str| {
                let dt: DateTime = case[key].as_str().unwrap().parse().unwrap();
                if case["zoned"].as_bool().unwrap() {
                    When::Zoned(dt, berlin.clone())
                } else {
                    When::Floating(dt)
                }
            };
            let lines: Vec<String> = serde_json::from_value(case["lines"].clone()).unwrap();
            let actual = match occurrences(&lines, &when("dtstart"), &when("lower"), &when("upper"), 5000) {
                Ok(found) => json!(
                    found
                        .iter()
                        .map(|w| match w {
                            When::Zoned(dt, tz) => crate::time::iso(&dt.to_zoned(tz.clone()).unwrap()),
                            other => other.civil().strftime("%Y-%m-%dT%H:%M:%S").to_string(),
                        })
                        .collect::<Vec<_>>()
                ),
                Err(error) => json!({"error": error.code()}),
            };
            assert_eq!(actual, case["expected"], "{:?}", case["lines"]);
        }
    }
}
