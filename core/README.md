# lazy-zip-core

A Rust library for browsing a ZIP archive over HTTP and downloading only the entries you pick.

It reads the archive's directory with HTTP Range requests, usually in two requests, then reads
each entry with requests for only that entry's bytes. Data is decompressed and checked against
its size and CRC as it arrives, and archives and servers are treated as hostile. It powers the
[Lazy ZIP](../README.md) desktop app and the [`lazy_zip` Python package](../python/README.md).

> **Pre-1.0.** The API may change in any release. Pin a commit (see below).

- [Adding the dependency](#adding-the-dependency)
- [Concepts](#concepts)
- [What it costs](#what-it-costs)
- [Recipes](#recipes)
- [Limits](#limits)
- [Security](#security)
- [More documentation](#more-documentation)

## Adding the dependency

The crate isn't on crates.io. Add it from Git, by its package name `lazy-zip-core`:

```toml
[dependencies]
lazy-zip-core = { git = "https://github.com/alenpaulvarghese/lazy-zip", rev = "<commit>" }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

Cargo finds the package inside the repository's workspace. Pin a `rev`, since the API changes
between commits (the `v0.2.0` tag predates the API described here). For a local checkout, use
`{ path = "../lazy-zip/core" }` instead. Import it as `lazy_zip_core`.

- **Rust:** edition 2021. No minimum version is declared; it's built and tested with current
  stable.
- **Tokio:** everything is async and needs a Tokio runtime with its I/O and time drivers enabled:
  `#[tokio::main]`, or `tokio::runtime::Builder` with `.enable_all()`. Multi-threaded and
  current-thread runtimes both work. The recipes below also use `futures`, `bytes` and
  `tokio-util`; add them if you copy those.
- **Features:**
  - `rustls` (default): HTTPS through `reqwest`'s `rustls` feature, which uses `aws-lc-rs` and
    the operating system's certificate store.
  - With `default-features = false` there is no TLS, and only `http://` URLs work. To use another
    TLS backend, enable it on `reqwest` 0.13 in your own `Cargo.toml`, such as
    `reqwest = { version = "0.13", default-features = false, features = ["native-tls"] }`. Cargo
    combines the features. `reqwest`'s other default features (HTTP/2, system proxy settings,
    charset) aren't enabled by this crate; enable them the same way if you need them.

## Concepts

| Type | What it is |
|------|------------|
| `archive::RemoteArchive` | An archive whose directory has been read. Open it once per URL and keep it: it's `Send + Sync`, so share it in an `Arc`. It keeps the end of the file it fetched, and reads entries without reading the directory again. |
| `archive::ArchiveEntry` | One directory record: `name()`, `raw_name()`, `lossy_name()`, `size`, `compressed_size`, `crc`, `compression`, `header_offset`, `is_dir`, `date_time`, `external_attributes`. These are the archive's claims; reading the entry checks them. An entry is identified by its index in `RemoteArchive::entries()`. |
| `archive::EntryReader` | One entry's data, decompressed and verified. Call `read(&mut buf)` until it returns 0. It owns its connection and doesn't borrow the archive, so it can move to another task. |
| `http_reader::ReaderConfig` | How to connect: the `reqwest` client (user agent, timeouts, custom headers, redirect rules), the `RetryPolicy`, and the `network_bytes` counter. Clones share the connection pool and the counter. |
| `zip_explorer::scan` → `ScanResult` | The archive as a tree of folders and files, without the entries that can't be written safely. Makes no requests. |
| `downloader` | `plan_selection` maps names to safe destination paths; `download_from` and `download_batch` download them concurrently, writing each through a temporary file. |
| `http_reader::RemoteFile`, `RemoteHttpReader` | Low-level: the fetched end of a remote file, and an `AsyncRead + AsyncSeek` reader over it. |
| `Error`, `ErrorKind` | Every failure. `Error`'s message is for people; `Error::kind()` is for code. |

## What it costs

| Operation | Requests | Bytes received |
|-----------|----------|----------------|
| `RemoteArchive::open` | 1 for the end of the file, plus 1 for the directory if it isn't inside that | The last `archive_limits::TAIL_BYTES` (about 66 KiB, or the whole file if it's smaller), plus the directory |
| `zip_explorer::scan`, `entries()`, `find()` | 0 | 0 |
| `open_entry`, read to the end | 1 if the entry fits in its first 64 KiB, otherwise 2; 0 if it's inside the fetched end | The local header (about 30 bytes plus the name) and the compressed data |
| `downloader::download_from` | `open_entry` for each file | `open_entry` for each file |
| `downloader::download_batch` | `open`, then `download_from` | `open`, then `download_from` |

- A server without suffix ranges (`bytes=-N`, which GitHub release assets refuse) costs two more
  requests to open.
- A dropped connection is resumed with a new request from the exact byte where it stopped.
- An entry refused after its local header is checked costs at most 64 KiB.
- No request is a `HEAD`, so URLs signed only for `GET` work, and nothing is fetched past the end
  of the entry being read.
- Directory size is roughly 50 to 100 bytes per entry, so listing a 5,000-entry archive costs
  well under 1 MiB however large the archive is.

## Recipes

### List an archive

`zip_explorer::scan` returns the folders and files as a flat list, each parent before its
children, with counts and sizes rolled up. `ScanResult` and its nodes are `serde::Serialize`, for
sending to a frontend, and `ScanResult::tree()` gives a nested view.

```rust no_run
use lazy_zip_core::archive::RemoteArchive;
use lazy_zip_core::http_reader::ReaderConfig;
use lazy_zip_core::zip_explorer;

#[tokio::main]
async fn main() -> lazy_zip_core::Result<()> {
    let archive = RemoteArchive::open("https://example.com/data.zip", ReaderConfig::new()?).await?;
    let listing = zip_explorer::scan(&archive);

    for node in &listing.nodes {
        let depth = std::iter::successors(node.parent, |&p| listing.nodes[p as usize].parent).count();
        let kind = if node.is_dir { "/" } else { "" };
        println!("{}{}{kind}  {} bytes", "  ".repeat(depth), node.name, node.total_size);
    }

    // Entries with names like `../evil` or `CON`, or that aren't valid UTF-8, are left out.
    if listing.rejected_count > 0 {
        println!("{} entries can't be written safely", listing.rejected_count);
        for rejected in &listing.rejected {
            println!("  {}: {}", rejected.name, rejected.reason);
        }
    }
    println!("{} of {} bytes used", listing.fetched_size, listing.total_size);
    Ok(())
}
```

For every directory record, including the unsafe ones, use `archive.entries()` instead.

### Read an entry into memory

```rust no_run
use lazy_zip_core::archive::RemoteArchive;
use lazy_zip_core::{Error, Result};

/// Reads the entry named `name`, refusing anything over `max_bytes`.
async fn read_entry(archive: &RemoteArchive, name: &str, max_bytes: u64) -> Result<Vec<u8>> {
    let index = archive.find(name).ok_or_else(|| Error::FileNotFound(name.to_string()))?;
    let entry = &archive.entries()[index];
    if entry.size > max_bytes {
        return Err(Error::LimitExceeded("the file is larger than allowed"));
    }

    // The reader stops at `entry.size`, so the claim is safe to allocate from once it's capped.
    let mut data = Vec::with_capacity(entry.size as usize);
    let mut reader = archive.open_entry(index, archive.network_counter()).await?;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buffer).await?;
        if n == 0 {
            // Only now have the size and CRC been checked.
            return Ok(data);
        }
        data.extend_from_slice(&buffer[..n]);
    }
}
```

`find` returns the last entry with that name, as the listing and `zipfile` do.

### Stream an entry into a file

The downloader does all of this for you (see the next recipe). Written by hand, it looks like this:
the name is turned into a safe path, the data goes to a temporary file, and the file is renamed
into place only after the last read has verified it.

```rust no_run
use lazy_zip_core::archive::RemoteArchive;
use lazy_zip_core::downloader::mark_as_downloaded;
use lazy_zip_core::safe_path::{destination, ensure_within};
use lazy_zip_core::{Error, Result};
use std::path::Path;
use tokio::io::AsyncWriteExt;

/// Writes the entry at `index` under `root`, keeping its folders.
async fn extract(archive: &RemoteArchive, index: usize, root: &Path) -> Result<()> {
    let entry = &archive.entries()[index];
    let name = entry.name().ok_or_else(|| Error::UnsafeEntry {
        entry: entry.lossy_name(),
        reason: "name is not valid UTF-8",
    })?;
    // Refuses `../`, absolute paths, reserved names and the like.
    let dest = destination(root, "", name)?;
    let parent = dest.parent().expect("a destination is always inside root");
    tokio::fs::create_dir_all(parent).await?;
    // Refuses a folder that's a symlink leading out of `root`.
    ensure_within(root, parent)?;

    let mut temp = dest.clone().into_os_string();
    temp.push(".part");
    let mut reader = archive.open_entry(index, archive.network_counter()).await?;
    let written = async {
        let mut file = tokio::fs::File::create(&temp).await?;
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            file.write_all(&buffer[..n]).await?;
        }
        file.flush().await?;
        Ok::<_, Error>(())
    }
    .await;
    if let Err(error) = written {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(error);
    }
    tokio::fs::rename(&temp, &dest).await?;
    // On Windows, records that the file came from the internet.
    mark_as_downloaded(&dest, archive.url());
    Ok(())
}
```

### Stream an entry as an HTTP response

`EntryReader` isn't `AsyncRead`; adapt it to a stream of `Bytes`. The stream is `Send + 'static`,
so a web framework can take it as a response body, such as axum's `Body::from_stream`.

```rust no_run
use bytes::Bytes;
use futures::stream::{self, Stream};
use lazy_zip_core::archive::EntryReader;

fn entry_stream(reader: EntryReader) -> impl Stream<Item = lazy_zip_core::Result<Bytes>> + Send + 'static {
    stream::try_unfold(reader, |mut reader| async move {
        let mut buffer = vec![0u8; 64 * 1024];
        let n = reader.read(&mut buffer).await?;
        if n == 0 {
            return Ok(None);
        }
        buffer.truncate(n);
        Ok(Some((Bytes::from(buffer), reader)))
    })
}
```

The client receives data before it's verified. If the size or CRC turns out wrong, the stream
ends with an error and the response is cut short. Send `Content-Length: entry.size` so clients
notice.

### Download a selection

`plan_selection` maps archive names to paths under a folder, relative to a folder in the archive,
and rejects names that are unsafe or that would collide on a case-insensitive file system.
`download_from` downloads the plan from the open archive, several files at once, and carries on
past individual failures. Each file is written to a temporary file and renamed into place once
verified, so a failure never damages an existing file or leaves a partial one.

```rust no_run
use lazy_zip_core::archive::RemoteArchive;
use lazy_zip_core::downloader::{download_from, find_existing, plan_selection, BatchEvent, BatchOptions};
use lazy_zip_core::http_reader::ReaderConfig;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Downloads every file under `folder/` in the archive into `out`, without the `folder/` prefix.
async fn download_folder(
    archive: &RemoteArchive,
    config: &ReaderConfig,
    folder: &str,
    out: &Path,
) -> lazy_zip_core::Result<()> {
    let prefix = format!("{folder}/");
    let names: BTreeSet<String> = archive
        .entries()
        .iter()
        .filter(|entry| !entry.is_dir)
        .filter_map(|entry| entry.name())
        .filter(|name| name.starts_with(&prefix))
        .map(str::to_string)
        .collect();

    let plan = plan_selection(out, folder, names.into_iter().collect());
    for rejected in &plan.rejected {
        eprintln!("skipping {}: {}", rejected.path, rejected.error);
    }
    let existing = find_existing(&plan.files).await;
    if !existing.is_empty() {
        eprintln!("{} files will be replaced", existing.len());
    }

    // `download_from` uses only the config's counter, so this counts this batch alone.
    let mut reader = config.clone();
    reader.network_bytes = Arc::default();
    let options = BatchOptions {
        concurrency: 8,
        cancel: CancellationToken::new(),
        root: Some(out.to_path_buf()),
        reader,
    };
    let written = AtomicU64::new(0);
    let report = download_from(archive, plan.files, options, |event| match event {
        BatchEvent::FileBytes { bytes, .. } => {
            written.fetch_add(bytes, Ordering::Relaxed);
        }
        BatchEvent::FileFailed { path, error, kind } => eprintln!("{path}: {error} ({kind:?})"),
        _ => {}
    })
    .await;

    println!(
        "{} downloaded, {} failed, {} bytes written, {} bytes received",
        report.completed, report.failed, report.written_bytes, report.network_bytes
    );
    Ok(())
}
```

- `plan_selection(out, "", names)` keeps the archive's full paths.
- `download_batch(url, files, options, on_event)` opens the archive itself first, using
  `options.reader`. Use it when nothing is open yet; otherwise `download_from` saves reading the
  directory again.
- `BatchOptions::root` makes each destination folder prove it resolves inside that folder, so a
  symlink can't redirect a write. Set it whenever the paths came from `plan_selection`.
- `on_event` is called from the download futures, so keep it quick.

### Run downloads in the background and cancel them

The futures returned by `download_from` and `download_batch` are `Send`, so they can be spawned.
This is how a GUI keeps working while files download: put the archive in an `Arc`, spawn the
batch, forward events to the UI thread, and keep the `CancellationToken` to stop it. Cancelling
removes partial files, and cancelled files get no `FileFailed` event.

```rust no_run
use lazy_zip_core::archive::RemoteArchive;
use lazy_zip_core::downloader::{download_from, BatchEvent, BatchFile, BatchOptions, BatchReport};
use lazy_zip_core::http_reader::ReaderConfig;
use lazy_zip_core::ErrorKind;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Progress the UI hears about, owned so it can cross threads.
enum Progress {
    Bytes(u64),
    Done(String),
    Failed { path: String, error: String, kind: ErrorKind },
}

/// Starts a download on the current Tokio runtime.
fn start_job(
    archive: Arc<RemoteArchive>,
    files: Vec<BatchFile>,
    config: ReaderConfig,
    events: Sender<Progress>,
) -> (CancellationToken, JoinHandle<BatchReport>) {
    let cancel = CancellationToken::new();
    let options = BatchOptions { concurrency: 4, cancel: cancel.clone(), root: None, reader: config };
    let job = tokio::spawn(async move {
        download_from(&archive, files, options, move |event| {
            let message = match event {
                BatchEvent::FileStarted { .. } => return,
                BatchEvent::FileBytes { bytes, .. } => Progress::Bytes(bytes),
                BatchEvent::FileFinished { path } => Progress::Done(path.to_string()),
                BatchEvent::FileFailed { path, error, kind } => {
                    Progress::Failed { path: path.to_string(), error, kind }
                }
            };
            let _ = events.send(message);
        })
        .await
    });
    (cancel, job)
}
```

`tokio::spawn` must be called inside the runtime; from another thread, use
`runtime.handle().spawn(...)`. The desktop app keeps the last scanned `RemoteArchive` keyed by URL
and headers, and downloads from it with `download_from`, so a download right after a scan reads no
directory at all.

### Pick one of several entries with the same name

An archive can hold several entries with the same name. The listing, `find`, and a `BatchFile`
without an `index` all use the last one. To reach the others, find them by index and set
`BatchFile::index`:

```rust no_run
use lazy_zip_core::archive::RemoteArchive;
use lazy_zip_core::downloader::BatchFile;
use std::path::Path;

/// Every copy of `name`, each written to its own file.
fn every_copy(archive: &RemoteArchive, name: &str, out: &Path) -> Vec<BatchFile> {
    archive
        .entries()
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.name() == Some(name))
        .map(|(index, _)| BatchFile {
            zip_path: name.to_string(),
            dest: out.join(format!("copy-{index}")),
            index: Some(index),
        })
        .collect()
}
```

Names that aren't valid UTF-8 are left out of the listing, but `find_raw(bytes)` and
`ArchiveEntry::raw_name()` reach them for reading. `dest` is used as given, so only build it
yourself from names you trust.

### Private archives

A presigned URL (S3, Google Cloud Storage, Azure SAS) carries its own authorization, so it needs
nothing extra. For a token or API key, add headers to the config; they're sent with every request.

```rust no_run
use lazy_zip_core::archive::RemoteArchive;
use lazy_zip_core::http_reader::ReaderConfig;
use std::time::Duration;

async fn open_private() -> lazy_zip_core::Result<()> {
    let presigned = "https://bucket.s3.amazonaws.com/data.zip?X-Amz-Signature=...";
    let _s3 = RemoteArchive::open(presigned, ReaderConfig::new()?).await?;

    // Authorization, Cookie and Proxy-Authorization are dropped if the server redirects to
    // another site, as browsers do, so a redirect to a CDN still works.
    let config = ReaderConfig::with_headers([("Authorization", "Bearer my-token")])?;
    let _private = RemoteArchive::open("https://example.com/private.zip", config).await?;

    // Any other header makes a redirect to another site fail instead of handing it over.
    let config = ReaderConfig::with_options(
        Duration::from_secs(5),
        Duration::from_secs(60),
        [("X-API-Key", "secret")],
    )?;
    let _keyed = RemoteArchive::open("https://api.example.com/files/data.zip", config).await?;
    Ok(())
}
```

- `Range` and `Accept-Encoding` can't be set; the client sets them. An invalid header fails with
  `ErrorKind::InvalidHeader`, naming the header but never its value.
- Header values are marked sensitive, and error messages never include the URL. `archive.url()`
  still returns it, so keep presigned URLs out of your own logs.
- The archive keeps the config it was opened with; to change headers, open it again.

### Count network use

Every byte received is counted in an `Arc<AtomicU64>`:

- `ReaderConfig::network_bytes` counts everything done with that config and its clones.
  `archive.network_counter()` returns the counter the archive was opened with.
- `RemoteArchive::fetched_size()` (and `ScanResult::fetched_size`) is what opening cost.
- `open_entry(index, counter)` counts one entry in a counter of your choice.
- `BatchReport::network_bytes` is the batch's `options.reader.network_bytes` at the end. Give each
  batch a fresh counter (`config.network_bytes = Arc::default()` on a clone) to measure it alone.

```rust no_run
use lazy_zip_core::archive::RemoteArchive;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Reads an entry, returning how many bytes it took.
async fn entry_cost(archive: &RemoteArchive, index: usize) -> lazy_zip_core::Result<u64> {
    let counter = Arc::new(AtomicU64::new(0));
    let mut reader = archive.open_entry(index, counter.clone()).await?;
    let mut buffer = vec![0u8; 64 * 1024];
    while reader.read(&mut buffer).await? != 0 {}
    Ok(counter.load(Ordering::Relaxed))
}
```

### Handle errors

Transient network failures are already retried inside the crate, following
`ReaderConfig::retry` (4 attempts with backoff by default, honoring `Retry-After`), and a dropped
download resumes where it stopped. An error that reaches you has survived that.

| `ErrorKind` | Meaning | What to do |
|-------------|---------|------------|
| `Network` | Couldn't connect, timed out, the connection kept breaking, or the server's response broke the rules | Try again later |
| `HttpStatus` | An error status, such as 403 or 404 (408, 429 and 5xx were retried) | Check the URL, login or expiry of a presigned URL |
| `RangeNotSupported` | The server sends whole files only | The archive can't be read remotely |
| `RemoteChanged` | The file's `ETag`, `Last-Modified` or length changed since it was opened | Open the archive again; old indexes are invalid |
| `NotAZip` | Not a ZIP archive, often an HTML page | Check the URL |
| `LimitExceeded` | Too many entries, or too large a directory | Give up |
| `Corrupt` | The data doesn't match its size or CRC, or can't be parsed | Retry once; if it persists, the archive is damaged |
| `Suspicious` | The entry looks crafted to cause harm, such as overlapping another | Don't retry; treat the archive as hostile |
| `Unsafe` | The name can't be written safely | Skip it |
| `Unsupported` | Encrypted, or LZMA | Can't be read |
| `NotFound` | No entry has that name or index | |
| `InvalidHeader` | A custom header is invalid | Fix the config |
| `Io` | A local file couldn't be read or written | Check the disk and permissions |
| `Cancelled` | The `CancellationToken` was cancelled | |

`ErrorKind` serializes in camelCase (`"remoteChanged"`), for frontends. Reopening after a change:

```rust no_run
use lazy_zip_core::archive::RemoteArchive;
use lazy_zip_core::http_reader::ReaderConfig;
use lazy_zip_core::{Error, ErrorKind, Result};

async fn read_all(archive: &RemoteArchive, name: &str) -> Result<Vec<u8>> {
    let index = archive.find(name).ok_or_else(|| Error::FileNotFound(name.to_string()))?;
    let mut reader = archive.open_entry(index, archive.network_counter()).await?;
    let (mut data, mut buffer) = (Vec::new(), vec![0u8; 64 * 1024]);
    loop {
        match reader.read(&mut buffer).await? {
            0 => return Ok(data),
            n => data.extend_from_slice(&buffer[..n]),
        }
    }
}

/// Reads `name`, reopening the archive once if it changed on the server.
async fn read_fresh(archive: &mut RemoteArchive, config: &ReaderConfig, name: &str) -> Result<Vec<u8>> {
    match read_all(archive, name).await {
        Err(error) if error.kind() == ErrorKind::RemoteChanged => {
            let url = archive.url().to_string();
            *archive = RemoteArchive::open(&url, config.clone()).await?;
            read_all(archive, name).await
        }
        other => other,
    }
}
```

A server that sends neither `ETag` nor `Last-Modified` can't reveal a change that keeps the
length the same; that shows up as `Corrupt` or `Suspicious` instead.

### Use it from synchronous code

Build one runtime, keep it, and `block_on` each call. Don't call `block_on` from inside async
code.

```rust no_run
use lazy_zip_core::archive::RemoteArchive;
use lazy_zip_core::http_reader::ReaderConfig;
use lazy_zip_core::zip_explorer;

fn main() -> lazy_zip_core::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let url = "https://example.com/data.zip";
    let archive = runtime.block_on(RemoteArchive::open(url, ReaderConfig::new()?))?;
    println!("{} nodes", zip_explorer::scan(&archive).nodes.len());
    Ok(())
}
```

The Python package works this way: each `RemoteZip` holds one `RemoteArchive`, and every call
blocks on a shared runtime.

### Read raw byte ranges

`RemoteHttpReader` reads any part of the file. Requests only go as far as `limit_to` allows, and
reads inside the fetched end of the file are free. `RemoteFile` is the first step of opening an
archive, so it can be checked before its directory is read with `RemoteArchive::from_reader`.

```rust no_run
use lazy_zip_core::archive::RemoteArchive;
use lazy_zip_core::http_reader::{ReaderConfig, RemoteFile};
use std::io::SeekFrom;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

async fn inspect(url: &str) -> lazy_zip_core::Result<()> {
    // One request: the end of the file, and its length.
    let file = RemoteFile::open(url, ReaderConfig::new()?).await?;
    println!("{} bytes, directory at {}", file.len(), file.directory_offset());

    // Parsing the directory needs a reader nothing has been read from. It lifts the file's
    // fetch limit once it's done.
    let archive = RemoteArchive::from_reader(file.reader()).await?;
    println!("{} entries", archive.entries().len());

    // The first 4 bytes, in one request for exactly those bytes.
    let mut reader = file.reader();
    reader.limit_to(4);
    reader.seek(SeekFrom::Start(0)).await?;
    let mut signature = Vec::new();
    reader.read_to_end(&mut signature).await?;
    println!("starts with {signature:?}");
    Ok(())
}
```

Until the directory has been parsed, the file's fetch limit bounds what any of its readers may
fetch. Lift it with `reader.fetch_limit().lift()` only for a file whose directory you won't parse,
since it's what keeps a hostile directory from fetching without limit.

## Limits

- At most `archive_limits::MAX_ENTRIES` (5,000,000) entries and a directory of at most
  `archive_limits::MAX_DIRECTORY_BYTES` (512 MiB); larger archives fail to open with
  `ErrorKind::LimitExceeded`.
- Stored, Deflate, Deflate64, BZip2, Zstandard and XZ can be read. LZMA entries are listed but fail
  with `ErrorKind::Unsupported`. An entry with any other method, such as PPMd or AES encryption,
  stops the whole archive from opening.
- Encrypted entries fail with `ErrorKind::Unsupported` after reading only their local header.
- An entry of 1 MiB or more claiming a compression ratio over 1100:1 is refused as `Suspicious`.
- Entry names are limited to 4096 bytes and 256 components to be written (`safe_path`).
- Read-only: archives can't be created or changed.
- The server must support Range requests and must not compress its responses.

## Security

What the crate guarantees:

- **Directory:** the end records are checked before anything is allocated from them, the records
  checked are the ones parsed, and parsing can fetch only a bounded amount.
- **Entries:** as in Python's `zipfile`, an entry is refused if its local header names a different
  file, if its data overlaps another entry or the directory, or if it shares a local header with
  another entry (overlapping zip bombs, CVE-2024-0450).
- **Data:** an entry never yields more than its declared size, and the last read checks the size
  and CRC-32.
- **Server:** every response must be the exact range asked for, uncompressed, no longer than asked
  for, and of the same file; bodies that run long are refused without being buffered. Requests
  time out, and errors don't include the URL.
- **Downloader:** names are validated by `safe_path`, names that collide by case are refused,
  destination folders must resolve inside `root`, files are written through a temporary file and
  renamed into place, and Windows files get a Mark-of-the-Web.

What remains yours:

- **The URL.** The crate fetches whatever URL it's given, `http` or `https`, and follows up to 10
  redirects. If URLs come from users of a server-side service, that's server-side request
  forgery: validate the scheme and host, and refuse loopback, private and link-local addresses.
  Since a redirect can lead anywhere, enforce that in the client too, by replacing
  `ReaderConfig::client` with one that has your own redirect policy and DNS resolver (it then
  replaces the crate's timeouts and header redirect rule as well).
- **Paths.** `open_entry` and `EntryReader` never touch the disk, and entry names are untrusted.
  Before writing, use `safe_path::destination` and `safe_path::ensure_within`, or let
  `plan_selection` and the downloader do it. A `BatchFile::dest` you build is used as given.
- **Unverified data.** Bytes are returned before the final size and CRC check. Don't act on them
  until `read` returns 0.
- **Resources.** Declared sizes are enforced as maximums, but a legitimately huge entry is still
  huge: check `ArchiveEntry::size` against memory and disk before reading. There's no overall
  timeout on a download; wrap it in `tokio::time::timeout` if you need one.
- **Overwrites.** Downloads replace existing files. Use `find_existing` to ask first.
- **Secrets in your logs.** `archive.url()` and your own logging can still leak a presigned URL or
  a header.

## More documentation

- API reference: `cargo doc -p lazy-zip-core --open` in a checkout of the repository.
- How it works inside: [docs/architecture.md](../docs/architecture.md).
- License: [MIT](../LICENSE).
