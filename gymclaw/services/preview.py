"""Session preview: one picture with every exercise's illustration, plus a short caption.

Used when the owner asks "what's on today?". One photo instead of a stack of messages; rendered
once per plan content and cached under data/previews/.
"""
from datetime import datetime
from hashlib import sha256
import json
from pathlib import Path
from zoneinfo import ZoneInfo

from sqlalchemy import select
from sqlalchemy.orm import Session

from gymclaw.models import PlannedSession, SetLog, WorkoutExercise
from gymclaw.services.errors import DomainError
from gymclaw.services.illustrations import for_exercise
from gymclaw.services.profile import get_profile
from gymclaw.services.templates import get_template
from gymclaw.services.workout import utc

PREVIEW_ROOT = Path(__file__).resolve().parents[2] / "data" / "previews"
FONT = Path(__file__).resolve().parents[1] / "web" / "fonts" / "DM-Sans.ttf"
TILE, LABEL, GAP, COLUMNS = 340, 70, 24, 3


def render_grid(items: list[tuple[str, str]]) -> str:
    """items: (label, png path). Returns a cached PNG path; same content renders once."""
    from PIL import Image, ImageDraw, ImageFont
    key = sha256(json.dumps(items).encode()).hexdigest()[:16]
    path = PREVIEW_ROOT / key / "preview.png"
    if path.exists():
        return str(path)
    rows = -(-len(items) // COLUMNS)
    width = COLUMNS * TILE + (COLUMNS + 1) * GAP
    height = rows * (TILE + LABEL) + (rows + 1) * GAP
    canvas = Image.new("RGB", (width, height), "black")
    draw = ImageDraw.Draw(canvas)
    font = ImageFont.truetype(str(FONT), 30)
    for index, (label, png) in enumerate(items):
        x = GAP + (index % COLUMNS) * (TILE + GAP)
        y = GAP + (index // COLUMNS) * (TILE + LABEL + GAP)
        art = Image.open(png).convert("RGB")
        margin = art.width // 12  # Telegram padding isn't needed inside the grid
        canvas.paste(art.crop((margin, margin, art.width - margin, art.height - margin)).resize((TILE, TILE)), (x, y))
        text = f"{index + 1}. {label}"
        while draw.textlength(text, font=font) > TILE and len(text) > 4:
            text = text[:-2] + "…"
        draw.text((x + TILE / 2, y + TILE + 18), text, font=font, fill="white", anchor="ma")
    path.parent.mkdir(parents=True, exist_ok=True)
    canvas.save(path, optimize=True)
    return str(path)


def last_set(db: Session, exercise_id: str) -> SetLog | None:
    return db.scalar(select(SetLog).join(WorkoutExercise).where(WorkoutExercise.exercise_id == exercise_id, SetLog.set_type == "WORKING").order_by(SetLog.logged_at.desc()).limit(1))


def plan_lines(db: Session, session: PlannedSession) -> list[str]:
    """'1. Barbell row · 2 sets of 8–12 · last 10 × 60 kg' per exercise, from the current template."""
    template = get_template(db, session.workout_template_id)
    sets = session.workout_plan_json.get("sets", {})
    lines = []
    for index, spec in enumerate(template.exercises, 1):
        reps = f"{spec.rep_min}" if spec.rep_min == spec.rep_max else f"{spec.rep_min}–{spec.rep_max}"
        last = last_set(db, spec.id)
        lines.append(f"{index}. {spec.name} · {sets.get(spec.id, spec.working_sets)} sets of {reps}" + (f" · last {last.reps} × {last.weight:g} kg" if last else ""))
    return lines


def session_preview(db: Session, *, now: datetime, planned_session_id: str | None = None) -> dict:
    """Card for a planned session (default: the next one), using the current template."""
    now = utc(now)
    query = select(PlannedSession).where(PlannedSession.status.in_(["TENTATIVE", "COMMITTED"]), PlannedSession.planned_end_at > now)
    session = db.get(PlannedSession, planned_session_id) if planned_session_id else db.scalar(query.order_by(PlannedSession.planned_start_at).limit(1))
    if session is None or not session.workout_template_id:
        raise DomainError("NO_PLANNED_SESSION", "No upcoming session with a workout plan")
    template = get_template(db, session.workout_template_id)
    zone = ZoneInfo(get_profile(db).timezone)
    start = session.planned_start_at.astimezone(zone)
    crowd = (session.workout_plan_json.get("crowd_forecast") or {}).get("feel")
    lines = [f"**{template.name}** · {start:%a %H:%M}" + (f" · {crowd}" if crowd else "")] + plan_lines(db, session)
    items = [(spec.name, picture["telegram_png"]) for spec in template.exercises if (picture := for_exercise(spec.name, spec.guide_id))]
    photo = render_grid(items) if items else None
    return {"text": "\n".join(lines), "photo": photo, "buttons": [], "rest_until": None, "kind": "preview"}
