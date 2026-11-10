# lazy-zip for Python

Read and extract files from remote ZIP archives without downloading the whole archive. `lazy_zip`
reads the archive's table of contents with HTTP Range requests, then fetches only the files you
ask for. The API follows `zipfile.ZipFile`, like the [`remotezip`](https://pypi.org/project/remotezip/)
package, and the work happens in Rust, outside the GIL.

**Pre-1.0 and not on PyPI yet.** The API may change between versions, so pin a commit.

- [Install](#install)
- [Quick start](#quick-start)
- [How it works](#how-it-works)
- [Integration guide](docs/integration.md): web apps, threads, asyncio, retries, testing, migrating
- [API reference](#api-reference) and [errors](#errors)
- [Command line](#command-line)
- [How much data it uses](#how-much-data-it-uses)
- [Safety](#safety)
- [Compared with remotezip and zipfile](#compared-with-remotezip-and-zipfile)
- [Known limitations](#known-limitations)

## Install

You need Python 3.8 or later and a [Rust toolchain](https://rustup.rs) to build it. TLS is built
in (rustls), so no OpenSSL is needed. It builds wherever Rust and maturin do: Windows, macOS and
Linux. Wheels are specific to a Python version and platform.

Straight from GitHub, in one step (pip builds it with maturin):

```bash
pip install "lazy-zip @ git+https://github.com/alenpaulvarghese/lazy-zip@<commit>#subdirectory=python"
```

The same line works in `requirements.txt`, or as a dependency in `pyproject.toml`. To build a
wheel once and install it elsewhere, such as in a Docker image without Rust:

```bash
git clone https://github.com/alenpaulvarghese/lazy-zip.git
cd lazy-zip/python
pip install maturin
maturin build --release            # writes ../target/wheels/lazy_zip-<version>-<tags>.whl
pip install ../target/wheels/lazy_zip-*.whl
```

To work on the package itself, run `maturin develop` in this folder instead, and
`pip install pytest && pytest tests` to test it.

## Quick start

```python
import lazy_zip

with lazy_zip.RemoteZip("https://example.com/files.zip") as archive:
    for info in archive.infolist():
        print(f"{info.file_size:>10}  {info.filename}")

    text = archive.read("docs/readme.txt").decode()      # one member, into memory

    with archive.open("data/records.csv") as f:          # streamed, so large members are fine
        header = f.readline()

    archive.extract("docs/readme.txt", "out")            # writes out/docs/readme.txt
    archive.extractall("out", workers=8)                 # everything, 8 files at a time
    print(archive.network_bytes, "of", archive.content_length, "bytes downloaded")
```

Private archives and timeouts:

```python
archive = lazy_zip.RemoteZip(
    "https://example.com/files.zip",
    headers={"Authorization": "Bearer ..."},  # sent with every request
    timeout=(5, 60),                          # seconds to connect, and to wait for each read
)
archive.close()
```

## How it works

- **Creating a `RemoteZip` reads the directory**, usually in one request (two for big
  directories). Keep the object and reuse it: listing and `getinfo()` then cost nothing, and every
  read uses the directory it already has.
- **Each member costs at most two requests** and about its compressed size: the first request
  gets its header (and all of a small file), the second the rest. Nothing outside the member is
  downloaded.
- **`network_bytes`** counts everything received for this archive, so you can log or limit what
  each job costs.
- **Every read is verified** against the member's size and CRC. `read()` checks before returning;
  a stream raises on the read that reaches the end.
- **Calls block but release the GIL.** Use threads or `asyncio.to_thread` for concurrency; one
  `RemoteZip` can be shared by many threads.

The [integration guide](docs/integration.md) covers recipes in detail: serving members from
FastAPI or Flask, handling errors and retries, threads and asyncio, cancellation, passing members
to pandas, `json` or Pillow, migrating from `remotezip`, and testing against a local server.

## API reference

Every class, method and property has a docstring, so `help(lazy_zip.RemoteZip)` shows the
details, and the package ships type hints.

`RemoteZip(url, headers=None, timeout=None)` reads the archive's directory.

- `headers`: a mapping or `(name, value)` pairs sent with every request. `Range` and
  `Accept-Encoding` can't be set. See [private archives](docs/integration.md#private-archives).
- `timeout`: seconds, or a `(connect, read)` tuple like `requests`. The default is 15 s to connect
  and 30 s for each wait for data; there's no limit on a whole download. Values must be positive
  and finite.

| Member | Returns | Description |
|--------|---------|-------------|
| `namelist()` | `list[str]` | Member names, in archive order |
| `infolist()` / `filelist` | `list[ZipInfo]` | Member details |
| `getinfo(name)` | `ZipInfo` | Details of one member (the last one, if the name repeats) |
| `open(name, mode="r")` | `ZipExtFile` | A read-only, seekable binary file object, downloaded as you read |
| `read(name)` | `bytes` | A member's data, verified |
| `extract(member, path=None)` | `str` | Writes a member under `path` (default: current folder) |
| `extractall(path=None, members=None, workers=8)` | `None` | Writes members concurrently; raises `ExtractError` if any failed |
| `read_to_file(name, filepath)` | `None` | Writes a member to exactly `filepath` |
| `testzip()` | `str \| None` | Reads every member and returns the first corrupt one |
| `printdir(file=None)` | `None` | Prints names, dates and sizes, like `zipfile` |
| `url` | `str` | The URL as given |
| `content_length` | `int` | The archive's size in bytes |
| `network_bytes` | `int` | Bytes received for this archive so far |
| `files` / `load()` | `list[FileEntry]` | The archive as a tree; folders have `children` |
| `close()` | `None` | Releases the archive; files already opened stay readable |

Members can be given by name or as a `ZipInfo`. `RemoteZip` works as a context manager.

- **`ZipInfo`**: `filename`, `orig_filename`, `date_time` (local time as stored), `file_size`,
  `compress_size`, `CRC`, `compress_type` (0 stored, 8 deflate, 9 deflate64, 12 bzip2, 14 LZMA,
  93 zstd, 95 xz), `external_attr`, `header_offset` and `is_dir()`.
- **`ZipExtFile`**: an `io.BufferedReader` with `name`. `read`, `read1`, `readinto`, `readline`,
  iteration, `peek`, `seek` and `tell` all work. Wrap it in `io.TextIOWrapper` for text.
- **`FileEntry`**: `name`, `path`, `size`, `compressed_size`, `is_dir`, `children` and
  `parent_path`.

Names are decoded like `zipfile` does: as UTF-8 when the archive says so (or they're plain
ASCII), otherwise as code page 437. When names repeat, a name means the last entry, like
`zipfile`, and a `ZipInfo` means its own entry, in `extractall` too.

## Errors

Exceptions have a `kind` attribute that says precisely what went wrong. The exception types are
the ones `zipfile` and the standard library use, so existing handlers keep working.

| `kind` | Exception | Meaning | What to do |
|--------|-----------|---------|------------|
| `"network"` | `ConnectionError` | Connection failed or timed out, or the response was unusable | Retry later; requests were already retried |
| `"http_status"` | `ConnectionError` | The server answered with an error status (in the message) | Check the URL or credentials |
| `"range_not_supported"` | `ConnectionError` | The server can't send parts of the file | Download the whole file another way |
| `"remote_changed"` | `ConnectionError` | The archive changed on the server | Open a new `RemoteZip` |
| `"not_a_zip"` | `BadZipFile` | Not a ZIP archive, often an HTML error page | Check the URL |
| `"limit_exceeded"` | `BadZipFile` | The directory is too large to open safely | Give up |
| `"corrupt"` | `BadZipFile` | Data doesn't match its size or CRC | Retry once, then give up |
| `"suspicious"` | `BadZipFile` | Crafted to cause harm, such as overlapping data | Reject the archive |
| `"unsafe"` | `BadZipFile` | A name that can't be written safely, such as `../evil.txt` | Skip or reject it |
| `"unsupported"` | `NotImplementedError` | Encrypted, or an unsupported compression method | Give up |
| `"not_found"` | `KeyError` | No member has that name | Check the name |
| `"invalid_header"` | `ValueError` | A header in `headers` is invalid | Fix the header |
| `"io"` | `OSError` | A local file couldn't be written | Check the disk and permissions |
| `"cancelled"` | `OSError` | The work was cancelled | Normally you see `KeyboardInterrupt` instead |

`lazy_zip.BadZipFile` is a subclass of `zipfile.BadZipFile`, and `NotImplementedError` of
`RuntimeError`, which `zipfile` raises for encrypted files. `extractall` attempts every member,
then raises `lazy_zip.ExtractError` if any failed; its `failures` maps each failed name to one of
the exceptions above. Using a closed archive or an invalid `timeout` raises `ValueError` without a
`kind`.

```python
try:
    with lazy_zip.RemoteZip("https://example.com/files.zip") as archive:
        archive.extractall("out")
except lazy_zip.ExtractError as error:
    for name, failure in error.failures.items():
        print(f"{name}: {failure.kind}: {failure}")
except ConnectionError as error:
    print("Couldn't download the archive:", error.kind, error)
```

## Command line

```bash
python -m lazy_zip https://example.com/files.zip -l                      # list
python -m lazy_zip https://example.com/files.zip                         # extract everything here
python -m lazy_zip https://example.com/files.zip -d out a.txt docs/b.md  # extract some files
python -m lazy_zip https://example.com/files.zip -H "Authorization: Bearer ..." -t 30 -j 4
```

Installing the package also adds a `lazy-zip` command that does the same.

| Option | Description |
|--------|-------------|
| `URL` | The archive's URL |
| `FILE ...` | Members to extract, or to list with `-l` (default: all) |
| `-l`, `--list` | List sizes and names instead of extracting |
| `-d DIR`, `--dir DIR` | Extract into this folder (default: the current one) |
| `-H 'NAME: VALUE'`, `--header` | Send an HTTP header with every request; can be repeated |
| `-j N`, `--workers N` | Files downloaded at once (default: 8) |
| `-t SECONDS`, `--timeout SECONDS` | Connect and read timeout |

Extraction prints how much was downloaded to stderr. The exit status is 0 on success, 1 on an
error, 2 for invalid arguments and 130 after Ctrl-C.

## How much data it uses

- **Opening** fetches the last 66 KB or so of the file (or all of a smaller file), plus one more
  request if the directory is bigger than that, roughly 50-100 bytes per entry.
- **Reading a member** costs about its compressed size, plus a header of about 30 bytes and its
  name, in at most two requests. Parts already fetched with the directory aren't fetched again.
- **Seeking** in an opened member costs what it skips: forward seeks download and discard,
  backward seeks read again from the member's start. Small backward seeks within the 64 KiB
  buffer are free.
- **`testzip()`** reads every member, so it costs about the whole archive.
- **Dropped connections** are resumed from where they stopped, so a retry costs only what's left.

`network_bytes` gives the real figure for each archive.

## Safety

`lazy_zip` is built to open archives you don't trust.

- `extract` and `extractall` refuse names that could escape the destination or misbehave on some
  system: `..`, absolute paths and drive letters, backslashes, Windows reserved names, and
  characters like `:` and `?`. The same rules apply on every platform. `zipfile` rewrites such
  names instead.
- Writes stay inside the destination even if a folder in it is a symlink pointing elsewhere.
- A member is never written past the size the directory claims, and impossible compression ratios
  are refused, which stops zip bombs. Entries whose data overlaps another, or whose local header
  names a different file, are refused.
- The size and CRC are checked on every read, even after seeks. Files are written to a temporary
  file and renamed into place only once verified, so an existing file is never replaced by bad or
  partial data.
- Directories over 5 million entries or 512 MB are refused, and reading the directory can't be
  made to download more than that.
- Header values are treated as secrets. On a redirect to another site, `Authorization` and cookies
  are dropped, and any other custom header makes the redirect fail, so no header reaches another
  site. Error messages don't include the URL, and `repr()` leaves its query string out.

See [guarantees and your responsibilities](docs/integration.md#guarantees-and-your-responsibilities)
for what's left to you, such as validating untrusted URLs.

## Compared with remotezip and zipfile

| | `lazy_zip` | `remotezip` | `zipfile` (local file) |
|---|---|---|---|
| Listing and reading API | `zipfile`'s read API | All of `zipfile` (subclass) | Full |
| Requests to read the directory | 1-2 | 1-2 | n/a |
| Requests per member | 1-2, only its bytes | 1, only its bytes | n/a |
| Interrupted downloads | Retried and resumed | Fail | n/a |
| Default timeout | 15 s connect, 30 s read | None (can wait forever) | n/a |
| Parallel `extractall` | Yes (`workers=8`) | No | No |
| Unsafe names | Refused | Rewritten | Rewritten |
| Zip bombs, overlapping entries | Refused | As `zipfile` | Some overlaps refused (recent versions) |
| CRC after a seek | Checked | As `zipfile` | Not always checked |
| Partial files on failure | Never | Possible | Possible |
| Ctrl-C | Stops within 0.1 s, removes partial files | Python's default | Python's default |
| Errors | Standard types plus `.kind` | `RemoteIOError` and friends | Standard types |
| Passwords / encryption | No (`NotImplementedError`) | Yes | Yes |
| Archive and member comments, extra fields | No | Yes | Yes |
| Name normalization (`\` to `/`, NUL) | No | Yes | Yes |
| Compression | Stored, deflate, deflate64, bzip2, zstd, xz | As `zipfile` | Stored, deflate, bzip2, LZMA (zstd on 3.14+) |
| Separate timeout exception | No: `ConnectionError`, `kind="network"` | `RemoteIOError` | n/a |
| `requests` options (`session`, `auth`, `proxies`) | No: use `headers`; proxies from `HTTPS_PROXY` | Yes | n/a |
| Writing archives | No | No | Yes |

The [migration guide](docs/integration.md#migrating-from-remotezip-or-zipfile) maps each call.

## Known limitations

- Read-only. No passwords (encrypted members raise `NotImplementedError`), archive or member
  comments, or extra fields. `ZipInfo` lacks `zipfile`'s `create_system`, `flag_bits`, `extra`
  and similar header details, and `RemoteZip` isn't a `zipfile.ZipFile`, so `zipfile.Path` can't
  wrap it.
- LZMA members (method 14) are listed but can't be read. An entry with an unknown method, such as
  AES encryption or PPMd, stops the whole archive from opening.
- Names aren't normalized the way `zipfile` does it: a backslash isn't turned into `/` on Windows
  and a name isn't cut at a NUL byte. `orig_filename` is always the same as `filename`.
- A name that also has an Info-ZIP Unicode path extra field is shown in Unicode, where `zipfile`
  shows the legacy cp437 name. A name flagged as UTF-8 that isn't valid UTF-8 is decoded as cp437
  instead of failing.
- `timeout` can't be `None` inside the tuple, there's no overall time limit for a download, and a
  timeout raises `ConnectionError` (`kind="network"`) rather than a dedicated exception.
- An HTTP error's status code is only in the message, not an attribute.
- The directory is read once, so a file replaced on the server after that fails with
  `kind="remote_changed"` until you create a new `RemoteZip`.
- There's no way to cancel a call running on a thread other than the main one. Stream with
  `open()` and stop reading instead.
