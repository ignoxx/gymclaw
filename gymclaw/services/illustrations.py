"""Workout Guide first-pose assets. Explicit catalog IDs or unique exact names only."""
from functools import lru_cache
import json
from pathlib import Path

ASSET_ROOT = Path(__file__).resolve().parents[1] / "assets" / "workout-guide"


@lru_cache(maxsize=1)
def catalog() -> dict[str, dict]:
    return {item["slug"]: item for item in json.loads((ASSET_ROOT / "manifest.json").read_text())}


def for_exercise(name: str, guide_id: str | None = None) -> dict | None:
    items = catalog()
    if guide_id:
        item = items.get(guide_id)
    else:
        matches = [item for item in items.values() if item["name"].casefold() == name.strip().casefold()]
        item = matches[0] if len(matches) == 1 else None
    if item is None:
        return None
    slug = item["slug"]
    return {"guide_id": slug, "name": item["name"], "equipment": item["equipment"],
        "svg_url": f"/illustrations/{slug}.svg", "telegram_png": str(ASSET_ROOT / slug / "frame-1.png"),
        "credit": "Workout Guide, Bryl Lim / Everkinetic", "license": "CC BY-SA 4.0",
        "changes": "GymClaw added a dark PNG background for Telegram. SVG files are unmodified.",
        "license_url": "https://creativecommons.org/licenses/by-sa/4.0/",
        "source_url": "https://github.com/bryllim/workout-guide", "attribution": item["frames"][0]["attribution"]}


def public_assets() -> dict[str, Path]:
    return {f"/illustrations/{slug}.svg": ASSET_ROOT / slug / "frame-1.svg" for slug in catalog()}
