import io
import json
import os
import re
import socket
import struct
import sys
import threading
import time
import warnings
import zipfile
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

SMALL = b"hello, remote zip\n" * 10
LINES = b"".join(b"line %d\n" % i for i in range(5000))
BIG = os.urandom(2 * 1024 * 1024)


def build_zip(entries):
    """entries: list of (name, data, compression). Each entry gets its own timestamp."""
    buffer = io.BytesIO()
    with warnings.catch_warnings(), zipfile.ZipFile(buffer, "w") as archive:
        warnings.simplefilter("ignore")  # Duplicate names are written on purpose.
        for i, (name, data, compression) in enumerate(entries):
            # An odd second, which ZIP's 2-second resolution rounds down.
            info = zipfile.ZipInfo(name, date_time=(2001 + i, 1 + i % 12, 1 + i, 13, 37 + i, 43))
            info.external_attr = (0o40755 << 16) | 0x10 if name.endswith("/") else 0o100644 << 16
            archive.writestr(info, data, compress_type=compression)
    return buffer.getvalue()


def build_raw_names_zip(entries):
    """Like build_zip, but names are bytes stored without the UTF-8 flag, as old tools write them."""
    placeholders = []
    for i, (name, _, _) in enumerate(entries):
        placeholder = ("~%d~" % i).ljust(len(name), "~")
        assert len(placeholder) == len(name)
        placeholders.append(placeholder)
    data = build_zip([(p, d, c) for p, (_, d, c) in zip(placeholders, entries)])
    for placeholder, (name, _, _) in zip(placeholders, entries):
        # Once in the local header and once in the central directory.
        assert data.count(placeholder.encode()) == 2
        data = data.replace(placeholder.encode(), name)
    return data


def corrupt(data, name):
    """Flips a byte inside the stored data of `name`."""
    archive = zipfile.ZipFile(io.BytesIO(data))
    info = archive.getinfo(name)
    data = bytearray(data)
    name_length = int.from_bytes(data[info.header_offset + 26 : info.header_offset + 28], "little")
    extra_length = int.from_bytes(data[info.header_offset + 28 : info.header_offset + 30], "little")
    start = info.header_offset + 30 + name_length + extra_length
    data[start + 3] ^= 0xFF
    return bytes(data)


MAIN = build_zip(
    [
        ("docs/", b"", zipfile.ZIP_STORED),
        ("docs/small.txt", SMALL, zipfile.ZIP_DEFLATED),
        ("docs/lines.txt", LINES, zipfile.ZIP_DEFLATED),
        ("big.bin", BIG, zipfile.ZIP_STORED),
        ("empty.txt", b"", zipfile.ZIP_STORED),
    ]
)
CORRUPT = corrupt(
    build_zip(
        [
            ("good.txt", SMALL, zipfile.ZIP_STORED),
            ("bad.txt", SMALL, zipfile.ZIP_STORED),
            ("also-good.txt", SMALL, zipfile.ZIP_STORED),
        ]
    ),
    "bad.txt",
)
UNSAFE = build_zip(
    [
        ("ok.txt", SMALL, zipfile.ZIP_STORED),
        ("../evil.txt", SMALL, zipfile.ZIP_STORED),
    ]
)
DUPLICATE = build_zip(
    [
        ("same.txt", b"first", zipfile.ZIP_STORED),
        ("same.txt", b"second", zipfile.ZIP_STORED),
    ]
)
SLOW = build_zip([("file-%d.bin" % i, os.urandom(256 * 1024), zipfile.ZIP_STORED) for i in range(8)])
CP437_NAME = b"caf\x82.txt"  # "café.txt" in code page 437
ALL_HIGH_BYTES_NAME = bytes(range(0x80, 0x100)) + b".bin"
RAW_NAMES = build_raw_names_zip(
    [
        (CP437_NAME, b"cafe data", zipfile.ZIP_DEFLATED),
        (b"plain.txt", b"plain data", zipfile.ZIP_STORED),
        (b"d\x8einfo/" + CP437_NAME, b"nested data", zipfile.ZIP_STORED),
        (ALL_HIGH_BYTES_NAME, b"high data", zipfile.ZIP_STORED),
    ]
)

def tiny_png():
    """A valid 1x1 PNG, so documentation examples can open an image."""

    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))

    header = struct.pack(">IIBBBBB", 1, 1, 8, 2, 0, 0, 0)
    pixels = zlib.compress(b"\0\xff\x80\0")  # One RGB row, after its filter byte.
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", pixels) + chunk(b"IEND", b"")


# What the README and docs/integration.md examples read as https://example.com/files.zip.
EXAMPLE = build_zip(
    [
        ("docs/", b"", zipfile.ZIP_STORED),
        ("docs/readme.txt", b"Hello from lazy_zip!\n", zipfile.ZIP_DEFLATED),
        (
            "data/records.csv",
            b"id,name,score\n" + b"".join(b"%d,item-%d,%d\n" % (i, i, i % 100) for i in range(1000)),
            zipfile.ZIP_DEFLATED,
        ),
        ("data/config.json", json.dumps({"name": "example", "version": 1}).encode(), zipfile.ZIP_DEFLATED),
        ("data/big.bin", os.urandom(1024 * 1024), zipfile.ZIP_STORED),
        ("images/logo.png", tiny_png(), zipfile.ZIP_STORED),
    ]
)

ARCHIVES = {
    "/files.zip": EXAMPLE,
    "/main.zip": MAIN,
    "/corrupt.zip": CORRUPT,
    "/unsafe.zip": UNSAFE,
    "/duplicate.zip": DUPLICATE,
    "/slow.zip": SLOW,
    "/stalled-entry.zip": SLOW,
    "/raw-names.zip": RAW_NAMES,
}
# Requests for this entry of /stalled-entry.zip are answered only after STALL_SECONDS.
STALLED_ENTRY = "file-3.bin"
STALLED_OFFSET = zipfile.ZipFile(io.BytesIO(SLOW)).getinfo(STALLED_ENTRY).header_offset
STALL_SECONDS = 0.5

RANGE = re.compile(r"bytes=(\d*)-(\d*)$")


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_HEAD(self):
        self.respond(body=False)

    def do_GET(self):
        self.respond(body=True)

    def respond(self, body):
        headers = {name.lower(): value for name, value in self.headers.items()}
        self.server.requests.append((self.command, self.path, headers))
        data = ARCHIVES.get(self.path)
        if data is None:
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        total = len(data)
        status, start, end = 200, 0, total - 1
        match = RANGE.match(self.headers.get("Range", ""))
        if match:
            first, last = match.groups()
            if first:
                start = int(first)
                end = min(int(last), total - 1) if last else total - 1
            else:
                start = max(total - int(last), 0)
            if start > end:
                self.send_response(416)
                self.send_header("Content-Range", f"bytes */{total}")
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            status = 206
        payload = data[start : end + 1]
        if self.path == "/slow.zip" and body and status == 206 and start < total - 64 * 1024:
            time.sleep(0.3)
        if self.path == "/stalled-entry.zip" and body and status == 206 and start == STALLED_OFFSET:
            time.sleep(STALL_SECONDS)
        self.send_response(status)
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("ETag", '"v1"')
        self.send_header("Content-Type", "application/zip")
        self.send_header("Content-Length", str(len(payload)))
        if status == 206:
            self.send_header("Content-Range", f"bytes {start}-{end}/{total}")
        self.end_headers()
        if body:
            try:
                self.wfile.write(payload)
            except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                pass


class QuietServer(ThreadingHTTPServer):
    def handle_error(self, request, client_address):
        # Clients drop connections they no longer need, such as on Ctrl-C.
        if not isinstance(sys.exc_info()[1], ConnectionError):
            super().handle_error(request, client_address)


class Server:
    def __init__(self):
        self.httpd = QuietServer(("127.0.0.1", 0), Handler)
        self.httpd.daemon_threads = True
        self.httpd.requests = []
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.thread.start()

    def url(self, path):
        host, port = self.httpd.server_address[:2]
        return f"http://{host}:{port}{path}"

    @property
    def requests(self):
        return self.httpd.requests

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()


@pytest.fixture(scope="session")
def server():
    server = Server()
    yield server
    server.close()


@pytest.fixture
def stalled_url():
    """A server that accepts connections but never answers."""
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.bind(("127.0.0.1", 0))
    listener.listen(64)
    host, port = listener.getsockname()
    yield f"http://{host}:{port}/stalled.zip"
    listener.close()
