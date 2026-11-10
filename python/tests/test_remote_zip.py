import _thread
import io
import os
import subprocess
import sys
import threading
import time
import zipfile

import pytest

import lazy_zip
from conftest import BIG, LINES, MAIN, SMALL


@pytest.fixture
def archive(server):
    with lazy_zip.RemoteZip(server.url("/main.zip")) as archive:
        yield archive


def test_listing_matches_zipfile(archive):
    local = zipfile.ZipFile(io.BytesIO(MAIN))
    assert archive.namelist() == local.namelist()
    for info, expected in zip(archive.infolist(), local.infolist()):
        assert info.filename == expected.filename
        assert info.file_size == expected.file_size
        assert info.compress_size == expected.compress_size
        assert info.CRC == expected.CRC
        assert info.compress_type == expected.compress_type
        assert info.header_offset == expected.header_offset
        assert info.is_dir() == expected.is_dir()
        assert info.date_time == expected.date_time
        assert info.external_attr == expected.external_attr
        assert info.orig_filename == expected.orig_filename
    assert archive.getinfo("docs/small.txt").compress_type == zipfile.ZIP_DEFLATED
    assert archive.content_length == len(MAIN)


def test_missing_names_raise_key_error(archive):
    with pytest.raises(KeyError):
        archive.getinfo("nope.txt")
    with pytest.raises(KeyError):
        archive.read("nope.txt")
    with pytest.raises(KeyError):
        archive.open("nope.txt")


def test_read(archive):
    assert archive.read("docs/small.txt") == SMALL
    assert archive.read(archive.getinfo("big.bin")) == BIG
    assert archive.read("empty.txt") == b""


def test_open_streams_in_chunks(archive):
    with archive.open("big.bin") as f:
        assert f.readable() and f.seekable()
        assert f.name == "big.bin"
        chunks = []
        while True:
            chunk = f.read(10_000)
            if not chunk:
                break
            assert len(chunk) <= 10_000
            chunks.append(chunk)
    assert f.closed
    assert b"".join(chunks) == BIG


def test_open_supports_lines_and_readinto(archive):
    with archive.open("docs/lines.txt") as f:
        assert f.readline() == b"line 0\n"
        assert next(f) == b"line 1\n"
        buffer = bytearray(7)
        assert f.readinto(buffer) == 7 and bytes(buffer) == b"line 2\n"
        assert f.read() == LINES[len(b"line 0\nline 1\nline 2\n") :]
    with archive.open(archive.getinfo("docs/lines.txt")) as f:
        assert list(f) == LINES.splitlines(keepends=True)


def test_reading_a_small_file_fetches_little(archive):
    before = archive.network_bytes
    assert before < archive.content_length
    archive.read("docs/small.txt")
    assert archive.network_bytes - before < 100 * 1024
    assert archive.network_bytes < archive.content_length / 4


def test_extract(archive, tmp_path):
    written = archive.extract("docs/small.txt", tmp_path)
    assert isinstance(written, str)
    assert written == str(tmp_path / "docs" / "small.txt")
    assert (tmp_path / "docs" / "small.txt").read_bytes() == SMALL
    assert archive.extract("docs/", tmp_path) == str(tmp_path / "docs")
    assert not [p for p in tmp_path.rglob("*") if p.name.endswith(".part")]


def test_extract_defaults_to_the_current_directory(archive, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    archive.extract("empty.txt")
    assert (tmp_path / "empty.txt").read_bytes() == b""


def test_extractall(archive, tmp_path):
    archive.extractall(tmp_path)
    assert (tmp_path / "docs" / "small.txt").read_bytes() == SMALL
    assert (tmp_path / "docs" / "lines.txt").read_bytes() == LINES
    assert (tmp_path / "big.bin").read_bytes() == BIG
    assert (tmp_path / "empty.txt").read_bytes() == b""


def test_extractall_members(archive, tmp_path):
    archive.extractall(tmp_path, members=["docs/small.txt", archive.getinfo("empty.txt")])
    assert sorted(p.name for p in tmp_path.rglob("*")) == ["docs", "empty.txt", "small.txt"]
    with pytest.raises(KeyError):
        archive.extractall(tmp_path, members=["nope.txt"])


def test_read_to_file(archive, tmp_path):
    archive.read_to_file("docs/lines.txt", tmp_path / "out.txt")
    assert (tmp_path / "out.txt").read_bytes() == LINES


def test_unsafe_names_are_refused(server, tmp_path):
    root = tmp_path / "root"
    with lazy_zip.RemoteZip(server.url("/unsafe.zip")) as archive:
        assert "../evil.txt" in archive.namelist()
        with pytest.raises(lazy_zip.BadZipFile):
            archive.extract("../evil.txt", root)
        with pytest.raises(lazy_zip.ExtractError) as raised:
            archive.extractall(root)
        assert list(raised.value.failures) == ["../evil.txt"]
        assert (root / "ok.txt").read_bytes() == SMALL
    assert not (tmp_path / "evil.txt").exists()


def test_corruption_is_detected(server, tmp_path):
    with lazy_zip.RemoteZip(server.url("/corrupt.zip")) as archive:
        assert archive.read("good.txt") == SMALL
        with pytest.raises(lazy_zip.BadZipFile):
            archive.read("bad.txt")
        with pytest.raises(zipfile.BadZipFile):
            with archive.open("bad.txt") as f:
                f.read()
        assert archive.testzip() == "bad.txt"
        with pytest.raises(lazy_zip.ExtractError) as raised:
            archive.extractall(tmp_path)
        assert list(raised.value.failures) == ["bad.txt"]
    assert sorted(p.name for p in tmp_path.iterdir()) == ["also-good.txt", "good.txt"]


def test_testzip_passes_a_good_archive(archive):
    assert archive.testzip() is None


def test_duplicate_names_use_the_last_entry(server):
    with lazy_zip.RemoteZip(server.url("/duplicate.zip")) as archive:
        assert archive.namelist() == ["same.txt", "same.txt"]
        assert archive.read("same.txt") == b"second"
        first, second = archive.infolist()
        assert archive.read(first) == b"first"
        assert archive.getinfo("same.txt").header_offset == second.header_offset


def test_headers_are_sent(server):
    start = len(server.requests)
    headers = {"Authorization": "Bearer secret", "X-Custom": "yes"}
    with lazy_zip.RemoteZip(server.url("/main.zip"), headers=headers) as archive:
        archive.read("docs/small.txt")
    requests = server.requests[start:]
    assert requests
    for _, _, sent in requests:
        assert sent.get("authorization") == "Bearer secret"
        assert sent.get("x-custom") == "yes"


def test_invalid_header_raises_value_error(server):
    with pytest.raises(ValueError):
        lazy_zip.RemoteZip(server.url("/main.zip"), headers={"Bad Name": "x"})
    with pytest.raises(ValueError):
        lazy_zip.RemoteZip(server.url("/main.zip"), headers={"Range": "bytes=0-1"})


def test_network_errors(server):
    with pytest.raises(ConnectionError):
        lazy_zip.RemoteZip(server.url("/missing.zip"))


def test_closed_archive(server):
    archive = lazy_zip.RemoteZip(server.url("/main.zip"))
    stream = archive.open("docs/lines.txt")
    archive.close()
    with pytest.raises(ValueError):
        archive.namelist()
    assert stream.read() == LINES
    stream.close()
    with pytest.raises(ValueError):
        stream.read()


def test_files_tree(archive):
    names = sorted(entry.name for entry in archive.files)
    assert names == ["big.bin", "docs", "empty.txt"]
    docs = next(entry for entry in archive.files if entry.name == "docs")
    assert sorted(child.name for child in docs.children) == ["lines.txt", "small.txt"]


def test_gil_is_released_during_downloads(server, tmp_path):
    archive = lazy_zip.RemoteZip(server.url("/slow.zip"))
    worker = threading.Thread(target=archive.extractall, args=(tmp_path,), kwargs={"workers": 2})
    ticks = 0
    worker.start()
    while worker.is_alive():
        ticks += 1
        time.sleep(0.01)
    worker.join()
    assert ticks > 20
    assert len(list(tmp_path.iterdir())) == 8


def test_ctrl_c_interrupts_extractall(server, tmp_path):
    archive = lazy_zip.RemoteZip(server.url("/slow.zip"))
    timer = threading.Timer(0.5, _thread.interrupt_main)
    timer.start()
    started = time.monotonic()
    with pytest.raises(KeyboardInterrupt):
        archive.extractall(tmp_path, workers=1)
    assert time.monotonic() - started < 3
    assert not [p for p in tmp_path.iterdir() if p.name.endswith(".part")]
    assert len(list(tmp_path.iterdir())) < 8


def test_cli_lists_and_extracts(server, tmp_path):
    env = dict(os.environ)
    listed = subprocess.run(
        [sys.executable, "-m", "lazy_zip", server.url("/main.zip"), "-l"],
        capture_output=True,
        text=True,
        env=env,
        check=True,
    )
    assert "docs/small.txt" in listed.stdout
    subprocess.run(
        [
            sys.executable,
            "-m",
            "lazy_zip",
            server.url("/main.zip"),
            "-d",
            str(tmp_path),
            "-H",
            "X-Custom: cli",
            "docs/small.txt",
        ],
        capture_output=True,
        env=env,
        check=True,
    )
    assert (tmp_path / "docs" / "small.txt").read_bytes() == SMALL
    assert any(sent.get("x-custom") == "cli" for _, _, sent in server.requests)
