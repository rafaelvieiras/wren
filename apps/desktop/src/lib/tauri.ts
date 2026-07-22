/**
 * Typed layer over the Tauri commands (`invoke`) and events (`listen`). The
 * SINGLE source of the data shapes and command signatures — all data hooks and
 * views consume from here. In `--mode mock` Vite swaps `@tauri-apps/api/core`
 * and `/event` for mocks; these wrappers do NOT change.
 *
 * The shapes mirror `SettingsPage.tsx`/`EmbeddedModels.tsx` and the Rust DTOs.
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type ActivationMode = "toggle" | "push_to_talk";
export type PasteMethod = "paste" | "ctrl_shift_v" | "type" | "wtype";
export type LogLevel = "error" | "warn" | "info" | "debug" | "trace";

export interface ProviderConfig {
  id: string;
  label: string;
  base_url: string;
  api_key: string | null;
  model: string;
  sends_audio_externally: boolean;
  /** Absent/"openai" = OpenAI-compatible server; "embedded" = local engine. */
  kind?: string;
}

export interface Settings {
  active_provider_id: string;
  providers: ProviderConfig[];
  shortcut: string;
  activation_mode: ActivationMode;
  cancel_shortcut: string;
  language: string | null;
  /** Internal pauses longer than this (ms) are shortened; null = disabled. */
  compress_pauses_over_ms: number | null;
  /** Input device; null = system default. */
  input_device: string | null;
  play_sounds: boolean;
  paste_method: PasteMethod;
  restore_clipboard: boolean;
  launch_at_login: boolean;
  /** Minimum severity captured by the local logger. Default: "info". */
  log_level: LogLevel;
}

/** A log record (mirrors `LogRecord`). */
export interface LogRecord {
  ts_ms: number;
  level: string;
  target: string;
  message: string;
}

export type StageName =
  | "capture_stop"
  | "vad"
  | "persist_audio"
  | "transcribe"
  | "deliver";

/** Duration of a pipeline stage (mirrors `StageTiming`). */
export interface StageTiming {
  stage: StageName;
  duration_ms: number;
  audio_bytes?: number;
}

export type SessionOutcome =
  | "delivered"
  | "discarded_no_speech"
  | "failed"
  | "cancelled";

/** Metrics for a session (mirrors `SessionMetrics`). */
export interface SessionMetrics {
  created_at_ms: number;
  outcome: SessionOutcome;
  provider_id: string;
  model: string;
  recorded_duration_ms: number;
  sent_audio_duration_ms: number;
  total_ms: number;
  stages: StageTiming[];
  rss_start_bytes?: number;
  rss_peak_bytes?: number;
}

export interface HistoryEntry {
  status: "done" | "failed";
  text: string;
  provider_id: string;
  latency_ms: number;
  audio_duration_ms: number;
  created_at_ms: number;
  error?: string | null;
  audio_path?: string | null;
  /** Dictated duration before pause compression (old rows: absent). */
  recorded_duration_ms?: number | null;
}

/** A model from the embedded catalog (mirrors `embedded_catalog`, camelCase). */
export interface EmbeddedModel {
  id: string;
  label: string;
  /** ISO 639-1 ("pt", "en") ou "multi". */
  language: string;
  sizeBytes: number;
}

/** Payload of the `embedded://download-progress` event. */
export interface DownloadProgress {
  id: string;
  downloaded: number;
  total: number;
  done: boolean;
  error?: string;
}

/** Typed command wrappers. Names/args identical to the backend's. */
export const tauri = {
  getSettings: () => invoke<Settings>("get_settings"),
  saveSettings: (settings: Settings) =>
    invoke<void>("save_settings", { settings }),
  providerPresets: () => invoke<ProviderConfig[]>("provider_presets"),
  listInputDevices: () => invoke<string[]>("list_input_devices"),
  listModels: (baseUrl: string, apiKey: string | null) =>
    invoke<string[]>("list_models", { baseUrl, apiKey }),
  getHistory: () => invoke<HistoryEntry[]>("get_history"),
  retryTranscription: (createdAtMs: number) =>
    invoke<string>("retry_transcription", { createdAtMs }),
  checkForUpdates: () => invoke<string>("check_for_updates"),
  getLogs: (opts: { limit?: number; level?: string | null; query?: string | null }) =>
    invoke<LogRecord[]>("get_logs", {
      limit: opts.limit ?? 500,
      level: opts.level || null,
      query: opts.query || null,
    }),
  clearLogs: () => invoke<void>("clear_logs"),
  logFilePath: () => invoke<string>("log_file_path"),
  getMetrics: (limit = 50) => invoke<SessionMetrics[]>("get_metrics", { limit }),
  embeddedCatalog: () => invoke<EmbeddedModel[]>("embedded_catalog"),
  embeddedLocalModels: () => invoke<string[]>("embedded_local_models"),
  embeddedDownloadModel: (id: string) =>
    invoke<void>("embedded_download_model", { id }),
  embeddedDeleteModel: (id: string) =>
    invoke<void>("embedded_delete_model", { id }),
};

/** Subscribes to embedded download progress. Resolves to the unlisten function;
 * call it in the effect cleanup. The handler receives only the payload. */
export function onDownloadProgress(
  cb: (p: DownloadProgress) => void,
): Promise<UnlistenFn> {
  return listen<DownloadProgress>("embedded://download-progress", (e) =>
    cb(e.payload),
  );
}
