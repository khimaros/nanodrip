"""tags on moments in time, kept on the device."""

import http.client
import json
from urllib.parse import urlencode

import pytest
from harness import CODE, NOW, Run
from sensor import Sensor

SECTOR_SIZE = 4096
HEADER_SIZE = 16
RECORD_SIZE = 40
RECORDS_PER_SECTOR = (SECTOR_SIZE - HEADER_SIZE) // RECORD_SIZE
MAX_NAME = 32
TIME = NOW - 3600


def start(tmp_path, *args):
    return Run(Sensor(CODE), tmp_path, args=["--http", "127.0.0.1:0",
                                             "--history-dir", str(tmp_path / "history"), *args])


def post(run, path, guarded=True, **fields):
    conn = http.client.HTTPConnection("127.0.0.1", run.http_port, timeout=5)
    conn.request("POST", path, urlencode(fields), {"x-nanodrip": "1"} if guarded else {})
    response = conn.getresponse()
    response.read()
    return response.status


def add(run, time, name):
    return post(run, "/api/tag", time=time, name=name)


def get(run, path):
    conn = http.client.HTTPConnection("127.0.0.1", run.http_port, timeout=5)
    conn.request("GET", path)
    return json.loads(conn.getresponse().read())


def listed(run):
    return [(tag["time"], tag["name"]) for tag in get(run, "/api/tags")]


def restarted(run, tmp_path, *args):
    run.close()
    return start(tmp_path, *args)


def test_no_tags_to_begin_with(tmp_path):
    run = start(tmp_path)
    assert listed(run) == []
    run.close()


def test_tags_are_listed_by_time(tmp_path):
    run = start(tmp_path)
    assert add(run, TIME, "lunch") == 200
    assert add(run, TIME - 600, "coffee") == 200
    assert listed(run) == [(TIME - 600, "coffee"), (TIME, "lunch")]
    run.close()


def test_two_tags_can_share_a_moment(tmp_path):
    run = start(tmp_path)
    add(run, TIME, "lunch")
    add(run, TIME, "coffee")
    assert sorted(listed(run)) == [(TIME, "coffee"), (TIME, "lunch")]
    run.close()


def test_the_same_tag_twice_is_one_tag(tmp_path):
    run = start(tmp_path)
    assert add(run, TIME, "lunch") == 200
    assert add(run, TIME, "lunch") == 200
    assert listed(run) == [(TIME, "lunch")]
    run.close()


def test_names_are_trimmed_and_lowercased(tmp_path):
    run = start(tmp_path)
    add(run, TIME, "  Cold Brew ")
    assert listed(run) == [(TIME, "cold brew")]
    run.close()


@pytest.mark.parametrize("name", ['say "hi"', "back\\slash", f"caf{chr(0xe9)} + a&b=c",
                                  chr(0x4e2d) + chr(0x6587)])
def test_names_survive_the_trip(tmp_path, name):
    run = start(tmp_path)
    add(run, TIME, name)
    run = restarted(run, tmp_path)
    assert listed(run) == [(TIME, name)]
    run.close()


def test_longest_name_is_accepted(tmp_path):
    run = start(tmp_path)
    assert add(run, TIME, "x" * MAX_NAME) == 200
    assert listed(run) == [(TIME, "x" * MAX_NAME)]
    run.close()


@pytest.mark.parametrize("name", ["", "   ", "x" * (MAX_NAME + 1), "a\nb"])
def test_bad_names_are_refused(tmp_path, name):
    run = start(tmp_path)
    assert add(run, TIME, name) == 400
    assert listed(run) == []
    run.close()


@pytest.mark.parametrize("time", ["", "soon", "0", "-5", "99999999999"])
def test_bad_times_are_refused(tmp_path, time):
    run = start(tmp_path)
    assert add(run, time, "lunch") == 400
    assert listed(run) == []
    run.close()


def test_tags_survive_a_restart(tmp_path):
    run = start(tmp_path)
    add(run, TIME, "lunch")
    add(run, TIME + 60, "walk")
    run = restarted(run, tmp_path)
    assert listed(run) == [(TIME, "lunch"), (TIME + 60, "walk")]
    run.close()


def test_a_tag_can_be_renamed(tmp_path):
    run = start(tmp_path)
    add(run, TIME, "lunch")
    assert post(run, "/api/tag", time=TIME, name="brunch", old="lunch") == 200
    assert listed(run) == [(TIME, "brunch")]
    run = restarted(run, tmp_path)
    assert listed(run) == [(TIME, "brunch")]
    run.close()


def test_renaming_onto_a_tag_at_the_same_moment_merges_them(tmp_path):
    run = start(tmp_path)
    add(run, TIME, "lunch")
    add(run, TIME, "brunch")
    assert post(run, "/api/tag", time=TIME, name="brunch", old="lunch") == 200
    assert listed(run) == [(TIME, "brunch")]
    run.close()


def test_a_tag_can_be_removed_for_good(tmp_path):
    run = start(tmp_path)
    add(run, TIME, "lunch")
    add(run, TIME, "coffee")
    assert post(run, "/api/tag/remove", time=TIME, name="lunch") == 200
    assert listed(run) == [(TIME, "coffee")]
    run = restarted(run, tmp_path)
    assert listed(run) == [(TIME, "coffee")]
    run.close()


def test_changing_a_tag_that_is_not_there_is_refused(tmp_path):
    run = start(tmp_path)
    assert post(run, "/api/tag/remove", time=TIME, name="lunch") == 400
    assert post(run, "/api/tag", time=TIME, name="brunch", old="lunch") == 400
    assert listed(run) == []
    run.close()


def test_changing_tags_needs_the_guard_header(tmp_path):
    run = start(tmp_path)
    assert post(run, "/api/tag", guarded=False, time=TIME, name="lunch") == 403
    assert listed(run) == []
    run.close()


def test_status_shows_when_tags_change(tmp_path):
    run = start(tmp_path)
    revisions = [get(run, "/api/status")["tag_revision"]]
    add(run, TIME, "lunch")
    revisions.append(get(run, "/api/status")["tag_revision"])
    post(run, "/api/tag", time=TIME, name="brunch", old="lunch")
    revisions.append(get(run, "/api/status")["tag_revision"])
    post(run, "/api/tag/remove", time=TIME, name="brunch")
    revisions.append(get(run, "/api/status")["tag_revision"])
    assert len(set(revisions)) == 4
    run.close()


def test_forgetting_a_sensor_keeps_tags(tmp_path):
    run = start(tmp_path)
    add(run, TIME, "lunch")
    assert post(run, "/api/forget") == 200
    assert listed(run) == [(TIME, "lunch")]
    run.close()


def test_a_full_ring_drops_the_oldest_tags(tmp_path):
    sectors = ["--tag-sectors", "2"]
    run = start(tmp_path, *sectors)
    total = RECORDS_PER_SECTOR * 3
    times = [TIME + at * 60 for at in range(total)]
    for time in times:
        assert add(run, time, "t") == 200
    assert len(listed(run)) == total, "the list is sent in pages"
    run = restarted(run, tmp_path, *sectors)
    kept = [time for time, _ in listed(run)]
    assert RECORDS_PER_SECTOR <= len(kept) <= 2 * RECORDS_PER_SECTOR
    assert kept == times[-len(kept):]
    add(run, TIME - 60, "older")
    run = restarted(run, tmp_path, *sectors)
    assert listed(run)[0] == (TIME - 60, "older")
    run.close()


def test_a_damaged_record_loses_only_itself(tmp_path):
    run = start(tmp_path)
    for at, name in enumerate(["coffee", "lunch", "dinner"]):
        add(run, TIME + at, name)
    run.close()
    image = tmp_path / "history" / "tags.bin"
    data = bytearray(image.read_bytes())
    data[HEADER_SIZE + RECORD_SIZE + 10] ^= 0xFF
    image.write_bytes(data)
    run = start(tmp_path)
    assert listed(run) == [(TIME, "coffee"), (TIME + 2, "dinner")]
    add(run, TIME + 3, "bed")
    run = restarted(run, tmp_path)
    assert [name for _, name in listed(run)] == ["coffee", "dinner", "bed"]
    run.close()


def test_a_foreign_sector_is_ignored_and_reused(tmp_path):
    run = start(tmp_path)
    run.close()
    image = tmp_path / "history" / "tags.bin"
    data = bytearray(image.read_bytes())
    data[:64] = b"NDLG" + b"\x01" * 60
    image.write_bytes(data)
    run = start(tmp_path)
    assert listed(run) == []
    assert add(run, TIME, "lunch") == 200
    run = restarted(run, tmp_path)
    assert listed(run) == [(TIME, "lunch")]
    run.close()


def test_tags_without_flash_stay_for_the_session(tmp_path):
    run = Run(Sensor(CODE), tmp_path, args=["--http", "127.0.0.1:0"])
    assert add(run, TIME, "lunch") == 200
    assert listed(run) == [(TIME, "lunch")]
    run.close()


def test_page_offers_tag_rows_and_an_add_button(tmp_path):
    run = start(tmp_path)
    conn = http.client.HTTPConnection("127.0.0.1", run.http_port, timeout=5)
    conn.request("GET", "/")
    page = conn.getresponse().read().decode("ascii")
    run.close()
    hero = page[page.index('<section id="hero">'):page.index("</section>")]
    assert 'id="add-tag"' in hero
    assert "tag-row" in page and "tip-tag" in page
    assert '"datetime-local"' in page
    assert '["-15m", 900]' in page


def test_page_uses_tags_and_scroll_controls(tmp_path):
    run = start(tmp_path)
    conn = http.client.HTTPConnection("127.0.0.1", run.http_port, timeout=5)
    conn.request("GET", "/")
    page = conn.getresponse().read().decode("ascii")
    run.close()
    for used in ("/api/tags", "/api/tag/remove", "shift+scroll", "ctrl+scroll", '"wheel"'):
        assert used in page
