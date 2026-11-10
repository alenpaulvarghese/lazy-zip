//! Browse a ZIP archive over HTTP and download only the entries you pick.
//!
//! `lazy-zip-core` reads a remote archive with HTTP Range requests. It fetches the end of the file
//! and the archive's directory once, then reads each chosen entry with requests for only that
//! entry's bytes. It's the library behind the Lazy ZIP desktop app and the `lazy_zip` Python
//! package.
//!
//! The server must answer Range requests with `206 Partial Content`.
//!
//! The crate's `README.md` is the integration guide, with recipes for streaming, background
//! downloads, private archives, error handling and synchronous use. This page is the API
//! reference.
//!
//! # Adding the dependency
//!
//! The crate isn't on crates.io. Depend on it from Git, pinned to a commit, since the API may
//! change in any release before 1.0:
//!
//! ```toml
//! [dependencies]
//! lazy-zip-core = { git = "https://github.com/alenpaulvarghese/lazy-zip", rev = "<commit>" }
//! tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
//! ```
//!
//! Everything is async and needs a Tokio runtime with its I/O and time drivers enabled, such as
//! `#[tokio::main]`. The default `rustls` feature provides HTTPS; with `default-features = false`
//! only `http://` URLs work, unless you enable a TLS feature on `reqwest` 0.13 yourself.
//!
//! # Modules
//!
//! - [`archive`]: [`RemoteArchive`](archive::RemoteArchive), the directory read once, and
//!   [`EntryReader`](archive::EntryReader), one entry's verified data.
//! - [`zip_explorer`]: turns the directory into a tree, leaving out entries that can't be written
//!   safely.
//! - [`downloader`]: plans where selected entries go and downloads them concurrently.
//! - [`http_reader`]: the Range-request reader, its configuration and its defences against a
//!   misbehaving server.
//! - [`archive_limits`]: checks the end records before the directory is parsed.
//! - [`safe_path`]: validates entry names before they become file paths.
//! - [`error`]: [`Error`] and its coarse [`ErrorKind`].
//!
//! # Data usage
//!
//! | Operation | Requests | Bytes received |
//! |-----------|----------|----------------|
//! | [`RemoteArchive::open`](archive::RemoteArchive::open) | 1 for the end of the file, plus 1 for the directory if it isn't inside that | The last [`TAIL_BYTES`](archive_limits::TAIL_BYTES) (about 66 KiB, or the whole file if it's smaller), plus the directory |
//! | [`zip_explorer::scan`] | 0 | 0 |
//! | [`RemoteArchive::open_entry`](archive::RemoteArchive::open_entry), read to the end | 1 if the entry fits in its first 64 KiB, otherwise 2; 0 if it's inside the fetched end | The local header (about 30 bytes plus the name) and the compressed data |
//! | [`downloader::download_from`] | The same as `open_entry`, for each file | The same as `open_entry`, for each file |
//! | [`downloader::download_batch`] | `open` once, then the same as `download_from` | `open`, then the same as `download_from` |
//!
//! A server that doesn't support suffix ranges (`bytes=-N`) costs two more requests when opening.
//! A dropped connection is resumed with a new request from the exact byte where it stopped. An
//! entry that's refused after its header is checked costs at most 64 KiB. No request is ever a
//! `HEAD`, and nothing is read past the end of the entry being read.
//!
//! Network use is counted in [`ReaderConfig::network_bytes`](http_reader::ReaderConfig::network_bytes),
//! which is shared by clones of the config.
//!
//! # Quick start
//!
//! ```no_run
//! use lazy_zip_core::archive::RemoteArchive;
//! use lazy_zip_core::downloader::{download_from, plan_selection, BatchEvent, BatchOptions};
//! use lazy_zip_core::http_reader::ReaderConfig;
//! use lazy_zip_core::zip_explorer;
//! use std::path::Path;
//! use tokio_util::sync::CancellationToken;
//!
//! # async fn example() -> lazy_zip_core::Result<()> {
//! let config = ReaderConfig::new()?;
//! let archive = RemoteArchive::open("https://example.com/data.zip", config.clone()).await?;
//!
//! // List it. This makes no requests.
//! let listing = zip_explorer::scan(&archive);
//! for node in listing.nodes.iter().filter(|node| !node.is_dir) {
//!     println!("{} ({} bytes)", node.path, node.size);
//! }
//! println!("listing used {} bytes", listing.fetched_size);
//!
//! // Read one entry into memory. Its size and CRC are checked by the last read.
//! if let Some(index) = archive.find("docs/readme.txt") {
//!     let mut reader = archive.open_entry(index, archive.network_counter()).await?;
//!     let (mut data, mut buffer) = (Vec::new(), vec![0u8; 64 * 1024]);
//!     loop {
//!         let n = reader.read(&mut buffer).await?;
//!         if n == 0 {
//!             break;
//!         }
//!         data.extend_from_slice(&buffer[..n]);
//!     }
//! }
//!
//! // Download a folder to `out/`, without reading the directory again.
//! let wanted = vec!["images/a.png".to_string(), "images/b.png".to_string()];
//! let plan = plan_selection(Path::new("out"), "images", wanted);
//! for rejected in &plan.rejected {
//!     eprintln!("skipped {}: {}", rejected.path, rejected.error);
//! }
//! let options = BatchOptions {
//!     concurrency: 4,
//!     cancel: CancellationToken::new(),
//!     root: Some("out".into()),
//!     reader: config,
//! };
//! let report = download_from(&archive, plan.files, options, |event| {
//!     if let BatchEvent::FileFailed { path, error, .. } = event {
//!         eprintln!("{path} failed: {error}");
//!     }
//! })
//! .await;
//! println!("{} downloaded, {} failed", report.completed, report.failed);
//! # Ok(())
//! # }
//! ```
//!
//! # Custom headers
//!
//! Servers that need a login get headers that are sent with every request. Their values are
//! marked sensitive, so they're left out of debug output. `Authorization`, `Cookie` and
//! `Proxy-Authorization` are dropped on a redirect to another site; if any other header is set,
//! such a redirect fails instead.
//!
//! ```no_run
//! use lazy_zip_core::archive::RemoteArchive;
//! use lazy_zip_core::http_reader::ReaderConfig;
//! use std::time::Duration;
//!
//! # async fn example() -> lazy_zip_core::Result<()> {
//! let config = ReaderConfig::with_headers([("Authorization", "Bearer my-token")])?;
//! let archive = RemoteArchive::open("https://example.com/private.zip", config).await?;
//!
//! // Custom timeouts too. The read timeout applies to each wait for data.
//! let config = ReaderConfig::with_options(
//!     Duration::from_secs(5),
//!     Duration::from_secs(60),
//!     [("X-API-Key", "secret")],
//! )?;
//! # Ok(())
//! # }
//! ```
//!
//! # Errors
//!
//! Every fallible function returns [`Result`]. Its [`Error`] has a message meant for people,
//! and [`Error::kind`] groups the variants into an [`ErrorKind`] for code that reacts
//! differently to each. `ErrorKind` serializes as camelCase, for frontends.
//!
//! ```no_run
//! use lazy_zip_core::archive::RemoteArchive;
//! use lazy_zip_core::http_reader::ReaderConfig;
//! use lazy_zip_core::ErrorKind;
//!
//! # async fn example() -> lazy_zip_core::Result<()> {
//! match RemoteArchive::open("https://example.com/data.zip", ReaderConfig::new()?).await {
//!     Ok(archive) => println!("{} entries", archive.entries().len()),
//!     Err(error) => match error.kind() {
//!         ErrorKind::RangeNotSupported => eprintln!("download the whole file instead"),
//!         ErrorKind::HttpStatus => eprintln!("check the link or the login: {error}"),
//!         ErrorKind::NotAZip | ErrorKind::LimitExceeded => eprintln!("not usable: {error}"),
//!         _ => return Err(error),
//!     },
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Error messages never include the URL, because it can contain a signed token.
//!
//! # Safety
//!
//! Archives and servers are treated as untrusted:
//!
//! - **End records** are checked before the directory is parsed: at most
//!   [`MAX_ENTRIES`](archive_limits::MAX_ENTRIES) entries and a directory of at most
//!   [`MAX_DIRECTORY_BYTES`](archive_limits::MAX_DIRECTORY_BYTES). Parsing can't fetch more than
//!   that, and the records checked are the ones parsed.
//! - **Entries** are refused, as Python's `zipfile` refuses them, when the local header names a
//!   different file, when their data overlaps another entry or the directory, or when two entries
//!   share a local header (overlapping zip bombs, CVE-2024-0450). Encrypted entries are refused
//!   after reading only the header.
//! - **Data** is checked while it's read: never more than the declared size, and the size and
//!   CRC-32 must match at the end. A claimed compression ratio above 1100:1 is refused up front.
//! - **Names** with `..`, absolute paths, backslashes, Windows reserved names, control or
//!   bidirectional characters, or invalid UTF-8 are left out of the tree and can't be downloaded,
//!   on every OS. Names that differ only by case can't be downloaded together.
//! - **Files** are written to a temporary file and renamed into place, so a failed download never
//!   replaces an existing file or leaves a partial one. Destination folders must resolve inside
//!   the chosen root, so a symlink can't redirect a write. On Windows, downloads get a
//!   Mark-of-the-Web.
//! - **Servers** must answer with the requested range, uncompressed, of the same file (checked by
//!   `ETag` or `Last-Modified`, and length). Bodies longer than requested are refused without
//!   being buffered. Requests time out, transient failures are retried with backoff, and
//!   `Retry-After` is honored up to 30 seconds.
//!
//! Some things remain the caller's job:
//!
//! - **URLs:** any URL is fetched, and up to 10 redirects are followed. If URLs come from
//!   untrusted users of a server-side service, guard against server-side request forgery
//!   yourself, such as by refusing private addresses.
//! - **Paths:** [`RemoteArchive::open_entry`](archive::RemoteArchive::open_entry) doesn't check
//!   names. Use [`safe_path::destination`] before writing an entry to disk, or let
//!   [`downloader::plan_selection`] do it.
//! - **Unverified data:** an [`EntryReader`](archive::EntryReader) returns data before the final
//!   size and CRC check. Don't act on it until `read` returns 0.
//! - **Sizes:** check [`ArchiveEntry::size`](archive::ArchiveEntry::size) against memory and disk
//!   before reading a large entry.
//!
//! See `docs/architecture.md` in the repository for where each defence lives.

#![warn(missing_docs)]

// Compiles the examples in the integration guide.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

pub mod archive;
pub mod archive_limits;
pub mod downloader;
pub mod error;
pub mod http_reader;
pub mod safe_path;
pub mod zip_explorer;

pub use error::{Error, ErrorKind, Result};
