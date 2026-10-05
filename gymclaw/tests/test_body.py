from datetime import datetime, timedelta, timezone
import json

from PIL import Image
import pytest
from sqlalchemy.orm import Session

from gymclaw.cli import main
from gymclaw.db import initialize, make_engine
from gymclaw.services import body
from gymclaw.services.errors import DomainError

NOW = datetime(2026, 10, 11, 17, tzinfo=timezone.utc)


@pytest.fixture
def engine(tmp_path):
    engine = make_engine(f"sqlite:///{tmp_path / 'body.db'}")
    initialize(engine)
    yield engine
    engine.dispose()


def scale_photo(path, stamp=None, offset=None):
    image = Image.new("RGB", (8, 8))
    exif = Image.Exif()
    if stamp:
        exif.get_ifd(0x8769)[0x9003] = stamp
    if offset:
        exif.get_ifd(0x8769)[0x9011] = offset
    image.save(path, exif=exif)
    return path


def run(engine, capsys, *args):
    code = main(["--db-url", str(engine.url), "body", *args, "--now", NOW.isoformat()])
    return code, json.loads(capsys.readouterr().out)


def test_backfill_uses_photo_dates_and_reimport_is_a_noop(engine, capsys, tmp_path):
    jpeg = scale_photo(tmp_path / "a.jpg", "2026:09:28 07:05:00", "+02:00")
    # HEIC from an iPhone: Pillow can't open it, the EXIF stamp is still in the header.
    heic = tmp_path / "b.heic"
    heic.write_bytes(b"\x00\x00\x00\x18ftypheic....Exif\x00\x002026:10:05 07:40:12\x00")
    entries = json.dumps([{"kg": 83.0, "photo": str(jpeg)}, {"kg": 82.4, "photo": str(heic)}, {"kg": 83.4, "at": "2026-09-27"}])
    code, out = run(engine, capsys, "log", "--entries", entries, "--request-id", "tg-1")
    assert code == 0, out
    assert [e["measured_at"] for e in out["data"]["logged"]] == ["2026-09-28T05:05:00+00:00", "2026-10-05T05:40:00+00:00", "2026-09-27T06:00:00+00:00"]
    # 7 days up to the Oct 5 weigh-in: 82.4. The 7 days before: (83.0 + 83.4) / 2.
    assert out["user_message_hint"] == "3 weigh-ins logged · -0.8 kg vs the week before"
    code, again = run(engine, capsys, "log", "--entries", entries, "--request-id", "tg-2")
    assert again["data"]["logged"] == [] and len(again["data"]["duplicates"]) == 3
    code, listed = run(engine, capsys, "list")
    assert [e["kg"] for e in listed["data"]["entries"]] == [82.4, 83.0, 83.4]


def test_compressed_photo_without_date_is_refused(engine, tmp_path):
    with Session(engine) as db, db.begin(), pytest.raises(DomainError, match="send|file"):
        body.log(db, [body.WeighIn(kg=80, photo=scale_photo(tmp_path / "c.jpg"))], now=NOW, request_id="r")


def test_briefing_line_shows_trend_then_nudges_when_stale(engine):
    with Session(engine) as db, db.begin():
        body.log(db, [body.WeighIn(kg=83.1, at=(NOW - timedelta(days=8)).isoformat()), body.WeighIn(kg=82.5)], now=NOW, request_id="r")
        assert body.briefing_line(db, now=NOW) == "⚖️ 82.5 kg · -0.6 kg vs the week before"
        assert body.briefing_line(db, now=NOW + timedelta(days=9)) == "⚖️ Last weigh-in 9 days ago. Send a scale photo when you get a chance."
