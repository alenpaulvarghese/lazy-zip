//! Downloads selected entries to disk, concurrently and safely.
//!
//! [`plan_selection`] turns archive names into destination paths, refusing unsafe names and
//! names that collide. [`download_from`] downloads them from an open [`RemoteArchive`], and
//! [`download_batch`] opens the archive first. Each file is written to a temporary file in its
//! destination folder and renamed into place once it has been verified, so a failure never
//! replaces an existing file or leaves a partial one behind.
//!
//! The futures returned are `Send`, so a batch can run on a spawned task while the caller
//! reports progress from [`BatchEvent`]s and stops it with a [`CancellationToken`].

use crate::archive::RemoteArchive;
use crate::error::{Error, ErrorKind, Result};
use crate::http_reader::ReaderConfig;
use crate::safe_path::{destination, ensure_within};
use futures::stream::{self, StreamExt};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio_util::sync::CancellationToken;

const COPY_BUFFER_BYTES: usize = 64 * 1024;
const WRITE_BUFFER_BYTES: usize = 1024 * 1024;
const EXISTENCE_CHECK_CONCURRENCY: usize = 32;

/// One file to download.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BatchFile {
    /// The entry's name in the archive. Events report the file by this name.
    pub zip_path: String,
    /// Where the file is written. It's used as given, so build it with [`plan_selection`] or
    /// [`destination`] when the name comes from the archive.
    pub dest: PathBuf,
    /// The entry to download, for picking one of several with the same name. Otherwise the last
    /// entry named `zip_path` is downloaded, matching the listing.
    pub index: Option<usize>,
}

/// A file that can't be downloaded, and why.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct FailedFile {
    /// The entry's name in the archive.
    pub path: String,
    /// A message for people.
    pub error: String,
    /// What sort of failure it is.
    pub kind: ErrorKind,
}

/// Where each selected file will be written, and the files that can't be.
#[derive(Debug, Default)]
pub struct SelectionPlan {
    /// The files to download, in the order they were selected.
    pub files: Vec<BatchFile>,
    /// The files refused because of their name.
    pub rejected: Vec<FailedFile>,
}

/// Maps selected entries to files under `root`, relative to `base` in the archive.
///
/// Names that differ only by case are rejected, since they collide on the default macOS and
/// Windows file systems. Names that aren't safe, or aren't inside `base`, are rejected with
/// [`ErrorKind::Unsafe`]. The plan picks no index, so the last entry with each name is used.
///
/// # Examples
///
/// ```
/// use lazy_zip_core::downloader::plan_selection;
/// use std::path::Path;
///
/// let selected = vec![
///     "photos/2024/a.jpg".to_string(),
///     "photos/2024/A.JPG".to_string(),
///     "photos/../evil.sh".to_string(),
/// ];
/// let plan = plan_selection(Path::new("out"), "photos", selected);
///
/// assert_eq!(plan.files.len(), 1);
/// assert_eq!(plan.files[0].dest, Path::new("out").join("2024").join("a.jpg"));
/// assert_eq!(plan.rejected.len(), 2);
/// ```
pub fn plan_selection(root: &Path, base: &str, zip_paths: Vec<String>) -> SelectionPlan {
    let mut plan = SelectionPlan::default();
    let mut seen = HashSet::with_capacity(zip_paths.len());

    for zip_path in zip_paths {
        match destination(root, base, &zip_path) {
            Ok(dest) if seen.insert(dest.to_string_lossy().to_lowercase()) => {
                plan.files.push(BatchFile {
                    zip_path,
                    dest,
                    index: None,
                });
            }
            Ok(_) => plan.rejected.push(FailedFile {
                path: zip_path,
                error: "Another selected file would be written to the same path".to_string(),
                kind: ErrorKind::Unsafe,
            }),
            Err(error) => plan.rejected.push(FailedFile {
                path: zip_path,
                kind: error.kind(),
                error: error.to_string(),
            }),
        }
    }
    plan
}

/// Returns the entries whose destination already exists, for asking before overwriting them.
/// Downloading replaces an existing file once the new one has been verified.
pub async fn find_existing(files: &[BatchFile]) -> Vec<String> {
    stream::iter(files.to_vec())
        .map(|file| async move {
            matches!(tokio::fs::try_exists(&file.dest).await, Ok(true)).then_some(file.zip_path)
        })
        .buffer_unordered(EXISTENCE_CHECK_CONCURRENCY)
        .filter_map(|existing| async move { existing })
        .collect()
        .await
}

/// Progress of a batch, reported as it happens. Events for different files interleave.
///
/// `path` is the file's [`BatchFile::zip_path`]. A cancelled file gets no final event.
#[derive(Debug, PartialEq)]
pub enum BatchEvent<'a> {
    /// A file started downloading.
    FileStarted {
        /// The file's name in the archive.
        path: &'a str,
    },
    /// More of a file was written.
    FileBytes {
        /// The file's name in the archive.
        path: &'a str,
        /// Decompressed bytes written since the last event for this file.
        bytes: u64,
    },
    /// A file was verified and moved into place.
    FileFinished {
        /// The file's name in the archive.
        path: &'a str,
    },
    /// A file failed. Its destination was left as it was.
    FileFailed {
        /// The file's name in the archive.
        path: &'a str,
        /// A message for people.
        error: String,
        /// What sort of failure it is.
        kind: ErrorKind,
    },
}

/// How a batch runs.
pub struct BatchOptions {
    /// Number of files downloaded at once. Zero is treated as one.
    pub concurrency: usize,
    /// Cancelling stops the batch, and removes the partial files.
    pub cancel: CancellationToken,
    /// When set, destination folders must resolve inside this folder.
    pub root: Option<PathBuf>,
    /// How to connect when the batch opens the archive itself. Network use is always counted in
    /// its `network_bytes`.
    ///
    /// With [`download_from`], only `network_bytes` is used: requests go through the config the
    /// archive was opened with.
    pub reader: ReaderConfig,
}

/// The outcome of a batch.
#[derive(Serialize, Debug, Default, PartialEq)]
pub struct BatchReport {
    /// Files downloaded and verified.
    pub completed: u32,
    /// Files that failed, not counting cancelled ones.
    pub failed: u32,
    /// Whether the batch was cancelled.
    pub cancelled: bool,
    /// Decompressed bytes written by completed files.
    pub written_bytes: u64,
    /// The value of [`BatchOptions::reader`]'s `network_bytes` counter when the batch ended. It
    /// includes earlier use of that counter, so give each batch its own counter to measure it.
    pub network_bytes: u64,
}

/// Removes the file when dropped, unless it has been renamed into place.
struct TempFileGuard(Option<PathBuf>);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn temp_path_for(dest: &Path) -> PathBuf {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    dest.with_file_name(format!(".lazy-zip-{}-{id}.part", std::process::id()))
}

/// Records where a file came from, so Windows treats it like a browser download. On macOS the
/// app's Info.plist asks the system to quarantine files it creates instead.
///
/// The downloads in this module do this already. Call it for files written another way, such as
/// from an [`EntryReader`](crate::archive::EntryReader). It does nothing on other systems, and
/// failures are ignored.
#[cfg(windows)]
pub fn mark_as_downloaded(dest: &Path, url: &str) {
    // The query string is left out, because it can contain a signed token.
    let host_url = url.split(['?', '#']).next().unwrap_or("");
    let mut stream = dest.as_os_str().to_os_string();
    stream.push(":Zone.Identifier");
    let contents = format!("[ZoneTransfer]\r\nZoneId=3\r\nHostUrl={host_url}\r\n");
    let _ = std::fs::write(stream, contents);
}

/// Records where a file came from, so Windows treats it like a browser download. On macOS the
/// app's Info.plist asks the system to quarantine files it creates instead.
///
/// The downloads in this module do this already. Call it for files written another way, such as
/// from an [`EntryReader`](crate::archive::EntryReader). It does nothing on other systems, and
/// failures are ignored.
#[cfg(not(windows))]
pub fn mark_as_downloaded(_dest: &Path, _url: &str) {}

/// Downloads one entry to a temporary file, then renames it over `dest`, so a failed or cancelled
/// download never replaces an existing file or leaves a partial one behind.
async fn download_entry(
    archive: &RemoteArchive,
    index: Option<usize>,
    file: &BatchFile,
    root: Option<&Path>,
    network_bytes: Arc<AtomicU64>,
    on_bytes: &(dyn Fn(u64) + Send + Sync),
) -> Result<u64> {
    let index = index.ok_or_else(|| Error::FileNotFound(file.zip_path.clone()))?;
    if let Some(parent) = file.dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
        if let Some(root) = root {
            ensure_within(root, parent)?;
        }
    }

    let mut entry_reader = archive.open_entry(index, network_bytes).await?;

    let temp_path = temp_path_for(&file.dest);
    let mut guard = TempFileGuard(None);
    let temp_file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .await?;
    guard.0 = Some(temp_path.clone());
    let mut writer = BufWriter::with_capacity(WRITE_BUFFER_BYTES, temp_file);

    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    let mut written = 0u64;
    loop {
        let n = entry_reader.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        written += n as u64;
        writer.write_all(&buffer[..n]).await?;
        on_bytes(n as u64);
    }
    writer.shutdown().await?;
    drop(writer);

    tokio::fs::rename(&temp_path, &file.dest).await?;
    guard.0 = None;
    mark_as_downloaded(&file.dest, archive.url());
    Ok(written)
}

/// Opens the archive once, then downloads files from it as [`download_from`] does. If the archive
/// can't be opened, every file fails with that error.
///
/// Prefer [`download_from`] when the archive is already open, such as after listing it, so its
/// directory isn't read again.
pub async fn download_batch<F>(
    url: &str,
    files: Vec<BatchFile>,
    options: BatchOptions,
    on_event: F,
) -> BatchReport
where
    F: for<'a> Fn(BatchEvent<'a>) + Send + Sync,
{
    let opened = tokio::select! {
        biased;
        _ = options.cancel.cancelled() => Err(Error::Cancelled),
        opened = RemoteArchive::open(url, options.reader.clone()) => opened,
    };
    match opened {
        Ok(archive) => download_from(&archive, files, options, on_event).await,
        Err(error) => {
            let cancelled = matches!(error, Error::Cancelled);
            if !cancelled {
                let (kind, error) = (error.kind(), error.to_string());
                for file in &files {
                    on_event(BatchEvent::FileFailed {
                        path: &file.zip_path,
                        error: error.clone(),
                        kind,
                    });
                }
            }
            BatchReport {
                failed: if cancelled { 0 } else { files.len() as u32 },
                cancelled,
                network_bytes: options.reader.network_bytes.load(Ordering::Relaxed),
                ..Default::default()
            }
        }
    }
}

/// Downloads files concurrently from an archive that's already open, carrying on past individual
/// failures.
///
/// The directory isn't read again, and each file costs about its compressed size. Every outcome is
/// reported through `on_event` as it happens, and cancelling stops work and removes partial files.
///
/// Missing parent folders are created. When [`BatchOptions::root`] is set, each destination
/// folder must resolve inside it, so a symlink on disk can't redirect a write.
///
/// # Examples
///
/// Running a batch on its own task, so it can be cancelled from elsewhere:
///
/// ```no_run
/// use lazy_zip_core::archive::RemoteArchive;
/// use lazy_zip_core::downloader::{download_from, plan_selection, BatchEvent, BatchOptions};
/// use lazy_zip_core::http_reader::ReaderConfig;
/// use std::path::Path;
/// use std::sync::Arc;
/// use tokio_util::sync::CancellationToken;
///
/// # async fn example(archive: Arc<RemoteArchive>) -> lazy_zip_core::Result<()> {
/// let plan = plan_selection(Path::new("out"), "", vec!["data/a.csv".to_string()]);
/// let cancel = CancellationToken::new();
/// let options = BatchOptions {
///     concurrency: 8,
///     cancel: cancel.clone(),
///     root: Some("out".into()),
///     reader: ReaderConfig::new()?,
/// };
/// let job = tokio::spawn(async move {
///     download_from(&archive, plan.files, options, |event| {
///         if let BatchEvent::FileFinished { path } = event {
///             println!("done: {path}");
///         }
///     })
///     .await
/// });
/// // Later, from anywhere: cancel.cancel();
/// let report = job.await.expect("the batch panicked");
/// println!("{} of the files downloaded", report.completed);
/// # Ok(())
/// # }
/// ```
pub async fn download_from<F>(
    archive: &RemoteArchive,
    files: Vec<BatchFile>,
    options: BatchOptions,
    on_event: F,
) -> BatchReport
where
    F: for<'a> Fn(BatchEvent<'a>) + Send + Sync,
{
    let completed = AtomicU32::new(0);
    let failed = AtomicU32::new(0);
    let written_bytes = AtomicU64::new(0);
    let indexes = archive.find_all(
        files
            .iter()
            .filter(|file| file.index.is_none())
            .map(|file| file.zip_path.as_str()),
    );

    // Collected first, because a lazy iterator held across the await would stop callers from
    // spawning this future.
    let downloads: Vec<_> = files.iter().map(|file| {
        let (cancel, root) = (&options.cancel, options.root.as_deref());
        let (completed, failed, written_bytes) = (&completed, &failed, &written_bytes);
        let network_bytes = options.reader.network_bytes.clone();
        let index = match file.index {
            Some(index) => (index < archive.entries().len()).then_some(index),
            None => indexes.get(file.zip_path.as_str()).copied(),
        };
        let on_event = &on_event;
        async move {
            if cancel.is_cancelled() {
                return;
            }
            let path = file.zip_path.as_str();
            on_event(BatchEvent::FileStarted { path });
            let on_bytes = |bytes| on_event(BatchEvent::FileBytes { path, bytes });
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(Error::Cancelled),
                result = download_entry(archive, index, file, root, network_bytes, &on_bytes) => result,
            };
            match result {
                Ok(written) => {
                    written_bytes.fetch_add(written, Ordering::Relaxed);
                    completed.fetch_add(1, Ordering::Relaxed);
                    on_event(BatchEvent::FileFinished { path });
                }
                Err(Error::Cancelled) => {}
                Err(error) => {
                    failed.fetch_add(1, Ordering::Relaxed);
                    on_event(BatchEvent::FileFailed {
                        path,
                        error: error.to_string(),
                        kind: error.kind(),
                    });
                }
            }
        }
    }).collect();

    stream::iter(downloads)
        .buffer_unordered(options.concurrency.max(1))
        .collect::<Vec<()>>()
        .await;

    BatchReport {
        completed: completed.load(Ordering::Relaxed),
        failed: failed.load(Ordering::Relaxed),
        cancelled: options.cancel.is_cancelled(),
        written_bytes: written_bytes.load(Ordering::Relaxed),
        network_bytes: options.reader.network_bytes.load(Ordering::Relaxed),
    }
}
