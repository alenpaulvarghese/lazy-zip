import { Channel, invoke } from "@tauri-apps/api/core";
import type { Header } from "./headers";
import type { DownloadTarget, JobEvent, ScanResult } from "./types";

export function scanZip(url: string, headers: Header[]): Promise<ScanResult> {
  return invoke("scan_zip", { url, headers });
}

export function findExistingFiles(destDir: string, base: string, files: string[]): Promise<string[]> {
  return invoke("find_existing_files", { destDir, base, files });
}

export function startDownload(
  url: string,
  headers: Header[],
  target: DownloadTarget,
  workers: number,
  onEvent: (event: JobEvent) => void,
): Promise<number> {
  const channel = new Channel<JobEvent>();
  channel.onmessage = onEvent;
  return invoke("start_download", { url, headers, target, workers, onEvent: channel });
}

export function cancelDownload(jobId: number): Promise<void> {
  return invoke("cancel_download", { jobId });
}
