use lazy_zip_core::archive::RemoteArchive;
use lazy_zip_core::downloader::{
    download_batch, download_from, find_existing, plan_selection, BatchEvent, BatchFile,
    BatchOptions, BatchReport, SelectionPlan,
};
use lazy_zip_core::http_reader::ReaderConfig;
use lazy_zip_core::zip_explorer::{scan, ScanResult};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::ipc::Channel;
use tauri::State;
use tokio_util::sync::CancellationToken;

const MAX_WORKERS: usize = 20;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const MAX_REPORTED_ACTIVE_FILES: usize = 8;
const QUIT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);

/// Tracks running downloads so they can be cancelled.
#[derive(Clone, Default)]
struct DownloadJobs {
    next_id: Arc<AtomicU64>,
    running: Arc<Mutex<HashMap<u64, CancellationToken>>>,
}

impl DownloadJobs {
    fn running(&self) -> std::sync::MutexGuard<'_, HashMap<u64, CancellationToken>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Cancels everything and waits briefly, so quitting doesn't leave partial files behind.
    fn cancel_all_and_wait(&self, timeout: Duration) {
        for cancel in self.running().values() {
            cancel.cancel();
        }
        let started = Instant::now();
        while !self.running().is_empty() && started.elapsed() < timeout {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// An HTTP header the user added, such as `Authorization`. Deliberately not `Debug`, so the value
/// can't end up in a log.
#[derive(Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Header {
    name: String,
    value: String,
}

fn reader_config(headers: &[Header]) -> Result<ReaderConfig, String> {
    ReaderConfig::with_headers(
        headers
            .iter()
            .map(|header| (header.name.as_str(), header.value.as_str())),
    )
    .map_err(|error| match error {
        // The core error names the header but never includes its value.
        lazy_zip_core::Error::InvalidHeader(name) => format!(
            "The header \"{}\" can't be used. Check that its name has no spaces or symbols like \
             \":\", and its value has only plain text. Range and Accept-Encoding are set by Lazy \
             ZIP and can't be added.",
            name.trim()
        ),
        other => other.to_string(),
    })
}

/// The most recently scanned archive, so downloads from it don't read its directory again. Only
/// one is kept, and a new scan replaces it. It's keyed by the headers too, since they can change
/// what the server returns.
type ScannedArchive = LastScan<RemoteArchive>;

struct LastScan<T>(Mutex<Option<Scanned<T>>>);

struct Scanned<T> {
    url: String,
    headers: Vec<Header>,
    archive: Arc<T>,
}

impl<T> Default for LastScan<T> {
    fn default() -> Self {
        Self(Mutex::new(None))
    }
}

impl<T> LastScan<T> {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Scanned<T>>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn replace(&self, url: String, headers: Vec<Header>, archive: T) {
        *self.lock() = Some(Scanned {
            url,
            headers,
            archive: Arc::new(archive),
        });
    }

    /// The scanned archive, if it was scanned from `url` with exactly `headers`.
    fn get(&self, url: &str, headers: &[Header]) -> Option<Arc<T>> {
        self.lock()
            .as_ref()
            .filter(|scanned| scanned.url == url && scanned.headers == headers)
            .map(|scanned| scanned.archive.clone())
    }
}

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum DownloadTarget {
    /// One file, saved to the path the user picked in the save dialog.
    File { zip_path: String, dest: PathBuf },
    /// Selected files, written under `dest_dir` relative to `base` in the archive.
    Selection {
        dest_dir: PathBuf,
        base: String,
        files: Vec<String>,
    },
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ActiveFile {
    path: String,
    done: u64,
}

#[derive(Serialize, Clone)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum JobEvent {
    Progress {
        done_bytes: u64,
        files_done: u32,
        active: Vec<ActiveFile>,
    },
    FileFailed {
        path: String,
        error: String,
    },
    Finished {
        completed: u32,
        failed: u32,
        cancelled: bool,
        written_bytes: u64,
        network_bytes: u64,
    },
}

/// Turns per-file events into progress snapshots sent at a limited rate.
struct ProgressTracker {
    channel: Channel<JobEvent>,
    state: Mutex<ProgressState>,
}

struct ProgressState {
    finished_bytes: u64,
    files_done: u32,
    active: HashMap<String, u64>,
    last_sent: Instant,
}

impl ProgressTracker {
    fn new(channel: Channel<JobEvent>) -> Self {
        Self {
            channel,
            state: Mutex::new(ProgressState {
                finished_bytes: 0,
                files_done: 0,
                active: HashMap::new(),
                last_sent: Instant::now() - PROGRESS_INTERVAL,
            }),
        }
    }

    fn handle(&self, event: BatchEvent<'_>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut force_send = false;
        match event {
            BatchEvent::FileStarted { path } => {
                state.active.insert(path.to_string(), 0);
            }
            BatchEvent::FileBytes { path, bytes } => {
                if let Some(done) = state.active.get_mut(path) {
                    *done += bytes;
                }
            }
            BatchEvent::FileFinished { path } => {
                let done = state.active.remove(path).unwrap_or(0);
                state.finished_bytes += done;
                state.files_done += 1;
            }
            BatchEvent::FileFailed { path, error, .. } => {
                state.active.remove(path);
                force_send = true;
                let _ = self.channel.send(JobEvent::FileFailed {
                    path: path.to_string(),
                    error,
                });
            }
        }

        if force_send || state.last_sent.elapsed() >= PROGRESS_INTERVAL {
            state.last_sent = Instant::now();
            let _ = self.channel.send(snapshot(&state));
        }
    }

    fn send_final_progress(&self) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let _ = self.channel.send(snapshot(&state));
    }
}

fn snapshot(state: &ProgressState) -> JobEvent {
    let active_bytes: u64 = state.active.values().sum();
    let mut active: Vec<ActiveFile> = state
        .active
        .iter()
        .map(|(path, done)| ActiveFile {
            path: path.clone(),
            done: *done,
        })
        .collect();
    active.sort_by(|a, b| a.path.cmp(&b.path));
    active.truncate(MAX_REPORTED_ACTIVE_FILES);
    JobEvent::Progress {
        done_bytes: state.finished_bytes + active_bytes,
        files_done: state.files_done,
        active,
    }
}

#[tauri::command]
async fn scan_zip(
    scanned: State<'_, ScannedArchive>,
    url: String,
    headers: Vec<Header>,
) -> Result<ScanResult, String> {
    let config = reader_config(&headers)?;
    let archive = RemoteArchive::open(&url, config)
        .await
        .map_err(|e| e.to_string())?;
    let result = scan(&archive);
    scanned.replace(url, headers, archive);
    Ok(result)
}

/// Returns the selected entries whose destination already exists.
#[tauri::command]
async fn find_existing_files(dest_dir: PathBuf, base: String, files: Vec<String>) -> Vec<String> {
    find_existing(&plan_selection(&dest_dir, &base, files).files).await
}

/// Downloads from `archive` if it's already open, so its directory isn't read again, or from `url`.
async fn download<F>(
    archive: Option<Arc<RemoteArchive>>,
    url: &str,
    files: Vec<BatchFile>,
    options: BatchOptions,
    on_event: F,
) -> BatchReport
where
    F: for<'a> Fn(BatchEvent<'a>) + Send + Sync,
{
    match archive {
        Some(archive) => download_from(&archive, files, options, on_event).await,
        None => download_batch(url, files, options, on_event).await,
    }
}

/// Starts a download and returns its job ID. Progress arrives through `on_event`.
///
/// If `url` and `headers` match the last archive scanned, its directory is reused instead of read
/// again.
#[tauri::command]
fn start_download(
    jobs: State<'_, DownloadJobs>,
    scanned: State<'_, ScannedArchive>,
    url: String,
    headers: Vec<Header>,
    target: DownloadTarget,
    workers: usize,
    on_event: Channel<JobEvent>,
) -> Result<u64, String> {
    let reader = reader_config(&headers)?;
    let (plan, root) = match target {
        DownloadTarget::File { zip_path, dest } => (
            SelectionPlan {
                files: vec![BatchFile {
                    zip_path,
                    dest,
                    index: None,
                }],
                rejected: Vec::new(),
            },
            None,
        ),
        DownloadTarget::Selection {
            dest_dir,
            base,
            files,
        } => (plan_selection(&dest_dir, &base, files), Some(dest_dir)),
    };

    let archive = scanned.get(&url, &headers);
    let cancel = CancellationToken::new();
    let job_id = jobs.next_id.fetch_add(1, Ordering::Relaxed);
    jobs.running().insert(job_id, cancel.clone());

    let jobs = jobs.inner().clone();
    tauri::async_runtime::spawn(async move {
        let rejected_count = plan.rejected.len() as u32;
        for rejected in plan.rejected {
            let _ = on_event.send(JobEvent::FileFailed {
                path: rejected.path,
                error: rejected.error,
            });
        }

        let tracker = ProgressTracker::new(on_event.clone());
        let options = BatchOptions {
            concurrency: workers.clamp(1, MAX_WORKERS),
            cancel,
            root,
            reader,
        };
        let report = download(archive, &url, plan.files, options, |event| {
            tracker.handle(event)
        })
        .await;

        jobs.running().remove(&job_id);
        tracker.send_final_progress();
        let _ = on_event.send(JobEvent::Finished {
            completed: report.completed,
            failed: report.failed + rejected_count,
            cancelled: report.cancelled,
            written_bytes: report.written_bytes,
            network_bytes: report.network_bytes,
        });
    });

    Ok(job_id)
}

#[tauri::command]
fn cancel_download(jobs: State<'_, DownloadJobs>, job_id: u64) {
    if let Some(cancel) = jobs.running().get(&job_id) {
        cancel.cancel();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let jobs = DownloadJobs::default();
    tauri::Builder::default()
        .manage(jobs.clone())
        .manage(ScannedArchive::default())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            scan_zip,
            find_existing_files,
            start_download,
            cancel_download
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(move |_, event| {
            if let tauri::RunEvent::Exit = event {
                jobs.cancel_all_and_wait(QUIT_CLEANUP_TIMEOUT);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;
    use tauri::ipc::InvokeResponseBody;

    #[test]
    fn download_targets_match_what_the_frontend_sends() {
        let selection: DownloadTarget = serde_json::from_value(json!({
            "kind": "selection", "destDir": "/out", "base": "data", "files": ["data/a.txt"],
        }))
        .unwrap();
        assert!(matches!(
            selection,
            DownloadTarget::Selection { dest_dir, base, files }
                if dest_dir == Path::new("/out") && base == "data" && files == ["data/a.txt"]
        ));

        let file: DownloadTarget = serde_json::from_value(json!({
            "kind": "file", "zipPath": "a.txt", "dest": "/out/a.txt",
        }))
        .unwrap();
        assert!(matches!(
            file,
            DownloadTarget::File { zip_path, dest }
                if zip_path == "a.txt" && dest == Path::new("/out/a.txt")
        ));
    }

    fn header(name: &str, value: &str) -> Header {
        Header {
            name: name.to_string(),
            value: value.to_string(),
        }
    }

    #[test]
    fn headers_match_what_the_frontend_sends() {
        let headers: Vec<Header> = serde_json::from_value(json!([
            { "name": "Authorization", "value": "Bearer abc" },
            { "name": "X-API-Key", "value": "123" },
        ]))
        .unwrap();
        assert!(
            headers
                == [
                    header("Authorization", "Bearer abc"),
                    header("X-API-Key", "123")
                ]
        );
        assert!(serde_json::from_value::<Vec<Header>>(json!([]))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn invalid_headers_are_explained_without_their_values() {
        assert!(reader_config(&[header("Authorization", "Bearer abc")]).is_ok());
        assert!(reader_config(&[]).is_ok());

        let error = reader_config(&[header("Range", "bytes=0-1")])
            .err()
            .unwrap();
        assert!(error.contains("\"Range\""), "{error}");
        assert!(!error.contains("bytes=0-1"), "{error}");

        let error = reader_config(&[header("X-Token", "secret\nvalue")])
            .err()
            .unwrap();
        assert!(error.contains("\"X-Token\""), "{error}");
        assert!(!error.contains("secret"), "{error}");

        assert!(reader_config(&[header("Bad Name", "x")]).is_err());
    }

    #[test]
    fn the_scanned_archive_is_reused_only_with_the_same_url_and_headers() {
        let scanned = LastScan::<&str>::default();
        let url = "https://example.com/a.zip";
        let token = vec![header("Authorization", "Bearer one")];
        assert!(scanned.get(url, &token).is_none());

        scanned.replace(url.to_string(), token.clone(), "archive");
        assert_eq!(scanned.get(url, &token).as_deref(), Some(&"archive"));
        assert!(scanned.get(url, &[]).is_none());
        assert!(scanned
            .get(url, &[header("Authorization", "Bearer two")])
            .is_none());
        assert!(scanned.get("https://example.com/b.zip", &token).is_none());

        scanned.replace(url.to_string(), Vec::new(), "without headers");
        assert_eq!(scanned.get(url, &[]).as_deref(), Some(&"without headers"));
        assert!(scanned.get(url, &token).is_none());
    }

    #[test]
    fn job_events_match_what_the_frontend_expects() {
        let finished = JobEvent::Finished {
            completed: 1,
            failed: 2,
            cancelled: false,
            written_bytes: 3,
            network_bytes: 4,
        };
        assert_eq!(
            serde_json::to_value(finished).unwrap(),
            json!({
                "kind": "finished", "completed": 1, "failed": 2, "cancelled": false,
                "writtenBytes": 3, "networkBytes": 4,
            })
        );

        let failed = JobEvent::FileFailed {
            path: "a".to_string(),
            error: "b".to_string(),
        };
        assert_eq!(
            serde_json::to_value(failed).unwrap(),
            json!({ "kind": "fileFailed", "path": "a", "error": "b" })
        );
    }

    fn collecting_tracker() -> (ProgressTracker, Arc<Mutex<Vec<serde_json::Value>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let collected = events.clone();
        let channel = Channel::new(move |body| {
            if let InvokeResponseBody::Json(text) = body {
                collected.lock().unwrap().push(serde_json::from_str(&text)?);
            }
            Ok(())
        });
        (ProgressTracker::new(channel), events)
    }

    #[test]
    fn progress_counts_active_and_finished_files_but_not_failed_ones() {
        let (tracker, events) = collecting_tracker();
        for path in ["a", "b", "c"] {
            tracker.handle(BatchEvent::FileStarted { path });
        }
        tracker.handle(BatchEvent::FileBytes {
            path: "a",
            bytes: 10,
        });
        tracker.handle(BatchEvent::FileBytes {
            path: "b",
            bytes: 20,
        });
        tracker.handle(BatchEvent::FileBytes {
            path: "c",
            bytes: 40,
        });
        tracker.handle(BatchEvent::FileFinished { path: "a" });
        tracker.handle(BatchEvent::FileFailed {
            path: "b",
            error: "boom".to_string(),
            kind: lazy_zip_core::ErrorKind::Network,
        });
        tracker.send_final_progress();

        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| event["kind"] == "fileFailed"));
        let last = events.last().unwrap();
        assert_eq!(last["kind"], "progress");
        assert_eq!(last["doneBytes"], 50);
        assert_eq!(last["filesDone"], 1);
        assert_eq!(last["active"], json!([{ "path": "c", "done": 40 }]));
    }

    #[test]
    fn quitting_cancels_running_jobs_and_waits_for_them() {
        let jobs = DownloadJobs::default();
        let cancel = CancellationToken::new();
        jobs.running().insert(1, cancel.clone());
        let background = jobs.clone();
        let worker = std::thread::spawn(move || {
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            background.running().remove(&1);
        });

        jobs.cancel_all_and_wait(Duration::from_secs(5));

        assert!(jobs.running().is_empty());
        worker.join().unwrap();
    }

    #[test]
    fn quitting_gives_up_waiting_after_the_timeout() {
        let jobs = DownloadJobs::default();
        jobs.running().insert(1, CancellationToken::new());
        let started = Instant::now();
        jobs.cancel_all_and_wait(Duration::from_millis(50));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn progress_is_rate_limited() {
        let (tracker, events) = collecting_tracker();
        tracker.handle(BatchEvent::FileStarted { path: "a" });
        for _ in 0..10_000 {
            tracker.handle(BatchEvent::FileBytes {
                path: "a",
                bytes: 1,
            });
        }
        assert!(events.lock().unwrap().len() < 100);
    }
}
