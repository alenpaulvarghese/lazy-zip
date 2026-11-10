//! Reads a remote file with HTTP Range requests, defending against a server that misbehaves.
//!
//! Most code only needs [`ReaderConfig`], which says how to connect, and passes it to
//! [`RemoteArchive::open`](crate::archive::RemoteArchive::open). The lower-level types read
//! arbitrary byte ranges:
//!
//! - [`RemoteFile`] fetches and keeps the end of the file, and finds its length, with one request.
//! - [`RemoteHttpReader`] is an `AsyncRead + AsyncSeek` view of it. Seeking costs nothing, reads
//!   inside the kept end make no request, and other reads stream one range from the position up
//!   to the reader's limit (see [`RemoteHttpReader::limit_to`]). A dropped connection is resumed
//!   from the exact byte where it stopped.
//!
//! Every response must be `206 Partial Content` for exactly the range asked for, uncompressed,
//! no longer than asked for, and of the same file: the length and the `ETag` or `Last-Modified`
//! must not change. Otherwise the read fails with an [`Error`] rather than returning wrong data.

use crate::archive_limits::{check_end_records, MAX_DIRECTORY_BYTES, TAIL_BYTES};
use crate::error::{Error, Result};
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use futures::task::{Context, Poll};
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_ENCODING,
    CONTENT_RANGE, COOKIE, PROXY_AUTHORIZATION, RANGE,
};
use reqwest::{redirect, Client, Response, StatusCode};
use std::cmp::min;
use std::io::SeekFrom;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};

const USER_AGENT: &str = concat!("lazy-zip/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CHUNK_BYTES: u64 = 1024 * 1024;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
/// The same limit as reqwest's default policy.
const MAX_REDIRECTS: usize = 10;

/// How failed requests are retried.
///
/// Connection failures, timeouts, truncated bodies and the statuses 408, 425, 429, 500, 502, 503
/// and 504 are retried. The wait doubles from `base_delay` up to `max_delay`, with up to 25%
/// jitter, unless the server sends `Retry-After` in seconds, which is honored up to 30 seconds.
/// The default is 4 attempts, from 250 ms up to 10 s.
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    /// Total tries for each request, including the first. While a range is streamed, this counts
    /// failures since data last arrived, so a long download survives many interruptions.
    pub max_attempts: u32,
    /// The wait before the first retry.
    pub base_delay: Duration,
    /// The longest wait between tries, before jitter.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(10),
        }
    }
}

impl RetryPolicy {
    fn delay(&self, attempt: u32) -> Duration {
        let doubled = self.base_delay.saturating_mul(1 << attempt.min(16));
        let delay = min(doubled, self.max_delay);
        // Up to 25% jitter, so parallel downloads don't all retry at the same moment.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |time| time.subsec_nanos());
        delay + delay.mul_f64(f64::from(nanos % 1000) / 4000.0)
    }
}

/// How readers connect. Cloning shares the connection pool and the network byte count.
///
/// The constructors build a client with a `lazy-zip/<version>` user agent, a connect timeout and
/// a read timeout (15 s and 30 s by default), and no overall timeout, since large downloads
/// legitimately take a long time. The fields are public so a caller can adjust them.
///
/// # Examples
///
/// ```
/// use lazy_zip_core::http_reader::ReaderConfig;
/// use std::sync::atomic::Ordering;
/// use std::sync::Arc;
///
/// let shared = ReaderConfig::new()?;
///
/// // The same connection pool, with network use counted separately.
/// let mut job = shared.clone();
/// job.network_bytes = Arc::default();
/// assert_eq!(job.network_bytes.load(Ordering::Relaxed), 0);
/// # Ok::<(), lazy_zip_core::Error>(())
/// ```
#[derive(Clone)]
pub struct ReaderConfig {
    /// The HTTP client. Replacing it, for example to use a proxy, also replaces the timeouts,
    /// headers and redirect rules the constructors set up. It must be a `reqwest` 0.13 client.
    pub client: Client,
    /// How failed requests are retried.
    pub retry: RetryPolicy,
    /// The most each request asks for while the fetch limit is in effect, which is while the
    /// directory is parsed. The directory the end records claim is the exception, and is asked for
    /// in one request. Once the limit is lifted, a read asks for everything up to the reader's
    /// limit in one request, and receives it as the consumer reads.
    pub chunk_bytes: u64,
    /// Bytes received over the network by every reader using this config, including data that
    /// was never read. Clones share it; replace it with a new counter to count separately.
    pub network_bytes: Arc<AtomicU64>,
}

impl ReaderConfig {
    /// The default config: a 15 s connect timeout, a 30 s read timeout, and no custom headers.
    ///
    /// # Errors
    ///
    /// Fails only if the HTTP client can't be built, such as when TLS can't be initialized.
    pub fn new() -> Result<Self> {
        Self::with_timeouts(CONNECT_TIMEOUT, READ_TIMEOUT)
    }

    /// A config with custom timeouts. The read timeout applies to each wait for data, not to a
    /// whole download.
    ///
    /// # Errors
    ///
    /// Fails only if the HTTP client can't be built.
    pub fn with_timeouts(connect: Duration, read: Duration) -> Result<Self> {
        Self::build(connect, read, HeaderMap::new())
    }

    /// Sends `headers` with every request, for servers that need a login or a token.
    ///
    /// The client sets `Range` and `Accept-Encoding` itself, so those are refused. Headers other
    /// than `Authorization` and cookies aren't removed on a redirect to another site, so such a
    /// redirect fails instead of handing them over.
    ///
    /// Names and values are trimmed, and values are marked sensitive so they're left out of debug
    /// output.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidHeader`], holding the name but never the value, for an invalid name or
    /// value, or for `Range` or `Accept-Encoding`.
    ///
    /// # Examples
    ///
    /// ```
    /// use lazy_zip_core::http_reader::ReaderConfig;
    /// use lazy_zip_core::ErrorKind;
    ///
    /// let config = ReaderConfig::with_headers([("Authorization", "Bearer my-token")])?;
    ///
    /// let refused = ReaderConfig::with_headers([("Range", "bytes=0-")]).err().unwrap();
    /// assert_eq!(refused.kind(), ErrorKind::InvalidHeader);
    /// # Ok::<(), lazy_zip_core::Error>(())
    /// ```
    pub fn with_headers<'a>(headers: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<Self> {
        Self::with_options(CONNECT_TIMEOUT, READ_TIMEOUT, headers)
    }

    /// Both custom timeouts and the headers of [`Self::with_headers`].
    ///
    /// # Errors
    ///
    /// The same as [`Self::with_headers`].
    pub fn with_options<'a>(
        connect: Duration,
        read: Duration,
        headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self> {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            let invalid = || Error::InvalidHeader(name.to_string());
            let name = HeaderName::from_bytes(name.trim().as_bytes()).map_err(|_| invalid())?;
            if name == RANGE || name == ACCEPT_ENCODING {
                return Err(invalid());
            }
            let mut value = HeaderValue::from_str(value.trim()).map_err(|_| invalid())?;
            value.set_sensitive(true);
            map.append(name, value);
        }
        Self::build(connect, read, map)
    }

    fn build(connect: Duration, read: Duration, headers: HeaderMap) -> Result<Self> {
        let only_stripped_on_redirect = headers
            .keys()
            .all(|name| [AUTHORIZATION, COOKIE, PROXY_AUTHORIZATION].contains(name));
        let redirects = if only_stripped_on_redirect {
            redirect::Policy::default()
        } else {
            redirect::Policy::custom(|attempt| {
                let previous = attempt.previous().last().map(|url| url.origin());
                if attempt.previous().len() >= MAX_REDIRECTS {
                    attempt.error(RedirectRefused("too many redirects"))
                } else if previous.is_some_and(|origin| origin != attempt.url().origin()) {
                    attempt.error(RedirectRefused(
                        "the server redirected to another site, which isn't sent your headers",
                    ))
                } else {
                    attempt.follow()
                }
            })
        };
        // There is no overall timeout, because large downloads legitimately take a long time.
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(connect)
            .read_timeout(read)
            .default_headers(headers)
            .redirect(redirects)
            .build()?;
        Ok(Self {
            client,
            retry: RetryPolicy::default(),
            chunk_bytes: DEFAULT_CHUNK_BYTES,
            network_bytes: Arc::default(),
        })
    }
}

#[derive(Debug)]
struct RedirectRefused(&'static str);

impl std::fmt::Display for RedirectRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for RedirectRefused {}

/// Identifies one version of the remote file.
#[derive(Clone, Debug, PartialEq)]
enum Validator {
    ETag(String),
    LastModified(String),
}

impl Validator {
    fn from_headers(headers: &HeaderMap) -> Option<Self> {
        let text = |name| headers.get(name)?.to_str().ok().map(str::to_string);
        text(reqwest::header::ETAG)
            .map(Validator::ETag)
            .or_else(|| text(reqwest::header::LAST_MODIFIED).map(Validator::LastModified))
    }
}

struct Remote {
    config: ReaderConfig,
    url: String,
    len: u64,
    validator: Option<Validator>,
    /// Where the archive's directory starts, as claimed by the end records that were checked.
    directory_offset: u64,
    /// Where the claimed directory ends, or the tail starts if that's sooner.
    directory_end: u64,
    /// Bytes requested while the limit is in effect, and the most that may be requested.
    fetched: AtomicU64,
    fetch_limit: AtomicU64,
}

/// Parses `bytes start-end/total`.
fn parse_content_range(headers: &HeaderMap) -> Result<(u64, u64, u64)> {
    let invalid = Error::InvalidResponse("missing or malformed Content-Range header");
    let value = headers
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes "));
    let parsed = value.and_then(|value| {
        let (range, total) = value.split_once('/')?;
        let (start, end) = range.split_once('-')?;
        Some((
            start.trim().parse().ok()?,
            end.trim().parse().ok()?,
            total.trim().parse().ok()?,
        ))
    });
    match parsed {
        Some((start, end, total)) if start <= end && end < total => Ok((start, end, total)),
        _ => Err(invalid),
    }
}

fn check_not_encoded(headers: &HeaderMap) -> Result<()> {
    match headers.get(CONTENT_ENCODING).map(|value| value.as_bytes()) {
        None | Some(b"identity") => Ok(()),
        Some(_) => Err(Error::InvalidResponse(
            "the server compressed a partial response",
        )),
    }
}

fn is_retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

fn is_retryable(error: &Error) -> bool {
    match error {
        Error::Http(e) => e.is_connect() || e.is_timeout() || e.is_body() || e.is_request(),
        Error::HttpStatus { status } => StatusCode::from_u16(*status)
            .map(is_retryable_status)
            .unwrap_or(false),
        Error::TruncatedResponse => true,
        _ => false,
    }
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let seconds: u64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(min(Duration::from_secs(seconds), MAX_RETRY_AFTER))
}

/// Reads a response body, stopping as soon as it's longer than `expected`.
async fn read_body(
    mut response: Response,
    expected: u64,
    network_bytes: &AtomicU64,
) -> Result<Vec<u8>> {
    let mut body = Vec::with_capacity(expected as usize);
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(e) if e.is_timeout() => return Err(e.into()),
            Err(_) => return Err(Error::TruncatedResponse),
        };
        network_bytes.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        if body.len() as u64 + chunk.len() as u64 > expected {
            return Err(Error::InvalidResponse("more data than was requested"));
        }
        body.extend_from_slice(&chunk);
    }
    if (body.len() as u64) < expected {
        return Err(Error::TruncatedResponse);
    }
    Ok(body)
}

/// Runs `attempt` under the retry policy. A failed attempt can ask for a specific wait.
async fn with_retries<T, F, Fut>(policy: &RetryPolicy, mut attempt: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, (Error, Option<Duration>)>>,
{
    let mut tries = 0;
    loop {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err((error, wait)) => {
                tries += 1;
                if tries >= policy.max_attempts || !is_retryable(&error) {
                    return Err(error);
                }
                tokio::time::sleep(wait.unwrap_or_else(|| policy.delay(tries - 1))).await;
            }
        }
    }
}

impl Remote {
    fn is_limited(&self) -> bool {
        self.fetch_limit.load(Ordering::Relaxed) != u64::MAX
    }

    /// Counts `len` bytes against the fetch limit before they're requested.
    fn reserve(&self, len: u64) -> Result<()> {
        let limit = self.fetch_limit.load(Ordering::Relaxed);
        if limit == u64::MAX {
            return Ok(());
        }
        if self.fetched.load(Ordering::Relaxed).saturating_add(len) > limit {
            return Err(Error::LimitExceeded(
                "reading its directory used too much data",
            ));
        }
        self.fetched.fetch_add(len, Ordering::Relaxed);
        Ok(())
    }

    /// Requests `start..end` and checks everything but the body. A server may legally send less
    /// than was asked for, so this also returns where the response's data ends.
    async fn request(
        &self,
        start: u64,
        end: u64,
    ) -> std::result::Result<(Response, u64), (Error, Option<Duration>)> {
        let response = self
            .config
            .client
            .get(&self.url)
            .header(RANGE, format!("bytes={}-{}", start, end - 1))
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|e| (Error::from(e), None))?;

        let status = response.status();
        let headers = response.headers();
        if status != StatusCode::PARTIAL_CONTENT {
            if status.is_success() {
                // Dropping the response closes the connection without reading the body.
                let changed =
                    self.validator.is_some() && self.validator != Validator::from_headers(headers);
                let error = if changed {
                    Error::RemoteChanged
                } else {
                    Error::RangeNotSupported
                };
                return Err((error, None));
            }
            let wait = retry_after(headers);
            return Err((
                Error::HttpStatus {
                    status: status.as_u16(),
                },
                wait,
            ));
        }

        let checked = (|| {
            check_not_encoded(headers)?;
            if let (Some(known), Some(current)) =
                (&self.validator, Validator::from_headers(headers))
            {
                if *known != current {
                    return Err(Error::RemoteChanged);
                }
            }
            let (got_start, got_end, total) = parse_content_range(headers)?;
            if total != self.len {
                return Err(Error::RemoteChanged);
            }
            if got_start != start || got_end >= end {
                return Err(Error::InvalidResponse(
                    "the data isn't the part that was requested",
                ));
            }
            // Refused before any of the body arrives, when the server says up front it's too long.
            if response
                .content_length()
                .is_some_and(|length| length > got_end - got_start + 1)
            {
                return Err(Error::InvalidResponse("more data than was requested"));
            }
            Ok(got_end + 1)
        })();
        let response_end = checked.map_err(|e| (e, None))?;
        Ok((response, response_end))
    }
}

/// One range of the file, received as it arrives. When a response fails or ends early, the rest
/// is requested again from where it stopped, until the retry policy gives up on making progress.
struct RangeStream {
    remote: Arc<Remote>,
    network_bytes: Arc<AtomicU64>,
    /// The next byte to receive.
    pos: u64,
    end: u64,
    /// The response being read, and where its data ends.
    response: Option<(Response, u64)>,
}

impl RangeStream {
    fn into_chunks(self) -> BoxStream<'static, Result<Bytes>> {
        stream::unfold(Some(self), |range| async move {
            let mut range = range?;
            match range.next_chunk().await {
                Ok(Some(chunk)) => Some((Ok(chunk), Some(range))),
                Ok(None) => None,
                Err(error) => Some((Err(error), None)),
            }
        })
        .boxed()
    }

    /// The next part of the range, or `None` once all of it has arrived.
    async fn next_chunk(&mut self) -> Result<Option<Bytes>> {
        let policy = self.remote.config.retry.clone();
        // Counts only failures since data last arrived, so a long download survives many.
        let mut failures = 0;
        loop {
            if self.pos >= self.end {
                return Ok(None);
            }
            match self.advance().await {
                Ok(Some(chunk)) => return Ok(Some(chunk)),
                Ok(None) => {}
                Err((error, wait)) => {
                    self.response = None;
                    failures += 1;
                    if failures >= policy.max_attempts || !is_retryable(&error) {
                        return Err(error);
                    }
                    tokio::time::sleep(wait.unwrap_or_else(|| policy.delay(failures - 1))).await;
                }
            }
        }
    }

    /// Connects, or receives what comes next on the connection, which may be no data.
    async fn advance(&mut self) -> std::result::Result<Option<Bytes>, (Error, Option<Duration>)> {
        let Some((response, response_end)) = &mut self.response else {
            self.response = Some(self.remote.request(self.pos, self.end).await?);
            return Ok(None);
        };
        let response_end = *response_end;
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) if self.pos < response_end => return Err((Error::TruncatedResponse, None)),
            Ok(None) => {
                self.response = None;
                return Ok(None);
            }
            Err(e) if e.is_timeout() => return Err((e.into(), None)),
            Err(_) => return Err((Error::TruncatedResponse, None)),
        };
        let len = chunk.len() as u64;
        self.network_bytes.fetch_add(len, Ordering::Relaxed);
        if self.pos + len > response_end {
            return Err((Error::InvalidResponse("more data than was requested"), None));
        }
        self.pos += len;
        Ok((!chunk.is_empty()).then_some(chunk))
    }
}

/// Finds the file's length and fetches its end. Not every server that supports ranges supports a
/// suffix range (GitHub release assets answer 501), so this falls back to an explicit one.
async fn open_tail(config: &ReaderConfig, url: &str) -> Result<(u64, Bytes, Option<Validator>)> {
    let get = |range: String, passes: fn(StatusCode) -> bool| async move {
        let response = config
            .client
            .get(url)
            .header(RANGE, range)
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|e| (Error::from(e), None))?;
        let status = response.status();
        if status.is_success() || passes(status) {
            return Ok(response);
        }
        Err((
            Error::HttpStatus {
                status: status.as_u16(),
            },
            retry_after(response.headers()),
        ))
    };

    let suffix = with_retries(&config.retry, || {
        get(format!("bytes=-{TAIL_BYTES}"), |status| {
            matches!(status.as_u16(), 400 | 416 | 501)
        })
    })
    .await?;
    let response = if suffix.status() == StatusCode::PARTIAL_CONTENT {
        suffix
    } else {
        drop(suffix);
        let probe = with_retries(&config.retry, || {
            get("bytes=0-0".to_string(), |status| {
                status == StatusCode::RANGE_NOT_SATISFIABLE
            })
        })
        .await?;
        match probe.status() {
            StatusCode::PARTIAL_CONTENT => {}
            // An empty file can't satisfy any range.
            StatusCode::RANGE_NOT_SATISFIABLE => return Err(Error::NotAZip),
            _ => return Err(Error::RangeNotSupported),
        }
        let (_, _, total) = parse_content_range(probe.headers())?;
        drop(probe);
        let start = total.saturating_sub(TAIL_BYTES);
        with_retries(&config.retry, || {
            get(format!("bytes={start}-{}", total - 1), |_| false)
        })
        .await?
    };

    if response.status() != StatusCode::PARTIAL_CONTENT {
        return Err(Error::RangeNotSupported);
    }
    check_not_encoded(response.headers())?;
    let validator = Validator::from_headers(response.headers());
    let (start, end, total) = parse_content_range(response.headers())?;
    if end + 1 != total || end - start + 1 > TAIL_BYTES {
        return Err(Error::InvalidResponse(
            "the data isn't the part that was requested",
        ));
    }
    let body = read_body(response, end - start + 1, &config.network_bytes).await?;
    Ok((total, Bytes::from(body), validator))
}

/// Lets directory parsing fetch only a bounded amount of data, until the limit is lifted.
///
/// A new [`RemoteFile`] starts limited to [`MAX_DIRECTORY_BYTES`] plus twice [`TAIL_BYTES`] in
/// total, and while limited, each request is at most [`ReaderConfig::chunk_bytes`] except the
/// one for the claimed directory. [`RemoteArchive`](crate::archive::RemoteArchive) lifts it once
/// the directory has been parsed.
pub struct FetchLimit(Arc<Remote>);

impl FetchLimit {
    /// Removes the limit for every reader of the file.
    pub fn lift(&self) {
        self.0.fetch_limit.store(u64::MAX, Ordering::Relaxed);
    }
}

/// A remote file whose end has been fetched and checked. It's cheap to clone, and every reader
/// made from it shares the fetched end and the connection pool.
#[derive(Clone)]
pub struct RemoteFile {
    remote: Arc<Remote>,
    /// The end of the file, kept for the file's whole life so the end records that were checked
    /// are always the ones parsed.
    tail: Bytes,
    tail_start: u64,
}

impl RemoteFile {
    /// Fetches the last [`TAIL_BYTES`] of the file, finds its length, and checks its end records.
    ///
    /// This is one request, or three when the server doesn't support suffix ranges. Nothing else
    /// is fetched.
    ///
    /// # Errors
    ///
    /// [`Error::RangeNotSupported`] if the server sends the whole file, [`Error::NotAZip`] or
    /// [`Error::LimitExceeded`] if the end records fail
    /// [`check_directory`](crate::archive_limits::check_directory), and network errors.
    pub async fn open(url: &str, config: ReaderConfig) -> Result<Self> {
        let (len, tail, validator) = open_tail(&config, url).await?;
        let directory = check_end_records(&tail, len)?;

        let tail_start = len - tail.len() as u64;
        let remote = Remote {
            config,
            url: url.to_string(),
            len,
            validator,
            directory_offset: directory.offset,
            directory_end: min(directory.end(), tail_start),
            fetched: AtomicU64::new(tail.len() as u64),
            fetch_limit: AtomicU64::new(MAX_DIRECTORY_BYTES.saturating_add(TAIL_BYTES * 2)),
        };
        Ok(Self {
            remote: Arc::new(remote),
            tail,
            tail_start,
        })
    }

    /// A reader whose network use is counted with the file's own.
    pub fn reader(&self) -> RemoteHttpReader {
        self.reader_counting(self.network_counter())
    }

    /// A reader whose network use is counted in `network_bytes`.
    pub fn reader_counting(&self, network_bytes: Arc<AtomicU64>) -> RemoteHttpReader {
        RemoteHttpReader {
            file: self.clone(),
            network_bytes,
            pos: 0,
            bytes_read: 0,
            end: self.remote.len,
            state: Some(State::Idle),
        }
    }

    /// Handle for lifting the limit on fetched data, once the directory has been parsed.
    pub fn fetch_limit(&self) -> FetchLimit {
        FetchLimit(self.remote.clone())
    }

    /// The counter of the config the file was opened with.
    pub fn network_counter(&self) -> Arc<AtomicU64> {
        self.remote.config.network_bytes.clone()
    }

    /// The file's length in bytes.
    pub fn len(&self) -> u64 {
        self.remote.len
    }

    /// Whether the file is empty. An open file never is, since it has end records.
    pub fn is_empty(&self) -> bool {
        self.remote.len == 0
    }

    /// Where the archive's directory starts, as claimed by the checked end records.
    pub fn directory_offset(&self) -> u64 {
        self.remote.directory_offset
    }

    /// The URL the file was opened from.
    pub fn url(&self) -> &str {
        &self.remote.url
    }
}

/// An `AsyncRead + AsyncSeek` reader over a [`RemoteFile`], starting at byte 0.
///
/// Seeking is free. Reading in the file's kept end makes no request. Any other read streams one
/// range, from the position to the reader's limit or the start of the kept end, and later reads
/// continue it while they stay sequential. Seeking elsewhere closes it, and the next read starts a
/// new one.
///
/// ```no_run
/// use lazy_zip_core::http_reader::{ReaderConfig, RemoteHttpReader};
/// use std::io::SeekFrom;
/// use tokio::io::{AsyncReadExt, AsyncSeekExt};
///
/// # async fn example() -> lazy_zip_core::Result<()> {
/// let config = ReaderConfig::new()?;
/// let mut reader = RemoteHttpReader::open("https://example.com/data.zip", config).await?;
/// // The limit exists for parsing the directory, which this doesn't do.
/// reader.fetch_limit().lift();
///
/// // Bytes 1000..1100, in one request for exactly those bytes.
/// reader.limit_to(1100);
/// reader.seek(SeekFrom::Start(1000)).await?;
/// let mut bytes = Vec::new();
/// reader.read_to_end(&mut bytes).await?;
/// assert_eq!(bytes.len(), 100);
/// # Ok(())
/// # }
/// ```
pub struct RemoteHttpReader {
    file: RemoteFile,
    network_bytes: Arc<AtomicU64>,
    pos: u64,
    bytes_read: u64,
    /// Reads stop here, so nothing past the part being read is fetched.
    end: u64,
    state: Option<State>,
}

enum State {
    Idle,
    /// A range `..end` being received, and the part of it not yet read, which starts at
    /// `chunk_start`. Dropping it closes the connection.
    Streaming {
        chunks: BoxStream<'static, Result<Bytes>>,
        chunk: Bytes,
        chunk_start: u64,
        end: u64,
    },
}

impl RemoteHttpReader {
    /// Opens `url` with the default [`ReaderConfig`].
    ///
    /// # Errors
    ///
    /// The same as [`RemoteFile::open`].
    pub async fn new(url: &str) -> Result<Self> {
        Self::open(url, ReaderConfig::new()?).await
    }

    /// Opens `url` as [`RemoteFile::open`] does, and returns a reader of it.
    ///
    /// # Errors
    ///
    /// The same as [`RemoteFile::open`].
    pub async fn open(url: &str, config: ReaderConfig) -> Result<Self> {
        Ok(RemoteFile::open(url, config).await?.reader())
    }

    /// The file being read.
    pub fn file(&self) -> &RemoteFile {
        &self.file
    }

    /// Handle for lifting the limit on fetched data, once the directory has been parsed.
    pub fn fetch_limit(&self) -> FetchLimit {
        self.file.fetch_limit()
    }

    /// Makes reads stop at `end`, as if the file ended there. Requests never go past it, though
    /// one already made under a later limit stays open, without anything past `end` being read.
    pub fn limit_to(&mut self, end: u64) {
        self.end = min(end, self.file.remote.len);
    }

    /// Bytes this reader has returned to its consumer.
    pub fn get_total_bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Bytes received over the network, which includes data never read by the consumer. This is
    /// the reader's counter, which may be shared with other readers.
    pub fn get_network_bytes(&self) -> u64 {
        self.network_bytes.load(Ordering::Relaxed)
    }

    /// The file's length in bytes.
    pub fn get_content_length(&self) -> u64 {
        self.file.remote.len
    }

    /// The URL the file was opened from.
    pub fn url(&self) -> &str {
        self.file.url()
    }

    fn copy_from(&mut self, data: &Bytes, data_start: u64, buf: &mut ReadBuf<'_>) -> bool {
        let data_end = min(data_start + data.len() as u64, self.end);
        if self.pos < data_start || self.pos >= data_end {
            return false;
        }
        let offset = (self.pos - data_start) as usize;
        let to_read = min((data_end - self.pos) as usize, buf.remaining());
        buf.put_slice(&data[offset..offset + to_read]);
        self.pos += to_read as u64;
        self.bytes_read += to_read as u64;
        true
    }
}

impl AsyncSeek for RemoteHttpReader {
    fn start_seek(mut self: Pin<&mut Self>, position: SeekFrom) -> std::io::Result<()> {
        let new_pos = match position {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(p) => self.file.remote.len.checked_add_signed(p),
            SeekFrom::Current(p) => self.pos.checked_add_signed(p),
        };
        let new_pos = new_pos.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek to a position before the start of the file",
            )
        })?;
        self.pos = min(new_pos, self.file.remote.len);
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<u64>> {
        Poll::Ready(Ok(self.pos))
    }
}

impl AsyncRead for RemoteHttpReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.pos >= self.end || buf.remaining() == 0 {
            return Poll::Ready(Ok(())); // EOF
        }

        let (tail, tail_start) = (self.file.tail.clone(), self.file.tail_start);
        if self.copy_from(&tail, tail_start, buf) {
            return Poll::Ready(Ok(()));
        }

        loop {
            // A state that isn't put back is dropped, which leaves the reader idle.
            match self.state.take().unwrap_or(State::Idle) {
                State::Idle => {
                    let start = self.pos;
                    let remote = self.file.remote.clone();
                    // The tail is already here, so stop where it starts when reading up to it.
                    let boundary = if start < tail_start {
                        tail_start
                    } else {
                        remote.len
                    };
                    let mut end = min(boundary, self.end);
                    if remote.is_limited() {
                        // The claimed directory comes in one request, anything else in chunks.
                        let in_directory =
                            (remote.directory_offset..remote.directory_end).contains(&start);
                        let most = if in_directory {
                            remote.directory_end
                        } else {
                            start.saturating_add(remote.config.chunk_bytes.max(1))
                        };
                        end = min(end, most);
                    }
                    if let Err(e) = remote.reserve(end - start) {
                        return Poll::Ready(Err(e.into()));
                    }
                    let range = RangeStream {
                        remote,
                        network_bytes: self.network_bytes.clone(),
                        pos: start,
                        end,
                        response: None,
                    };
                    self.state = Some(State::Streaming {
                        chunks: range.into_chunks(),
                        chunk: Bytes::new(),
                        chunk_start: start,
                        end,
                    });
                }
                State::Streaming {
                    mut chunks,
                    chunk,
                    chunk_start,
                    end,
                } => {
                    if self.copy_from(&chunk, chunk_start, buf) {
                        self.state = Some(State::Streaming {
                            chunks,
                            chunk,
                            chunk_start,
                            end,
                        });
                        return Poll::Ready(Ok(()));
                    }
                    // After a seek elsewhere, or at the end of the range, a new range is needed.
                    let next = chunk_start + chunk.len() as u64;
                    if self.pos != next || next >= end {
                        continue;
                    }
                    match chunks.poll_next_unpin(cx) {
                        Poll::Ready(Some(Ok(chunk))) => {
                            self.state = Some(State::Streaming {
                                chunks,
                                chunk,
                                chunk_start: next,
                                end,
                            });
                        }
                        Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(e.into())),
                        Poll::Ready(None) => {
                            return Poll::Ready(Err(Error::TruncatedResponse.into()));
                        }
                        Poll::Pending => {
                            self.state = Some(State::Streaming {
                                chunks,
                                chunk: Bytes::new(),
                                chunk_start: next,
                                end,
                            });
                            return Poll::Pending;
                        }
                    }
                }
            }
        }
    }
}
