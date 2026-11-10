use futures::stream::{self, StreamExt};
use lazy_zip_core::archive::{ArchiveEntry, EntryReader, RemoteArchive};
use lazy_zip_core::downloader::{
    download_from, mark_as_downloaded, plan_selection, BatchEvent, BatchOptions,
};
use lazy_zip_core::http_reader::ReaderConfig;
use lazy_zip_core::safe_path::{destination, ensure_within};
use lazy_zip_core::zip_explorer::{scan, FileNode};
use lazy_zip_core::{Error, ErrorKind};
use pyo3::exceptions::{
    PyConnectionError, PyException, PyKeyError, PyNotImplementedError, PyOSError, PyRuntimeError,
    PyValueError,
};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyTuple};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

const READ_BUFFER_BYTES: usize = 64 * 1024;
const MAX_STREAM_READ_BYTES: usize = 1024 * 1024;
const WRITE_BUFFER_BYTES: usize = 1024 * 1024;
/// `read()` reserves at most this much up front, since the directory's sizes aren't trusted.
const MAX_PREALLOCATION_BYTES: u64 = 64 * 1024 * 1024;
/// How often a waiting call wakes up to let Python handle Ctrl-C.
const SIGNAL_CHECK_INTERVAL: Duration = Duration::from_millis(100);
const DEFAULT_WORKERS: usize = 8;
const TEST_CONCURRENCY: usize = 8;
const LISTED_FAILURES: usize = 10;

/// Code page 437 from 0x80 up, which `zipfile` uses for names without the UTF-8 flag. The lower
/// half is ASCII.
const CP437_HIGH: &str = concat!(
    "ÇüéâäàåçêëèïîìÄÅ",
    "ÉæÆôöòûùÿÖÜ¢£¥₧ƒ",
    "áíóúñÑªº¿⌐¬½¼¡«»",
    "░▒▓│┤╡╢╖╕╣║╗╝╜╛┐",
    "└┴┬├─┼╞╟╚╔╩╦╠═╬╧",
    "╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀",
    "αßΓπΣσµτΦΘΩδ∞φε∩",
    "≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}",
);

mod py_zipfile {
    pyo3::import_exception!(zipfile, BadZipFile);
}

pyo3::create_exception!(
    lazy_zip,
    BadZipFile,
    py_zipfile::BadZipFile,
    "The archive or one of its members is corrupt, looks malicious, or can't be written safely.

A subclass of `zipfile.BadZipFile`, so `except zipfile.BadZipFile` catches it too.

Attributes:
    kind (str): What went wrong:
        `\"not_a_zip\"`: the file isn't a ZIP archive, or its end is damaged.
        `\"limit_exceeded\"`: the directory is too large to open safely.
        `\"corrupt\"`: data doesn't match its size or CRC, or can't be parsed.
        `\"suspicious\"`: the member looks crafted to cause harm, such as overlapping data.
        `\"unsafe\"`: the name can't be written safely, such as `../evil.txt`."
);
pyo3::create_exception!(
    lazy_zip,
    ExtractError,
    PyException,
    "Raised by `RemoteZip.extractall()` when some members couldn't be extracted.

Every member is attempted first, so the others are written. The message lists up to ten
failures.

Attributes:
    failures (dict of str to Exception): Each failed member name and its exception. Each
        exception is one of the types `RemoteZip` raises elsewhere (`BadZipFile`,
        `ConnectionError`, `NotImplementedError`, `OSError`, ...) and has a `kind` attribute."
);

/// The `kind` attribute set on exceptions raised for a core error.
fn kind_name(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Network => "network",
        ErrorKind::HttpStatus => "http_status",
        ErrorKind::RangeNotSupported => "range_not_supported",
        ErrorKind::RemoteChanged => "remote_changed",
        ErrorKind::NotAZip => "not_a_zip",
        ErrorKind::LimitExceeded => "limit_exceeded",
        ErrorKind::Corrupt => "corrupt",
        ErrorKind::Suspicious => "suspicious",
        ErrorKind::Unsupported => "unsupported",
        ErrorKind::NotFound => "not_found",
        ErrorKind::Unsafe => "unsafe",
        ErrorKind::InvalidHeader => "invalid_header",
        ErrorKind::Io => "io",
        ErrorKind::Cancelled => "cancelled",
    }
}

fn exception_for(kind: ErrorKind, message: String) -> PyErr {
    match kind {
        ErrorKind::NotAZip
        | ErrorKind::LimitExceeded
        | ErrorKind::Corrupt
        | ErrorKind::Suspicious
        | ErrorKind::Unsafe => BadZipFile::new_err(message),
        ErrorKind::Network
        | ErrorKind::HttpStatus
        | ErrorKind::RangeNotSupported
        | ErrorKind::RemoteChanged => PyConnectionError::new_err(message),
        // A subclass of RuntimeError, which `zipfile` raises for encrypted files.
        ErrorKind::Unsupported => PyNotImplementedError::new_err(message),
        ErrorKind::NotFound => PyKeyError::new_err(message),
        ErrorKind::InvalidHeader => PyValueError::new_err(message),
        ErrorKind::Io | ErrorKind::Cancelled => PyOSError::new_err(message),
    }
}

fn with_kind(py: Python<'_>, error: PyErr, kind: ErrorKind) -> PyErr {
    // Setting an attribute on a fresh exception instance can't fail in practice.
    let _ = error.value(py).setattr("kind", kind_name(kind));
    error
}

fn to_py_err(error: Error) -> PyErr {
    let kind = error.kind();
    let error = match error {
        Error::FileNotFound(name) => return missing(&name),
        // Keeps the errno, so this is FileNotFoundError, PermissionError and so on.
        Error::Io(error) => error.into(),
        other => exception_for(kind, other.to_string()),
    };
    Python::attach(|py| with_kind(py, error, kind))
}

/// Whether `testzip()` reports the entry rather than raising.
fn is_corrupt(error: &Error) -> bool {
    matches!(error.kind(), ErrorKind::Corrupt | ErrorKind::Suspicious)
}

fn missing(name: &str) -> PyErr {
    let error = PyKeyError::new_err(format!("There is no item named {name:?} in the archive"));
    Python::attach(|py| with_kind(py, error, ErrorKind::NotFound))
}

fn closed_archive() -> PyErr {
    PyValueError::new_err("Attempt to use ZIP archive that was already closed")
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The name Python sees: the stored name when it's UTF-8, otherwise its bytes decoded as cp437,
/// as `zipfile` does for names without the UTF-8 flag.
fn python_name(entry: &ArchiveEntry) -> String {
    if let Some(name) = entry.name() {
        return name.to_string();
    }
    let high: Vec<char> = CP437_HIGH.chars().collect();
    entry
        .raw_name()
        .iter()
        .map(|&byte| {
            if byte < 0x80 {
                char::from(byte)
            } else {
                high[usize::from(byte - 0x80)]
            }
        })
        .collect()
}

/// One runtime for the whole process, so archives and open files can outlive each other.
fn runtime() -> PyResult<&'static Runtime> {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    if let Some(runtime) = RUNTIME.get() {
        return Ok(runtime);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("lazy-zip")
        .build()
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
    Ok(RUNTIME.get_or_init(|| runtime))
}

/// Runs `future` on the runtime without holding the GIL. Every [`SIGNAL_CHECK_INTERVAL`] the GIL
/// is taken back briefly so Python can run signal handlers. If one raises, such as
/// KeyboardInterrupt on Ctrl-C, the work is cancelled through `cancel` (or aborted when there's no
/// token), and the call returns that exception once the work has stopped.
fn wait<T, F>(py: Python<'_>, cancel: Option<&CancellationToken>, future: F) -> PyResult<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let runtime = runtime()?;
    let mut task = runtime.spawn(future);
    loop {
        let finished = py.detach(|| {
            runtime.block_on(async { tokio::time::timeout(SIGNAL_CHECK_INTERVAL, &mut task).await })
        });
        match finished {
            Ok(result) => {
                return result.map_err(|e| PyRuntimeError::new_err(format!("Task failed: {e}")))
            }
            Err(_elapsed) => {
                if let Err(interrupt) = py.check_signals() {
                    match cancel {
                        Some(token) => token.cancel(),
                        None => task.abort(),
                    }
                    // Waiting lets partial files be removed before the exception reaches Python.
                    let _ = py.detach(|| runtime.block_on(task));
                    return Err(interrupt);
                }
            }
        }
    }
}

/// One file or folder of the archive's tree, from `RemoteZip.files`.
///
/// Folders are listed whether or not the archive has an entry for them, and entries whose names
/// can't be written safely (or aren't valid UTF-8) are left out. Siblings are sorted by name.
/// Read-only.
#[pyclass(frozen, module = "lazy_zip")]
#[derive(Clone)]
pub struct FileEntry {
    /// The last part of the path, such as `readme.txt`.
    #[pyo3(get)]
    pub name: String,
    /// For a file, its member name, usable with `RemoteZip.read()` and friends. For a folder, its
    /// path without a trailing slash, such as `docs/guides`.
    #[pyo3(get)]
    pub path: String,
    /// The file's uncompressed size in bytes. 0 for folders.
    #[pyo3(get)]
    pub size: u64,
    /// The file's compressed size in bytes. 0 for folders.
    #[pyo3(get)]
    pub compressed_size: u64,
    /// Whether this is a folder.
    #[pyo3(get)]
    pub is_dir: bool,
    /// A folder's contents, sorted by name. Empty for files.
    #[pyo3(get)]
    pub children: Vec<FileEntry>,
    /// The containing folder's `path`, or `None` at the top level.
    #[pyo3(get)]
    pub parent_path: Option<String>,
}

impl From<&FileNode> for FileEntry {
    fn from(node: &FileNode) -> Self {
        Self {
            name: node.name.clone(),
            path: node.path.clone(),
            size: node.size,
            compressed_size: node.compressed_size,
            is_dir: node.is_dir,
            children: node.children.iter().map(FileEntry::from).collect(),
            parent_path: node.parent_path.clone(),
        }
    }
}

#[pymethods]
impl FileEntry {
    fn __repr__(&self) -> String {
        format!(
            "<FileEntry path={:?} size={} is_dir={}>",
            self.path,
            self.size,
            if self.is_dir { "True" } else { "False" }
        )
    }
}

/// Information about one member of the archive, like `zipfile.ZipInfo`.
///
/// Returned by `RemoteZip.infolist()` and `RemoteZip.getinfo()`, and accepted wherever a member
/// is expected. A `ZipInfo` always refers to its own entry, even when several entries share a
/// name; one from a different `RemoteZip` is looked up by `filename`. Read-only, and it can't be
/// created from Python. The sizes and CRC are what the directory claims; they're checked when
/// the member is read.
#[pyclass(frozen, from_py_object, module = "lazy_zip")]
#[derive(Clone)]
pub struct ZipInfo {
    /// The member's name. UTF-8 names are kept as stored; names without the UTF-8 flag are
    /// decoded as code page 437, like `zipfile`. Folder names end with `/`.
    #[pyo3(get)]
    filename: String,
    /// The same as `filename`: names aren't normalized, so the two never differ.
    #[pyo3(get)]
    orig_filename: String,
    /// The modification time as stored, in the archive's local time: a tuple of year, month,
    /// day, hour, minute and second.
    #[pyo3(get)]
    date_time: (u16, u8, u8, u8, u8, u8),
    /// The uncompressed size in bytes.
    #[pyo3(get)]
    file_size: u64,
    /// The compressed size in bytes. Reading the member downloads about this much.
    #[pyo3(get)]
    compress_size: u64,
    /// The CRC-32 of the uncompressed data.
    #[pyo3(get, name = "CRC")]
    crc: u32,
    /// The compression method: 0 stored, 8 deflate, 9 deflate64, 12 bzip2, 14 LZMA, 93 zstd or
    /// 95 xz. LZMA members are listed but reading them raises `NotImplementedError`.
    #[pyo3(get)]
    compress_type: u16,
    /// Host-dependent file attributes. On archives made on Unix, the high 16 bits are the mode.
    #[pyo3(get)]
    external_attr: u32,
    /// Where the member's local header starts in the archive, in bytes.
    #[pyo3(get)]
    header_offset: u64,
    dir: bool,
    /// Which entry this is, so a duplicate name opens the entry it was listed for.
    index: usize,
}

impl ZipInfo {
    fn new(index: usize, name: &str, entry: &ArchiveEntry) -> Self {
        Self {
            filename: name.to_string(),
            orig_filename: name.to_string(),
            date_time: entry.date_time,
            file_size: entry.size,
            compress_size: entry.compressed_size,
            crc: entry.crc,
            compress_type: entry.compression_method(),
            external_attr: entry.external_attributes,
            header_offset: entry.header_offset,
            dir: entry.is_dir,
            index,
        }
    }
}

#[pymethods]
impl ZipInfo {
    /// Returns whether the member is a folder: it's flagged as one, or its name ends with `/`.
    fn is_dir(&self) -> bool {
        self.dir
    }

    fn __repr__(&self) -> String {
        format!(
            "<ZipInfo filename={:?} compress_type={} file_size={} compress_size={}>",
            self.filename, self.compress_type, self.file_size, self.compress_size
        )
    }
}

/// A member given by name or by a `ZipInfo` from `infolist()`.
#[derive(FromPyObject)]
enum Member {
    Info(ZipInfo),
    Name(String),
}

/// An archive whose directory has been read.
struct Opened {
    archive: RemoteArchive,
    config: ReaderConfig,
    /// Each entry's name as Python sees it, by index.
    names: Vec<String>,
    /// The last entry with each name, matching the listing.
    by_name: HashMap<String, usize>,
}

impl Opened {
    fn new(archive: RemoteArchive, config: ReaderConfig) -> Self {
        let names: Vec<String> = archive.entries().iter().map(python_name).collect();
        let by_name = names
            .iter()
            .enumerate()
            .map(|(index, name)| (name.clone(), index))
            .collect();
        Self {
            archive,
            config,
            names,
            by_name,
        }
    }

    fn find(&self, name: &str) -> PyResult<usize> {
        self.by_name.get(name).copied().ok_or_else(|| missing(name))
    }

    fn resolve(&self, member: &Member) -> PyResult<usize> {
        match member {
            Member::Name(name) => self.find(name),
            Member::Info(info) => {
                let same = self.names.get(info.index) == Some(&info.filename)
                    && self.entry(info.index).header_offset == info.header_offset;
                if same {
                    Ok(info.index)
                } else {
                    self.find(&info.filename)
                }
            }
        }
    }

    fn entry(&self, index: usize) -> &ArchiveEntry {
        &self.archive.entries()[index]
    }

    fn info(&self, index: usize) -> ZipInfo {
        ZipInfo::new(index, &self.names[index], self.entry(index))
    }

    async fn open_entry(&self, index: usize) -> lazy_zip_core::Result<EntryReader> {
        self.archive
            .open_entry(index, self.archive.network_counter())
            .await
    }
}

async fn read_all(opened: Arc<Opened>, index: usize) -> lazy_zip_core::Result<Vec<u8>> {
    let size = opened.entry(index).size.min(MAX_PREALLOCATION_BYTES);
    let mut data = Vec::with_capacity(size as usize);
    let mut reader = opened.open_entry(index).await?;
    let mut buffer = vec![0u8; READ_BUFFER_BYTES];
    loop {
        let n = reader.read(&mut buffer).await?;
        if n == 0 {
            return Ok(data);
        }
        data.extend_from_slice(&buffer[..n]);
    }
}

/// Reads and discards an entry, which checks its size and checksum.
async fn verify(opened: &Opened, index: usize) -> lazy_zip_core::Result<()> {
    let mut reader = opened.open_entry(index).await?;
    let mut buffer = vec![0u8; READ_BUFFER_BYTES];
    while reader.read(&mut buffer).await? != 0 {}
    Ok(())
}

/// Returns the name of the first entry, in archive order, whose data is corrupt. Other errors, such
/// as network failures, are returned if they happen before the first corrupt entry.
async fn first_bad_entry(opened: Arc<Opened>) -> lazy_zip_core::Result<Option<String>> {
    let indexes: Vec<usize> = opened
        .archive
        .entries()
        .iter()
        .enumerate()
        .filter(|(_, entry)| !(entry.is_dir && entry.size == 0))
        .map(|(index, _)| index)
        .collect();
    let opened = &opened;
    let mut results: Vec<(usize, lazy_zip_core::Result<()>)> = stream::iter(indexes)
        .map(|index| async move { (index, verify(opened, index).await) })
        .buffer_unordered(TEST_CONCURRENCY)
        .collect()
        .await;
    results.sort_unstable_by_key(|(index, _)| *index);
    for (index, result) in results {
        match result {
            Ok(()) => {}
            Err(error) if is_corrupt(&error) => return Ok(Some(opened.names[index].clone())),
            Err(error) => return Err(error),
        }
    }
    Ok(None)
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
    dest.with_file_name(format!(".lazy-zip-py-{}-{id}.part", std::process::id()))
}

/// Creates a directory member's folder. It must resolve inside `root`.
async fn make_dir(root: &Path, dest: &Path) -> lazy_zip_core::Result<()> {
    tokio::fs::create_dir_all(dest).await?;
    ensure_within(root, dest)
}

/// Streams an entry to a temporary file next to `dest`, then renames it into place, so a failed or
/// cancelled read never leaves a partial file behind. With `root`, the folder must resolve inside it.
async fn write_entry(
    opened: &Opened,
    index: usize,
    dest: &Path,
    root: Option<&Path>,
) -> lazy_zip_core::Result<()> {
    if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent).await?;
        if let Some(root) = root {
            ensure_within(root, parent)?;
        }
    }
    let mut reader = opened.open_entry(index).await?;

    let temp_path = temp_path_for(dest);
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .await?;
    let mut guard = TempFileGuard(Some(temp_path.clone()));
    let mut writer = BufWriter::with_capacity(WRITE_BUFFER_BYTES, file);
    let mut buffer = vec![0u8; READ_BUFFER_BYTES];
    loop {
        let n = reader.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        writer.write_all(&buffer[..n]).await?;
    }
    writer.flush().await?;
    writer.into_inner().sync_all().await?;

    tokio::fs::rename(&temp_path, dest).await?;
    guard.0 = None;
    mark_as_downloaded(dest, opened.archive.url());
    Ok(())
}

fn header_pairs(headers: Option<&Bound<'_, PyAny>>) -> PyResult<Vec<(String, String)>> {
    let Some(headers) = headers else {
        return Ok(Vec::new());
    };
    let items = if headers.hasattr("items")? {
        headers.call_method0("items")?
    } else {
        headers.clone()
    };
    items
        .try_iter()?
        .map(|item| item?.extract::<(String, String)>())
        .collect()
}

fn timeout_seconds(value: &Bound<'_, PyAny>) -> PyResult<Duration> {
    let seconds: f64 = value.extract()?;
    if seconds.is_finite() && seconds > 0.0 {
        Duration::try_from_secs_f64(seconds)
            .map_err(|_| PyValueError::new_err("timeout is too large"))
    } else {
        Err(PyValueError::new_err(format!(
            "timeout must be a positive number of seconds, not {seconds}"
        )))
    }
}

/// Reads `timeout` as seconds for both phases, or a `(connect, read)` tuple, like `requests`.
fn timeouts(timeout: Option<&Bound<'_, PyAny>>) -> PyResult<Option<(Duration, Duration)>> {
    let Some(timeout) = timeout else {
        return Ok(None);
    };
    if timeout.is_instance_of::<PyTuple>() {
        let (connect, read): (Bound<'_, PyAny>, Bound<'_, PyAny>) =
            timeout.extract().map_err(|_| {
                PyValueError::new_err("timeout must be a number or a (connect, read) tuple")
            })?;
        return Ok(Some((timeout_seconds(&connect)?, timeout_seconds(&read)?)));
    }
    let both = timeout_seconds(timeout)?;
    Ok(Some((both, both)))
}

fn current_dir() -> PyResult<PathBuf> {
    std::env::current_dir().map_err(PyErr::from)
}

/// A remote ZIP archive, read with HTTP range requests. Its directory is read when it's created.
///
/// This is the native base class. Use `lazy_zip.RemoteZip`, which adds `open()`, `filelist` and
/// `printdir()`; see its documentation for the arguments.
#[pyclass(frozen, subclass, module = "lazy_zip")]
pub struct RemoteZip {
    url: String,
    content_length: u64,
    network_bytes: Arc<AtomicU64>,
    opened: Mutex<Option<Arc<Opened>>>,
    files: Mutex<Option<Vec<FileEntry>>>,
}

impl RemoteZip {
    fn opened(&self) -> PyResult<Arc<Opened>> {
        lock(&self.opened).clone().ok_or_else(closed_archive)
    }
}

#[pymethods]
impl RemoteZip {
    #[new]
    #[pyo3(signature = (url, headers=None, timeout=None))]
    fn new(
        py: Python<'_>,
        url: String,
        headers: Option<Bound<'_, PyAny>>,
        timeout: Option<Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        let pairs = header_pairs(headers.as_ref())?;
        let headers = pairs.iter().map(|(k, v)| (k.as_str(), v.as_str()));
        let config = match timeouts(timeout.as_ref())? {
            Some((connect, read)) => ReaderConfig::with_options(connect, read, headers),
            None => ReaderConfig::with_headers(headers),
        }
        .map_err(to_py_err)?;
        let (open_url, open_config) = (url.clone(), config.clone());
        let archive = wait(py, None, async move {
            RemoteArchive::open(&open_url, open_config).await
        })?
        .map_err(to_py_err)?;

        Ok(Self {
            url,
            content_length: archive.content_length(),
            network_bytes: config.network_bytes.clone(),
            opened: Mutex::new(Some(Arc::new(Opened::new(archive, config)))),
            files: Mutex::new(None),
        })
    }

    /// Returns the archive itself, for use in a `with` block.
    fn __enter__<'py>(slf: &Bound<'py, Self>) -> Bound<'py, Self> {
        slf.clone()
    }

    /// Closes the archive at the end of a `with` block. Exceptions aren't suppressed.
    #[pyo3(text_signature = "($self, exc_type, exc_value, traceback)")]
    fn __exit__(
        &self,
        _exc_type: Option<Bound<'_, PyAny>>,
        _exc_value: Option<Bound<'_, PyAny>>,
        _traceback: Option<Bound<'_, PyAny>>,
    ) {
        self.close();
    }

    fn __repr__(&self) -> String {
        let url = self.url.split(['?', '#']).next().unwrap_or("");
        let state = if lock(&self.opened).is_some() {
            ""
        } else {
            " [closed]"
        };
        format!("<lazy_zip.RemoteZip url={url:?}{state}>")
    }

    /// The URL the archive was opened with, exactly as given.
    ///
    /// It may contain credentials, such as a presigned query string, so take care when logging
    /// it. `repr()` and error messages leave the query string out.
    #[getter]
    fn url(&self) -> &str {
        &self.url
    }

    /// The archive's size in bytes, as reported by the server.
    #[getter]
    fn content_length(&self) -> u64 {
        self.content_length
    }

    /// Bytes received from the server so far by this archive.
    ///
    /// Counts the response data of every request: reading the directory, and every read,
    /// stream, seek, extraction and `testzip()`, including retried requests and files opened
    /// with `open()` that are still in use after `close()`. HTTP headers and TLS overhead aren't
    /// counted. It only grows, and is safe to read from any thread while downloads run.
    #[getter]
    fn network_bytes(&self) -> u64 {
        self.network_bytes.load(Ordering::Relaxed)
    }

    /// Returns the members' names, in archive order. Repeated names appear once per entry.
    ///
    /// Uses the directory read when the archive was opened, so it makes no requests.
    ///
    /// Raises:
    ///     ValueError: The archive is closed.
    fn namelist(&self) -> PyResult<Vec<String>> {
        Ok(self.opened()?.names.clone())
    }

    /// Returns a `ZipInfo` for each member, in archive order. Makes no requests.
    ///
    /// Raises:
    ///     ValueError: The archive is closed.
    fn infolist(&self) -> PyResult<Vec<ZipInfo>> {
        let opened = self.opened()?;
        Ok((0..opened.names.len())
            .map(|index| opened.info(index))
            .collect())
    }

    /// Returns the `ZipInfo` of the member called `name`. Makes no requests.
    ///
    /// When several entries share the name, the last one is returned, like `zipfile`.
    ///
    /// Raises:
    ///     KeyError: No member has that name (`kind` is `"not_found"`).
    ///     ValueError: The archive is closed.
    fn getinfo(&self, name: &str) -> PyResult<ZipInfo> {
        let opened = self.opened()?;
        Ok(opened.info(opened.find(name)?))
    }

    /// Reads a member into memory and returns its data.
    ///
    /// The size and CRC are checked before anything is returned. Costs at most two requests
    /// and about the member's compressed size. Use `open()` for members too large to hold in
    /// memory.
    ///
    /// Args:
    ///     name: A member name, or a `ZipInfo` to pick one of several entries with the same name.
    ///
    /// Returns:
    ///     The uncompressed data.
    ///
    /// Raises:
    ///     KeyError: No member has that name.
    ///     BadZipFile: The member is corrupt or looks malicious.
    ///     NotImplementedError: The member is encrypted or uses an unsupported compression method.
    ///     ConnectionError: The download failed, timed out, or the archive changed on the server.
    ///     ValueError: The archive is closed.
    fn read<'py>(&self, py: Python<'py>, name: Member) -> PyResult<Bound<'py, PyBytes>> {
        let opened = self.opened()?;
        let index = opened.resolve(&name)?;
        let data = wait(py, None, read_all(opened, index))?.map_err(to_py_err)?;
        Ok(PyBytes::new(py, &data))
    }

    /// Opens a member as an unbuffered `EntryStream`. Use `open()`, which wraps it in a
    /// `ZipExtFile`.
    ///
    /// The member's local header is fetched and checked before this returns.
    fn _open_stream(&self, py: Python<'_>, name: Member) -> PyResult<EntryStream> {
        let opened = self.opened()?;
        let index = opened.resolve(&name)?;
        let reader = {
            let opened = opened.clone();
            wait(py, None, async move { opened.open_entry(index).await })?.map_err(to_py_err)?
        };
        Ok(EntryStream {
            size: opened.entry(index).size,
            opened,
            index,
            position: AtomicU64::new(0),
            state: Mutex::new(StreamState::Ready(reader)),
        })
    }

    /// Extracts a member under `path` and returns where it was written.
    ///
    /// Missing folders are created. The data is written to a temporary file next to the
    /// destination and renamed into place once its size and CRC have been checked, so a failed
    /// or interrupted extraction never leaves a partial file and an existing file is replaced
    /// only by verified data. A folder member just creates the folder. Unlike `zipfile`, names
    /// that aren't safe to write, such as `../evil.txt` or `C:/x`, are refused rather than
    /// rewritten. On Windows the file is marked as downloaded from the internet.
    ///
    /// Args:
    ///     member: A member name, or a `ZipInfo`.
    ///     path: The folder to extract into. Defaults to the current directory.
    ///
    /// Returns:
    ///     The path of the written file or folder, as a `str`.
    ///
    /// Raises:
    ///     KeyError: No member has that name.
    ///     BadZipFile: The name isn't safe to write (`kind` is `"unsafe"`), or the member is
    ///         corrupt or looks malicious.
    ///     NotImplementedError: The member is encrypted or uses an unsupported compression method.
    ///     ConnectionError: The download failed, timed out, or the archive changed on the server.
    ///     OSError: The file couldn't be written.
    ///     ValueError: The archive is closed.
    #[pyo3(signature = (member, path=None))]
    fn extract(
        &self,
        py: Python<'_>,
        member: Member,
        path: Option<PathBuf>,
    ) -> PyResult<std::ffi::OsString> {
        let opened = self.opened()?;
        let index = opened.resolve(&member)?;
        let root = path.map_or_else(current_dir, Ok)?;
        let dest = destination(&root, "", &opened.names[index]).map_err(to_py_err)?;
        let is_dir = opened.entry(index).is_dir;

        let written = dest.clone();
        wait(py, None, async move {
            tokio::fs::create_dir_all(&root).await?;
            if is_dir {
                make_dir(&root, &dest).await
            } else {
                write_entry(&opened, index, &dest, Some(&root)).await
            }
        })?
        .map_err(to_py_err)?;
        Ok(written.into_os_string())
    }

    /// Extracts members under `path`, several at a time.
    ///
    /// Every member is attempted, and each is written as `extract()` writes it. If any fail,
    /// `ExtractError` is raised once the rest are done; the members that succeeded stay
    /// written. When a name is selected more than once, the last selected entry is written. Two
    /// names that differ only by case would collide on Windows and macOS, so the second fails
    /// with `kind` `"unsafe"`.
    ///
    /// Args:
    ///     path: The folder to extract into. Defaults to the current directory.
    ///     members: Names or `ZipInfo` objects to extract. Defaults to every member.
    ///     workers: How many members to download at once. 0 counts as 1.
    ///
    /// Raises:
    ///     ExtractError: Some members couldn't be extracted; see its `failures`.
    ///     KeyError: A name in `members` isn't in the archive. Raised before anything is written.
    ///     OSError: `path` couldn't be created.
    ///     ValueError: The archive is closed.
    ///     KeyboardInterrupt: Ctrl-C was pressed (main thread only). Downloads stop, and
    ///         partly written files are removed before this is raised.
    #[pyo3(
        signature = (path=None, members=None, workers=DEFAULT_WORKERS),
        text_signature = "($self, path=None, members=None, workers=8)"
    )]
    fn extractall(
        &self,
        py: Python<'_>,
        path: Option<PathBuf>,
        members: Option<Vec<Member>>,
        workers: usize,
    ) -> PyResult<()> {
        let opened = self.opened()?;
        let root = path.map_or_else(current_dir, Ok)?;
        let selected: Vec<usize> = match members {
            Some(members) => members
                .iter()
                .map(|member| opened.resolve(member))
                .collect::<PyResult<_>>()?,
            None => (0..opened.names.len()).collect(),
        };
        // When a name repeats, the last selected entry is written, as if each overwrote the last.
        let mut chosen: HashMap<String, usize> = HashMap::with_capacity(selected.len());
        let mut names = Vec::with_capacity(selected.len());
        for index in selected {
            let name = &opened.names[index];
            if chosen.insert(name.clone(), index).is_none() {
                names.push(name.clone());
            }
        }
        let total = names.len();
        let plan = plan_selection(&root, "", names);
        let failures: Arc<Mutex<Vec<(String, ErrorKind, String)>>> = Arc::new(Mutex::new(
            plan.rejected
                .into_iter()
                .map(|file| (file.path, file.kind, file.error))
                .collect(),
        ));

        let cancel = CancellationToken::new();
        let work = {
            let (opened, failures, cancel) = (opened.clone(), failures.clone(), cancel.clone());
            async move {
                tokio::fs::create_dir_all(&root).await?;
                let mut files = Vec::with_capacity(plan.files.len());
                for mut file in plan.files {
                    let index = chosen[&file.zip_path];
                    if !opened.entry(index).is_dir {
                        file.index = Some(index);
                        files.push(file);
                    } else if let Err(error) = make_dir(&root, &file.dest).await {
                        lock(&failures).push((file.zip_path, error.kind(), error.to_string()));
                    }
                }
                let options = BatchOptions {
                    concurrency: workers,
                    cancel,
                    root: Some(root),
                    reader: opened.config.clone(),
                };
                download_from(&opened.archive, files, options, |event| {
                    if let BatchEvent::FileFailed { path, error, kind } = event {
                        lock(&failures).push((path.to_string(), kind, error));
                    }
                })
                .await;
                Ok::<(), Error>(())
            }
        };
        wait(py, Some(&cancel), work)?.map_err(to_py_err)?;

        let mut failures = std::mem::take(&mut *lock(&failures));
        if failures.is_empty() {
            return Ok(());
        }
        failures.sort_by(|a, b| a.0.cmp(&b.0));
        let mut message = format!("{} of {total} files couldn't be extracted:", failures.len());
        for (name, _, error) in failures.iter().take(LISTED_FAILURES) {
            message.push_str(&format!("\n  {name}: {error}"));
        }
        if failures.len() > LISTED_FAILURES {
            message.push_str(&format!(
                "\n  …and {} more",
                failures.len() - LISTED_FAILURES
            ));
        }
        let error = ExtractError::new_err(message);
        let details = PyDict::new(py);
        for (name, kind, reason) in failures {
            let exception = with_kind(py, exception_for(kind, reason), kind);
            details.set_item(name, exception.into_value(py))?;
        }
        error.value(py).setattr("failures", details)?;
        Err(error)
    }

    /// Reads every member and returns the name of the first corrupt one, or `None`.
    ///
    /// Members are read several at a time and checked against their size and CRC, which
    /// downloads about the whole archive. "First" is in archive order. A member that looks
    /// malicious counts as corrupt.
    ///
    /// Returns:
    ///     The first corrupt member's name, or `None` if every member is intact.
    ///
    /// Raises:
    ///     NotImplementedError: A member is encrypted or uses an unsupported compression method.
    ///     ConnectionError: A download failed, timed out, or the archive changed on the server.
    ///     ValueError: The archive is closed.
    fn testzip(&self, py: Python<'_>) -> PyResult<Option<String>> {
        let opened = self.opened()?;
        wait(py, None, first_bad_entry(opened))?.map_err(to_py_err)
    }

    /// Streams a member to exactly `filepath`, whatever the member is called.
    ///
    /// Like `extract()`, the data goes to a temporary file that replaces `filepath` only once
    /// it has been verified. The member's name isn't used for the destination, so it isn't
    /// checked for safety; `filepath` is trusted as given. Its folder is created if needed.
    ///
    /// Args:
    ///     path: A member name, or a `ZipInfo`.
    ///     filepath: Where to write the data.
    ///
    /// Raises:
    ///     KeyError: No member has that name.
    ///     BadZipFile: The member is corrupt or looks malicious.
    ///     NotImplementedError: The member is encrypted or uses an unsupported compression method.
    ///     ConnectionError: The download failed, timed out, or the archive changed on the server.
    ///     OSError: The file couldn't be written.
    ///     ValueError: The archive is closed.
    fn read_to_file(&self, py: Python<'_>, path: Member, filepath: PathBuf) -> PyResult<()> {
        let opened = self.opened()?;
        let index = opened.resolve(&path)?;
        wait(py, None, async move {
            write_entry(&opened, index, &filepath, None).await
        })?
        .map_err(to_py_err)
    }

    /// The top level of the archive as a tree of `FileEntry` objects, sorted by name.
    ///
    /// Built from the directory on first use and cached, with no requests. Entries that can't be
    /// written safely, or whose names aren't valid UTF-8, are left out.
    ///
    /// Raises:
    ///     ValueError: The archive is closed.
    #[getter]
    fn files(&self, py: Python<'_>) -> PyResult<Vec<FileEntry>> {
        if let Some(files) = lock(&self.files).as_ref() {
            return Ok(files.clone());
        }
        self.load(py)?;
        Ok(lock(&self.files).clone().unwrap_or_default())
    }

    /// Builds the tree behind `files` now, rather than on first use. Makes no requests.
    ///
    /// Raises:
    ///     ValueError: The archive is closed.
    fn load(&self, py: Python<'_>) -> PyResult<()> {
        let opened = self.opened()?;
        let files: Vec<FileEntry> = py.detach(|| {
            let tree = scan(&opened.archive).tree();
            tree.iter().map(FileEntry::from).collect()
        });
        *lock(&self.files) = Some(files);
        Ok(())
    }

    /// Releases the archive's directory. Calling it again does nothing.
    ///
    /// Afterwards, other methods raise `ValueError`. Files already opened with `open()` stay
    /// readable and seekable, and calls already running on other threads finish normally.
    fn close(&self) {
        lock(&self.opened).take();
        lock(&self.files).take();
    }
}

enum StreamState {
    Ready(EntryReader),
    /// A read or seek is in progress on another thread.
    Busy,
    /// The whole member has been read and verified.
    Finished,
    Failed,
    Closed,
}

/// What a read or seek starts from. The state is `Busy` until it's put back.
enum Taken {
    Reader(EntryReader),
    Finished,
}

/// The raw, unbuffered data of one member. Use `RemoteZip.open()`, which buffers it.
///
/// It stays usable after its archive is closed. Only one read or seek can run at a time; a
/// concurrent one raises `RuntimeError`. After a read or seek fails, every later one raises
/// `OSError`, so open the member again.
#[pyclass(frozen, module = "lazy_zip")]
pub struct EntryStream {
    /// Kept so a backward seek can reopen the member after the archive is closed.
    opened: Arc<Opened>,
    index: usize,
    size: u64,
    /// Bytes of the member returned or skipped so far.
    position: AtomicU64,
    state: Mutex<StreamState>,
}

impl EntryStream {
    fn take(&self) -> PyResult<Taken> {
        let mut state = lock(&self.state);
        match std::mem::replace(&mut *state, StreamState::Busy) {
            StreamState::Ready(reader) => Ok(Taken::Reader(reader)),
            StreamState::Finished => Ok(Taken::Finished),
            StreamState::Busy => Err(PyRuntimeError::new_err(
                "The file is being read by another thread",
            )),
            other => {
                let error = match other {
                    StreamState::Failed => {
                        PyOSError::new_err("An earlier read of this file failed")
                    }
                    _ => PyValueError::new_err("I/O operation on closed file."),
                };
                *state = other;
                Err(error)
            }
        }
    }

    fn put_back(&self, next: StreamState) {
        let mut state = lock(&self.state);
        // A close() during the read wins.
        if !matches!(*state, StreamState::Closed) {
            *state = next;
        }
    }
}

async fn read_chunk(
    mut reader: EntryReader,
    size: usize,
) -> (EntryReader, lazy_zip_core::Result<(Vec<u8>, bool)>) {
    let mut buffer = vec![0u8; size];
    let mut filled = 0;
    let result = loop {
        if filled == size {
            break Ok(false);
        }
        match reader.read(&mut buffer[filled..]).await {
            Ok(0) => break Ok(true),
            Ok(n) => filled += n,
            Err(error) => break Err(error),
        }
    };
    buffer.truncate(filled);
    (reader, result.map(|finished| (buffer, finished)))
}

/// Moves to `target` by reading and discarding. Going backward, or from a finished reader, opens
/// the member again from the start. Every byte still passes through the reader, so the checksum
/// is checked as usual when the end is read. Returns the reader and the position reached.
async fn seek_to(
    opened: Arc<Opened>,
    index: usize,
    reader: Option<EntryReader>,
    position: u64,
    target: u64,
) -> lazy_zip_core::Result<(EntryReader, u64)> {
    let (mut reader, mut position) = match reader {
        Some(reader) if target >= position => (reader, position),
        stale => {
            drop(stale);
            (opened.open_entry(index).await?, 0)
        }
    };
    let mut buffer = vec![0u8; READ_BUFFER_BYTES];
    while position < target {
        let wanted =
            usize::try_from(target - position).map_or(buffer.len(), |n| n.min(buffer.len()));
        let n = reader.read(&mut buffer[..wanted]).await?;
        if n == 0 {
            break;
        }
        position += n as u64;
    }
    Ok((reader, position))
}

#[pymethods]
impl EntryStream {
    /// Reads up to `size` bytes and returns them.
    ///
    /// Returns `b""` only once the whole member has been read and its size and CRC verified;
    /// the read that reaches the end raises `BadZipFile` instead if they don't match.
    ///
    /// Args:
    ///     size: The most bytes to return. Negative means 64 KiB. At most 1 MiB is returned.
    ///
    /// Raises:
    ///     BadZipFile: The data is corrupt or breaks a limit the directory set.
    ///     ConnectionError: The download failed or timed out.
    ///     ValueError: The stream is closed.
    ///     OSError: An earlier read or seek failed.
    ///     RuntimeError: Another thread is reading or seeking.
    #[pyo3(signature = (size=-1), text_signature = "($self, size=-1)")]
    fn read<'py>(&self, py: Python<'py>, size: isize) -> PyResult<Bound<'py, PyBytes>> {
        let size =
            usize::try_from(size).map_or(READ_BUFFER_BYTES, |n| n.min(MAX_STREAM_READ_BYTES));
        let reader = match self.take()? {
            Taken::Reader(reader) => reader,
            Taken::Finished => {
                self.put_back(StreamState::Finished);
                return Ok(PyBytes::new(py, b""));
            }
        };
        if size == 0 {
            self.put_back(StreamState::Ready(reader));
            return Ok(PyBytes::new(py, b""));
        }
        match wait(py, None, read_chunk(reader, size)) {
            Ok((reader, Ok((data, finished)))) => {
                self.position
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
                self.put_back(if finished {
                    StreamState::Finished
                } else {
                    StreamState::Ready(reader)
                });
                Ok(PyBytes::new(py, &data))
            }
            Ok((_, Err(error))) => {
                self.put_back(StreamState::Failed);
                Err(to_py_err(error))
            }
            Err(error) => {
                self.put_back(StreamState::Failed);
                Err(error)
            }
        }
    }

    /// Moves to a new position in the uncompressed data and returns it.
    ///
    /// The target is clamped to the member, like `zipfile`. A forward seek downloads and
    /// discards the data it skips; a backward seek opens the member again and reads from its
    /// start. Either way every byte passes through the CRC check, which still runs when the
    /// end is read. Seeking to the current position costs nothing.
    ///
    /// Args:
    ///     offset: The position, relative to `whence`.
    ///     whence: 0 for the start, 1 for the current position, 2 for the end.
    ///
    /// Raises:
    ///     ValueError: `whence` isn't 0, 1 or 2, or the stream is closed.
    ///     BadZipFile: The data is corrupt or breaks a limit the directory set.
    ///     ConnectionError: The download failed or timed out.
    ///     OSError: An earlier read or seek failed.
    ///     RuntimeError: Another thread is reading or seeking.
    #[pyo3(signature = (offset, whence=0))]
    fn seek(&self, py: Python<'_>, offset: i64, whence: i32) -> PyResult<u64> {
        if !(0..=2).contains(&whence) {
            return Err(PyValueError::new_err(format!(
                "invalid whence ({whence}, should be 0, 1 or 2)"
            )));
        }
        let taken = self.take()?;
        let position = self.position.load(Ordering::Relaxed);
        let base = match whence {
            0 => 0,
            1 => i128::from(position),
            _ => i128::from(self.size),
        };
        let target = (base + i128::from(offset)).clamp(0, i128::from(self.size)) as u64;
        let reader = match taken {
            Taken::Reader(reader) if target == position => {
                self.put_back(StreamState::Ready(reader));
                return Ok(position);
            }
            Taken::Finished if target == position => {
                self.put_back(StreamState::Finished);
                return Ok(position);
            }
            Taken::Reader(reader) => Some(reader),
            Taken::Finished => None,
        };
        let opened = self.opened.clone();
        match wait(
            py,
            None,
            seek_to(opened, self.index, reader, position, target),
        ) {
            Ok(Ok((reader, reached))) => {
                self.position.store(reached, Ordering::Relaxed);
                self.put_back(StreamState::Ready(reader));
                Ok(reached)
            }
            Ok(Err(error)) => {
                self.put_back(StreamState::Failed);
                Err(to_py_err(error))
            }
            Err(error) => {
                self.put_back(StreamState::Failed);
                Err(error)
            }
        }
    }

    /// Returns the current position in the uncompressed data.
    fn tell(&self) -> u64 {
        self.position.load(Ordering::Relaxed)
    }

    /// The member's uncompressed size in bytes, as the directory claims.
    #[getter]
    fn size(&self) -> u64 {
        self.size
    }

    /// Whether `close()` has been called.
    #[getter]
    fn closed(&self) -> bool {
        matches!(*lock(&self.state), StreamState::Closed)
    }

    /// Closes the stream and its connection. Calling it again does nothing.
    fn close(&self) {
        let previous = std::mem::replace(&mut *lock(&self.state), StreamState::Closed);
        if let Ok(runtime) = runtime() {
            let _guard = runtime.enter();
            drop(previous);
        }
    }
}

impl Drop for EntryStream {
    fn drop(&mut self) {
        self.close();
    }
}

/// The native part of `lazy_zip`. Import from `lazy_zip` instead.
#[pymodule]
#[pyo3(name = "_lazy_zip")]
fn lazy_zip_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add_class::<RemoteZip>()?;
    m.add_class::<ZipInfo>()?;
    m.add_class::<FileEntry>()?;
    m.add_class::<EntryStream>()?;
    m.add("BadZipFile", py.get_type::<BadZipFile>())?;
    m.add("ExtractError", py.get_type::<ExtractError>())?;
    Ok(())
}
