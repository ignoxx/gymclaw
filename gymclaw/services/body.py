"""Body weight: weigh-ins read from scale photos (or typed), a weekly trend and a quiet nudge.

The agent reads the number with its own vision. For old photos (backfill) it passes the file, and
the capture date comes from the photo's EXIF data. Telegram strips EXIF from compressed photos, so
backfilled pictures must be sent as files.
"""
from datetime import date, datetime, time, timedelta
from pathlib import Path
import re
from zoneinfo import ZoneInfo

from PIL import Image
from pydantic import BaseModel, Field
from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import BodyWeight
from gymclaw.services.errors import DomainError
from gymclaw.services.profile import get_profile
from gymclaw.services.workout import mutate, utc

# Backfilled entries with only a date get a typical morning weigh-in time.
DATE_ONLY_TIME = time(8)
# No weigh-in for this long: the Sunday briefing asks for one. Never a separate message.
NUDGE_AFTER = timedelta(days=7)
EXIF_STAMP = re.compile(rb"(\d{4}):(\d{2}):(\d{2}) (\d{2}):(\d{2}):(\d{2})")


class WeighIn(BaseModel):
    kg: float = Field(ge=20, le=400)
    # Exact time with offset ("2026-03-02T07:10:00+01:00") or a date ("2026-03-02"). Neither and no photo: now.
    at: str | None = None
    # Local path of a scale photo sent as a file; its capture date becomes the time.
    photo: Path | None = None


def photo_taken_at(path: Path, zone: ZoneInfo) -> datetime:
    """Capture time from EXIF. HEIC and other formats Pillow can't open: the first EXIF-style stamp in the header."""
    stamp, offset = None, None
    try:
        with Image.open(path) as image:
            exif = image.getexif()
            details = exif.get_ifd(0x8769)
            stamp, offset = details.get(0x9003) or exif.get(0x0132), details.get(0x9011)
    except OSError:
        with open(path, "rb") as file:
            match = EXIF_STAMP.search(file.read(1 << 18))
        stamp = match.group(0).decode() if match else None
    if not stamp:
        raise DomainError("PHOTO_DATE_UNKNOWN", "Photo has no capture date (Telegram strips it from compressed photos). "
            "Ask for the original sent as a file, or pass the date with --at.")
    taken = datetime.strptime(str(stamp).strip("\x00 "), "%Y:%m:%d %H:%M:%S")
    try:
        return taken.replace(tzinfo=datetime.strptime(str(offset).strip("\x00 "), "%z").tzinfo) if offset else taken.replace(tzinfo=zone)
    except ValueError:
        return taken.replace(tzinfo=zone)


def measured_at(entry: WeighIn, *, now: datetime, zone: ZoneInfo) -> datetime:
    if entry.photo and entry.at:
        raise ValueError("Give a photo or a time, not both")
    if entry.photo:
        at = photo_taken_at(entry.photo, zone)
    elif entry.at and len(entry.at) == 10:
        at = datetime.combine(date.fromisoformat(entry.at), DATE_ONLY_TIME, zone)
    elif entry.at:
        at = datetime.fromisoformat(entry.at)
        if at.tzinfo is None or at.utcoffset() is None:
            raise ValueError("--at needs a UTC offset, or pass just the date")
    else:
        at = now
    at = utc(at)
    if at > now + timedelta(minutes=5) or at.year < 2000:
        raise DomainError("WEIGH_IN_DATE_INVALID", f"Weigh-in date {at.astimezone(zone):%Y-%m-%d} is in the future or implausible")
    return at.replace(second=0, microsecond=0)


def log(db: Session, entries: list[WeighIn], *, now: datetime, request_id: str) -> dict:
    """Save one or more weigh-ins (a whole backfill in one call). Same time as a saved entry: skipped as a duplicate."""
    now = utc(now)
    if not entries:
        raise ValueError("No weigh-ins given")

    def action():
        zone = ZoneInfo(get_profile(db).timezone)
        logged, duplicates = [], []
        for entry in entries:
            at = measured_at(entry, now=now, zone=zone)
            if db.scalar(select(BodyWeight.id).where(BodyWeight.measured_at == at)):
                duplicates.append({"kg": entry.kg, "measured_at": at.isoformat()})
                continue
            row = BodyWeight(measured_at=at, kg=round(entry.kg, 1), created_at=now)
            db.add(row)
            db.flush()
            logged.append(entry_data(row, zone))
        summary = trend(db, now=now)
        # Echo the number back so a misread scale gets caught right away.
        hint = (f"{logged[0]['kg']:g} kg logged" if len(logged) == 1 else f"{len(logged)} weigh-ins logged") if logged else "Already logged."
        if summary["change_kg"] is not None and logged:
            hint += f" · {summary['change_kg']:+g} kg vs the week before"
        return {"logged": logged, "duplicates": duplicates, "trend": summary, "instruction": hint}

    return mutate(db, "body.weight_logged", request_id, {"entries": [e.model_dump(mode="json") for e in entries]}, now, action)


def delete(db: Session, entry_id: str, *, now: datetime, request_id: str) -> dict:
    """Remove a misread or mistyped weigh-in."""
    def action():
        row = db.get(BodyWeight, entry_id)
        if row is None:
            raise DomainError("WEIGH_IN_NOT_FOUND", "Unknown weigh-in")
        data = entry_data(row, ZoneInfo(get_profile(db).timezone))
        db.delete(row)
        db.flush()
        return {"deleted": data}

    return mutate(db, "body.weight_deleted", request_id, {"entry_id": entry_id}, now, action)


def entry_data(row: BodyWeight, zone: ZoneInfo) -> dict:
    return {"id": row.id, "kg": row.kg, "measured_at": row.measured_at.isoformat(), "local_date": row.measured_at.astimezone(zone).date().isoformat()}


def entries(db: Session) -> list[BodyWeight]:
    return list(db.scalars(select(BodyWeight).order_by(BodyWeight.measured_at)))


def trend(db: Session, *, now: datetime) -> dict:
    """Latest weigh-in, and the 7-day average versus the 7 days before it. Daily weight swings by
    1–2 kg, so weeks are compared, not single readings. With weekly logging it's simply last vs previous."""
    rows = [r for r in entries(db) if r.measured_at <= utc(now)]
    if not rows:
        return {"latest": None, "change_kg": None, "days_since": None}
    latest = rows[-1]
    week = [r.kg for r in rows if latest.measured_at - timedelta(days=7) < r.measured_at]
    before = [r.kg for r in rows if latest.measured_at - timedelta(days=14) < r.measured_at <= latest.measured_at - timedelta(days=7)]
    change = round(sum(week) / len(week) - sum(before) / len(before), 1) if before else None
    zone = ZoneInfo(get_profile(db).timezone)
    return {"latest": entry_data(latest, zone), "change_kg": change, "days_since": (utc(now) - latest.measured_at).days}


def briefing_line(db: Session, *, now: datetime) -> str | None:
    """One line for the Sunday briefing: the trend when weighed recently, else a gentle ask."""
    summary = trend(db, now=now)
    latest = summary["latest"]
    if latest is None:
        return "⚖️ Snap a photo of your scale anytime and I'll track your weight."
    if summary["days_since"] >= NUDGE_AFTER.days:
        return f"⚖️ Last weigh-in {summary['days_since']} days ago. Send a scale photo when you get a chance."
    change = f" · {summary['change_kg']:+g} kg vs the week before" if summary["change_kg"] is not None else ""
    return f"⚖️ {latest['kg']:g} kg{change}"
