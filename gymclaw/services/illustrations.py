"""Workout Guide assets (first-pose stills, three-pose animations) and catalog search.

Every planned exercise carries a `guide_id`, so every exercise has an image.
Attribution lives in the asset NOTICE and README, not in chat captions.
"""
from functools import lru_cache
import json
import math
from pathlib import Path
import re

ASSET_ROOT = Path(__file__).resolve().parents[1] / "assets" / "workout-guide"


@lru_cache(maxsize=1)
def catalog() -> dict[str, dict]:
    return {item["slug"]: item for item in json.loads((ASSET_ROOT / "manifest.json").read_text())}


def entry(item: dict) -> dict:
    slug = item["slug"]
    return {"guide_id": slug, "name": item["name"], "equipment": item["equipment"], "primary_muscle": item["primaryMuscle"],
        "svg_url": f"/illustrations/{slug}.svg", "telegram_png": str(ASSET_ROOT / slug / "frame-1.png"),
        # Looping rep (all three poses) for cards; Telegram plays a .gif as an animation.
        "telegram_animation": str(ASSET_ROOT / slug / "animation.gif")}


def for_exercise(name: str, guide_id: str | None = None) -> dict | None:
    """Explicit guide_id, else a unique exact name match. No fuzzy guessing at runtime."""
    items = catalog()
    if guide_id:
        item = items.get(guide_id)
    else:
        matches = [item for item in items.values() if item["name"].casefold() == name.strip().casefold()]
        item = matches[0] if len(matches) == 1 else None
    return entry(item) if item else None


def primary_muscle(guide_id: str | None) -> str | None:
    item = catalog().get(guide_id) if guide_id else None
    return item["primaryMuscle"] if item else None


# Gym shorthand the owner types ("DB RDL"), spelled out the way catalog names are.
ALIASES = {"db": "dumbbell", "bb": "barbell", "kb": "kettlebell", "rdl": "romanian deadlift", "sldl": "stiff leg deadlift",
    "ohp": "overhead press", "ext": "extension", "tri": "tricep", "bi": "bicep"}


def words(text: str) -> set[str]:
    spelled = " ".join(ALIASES.get(w, w) for w in re.findall(r"[a-z]+", text.casefold()))
    return {w.rstrip("s") for w in spelled.split()}


@lru_cache(maxsize=1)
def rarity() -> dict[str, float]:
    """Inverse document frequency of name words: 'incline' says more than 'machine'."""
    counts: dict[str, int] = {}
    for item in catalog().values():
        for word in words(item["name"]):
            counts[word] = counts.get(word, 0) + 1
    return {word: math.log(len(catalog()) / count) for word, count in counts.items()}


def search(query: str = "", *, muscle: str | None = None, equipment: str | None = None, limit: int = 8) -> list[dict]:
    """Rank catalog entries by rarity-weighted words shared with the query; filters are case-insensitive exact matches."""
    wanted = words(query)
    weight = rarity()
    results = []
    for item in catalog().values():
        if muscle and item["primaryMuscle"].casefold() != muscle.casefold():
            continue
        if equipment and item["equipment"].casefold() != equipment.casefold():
            continue
        name = words(item["name"])
        score = sum(weight.get(w, 1) for w in wanted & name) + 0.5 * len(wanted & words(item["equipment"] + " " + item["primaryMuscle"])) - 0.1 * len(name - wanted)
        if wanted and score <= 0:
            continue
        results.append((score, item["name"], entry(item)))
    return [result for _, _, result in sorted(results, key=lambda r: (-r[0], r[1]))[:limit]]


def artwork(slug: str) -> str:
    """Identity of an entry's picture. A few catalog entries are the same movement under two names
    with the same art (Leg Curl / Lying Leg Curl); offering one for the other isn't a swap."""
    item = catalog()[slug]
    return (item["frames"][0]["attribution"].get("source") or {}).get("url") or slug


def similar(guide_id: str, *, exclude: set[str] = frozenset(), prefer: set[str] = frozenset(), limit: int = 3) -> list[dict]:
    """Same primary muscle and exercise type, no stretches, never the same artwork as the original or an
    excluded entry. `prefer` slugs (e.g. owner history) rank first, then shared movement words (press,
    fly, incline), then different equipment (the original may be occupied)."""
    original = catalog().get(guide_id)
    if original is None:
        return []
    base = words(original["name"])
    taken = {artwork(slug) for slug in exclude | {guide_id} if slug in catalog()}
    options = []
    for item in catalog().values():
        if item["slug"] in exclude or artwork(item["slug"]) in taken or item["isStretch"]:
            continue
        if item["primaryMuscle"] != original["primaryMuscle"] or item["exerciseType"] != original["exerciseType"]:
            continue
        rank = (item["slug"] not in prefer, -len(base & words(item["name"])), item["equipment"] == original["equipment"], item["name"])
        options.append((rank, entry(item)))
    ranked = [option for _, option in sorted(options, key=lambda o: o[0])]
    # The catalog has near-duplicates (e.g. "Rear Delt Fly" vs "Bent-Over Rear Delt Raise"): pick
    # options on different equipment first, so the choices are real alternatives.
    picked = []
    for option in ranked:
        if len(picked) < limit and option["equipment"] not in {p["equipment"] for p in picked}:
            picked.append(option)
    picked += [option for option in ranked if option not in picked][:limit - len(picked)]
    return picked


def public_assets() -> dict[str, Path]:
    return {f"/illustrations/{slug}.svg": ASSET_ROOT / slug / "frame-1.svg" for slug in catalog()}
