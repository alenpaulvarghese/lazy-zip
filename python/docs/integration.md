# Integrating lazy_zip

A guide for using the `lazy_zip` package inside your own application: installing it, what each
call costs, recipes for common jobs, and what you can rely on. For the reference tables, see the
[README](../README.md).

`lazy_zip` is pre-1.0, so the API may change between versions. Pin a release tag and read the changes
before upgrading.

<!-- The Python examples run in tests/test_docs.py against a local server that stands in for
     https://example.com/. A "test: requires MODULE" comment before a block runs it only when
     MODULE is installed, and "test: compile-only" only checks its syntax. -->

- [Installing it in a project](#installing-it-in-a-project)
- [The mental model](#the-mental-model)
- [Listing and inspecting](#listing-and-inspecting)
- [Reading members](#reading-members)
- [Streaming to a file or a web response](#streaming-to-a-file-or-a-web-response)
- [Extracting](#extracting)
- [Private archives](#private-archives)
- [Timeouts](#timeouts)
- [Handling errors](#handling-errors)
- [Threads](#threads)
- [asyncio](#asyncio)
- [Cancellation and Ctrl-C](#cancellation-and-ctrl-c)
- [Libraries that take file objects](#libraries-that-take-file-objects)
- [Migrating from remotezip or zipfile](#migrating-from-remotezip-or-zipfile)
- [Testing your integration](#testing-your-integration)
- [Guarantees and your responsibilities](#guarantees-and-your-responsibilities)

## Installing it in a project

`lazy_zip` isn't on PyPI yet, so it's built from source. Building needs a
[Rust toolchain](https://rustup.rs); the result doesn't. It supports Python 3.8 and later
(tested most on 3.12) on Windows, macOS and Linux. TLS is built in (rustls), so there's no
OpenSSL dependency.

**From GitHub.** pip clones the repository and builds the `python/` folder with maturin. Pin a
commit so builds are repeatable:

```bash
pip install "lazy-zip @ git+https://github.com/alenpaulvarghese/lazy-zip@v0.3.0#subdirectory=python"
```

In `requirements.txt`, use the same `lazy-zip @ git+...` line. In `pyproject.toml`:

```toml
[project]
dependencies = [
    "lazy-zip @ git+https://github.com/alenpaulvarghese/lazy-zip@v0.3.0#subdirectory=python",
]
```

**As a wheel.** Build once on a machine with Rust, then install the wheel wherever you deploy,
such as in a Docker image without Rust. A wheel only works on the platform and Python version it
was built for (for example CPython 3.12 on 64-bit Linux), so build it on the same base image you
deploy on.

```bash
git clone https://github.com/alenpaulvarghese/lazy-zip.git
cd lazy-zip/python
pip install maturin
maturin build --release     # writes ../target/wheels/lazy_zip-<version>-<tags>.whl
pip install ../target/wheels/lazy_zip-*.whl
```

The package is typed (it ships `py.typed` and stubs), so mypy and editors know its API.

## The mental model

One `RemoteZip` is one read of the archive's directory. Create it once per archive, reuse it for
everything, and close it when you're done.

| Operation | Requests | Data |
|-----------|----------|------|
| `RemoteZip(url)` | 1, or 2 for a big directory | The last ~66 KB, plus the directory if it's bigger |
| `namelist()`, `infolist()`, `getinfo()`, `files` | 0 | None |
| `read(name)`, `extract(name)`, `open(name)` and reading to the end | 1-2 | About the member's compressed size |
| `extractall(workers=n)` | 1-2 per member, `n` at a time | About the members' compressed sizes |
| Forward `seek()` in an open member | Continues the current one | What it skips |
| Backward `seek()` beyond the 64 KiB buffer | 1-2 | From the member's start to the target |
| `testzip()` | 1-2 per member | About the whole archive |

Creating a new `RemoteZip` for every request in a web app works, but pays for the directory each
time. Keep one per archive instead; it's safe to share between threads.

`network_bytes` counts every byte received for the archive, from all threads, including the
directory, retries and files still open after `close()`. Use it to log or bill what each job
costs:

```python
import lazy_zip

with lazy_zip.RemoteZip("https://example.com/files.zip") as archive:
    opened = archive.network_bytes
    archive.read("docs/readme.txt")
    print(f"directory: {opened} bytes, readme: {archive.network_bytes - opened} bytes")
```

## Listing and inspecting

Listing uses the directory already downloaded, so it's free. Check sizes before downloading
anything, especially for archives you don't trust:

```python
import lazy_zip

MAX_TOTAL = 100 * 1024 * 1024

with lazy_zip.RemoteZip("https://example.com/files.zip") as archive:
    csv_files = [i for i in archive.infolist() if i.filename.endswith(".csv")]
    total = sum(i.file_size for i in csv_files)
    if total > MAX_TOTAL:
        raise ValueError(f"{total} bytes is too much")

    info = archive.getinfo("data/records.csv")
    print(info.filename, info.file_size, info.compress_size, info.date_time, info.is_dir())
```

`files` gives the same entries as a tree, for building a file browser. Folders appear even when
the archive has no entry for them, and names that can't be written safely are left out:

```python
def walk(entries, depth=0):
    for entry in entries:
        print("  " * depth + entry.name + ("/" if entry.is_dir else f" ({entry.size} bytes)"))
        walk(entry.children, depth + 1)

with lazy_zip.RemoteZip("https://example.com/files.zip") as archive:
    walk(archive.files)
```

## Reading members

`read()` returns the whole member, verified. Use it for anything that comfortably fits in memory:

```python
import json

with lazy_zip.RemoteZip("https://example.com/files.zip") as archive:
    config = json.loads(archive.read("data/config.json"))
```

`open()` returns a `ZipExtFile`, a normal binary file object that downloads as you read. Wrap it
in `io.TextIOWrapper` for text:

```python
import csv
import io

with lazy_zip.RemoteZip("https://example.com/files.zip") as archive:
    with archive.open("data/records.csv") as f:
        rows = list(csv.DictReader(io.TextIOWrapper(f, encoding="utf-8", newline="")))
print(len(rows), rows[0])
```

The size and CRC are checked when the end is reached: the read that gets there raises
`BadZipFile` if they don't match. So treat data from a stream as unverified until you've read to
the end without an error. `read()`, `extract()` and `read_to_file()` only hand over verified data.

## Streaming to a file or a web response

To save one member to a path of your choice, use `read_to_file()`. It writes to a temporary file
and replaces the destination only once the data is verified:

```python
with lazy_zip.RemoteZip("https://example.com/files.zip") as archive:
    archive.read_to_file("data/big.bin", "big-copy.bin")
```

To send a member somewhere else as it downloads, read it in chunks:

```python
import shutil

with lazy_zip.RemoteZip("https://example.com/files.zip") as archive:
    with archive.open("data/big.bin") as source, open("big.bin", "wb") as target:
        shutil.copyfileobj(source, target, 1024 * 1024)
```

**Web apps.** Open the archive once at startup and stream members from it. A missing name raises
`KeyError` from `getinfo()` or `open()`, before the response starts, so it can become a 404. With
FastAPI or Starlette, a plain generator runs in the thread pool, so it doesn't block the event
loop:

<!-- test: requires fastapi -->
```python
import lazy_zip
from fastapi import FastAPI, HTTPException
from fastapi.responses import StreamingResponse

app = FastAPI()
archive = lazy_zip.RemoteZip("https://example.com/files.zip")


def chunks(f):
    with f:
        while chunk := f.read(64 * 1024):
            yield chunk


@app.get("/files/{name:path}")
def download(name: str):
    try:
        info = archive.getinfo(name)
        f = archive.open(info)
    except KeyError:
        raise HTTPException(status_code=404)
    headers = {"Content-Length": str(info.file_size)}
    return StreamingResponse(chunks(f), media_type="application/octet-stream", headers=headers)
```

With Flask:

<!-- test: requires flask -->
```python
import lazy_zip
from flask import Flask, Response, abort

app = Flask(__name__)
archive = lazy_zip.RemoteZip("https://example.com/files.zip")


@app.get("/files/<path:name>")
def download(name):
    try:
        f = archive.open(name)
    except KeyError:
        abort(404)

    def chunks():
        with f:
            while chunk := f.read(64 * 1024):
                yield chunk

    return Response(chunks(), mimetype="application/octet-stream")
```

If the member turns out to be corrupt, the error comes on the last chunk, after the headers and
most of the body have been sent, so the server can only cut the connection. Clients see a
truncated download (with `Content-Length` set, they can tell). If clients must never receive
unverified bytes, `read()` the member, or `read_to_file()` it to a temporary file, before
responding.

Never pass a user-supplied URL straight to `RemoteZip` in a web app; see
[your responsibilities](#guarantees-and-your-responsibilities).

## Extracting

`extract()` writes one member under a folder, keeping its path inside the archive, and returns
where it went. `extractall()` writes many, several at a time:

```python
with lazy_zip.RemoteZip("https://example.com/files.zip") as archive:
    path = archive.extract("docs/readme.txt", "out")       # out/docs/readme.txt
    archive.extractall("out", workers=4)                   # everything
    wanted = [i for i in archive.infolist() if i.filename.startswith("data/")]
    archive.extractall("data-only", members=wanted)        # a selection
```

- `workers` is how many members download at once (default 8). More helps with many small files
  on a fast connection; keep it modest for servers that rate-limit.
- Every member is attempted. If any fail, `ExtractError` is raised at the end, and the others are
  already written. A name in `members` that isn't in the archive raises `KeyError` before anything
  is written.
- Existing files are replaced, but only by verified data, and a failure never leaves a partial
  file. Extract into a fresh folder if replacing files isn't acceptable.
- Missing folders are created. Unix permissions and timestamps aren't restored. On Windows, files
  are marked as downloaded from the internet.

```python
with lazy_zip.RemoteZip("https://example.com/files.zip") as archive:
    try:
        archive.extractall("out")
    except lazy_zip.ExtractError as error:
        retry = [name for name, e in error.failures.items() if e.kind == "network"]
        rejected = [name for name, e in error.failures.items() if e.kind in ("unsafe", "suspicious")]
        if retry:
            archive.extractall("out", members=retry)
```

## Private archives

`headers` are sent with every request. Use them for bearer tokens, API keys, cookies or basic
authentication:

```python
import base64

TOKEN = "..."  # from your secret store
archive = lazy_zip.RemoteZip("https://example.com/files.zip", headers={"Authorization": f"Bearer {TOKEN}"})
archive.close()

archive = lazy_zip.RemoteZip("https://example.com/files.zip", headers={"X-API-Key": TOKEN})
archive.close()

basic = base64.b64encode(b"user:password").decode()
archive = lazy_zip.RemoteZip("https://example.com/files.zip", headers={"Authorization": f"Basic {basic}"})
archive.close()
```

Presigned URLs, such as those from S3, Google Cloud Storage or Azure SAS, need no headers: the
URL carries the signature. The URL must stay valid while you use the `RemoteZip`, because every
read is a new request. If it expires, reads fail with `ConnectionError` (`kind="http_status"`,
usually 403); create a new `RemoteZip` with a fresh URL.

<!-- test: compile-only -->
```python
url = s3.generate_presigned_url("get_object", Params={"Bucket": "b", "Key": "files.zip"}, ExpiresIn=3600)
archive = lazy_zip.RemoteZip(url)
```

**Redirects.** Many hosts redirect downloads to another domain (GitHub release assets go to
`objects.githubusercontent.com`, for example). Redirects are followed, up to 10. What happens to
your headers on a redirect to another site (a different scheme, host or port) depends on them:

- With no headers, or only `Authorization`, `Cookie` and `Proxy-Authorization`, the redirect is
  followed and those headers are dropped, as browsers and `requests` do. A presigned target
  works; one that needs your token fails with `http_status`.
- With any other header, such as `X-API-Key`, the redirect fails with `ConnectionError`
  (`kind="network"`), because `lazy_zip` can't tell whether that header is a secret and won't
  hand it to another site. Resolve the redirect yourself and open the final URL with the headers
  it needs.

Redirects within the same site keep all headers. `Range` and `Accept-Encoding` are set by
`lazy_zip` and can't be passed; invalid header names or values raise `ValueError`
(`kind="invalid_header"`).

## Timeouts

```python
archive = lazy_zip.RemoteZip("https://example.com/files.zip", timeout=10)        # both phases
archive.close()
archive = lazy_zip.RemoteZip("https://example.com/files.zip", timeout=(5, 120))  # (connect, read)
archive.close()
```

The default is 15 seconds to connect and 30 seconds for each wait for data. The read timeout
applies to every wait, not the whole download, so a large file can take as long as it needs while
data keeps arriving. Timed-out requests are retried a few times before `ConnectionError`
(`kind="network"`) is raised, so the worst case is a few times the timeout. There's no overall
time limit; enforce one yourself if you need it (see [cancellation](#cancellation-and-ctrl-c)).

## Handling errors

Each exception has a `kind` that says what went wrong; the [README](../README.md#errors) lists
them all with their exception types. Before raising, `lazy_zip` already retries each request up to
4 times with backoff (honoring `Retry-After`, such as on 429 and 503 responses) and resumes dropped
downloads, so an error means those retries are used up.

| `kind` | Retry? | Typical cause and response |
|--------|--------|----------------------------|
| `network` | Later, with backoff | Server down or unreachable, timeout, refused redirect |
| `http_status` | No, unless you can fix it | 401/403: refresh credentials or the presigned URL. 404: wrong URL |
| `remote_changed` | Yes, with a new `RemoteZip` | The file was replaced; the old directory no longer applies |
| `range_not_supported`, `not_a_zip` | No | Wrong URL (often an HTML page), or the server can't serve parts |
| `corrupt` | Once at most | Damaged archive, or rarely a broken proxy |
| `suspicious`, `unsafe` | No | Treat the archive or member as hostile; reject it |
| `limit_exceeded`, `unsupported` | No | Too big to open, encrypted, or an unsupported method |
| `not_found`, `invalid_header` | No | A bug in the caller |
| `io` | After fixing the disk | Permissions, disk full |

A small wrapper that reopens on `remote_changed` and backs off on network errors:

```python
import time

import lazy_zip


def read_member(url, name, attempts=3):
    """Reads one member, retrying the failures that can succeed on another try."""
    for attempt in range(1, attempts + 1):
        try:
            with lazy_zip.RemoteZip(url, timeout=(5, 30)) as archive:
                return archive.read(name)
        except ConnectionError as error:
            if getattr(error, "kind", None) not in ("network", "remote_changed") or attempt == attempts:
                raise
            time.sleep(2 ** attempt)


data = read_member("https://example.com/files.zip", "docs/readme.txt")
```

To tell a hostile archive from a damaged one:

```python
try:
    with lazy_zip.RemoteZip("https://example.com/files.zip") as archive:
        data = archive.read("data/config.json")
except lazy_zip.BadZipFile as error:
    if error.kind in ("suspicious", "unsafe"):
        print("Rejected a malicious archive:", error)
    else:
        print("Damaged or not a ZIP archive:", error)
except NotImplementedError as error:
    print("Encrypted or unsupported compression:", error)
```

The exception types match `zipfile`, so code written for it keeps working:
`lazy_zip.BadZipFile` is a `zipfile.BadZipFile`, `NotImplementedError` is a `RuntimeError` (what
`zipfile` raises for encrypted members), and `ConnectionError` is an `OSError`. An HTTP error's
status code is only in the message. A closed archive raises `ValueError` without a `kind`.

## Threads

- **A `RemoteZip` can be shared by any number of threads.** Its methods are safe to call at the
  same time, and run in parallel because they release the GIL while they wait. `close()` doesn't
  interrupt calls already running; they finish normally.
- **A `ZipExtFile` belongs to one thread.** Reads from several threads won't crash (they're
  serialized), but they share one position, so each thread gets arbitrary pieces. Have each thread
  `open()` the member itself.

```python
from concurrent.futures import ThreadPoolExecutor

with lazy_zip.RemoteZip("https://example.com/files.zip") as archive:
    names = [i.filename for i in archive.infolist() if not i.is_dir()]
    with ThreadPoolExecutor(max_workers=4) as pool:
        sizes = dict(zip(names, pool.map(lambda name: len(archive.read(name)), names)))
print(sizes)
```

For extracting many files, `extractall(workers=n)` is simpler and does the same internally.

## asyncio

Every call blocks until its network work is done, but releases the GIL, so run calls in a thread
with `asyncio.to_thread` (or `loop.run_in_executor(None, ...)` on Python 3.8):

```python
import asyncio

import lazy_zip


async def main():
    archive = await asyncio.to_thread(lazy_zip.RemoteZip, "https://example.com/files.zip")
    try:
        readme, config = await asyncio.gather(
            asyncio.to_thread(archive.read, "docs/readme.txt"),
            asyncio.to_thread(archive.read, "data/config.json"),
        )
    finally:
        archive.close()
    return readme, config


readme, config = asyncio.run(main())
```

Cancelling the awaiting task doesn't stop a call already running in its thread; it runs to
completion or to its timeout. For long transfers, read in chunks, so cancellation takes effect
between them:

```python
async def stream(archive, name, chunk_size=256 * 1024):
    f = await asyncio.to_thread(archive.open, name)
    try:
        while chunk := await asyncio.to_thread(f.read, chunk_size):
            yield chunk
    finally:
        f.close()


async def count_bytes():
    with lazy_zip.RemoteZip("https://example.com/files.zip") as archive:
        return sum([len(chunk) async for chunk in stream(archive, "data/big.bin")])


print(asyncio.run(count_bytes()))
```

That async generator can be passed straight to Starlette's or FastAPI's `StreamingResponse`.

## Cancellation and Ctrl-C

- **On the main thread**, Ctrl-C raises `KeyboardInterrupt` within about 0.1 seconds, even in the
  middle of a download. The work stops, and partly written files are removed before the exception
  reaches you. In `extractall`, members already finished stay written.
- **On other threads**, Python doesn't deliver Ctrl-C, so a running call continues until it
  finishes or fails. Make long-running worker threads daemons, or keep the blocking calls on the
  main thread, if Ctrl-C must exit promptly.
- **There's no cancel method.** To stop a transfer early, or enforce an overall deadline, stream it
  with `open()` and stop between chunks; closing the `ZipExtFile` ends its download:

```python
import threading
import time

stop = threading.Event()
deadline = time.monotonic() + 60

with lazy_zip.RemoteZip("https://example.com/files.zip") as archive:
    with archive.open("data/big.bin") as source, open("partial.bin", "wb") as target:
        while not stop.is_set() and time.monotonic() < deadline:
            chunk = source.read(256 * 1024)
            if not chunk:
                break
            target.write(chunk)
```

## Libraries that take file objects

A `ZipExtFile` is an ordinary binary file object, so it can go straight to most libraries:

```python
import json

with lazy_zip.RemoteZip("https://example.com/files.zip") as archive:
    with archive.open("data/config.json") as f:
        config = json.load(f)
```

<!-- test: requires pandas -->
```python
import pandas as pd

with lazy_zip.RemoteZip("https://example.com/files.zip") as archive:
    with archive.open("data/records.csv") as f:
        frame = pd.read_csv(f)
```

<!-- test: requires PIL -->
```python
from PIL import Image

with lazy_zip.RemoteZip("https://example.com/files.zip") as archive:
    with archive.open("images/logo.png") as f:
        image = Image.open(f)
        image.load()  # Pillow reads lazily, so load before the file closes
print(image.size)
```

Mind what seeking costs. Libraries that read from start to end (CSV, JSON, most image decoders)
are cheap: they read the member once, and small backward seeks stay within the 64 KiB buffer.
Libraries that jump around (a ZIP inside the ZIP, Parquet, SQLite, PDF) would download the member
again for each backward jump. Give those an in-memory or temporary copy:

<!-- test: compile-only -->
```python
import io
import zipfile

with lazy_zip.RemoteZip(url) as archive:
    inner = zipfile.ZipFile(io.BytesIO(archive.read("archives/inner.zip")))
    archive.read_to_file("data/table.parquet", "/tmp/table.parquet")
```

## Migrating from remotezip or zipfile

Most read-only code only needs the import and constructor changed.

| `remotezip` / `zipfile` | `lazy_zip` |
|-------------------------|------------|
| `remotezip.RemoteZip(url, headers=h, timeout=t)` | `lazy_zip.RemoteZip(url, headers=h, timeout=t)` |
| `RemoteZip(url, auth=(user, pw))` | `headers={"Authorization": "Basic " + base64...}` |
| `RemoteZip(url, session=s, proxies=p, verify=False)` | Not supported. Proxies come from `HTTPS_PROXY`/`HTTP_PROXY`/`NO_PROXY`; certificates are always verified |
| `support_suffix_range=False`, `initial_buffer_size=n` | Not needed: servers without suffix ranges are handled automatically |
| `z.size()` (remotezip) | `z.content_length` |
| `namelist`, `infolist`, `filelist`, `getinfo`, `read`, `open`, `extract`, `extractall`, `testzip`, `printdir`, `close` | Same names and arguments, minus `pwd` |
| `pwd=`, `setpassword()` | Not supported; encrypted members raise `NotImplementedError` |
| `z.comment`, `ZipInfo.comment`, `.extra`, `.flag_bits`, `.create_system`, ... | Not available |
| `zipfile.BadZipFile` | Still works: `lazy_zip.BadZipFile` subclasses it |
| `remotezip.RemoteIOError`, `RangeNotSupported` | `ConnectionError` with `kind` |
| `requests.Timeout` | `ConnectionError` with `kind="network"` |
| `zipfile.Path(z)`, `isinstance(z, zipfile.ZipFile)` | Not supported: `RemoteZip` isn't a `ZipFile` subclass |

Behavior that changes:

- **Unsafe names are refused, not rewritten.** `zipfile` turns `../evil.txt` into `evil.txt`;
  `lazy_zip` raises `BadZipFile` (`kind="unsafe"`) from `extract`, and reports it in
  `ExtractError` from `extractall`. Names with backslashes, `:`, `?` or Windows reserved names are
  refused on every platform. Two names that differ only by case can't both be extracted at once.
- **`extractall` keeps going.** `zipfile` stops at the first error; `lazy_zip` attempts every
  member, then raises `ExtractError` with each failure. It also downloads 8 members at once by
  default.
- **Hostile archives are refused** where `zipfile` might read them: overlapping entries, local
  headers that disagree with the directory, impossible compression ratios, oversized directories.
- **Checks are stricter.** The CRC is verified even after seeks, and nothing partial is written.
- **Compression support differs.** Deflate64 and Zstandard work; LZMA (method 14) doesn't.
- **Names aren't normalized** (no `\` to `/` on Windows, no cut at NUL), and `orig_filename`
  equals `filename`.
- **Timeouts have defaults** (15 s connect, 30 s read) where `remotezip` waits forever.
- **Each member costs up to two requests**, where `remotezip` uses one; the first is small, so a
  refused member costs little.

## Testing your integration

Python's built-in `http.server` doesn't support Range requests, so use a small server of your
own. This one serves in-memory archives and is enough for `lazy_zip`:

```python
import io
import re
import threading
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def serve_zip(data):
    """Serves `data` with Range support on a free local port. Returns the server and its URL."""

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            match = re.fullmatch(r"bytes=(\d*)-(\d*)", self.headers.get("Range", ""))
            start, end = 0, len(data) - 1
            if match and match.group(1):
                start = int(match.group(1))
                end = min(int(match.group(2) or end), end)
            elif match:
                start = max(len(data) - int(match.group(2)), 0)
            self.send_response(206 if match else 200)
            if match:
                self.send_header("Content-Range", f"bytes {start}-{end}/{len(data)}")
            self.send_header("Content-Length", str(end - start + 1))
            self.end_headers()
            self.wfile.write(data[start : end + 1])

        def log_message(self, *args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    host, port = server.server_address[:2]
    return server, f"http://{host}:{port}/test.zip"


buffer = io.BytesIO()
with zipfile.ZipFile(buffer, "w") as z:
    z.writestr("hello.txt", "hello")

server, url = serve_zip(buffer.getvalue())
try:
    with lazy_zip.RemoteZip(url) as archive:
        assert archive.read("hello.txt") == b"hello"
finally:
    server.shutdown()
    server.server_close()
```

As a pytest fixture, so each test gets an archive URL:

<!-- test: requires pytest -->
```python
import pytest


@pytest.fixture
def archive_url():
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as z:
        z.writestr("hello.txt", "hello")
    server, url = serve_zip(buffer.getvalue())
    yield url
    server.shutdown()
    server.server_close()


def test_reads_hello(archive_url):
    with lazy_zip.RemoteZip(archive_url) as archive:
        assert archive.read("hello.txt") == b"hello"
```

Build the edge cases you care about with `zipfile`: corrupt data (flip a byte after writing),
names like `../evil.txt`, duplicate names, or a large member to exercise streaming. Make the
handler return 404, 403 or 200 without `Content-Range` to test your error handling. The project's
own test server, in [`tests/conftest.py`](../tests/conftest.py), also simulates slow and stalled
responses.

## Guarantees and your responsibilities

What `lazy_zip` guarantees:

- Extraction never writes outside the destination folder, whatever the names in the archive or
  symlinks already in the folder.
- No member is written beyond the size the directory claims, and data is only handed over or
  renamed into place after its size and CRC match. Failed and interrupted writes leave no partial
  files and don't touch an existing file.
- Overlapping entries, mismatched local headers, impossible compression ratios, directories over 5
  million entries or 512 MB, and encrypted or unsupported members are refused. An encrypted member
  costs only its header.
- Opening an archive downloads at most the end of the file and a 512 MB directory, whatever the
  archive claims.
- Responses are validated: a server that ignores ranges, compresses partial responses, sends the
  wrong range, or changes the file between requests causes an error rather than wrong data.
- Header values are never sent to another site on a redirect. Error messages don't include the
  URL, and `repr()` leaves out its query string. `url` still returns it in full.
- Certificates are always verified.

What remains your job:

- **Untrusted URLs (SSRF).** `lazy_zip` fetches whatever `http://` or `https://` URL it's given,
  including `localhost`, private addresses and cloud metadata endpoints, and follows redirects.
  If URLs come from users, check them against an allowlist, and remember a redirect can lead from
  an allowed host to an internal one. Network-level egress rules are the reliable defense.
- **Resource limits.** Before reading, check `file_size`, `compress_size` and the number of members
  against what you're willing to download, hold in memory (`read()` returns the whole member) or
  write to disk. There's no overall time limit on a download, and `extractall` can fill a disk.
- **Archive contents.** A matching CRC means the data is what the archive contains, not that it's
  safe. Treat extracted files as untrusted input: don't execute them, and validate them before
  parsing with tools that have their own vulnerabilities.
- **Where files go.** `extract` and `extractall` replace existing files at the destination.
  `read_to_file()` writes to exactly the path you give it, without any checks, so don't build that
  path from member names.
- **Secrets.** `url` returns the full URL, including any presigned signature; keep it out of logs.
