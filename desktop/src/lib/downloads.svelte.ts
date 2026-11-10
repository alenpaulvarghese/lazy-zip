import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { cancelDownload, startDownload } from "./api";
import type { Header } from "./headers";
import type { ActiveFile, DownloadTarget, JobEvent } from "./types";

export type JobStatus = "running" | "done" | "failed" | "cancelled";

export interface FileFailure {
  path: string;
  error: string;
}

export interface DownloadJob {
  key: number;
  jobId: number | null;
  label: string;
  url: string;
  status: JobStatus;
  totalBytes: number;
  totalFiles: number;
  doneBytes: number;
  filesDone: number;
  bytesPerSecond: number;
  active: ActiveFile[];
  failures: FileFailure[];
  networkBytes: number;
  revealPath: string;
}

export interface NewDownload {
  label: string;
  url: string;
  /** Kept only in memory, like every request here. */
  headers: Header[];
  target: DownloadTarget;
  /** Uncompressed size of each file, by its path in the archive. */
  sizes: Map<string, number>;
  workers: number;
}

const SPEED_SMOOTHING = 0.3;

function totalOf(sizes: Map<string, number>): number {
  let total = 0;
  for (const size of sizes.values()) total += size;
  return total;
}

/** Progress by bytes, or by files when all the files are empty. */
export function calcPercent(job: DownloadJob): number {
  const [done, total] =
    job.totalBytes > 0 ? [job.doneBytes, job.totalBytes] : [job.filesDone, job.totalFiles];
  return total === 0 ? 100 : Math.min(100, (done / total) * 100);
}

export class DownloadManager {
  jobs = $state<DownloadJob[]>([]);
  // Selections can hold many thousands of paths, which don't need to be reactive.
  private requests = new Map<number, NewDownload>();
  private nextKey = 0;

  get runningCount(): number {
    return this.jobs.filter((job) => job.status === "running").length;
  }

  async start(download: NewDownload): Promise<void> {
    const key = this.nextKey++;
    this.requests.set(key, download);
    const { target, sizes } = download;
    this.jobs.unshift({
      key,
      jobId: null,
      label: download.label,
      url: download.url,
      status: "running",
      totalBytes: totalOf(sizes),
      totalFiles: sizes.size,
      doneBytes: 0,
      filesDone: 0,
      bytesPerSecond: 0,
      active: [],
      failures: [],
      networkBytes: 0,
      revealPath: target.kind === "file" ? target.dest : target.destDir,
    });
    const job = this.jobs[0];

    let lastBytes = 0;
    let lastTime = performance.now();
    const onEvent = (event: JobEvent) => {
      switch (event.kind) {
        case "progress": {
          const now = performance.now();
          const seconds = (now - lastTime) / 1000;
          if (seconds > 0) {
            const instant = (event.doneBytes - lastBytes) / seconds;
            job.bytesPerSecond =
              job.bytesPerSecond === 0
                ? instant
                : job.bytesPerSecond + SPEED_SMOOTHING * (instant - job.bytesPerSecond);
          }
          lastBytes = event.doneBytes;
          lastTime = now;
          job.doneBytes = event.doneBytes;
          job.filesDone = event.filesDone;
          job.active = event.active;
          break;
        }
        case "fileFailed":
          job.failures.push({ path: event.path, error: event.error });
          break;
        case "finished":
          job.active = [];
          job.bytesPerSecond = 0;
          job.networkBytes = event.networkBytes;
          job.status = event.cancelled ? "cancelled" : event.failed > 0 ? "failed" : "done";
          break;
      }
    };

    try {
      job.jobId = await startDownload(download.url, download.headers, target, download.workers, onEvent);
    } catch (err: unknown) {
      job.status = "failed";
      job.failures.push({ path: download.label, error: String(err) });
    }
  }

  cancel(job: DownloadJob): void {
    if (job.jobId !== null) void cancelDownload(job.jobId);
  }

  /** Downloads only the files that failed, to the same place. */
  retryFailed(job: DownloadJob): Promise<void> {
    const request = this.requests.get(job.key);
    if (!request) return Promise.resolve();

    const failed = new Set(job.failures.map((failure) => failure.path));
    const sizes = new Map([...request.sizes].filter(([path]) => failed.has(path)));
    const target: DownloadTarget =
      request.target.kind === "file"
        ? request.target
        : { ...request.target, files: request.target.files.filter((path) => failed.has(path)) };

    this.remove(job);
    return this.start({ ...request, target, sizes });
  }

  sizeOf(job: DownloadJob, path: string): number {
    return this.requests.get(job.key)?.sizes.get(path) ?? 0;
  }

  reveal(job: DownloadJob): Promise<void> {
    return revealItemInDir(job.revealPath);
  }

  remove(job: DownloadJob): void {
    this.requests.delete(job.key);
    this.jobs = this.jobs.filter((other) => other.key !== job.key);
  }

  clearFinished(): void {
    for (const job of this.jobs) {
      if (job.status !== "running") this.requests.delete(job.key);
    }
    this.jobs = this.jobs.filter((job) => job.status === "running");
  }
}
