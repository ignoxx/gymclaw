//! Read-only ICS feed (iCloud "Public Calendar" link, Google secret iCal address). Port of
//! `providers/ics_feed.py`, which used `icalendar` + `recurring-ical-events`.
//!
//! GET only, nothing is ever written back. Yields busy intervals; titles/notes are discarded.
use std::collections::{BTreeSet, HashMap};

use jiff::SignedDuration;
use jiff::tz::TimeZone;

use super::http::{Method, Request, Transport};
use super::recurrence::{
    ContentLine, Instance, RDate, Series, When, compare, difference, parse_duration, rdate_values,
};
use crate::bail;
use crate::domain::planning::Interval;
use crate::error::{Error, Result};
use crate::time::Ts;

pub const MAX_BYTES: usize = 5_000_000;

/// Apple shares feeds as webcal://; fetch them over https.
pub fn normalize_url(url: &str) -> Result<String> {
    let url = url.trim();
    let url = match url.strip_prefix("webcal://") {
        Some(rest) => format!("https://{rest}"),
        None => url.to_string(),
    };
    let host = url
        .strip_prefix("https://")
        .map(|rest| rest.split(['/', '?', '#']).next().unwrap_or(""));
    // Python `urlsplit(...).hostname`: strip userinfo and port.
    let host = host
        .map(|h| h.rsplit('@').next().unwrap_or(""))
        .map(|h| h.split(':').next().unwrap_or(""));
    if !url
        .get(..8)
        .is_some_and(|s| s.eq_ignore_ascii_case("https://"))
        || host.is_none_or(str::is_empty)
    {
        bail!(
            "PERSONAL_CALENDAR_URL_INVALID",
            "Personal calendar feed must be an https:// or webcal:// ICS link"
        );
    }
    Ok(url)
}

/// GET the feed body. The URL is a capability secret and never appears in errors.
pub fn fetch(url: &str, transport: &dyn Transport) -> Result<Vec<u8>> {
    let request = Request::new(Method::Get, url, 30).header("Accept", "text/calendar");
    let response = transport.send(&request).map_err(|_| {
        Error::domain(
            "PERSONAL_CALENDAR_UNAVAILABLE",
            "Personal calendar feed request failed",
        )
    })?;
    if response.status >= 400 {
        bail!(
            "PERSONAL_CALENDAR_UNAVAILABLE",
            "Personal calendar feed failed (HTTP {})",
            response.status
        );
    }
    if response.body.len() > MAX_BYTES {
        bail!(
            "PERSONAL_CALENDAR_TOO_LARGE",
            "Personal calendar feed exceeds size limit"
        );
    }
    Ok(response.body)
}

/// Busy intervals overlapping [start, end), sorted and deduplicated. Recurrences, EXDATE and
/// RECURRENCE-ID overrides are expanded; free (TRANSP:TRANSPARENT) and cancelled events don't block.
/// Floating times and all-day dates are read in `zone` (the owner's).
pub fn busy_intervals(data: &[u8], start: Ts, end: Ts, zone: &TimeZone) -> Result<Vec<Interval>> {
    expand(data, start, end, zone).map_err(|_| {
        Error::domain(
            "PERSONAL_CALENDAR_INVALID",
            "Personal calendar feed could not be parsed",
        )
    })
}

/// One VEVENT, reduced to what busy time needs.
struct Event {
    uid: Option<String>,
    start: When,
    end: When,
    rrules: Vec<String>,
    rdates: Vec<RDate>,
    exdates: Vec<When>,
    recurrence_id: Option<When>,
    this_and_future: bool,
    sequence: i64,
    blocks: bool,
}

impl Event {
    fn duration(&self) -> Result<SignedDuration> {
        difference(&self.end, &self.start)
    }

    fn has_rules(&self) -> bool {
        !self.rrules.is_empty() || !self.rdates.is_empty() || !self.exdates.is_empty()
    }

    fn ids(&self) -> Vec<jiff::civil::DateTime> {
        self.recurrence_id
            .as_ref()
            .map(|r| {
                Instance {
                    start: r.clone(),
                    length: None,
                }
                .ids()
            })
            .unwrap_or_default()
    }

    /// How far THISANDFUTURE overrides shift later instances.
    fn moved_by(&self) -> Result<SignedDuration> {
        match (&self.recurrence_id, self.this_and_future) {
            (Some(rid), true) => difference(&self.start, rid),
            _ => Ok(SignedDuration::ZERO),
        }
    }
}

fn unfold(data: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(data);
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        match raw.strip_prefix([' ', '\t']) {
            Some(rest) if !lines.is_empty() => lines.last_mut().expect("nonempty").push_str(rest),
            _ if raw.is_empty() => {}
            _ => lines.push(raw.to_string()),
        }
    }
    lines
}

/// Top-level VEVENTs of the first VCALENDAR (nested VALARMs etc. are skipped).
fn events(data: &[u8]) -> Result<Vec<Event>> {
    let lines = unfold(data);
    let mut depth = 0usize;
    let mut current: Option<Vec<ContentLine<'_>>> = None;
    let mut out = Vec::new();
    for line in &lines {
        let parsed = ContentLine::parse(line).ok_or_else(|| Error::domain("ICS", "bad line"))?;
        match parsed.name.as_str() {
            "BEGIN" => {
                depth += 1;
                if depth == 2 && parsed.value.trim().eq_ignore_ascii_case("VEVENT") {
                    current = Some(Vec::new());
                }
            }
            "END" => {
                if depth == 2
                    && let Some(props) = current.take()
                {
                    out.push(event(&props)?);
                }
                depth = depth.saturating_sub(1);
            }
            _ if depth == 2 => {
                if let Some(props) = current.as_mut() {
                    props.push(parsed);
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn event(props: &[ContentLine<'_>]) -> Result<Event> {
    let get = |name: &str| props.iter().find(|p| p.name == name);
    let text = |name: &str| {
        get(name)
            .map(|p| p.value.trim().to_ascii_uppercase())
            .unwrap_or_default()
    };
    let one = |p: &ContentLine<'_>| -> Result<When> {
        p.times()?
            .into_iter()
            .next()
            .ok_or_else(|| Error::domain("ICS", "empty time"))
    };
    let start = one(get("DTSTART").ok_or_else(|| Error::domain("ICS", "DTSTART missing"))?)?;
    let raw_end = match (get("DTEND"), get("DURATION")) {
        (Some(end), _) => one(end)?,
        (None, Some(duration)) => {
            let duration = parse_duration(duration.value)?;
            match &start {
                // Python: a date plus a duration with a time part becomes a floating date-time.
                When::Date(d) if duration.as_secs().rem_euclid(86_400) != 0 => When::Floating(
                    d.to_datetime(jiff::civil::Time::midnight())
                        .checked_add(duration)?,
                ),
                When::Date(d) => When::Date(
                    d.checked_add(jiff::Span::new().days(duration.as_secs().div_euclid(86_400)))?,
                ),
                When::Floating(dt) => When::Floating(dt.checked_add(duration)?),
                When::Zoned(dt, tz) => When::Zoned(dt.checked_add(duration)?, tz.clone()),
            }
        }
        (None, None) => match &start {
            When::Date(d) => When::Date(d.tomorrow()?),
            other => other.clone(),
        },
    };
    // recurring-ical-events orders a reversed span.
    let (start, end) = if compare(&start, &raw_end)?.is_gt() {
        (raw_end, start)
    } else {
        (start, raw_end)
    };
    let mut rdates = Vec::new();
    let mut exdates = Vec::new();
    for p in props {
        match p.name.as_str() {
            "RDATE" => rdates.extend(rdate_values(p)?),
            "EXDATE" => exdates.extend(p.times()?),
            _ => {}
        }
    }
    let recurrence = get("RECURRENCE-ID");
    Ok(Event {
        uid: get("UID").map(|p| p.value.to_string()),
        start,
        end,
        rrules: props
            .iter()
            .filter(|p| p.name == "RRULE")
            .map(|p| p.value.to_string())
            .collect(),
        rdates,
        exdates,
        recurrence_id: recurrence.map(one).transpose()?,
        this_and_future: recurrence
            .and_then(|r| r.param("RANGE"))
            .is_some_and(|r| r == "THISANDFUTURE"),
        sequence: get("SEQUENCE")
            .and_then(|p| p.value.trim().parse().ok())
            .unwrap_or(-1),
        blocks: text("TRANSP") != "TRANSPARENT" && text("STATUS") != "CANCELLED",
    })
}

/// `max(existing, new)` by SEQUENCE; ties keep the first, like Python's `max`.
fn highest<'a>(existing: Option<&'a Event>, new: &'a Event) -> &'a Event {
    match existing {
        Some(e) if e.sequence >= new.sequence => e,
        _ => new,
    }
}

struct Occurrence {
    start: When,
    end: When,
    blocks: bool,
}

fn expand(data: &[u8], window_start: Ts, window_end: Ts, zone: &TimeZone) -> Result<Vec<Interval>> {
    let events = events(data)?;
    // Series by UID, in feed order; events without UID stand alone.
    let mut order: Vec<Vec<&Event>> = Vec::new();
    let mut by_uid: HashMap<&str, usize> = HashMap::new();
    for event in &events {
        match event.uid.as_deref().and_then(|uid| by_uid.get(uid)) {
            Some(&i) => order[i].push(event),
            None => {
                if let Some(uid) = event.uid.as_deref() {
                    by_uid.insert(uid, order.len());
                }
                order.push(vec![event]);
            }
        }
    }
    let lower = When::Zoned(zone.to_datetime(window_start.0), zone.clone());
    let upper = When::Zoned(zone.to_datetime(window_end.0), zone.clone());
    let mut busy = BTreeSet::new();
    for group in order {
        for occurrence in series_occurrences(&group, &lower, &upper)? {
            if !occurrence.blocks {
                continue;
            }
            let (begin, finish) = (occurrence.start.to_ts(zone)?, occurrence.end.to_ts(zone)?);
            if finish > begin && begin < window_end && finish > window_start {
                busy.insert((begin, finish));
            }
        }
    }
    Ok(busy
        .into_iter()
        .map(|(start, end)| Interval { start, end })
        .collect())
}

/// Occurrences of one UID's master + overrides that may touch [lower, upper) (a superset; the caller filters).
fn series_occurrences(group: &[&Event], lower: &When, upper: &When) -> Result<Vec<Occurrence>> {
    let mut core: Option<&Event> = None;
    let mut modifications: HashMap<jiff::civil::DateTime, &Event> = HashMap::new();
    for event in group {
        let ids = event.ids();
        if ids.is_empty() {
            core = Some(highest(core, event));
        }
        for id in ids {
            let existing = modifications.get(&id).copied();
            modifications.insert(id, highest(existing, event));
        }
    }
    let mut unique: Vec<&Event> = Vec::new();
    for m in modifications.values() {
        if !unique.iter().any(|u| std::ptr::eq(*u, *m)) {
            unique.push(m);
        }
    }
    let mut this_and_future: Vec<(jiff::civil::DateTime, &Event)> = unique
        .iter()
        .filter(|m| m.this_and_future)
        .map(|m| (m.ids()[0], *m))
        .collect();
    this_and_future.sort_by_key(|(id, _)| *id);
    let modification_for = |ids: &[jiff::civil::DateTime]| {
        ids.iter()
            .rev()
            .find_map(|id| modifications.get(id).copied())
    };
    let occurrence = |e: &Event| Occurrence {
        start: e.start.clone(),
        end: e.end.clone(),
        blocks: e.blocks,
    };

    let mut out = Vec::new();
    let mut returned: Vec<&Event> = Vec::new();
    let Some(core) = core else {
        return Ok(unique.iter().map(|m| occurrence(m)).collect());
    };
    let series = Series::new(
        &core.start,
        Some(&core.end),
        &core.rrules.iter().map(String::as_str).collect::<Vec<_>>(),
        core.rdates.clone(),
        &core.exdates,
        true,
    )?;
    // Generous query span: long events and moved overrides must still be found.
    let mut reach = SignedDuration::from_hours(24);
    for e in std::iter::once(core).chain(unique.iter().copied()) {
        reach += e.duration()?.abs();
        if let Some(rid) = &e.recurrence_id {
            reach += difference(&e.start, rid)?.abs();
        }
    }
    let shift = |w: &When, d: SignedDuration| -> Result<When> {
        Ok(When::Zoned(
            TimeZone::UTC.to_datetime(w.to_ts(&TimeZone::UTC)?.0.checked_add(d)?),
            TimeZone::UTC,
        ))
    };
    for instance in series.between(&shift(lower, -reach)?, &shift(upper, reach)?, usize::MAX)? {
        let ids = instance.ids();
        if let Some(m) = modification_for(&ids) {
            if !returned.iter().any(|r| std::ptr::eq(*r, m)) {
                returned.push(m);
                out.push(occurrence(m));
            }
            continue;
        }
        let component = this_and_future
            .iter()
            .take_while(|(id, _)| *id < ids[0])
            .last()
            .map_or(core, |(_, m)| *m);
        let moved = component.moved_by()?;
        let start = Series::end_of(
            &Instance {
                start: instance.start.clone(),
                length: None,
            },
            moved,
        )?;
        let end = Series::end_of(
            &Instance {
                start: start.clone(),
                length: instance.length,
            },
            component.duration()?,
        )?;
        out.push(Occurrence {
            start,
            end,
            blocks: component.blocks,
        });
    }
    for m in unique {
        if returned.iter().any(|r| std::ptr::eq(*r, m))
            || series.excludes(&m.ids())
            || skip_stale_modification(m, core, &series)?
        {
            continue;
        }
        out.push(occurrence(m));
    }
    Ok(out)
}

/// recurring-ical-events issue 148/164/253: an older override that carries its own rules only
/// counts when the master still produces its RECURRENCE-ID.
fn skip_stale_modification(m: &Event, core: &Event, series: &Series) -> Result<bool> {
    if !m.has_rules() || m.sequence >= core.sequence {
        return Ok(false);
    }
    let Some(rid) = &m.recurrence_id else {
        return Ok(false);
    };
    let ids = m.ids();
    let day = |w: &When, days: i64| -> Result<When> {
        let midnight = w
            .civil()
            .date()
            .checked_add(jiff::Span::new().days(days))?
            .to_datetime(jiff::civil::Time::midnight());
        Ok(match w {
            When::Zoned(_, tz) => When::Zoned(midnight, tz.clone()),
            _ => When::Floating(midnight),
        })
    };
    let found = series.between(&day(rid, 0)?, &day(rid, 1)?, usize::MAX)?;
    Ok(!found
        .iter()
        .any(|i| i.ids().iter().any(|id| ids.contains(id))))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::time::zone;

    /// Feeds covering RRULE (UNTIL/COUNT/BYDAY/BYSETPOS/…), EXDATE, RDATE, RECURRENCE-ID overrides,
    /// all-day, floating, TZID with DST, cancelled and free events. `expected.json` is the Python
    /// implementation's output (`testdata/ics/expected.py`).
    #[test]
    fn matches_python_expansion() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("testdata/ics/expected.json")).unwrap();
        let mut failures = Vec::new();
        for case in &cases {
            let feed = std::fs::read(
                concat!(env!("CARGO_MANIFEST_DIR"), "/src/providers/testdata/ics/").to_string()
                    + case["feed"].as_str().unwrap(),
            )
            .unwrap();
            let start = Ts::parse(case["start"].as_str().unwrap(), None).unwrap();
            let end = Ts::parse(case["end"].as_str().unwrap(), None).unwrap();
            let actual = match busy_intervals(
                &feed,
                start,
                end,
                &zone(case["zone"].as_str().unwrap()).unwrap(),
            ) {
                Ok(busy) => json!(
                    busy.iter()
                        .map(|i| [i.start.iso(), i.end.iso()])
                        .collect::<Vec<_>>()
                ),
                Err(error) => json!({"error": error.code()}),
            };
            if actual != case["expected"] {
                failures.push(format!(
                    "{} {} {}..{}\n  python: {}\n  rust:   {}",
                    case["feed"],
                    case["zone"],
                    case["start"],
                    case["end"],
                    case["expected"],
                    actual
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn webcal_link_normalized_and_plain_http_rejected() {
        assert_eq!(
            normalize_url("webcal://p42-caldav.icloud.com/published/2/abc").unwrap(),
            "https://p42-caldav.icloud.com/published/2/abc"
        );
        assert_eq!(
            normalize_url("http://example.com/cal.ics")
                .unwrap_err()
                .code(),
            "PERSONAL_CALENDAR_URL_INVALID"
        );
    }
}
