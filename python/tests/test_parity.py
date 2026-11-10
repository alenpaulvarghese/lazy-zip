"""Behavior that matches zipfile: timestamps, legacy names, seeking, and structured errors."""

import contextlib
import io
import math
import random
import time
import zipfile

import pytest

import lazy_zip
from conftest import (
    ALL_HIGH_BYTES_NAME,
    BIG,
    CP437_NAME,
    LINES,
    MAIN,
    RAW_NAMES,
    SMALL,
    STALLED_ENTRY,
)

CAFE = CP437_NAME.decode("cp437")


@pytest.fixture
def archive(server):
    with lazy_zip.RemoteZip(server.url("/main.zip")) as archive:
        yield archive


@pytest.fixture
def raw_names(server):
    with lazy_zip.RemoteZip(server.url("/raw-names.zip")) as archive:
        yield archive


# Timestamps and attributes


def test_date_time_matches_zipfile(archive):
    local = zipfile.ZipFile(io.BytesIO(MAIN))
    assert [i.date_time for i in archive.infolist()] == [i.date_time for i in local.infolist()]
    assert archive.getinfo("docs/small.txt").date_time == (2002, 2, 2, 13, 38, 42)
    assert [i.external_attr for i in archive.infolist()] == [
        i.external_attr for i in local.infolist()
    ]


def test_printdir_matches_zipfile(archive):
    ours, theirs = io.StringIO(), io.StringIO()
    archive.printdir(ours)
    zipfile.ZipFile(io.BytesIO(MAIN)).printdir(theirs)
    assert ours.getvalue() == theirs.getvalue()


# Names that aren't UTF-8


def test_legacy_names_are_decoded_like_zipfile(raw_names):
    local = zipfile.ZipFile(io.BytesIO(RAW_NAMES))
    assert raw_names.namelist() == local.namelist()
    assert CAFE == "café.txt"
    assert CAFE in raw_names.namelist()
    assert ALL_HIGH_BYTES_NAME.decode("cp437") in raw_names.namelist()
    for info, expected in zip(raw_names.infolist(), local.infolist()):
        assert info.filename == expected.filename
        assert info.orig_filename == expected.orig_filename
        assert info.date_time == expected.date_time


def test_legacy_names_can_be_read(raw_names):
    assert raw_names.read(CAFE) == b"cafe data"
    assert raw_names.read(raw_names.getinfo(CAFE)) == b"cafe data"
    assert raw_names.read(ALL_HIGH_BYTES_NAME.decode("cp437")) == b"high data"
    with raw_names.open(CAFE) as f:
        assert f.name == CAFE
        assert f.read() == b"cafe data"
    assert raw_names.testzip() is None


def test_legacy_names_can_be_extracted(raw_names, tmp_path):
    written = raw_names.extract(CAFE, tmp_path)
    assert written == str(tmp_path / CAFE)
    assert (tmp_path / CAFE).read_bytes() == b"cafe data"

    nested = "dÄinfo/" + CAFE
    assert nested in raw_names.namelist()
    out = tmp_path / "all"
    raw_names.extractall(out, members=[nested, raw_names.getinfo("plain.txt")])
    assert (out / "dÄinfo" / CAFE).read_bytes() == b"nested data"
    assert (out / "plain.txt").read_bytes() == b"plain data"


# Duplicate names


def test_extractall_writes_the_given_duplicate(server, tmp_path):
    with lazy_zip.RemoteZip(server.url("/duplicate.zip")) as archive:
        first, second = archive.infolist()
        archive.extractall(tmp_path / "a", members=[first])
        assert (tmp_path / "a" / "same.txt").read_bytes() == b"first"
        archive.extractall(tmp_path / "b", members=[second])
        assert (tmp_path / "b" / "same.txt").read_bytes() == b"second"
        archive.extractall(tmp_path / "c", members=[second, first])
        assert (tmp_path / "c" / "same.txt").read_bytes() == b"first"
        archive.extractall(tmp_path / "d")
        assert (tmp_path / "d" / "same.txt").read_bytes() == b"second"
        archive.extract(first, tmp_path / "e")
        assert (tmp_path / "e" / "same.txt").read_bytes() == b"first"


# Structured errors


def test_extract_error_failures_are_typed(server, tmp_path):
    with lazy_zip.RemoteZip(server.url("/unsafe.zip")) as archive:
        with pytest.raises(lazy_zip.ExtractError) as raised:
            archive.extractall(tmp_path / "unsafe")
    failure = raised.value.failures["../evil.txt"]
    assert isinstance(failure, lazy_zip.BadZipFile)
    assert failure.kind == "unsafe"
    assert "evil.txt" in str(failure)

    with lazy_zip.RemoteZip(server.url("/corrupt.zip")) as archive:
        with pytest.raises(lazy_zip.ExtractError) as raised:
            archive.extractall(tmp_path / "corrupt")
        failure = raised.value.failures["bad.txt"]
        assert isinstance(failure, zipfile.BadZipFile)
        assert failure.kind == "corrupt"
        assert "bad.txt" in str(raised.value)

        with pytest.raises(lazy_zip.BadZipFile) as direct:
            archive.read("bad.txt")
        assert direct.value.kind == "corrupt"


def test_errors_have_a_kind(server, archive):
    with pytest.raises(ConnectionError) as raised:
        lazy_zip.RemoteZip(server.url("/missing.zip"))
    assert raised.value.kind == "http_status"
    with pytest.raises(KeyError) as missing:
        archive.read("nope.txt")
    assert missing.value.kind == "not_found"
    with pytest.raises(ValueError) as invalid:
        lazy_zip.RemoteZip(server.url("/main.zip"), headers={"Bad Name": "x"})
    assert invalid.value.kind == "invalid_header"


# Timeouts


@pytest.mark.parametrize(
    "timeout", [0, -1, math.nan, math.inf, (1, 0), (0, 1), (1, -2), (1, 2, 3), ()]
)
def test_invalid_timeouts_raise_value_error(server, timeout):
    with pytest.raises(ValueError):
        lazy_zip.RemoteZip(server.url("/main.zip"), timeout=timeout)


def test_non_numeric_timeout_raises_type_error(server):
    with pytest.raises(TypeError):
        lazy_zip.RemoteZip(server.url("/main.zip"), timeout="soon")


@pytest.mark.parametrize("timeout", [5, 2.5, (3, 10)])
def test_valid_timeouts(server, timeout):
    with lazy_zip.RemoteZip(server.url("/main.zip"), timeout=timeout) as archive:
        assert archive.read("docs/small.txt") == SMALL


def test_stalled_server_times_out(stalled_url):
    started = time.monotonic()
    with pytest.raises(ConnectionError) as raised:
        lazy_zip.RemoteZip(stalled_url, timeout=0.2)
    assert raised.value.kind == "network"
    assert time.monotonic() - started < 15


def test_slow_reads_time_out(server):
    with lazy_zip.RemoteZip(server.url("/stalled-entry.zip"), timeout=(5, 0.1)) as archive:
        assert archive.read("file-0.bin")
        with pytest.raises(ConnectionError) as raised:
            archive.read(STALLED_ENTRY)
        assert raised.value.kind == "network"


# Seeking


def test_seek_forward_backward_and_from_the_end(archive):
    with archive.open("docs/lines.txt") as f:
        assert f.seekable()
        assert f.seek(100) == 100 and f.tell() == 100
        assert f.read(10) == LINES[100:110]
        assert f.seek(-60, io.SEEK_CUR) == 50
        assert f.read(5) == LINES[50:55]
        assert f.seek(-10, io.SEEK_END) == len(LINES) - 10
        assert f.read() == LINES[-10:]
        assert f.read() == b""
        # After the end has been read and verified.
        assert f.seek(3) == 3
        assert f.read() == LINES[3:]


def test_seek_is_clamped_like_zipfile(archive):
    with archive.open("docs/small.txt") as f:
        assert f.seek(len(SMALL) + 100) == len(SMALL)
        assert f.read() == b""
        assert f.seek(-5) == 0
        assert f.read(5) == SMALL[:5]
        with pytest.raises(ValueError):
            f.seek(0, 3)


def test_seek_backward_past_the_buffer_reopens_the_member(archive):
    with archive.open("big.bin") as f:
        assert f.read(300_000) == BIG[:300_000]
        assert f.seek(10) == 10
        assert f.read(20) == BIG[10:30]
        assert f.tell() == 30
        assert f.read() == BIG[30:]


def test_forward_seek_downloads_about_what_it_skips(archive):
    with archive.open("big.bin") as f:
        before = archive.network_bytes
        assert f.seek(len(BIG) // 2) == len(BIG) // 2
        skipped = archive.network_bytes - before
        assert skipped <= len(BIG) // 2 + 1024 * 1024
        assert f.read(100) == BIG[len(BIG) // 2 : len(BIG) // 2 + 100]


def test_random_seeks_match_zipfile(archive):
    rng = random.Random(1234)
    local = zipfile.ZipFile(io.BytesIO(MAIN))
    with archive.open("docs/lines.txt") as ours, local.open("docs/lines.txt") as theirs:
        for _ in range(200):
            whence = rng.choice([io.SEEK_SET, io.SEEK_CUR, io.SEEK_END])
            offset = rng.randint(-len(LINES), len(LINES))
            if whence == io.SEEK_SET:
                offset = abs(offset)
            assert ours.seek(offset, whence) == theirs.seek(offset, whence)
            size = rng.randint(0, 5000)
            assert ours.read(size) == theirs.read(size)
            assert ours.tell() == theirs.tell()


def test_seeking_keeps_the_checksum_checked(server):
    with lazy_zip.RemoteZip(server.url("/corrupt.zip")) as archive:
        with archive.open("bad.txt") as f:
            f.seek(50)
            with pytest.raises(lazy_zip.BadZipFile):
                f.read()
        with archive.open("bad.txt") as f:
            with pytest.raises(lazy_zip.BadZipFile):
                f.seek(0, io.SEEK_END)
                f.read()


def test_seek_after_the_archive_is_closed(server):
    archive = lazy_zip.RemoteZip(server.url("/main.zip"))
    with contextlib.closing(archive.open("docs/lines.txt")) as f:
        f.read(100_000)
        archive.close()
        assert f.seek(7) == 7
        assert f.read(7) == LINES[7:14]
