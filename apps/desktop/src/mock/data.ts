/**
 * Fake data to run the Settings window in a plain browser, without the Tauri
 * backend. Used ONLY in `--mode mock` (see vite.config.ts and the `dev:mock`
 * script). None of this ends up in the real app build.
 *
 * The shapes mirror the TS interfaces in `SettingsPage.tsx`/`EmbeddedModels.tsx`
 * and the DTOs in `src-tauri/src/lib.rs`. Filled with plausible data so the UX
 * reviewer sees a "full" UI.
 */

const now = Date.now();
const min = 60_000;

export const settings = {
  active_provider_id: "groq",
  providers: [
    {
      id: "groq",
      label: "Groq (Whisper)",
      base_url: "https://api.groq.com/openai/v1",
      api_key: "gsk_wren_example_do_not_use_in_production_0000",
      model: "whisper-large-v3-turbo",
      sends_audio_externally: true,
      kind: "openai",
    },
    {
      id: "openai",
      label: "OpenAI",
      base_url: "https://api.openai.com/v1",
      api_key: "sk-wren-example-0000",
      model: "gpt-4o-transcribe",
      sends_audio_externally: true,
      kind: "openai",
    },
    {
      id: "local",
      label: "Local server (whisper.cpp)",
      base_url: "http://localhost:8555/v1",
      api_key: null,
      model: "ggml-large-v3",
      sends_audio_externally: false,
      kind: "openai",
    },
    {
      id: "embedded-parakeet-v3",
      label: "Parakeet V3 (offline)",
      base_url: "",
      api_key: null,
      model: "parakeet-v3",
      sends_audio_externally: false,
      kind: "embedded",
    },
  ],
  shortcut: "ctrl+shift+space",
  activation_mode: "toggle",
  cancel_shortcut: "escape",
  language: "pt",
  compress_pauses_over_ms: 2000,
  input_device: null,
  play_sounds: true,
  paste_method: "paste",
  restore_clipboard: false,
  launch_at_login: true,
  log_level: "info",
};

export const providerPresets = [
  {
    id: "groq",
    label: "Groq (Whisper)",
    base_url: "https://api.groq.com/openai/v1",
    api_key: null,
    model: "whisper-large-v3-turbo",
    sends_audio_externally: true,
    kind: "openai",
  },
  {
    id: "openai",
    label: "OpenAI",
    base_url: "https://api.openai.com/v1",
    api_key: null,
    model: "gpt-4o-transcribe",
    sends_audio_externally: true,
    kind: "openai",
  },
  {
    id: "local",
    label: "Servidor local (whisper.cpp)",
    base_url: "http://localhost:8555/v1",
    api_key: null,
    model: "ggml-large-v3",
    sends_audio_externally: false,
    kind: "openai",
  },
];

export const inputDevices = [
  "Internal microphone (Analog)",
  "Yeti Nano USB",
  "Bluetooth headphones (WH-1000XM4)",
  "Logitech C920 webcam",
];

/** Models per base_url. Groq/OpenAI expose /models; the local one (whisper.cpp)
 * simulates a server that doesn't list (falls back to free text). */
export function modelsFor(baseUrl: string): string[] {
  const b = (baseUrl || "").toLowerCase();
  if (b.includes("groq")) {
    return [
      "whisper-large-v3",
      "whisper-large-v3-turbo",
      "distil-whisper-large-v3-en",
    ];
  }
  if (b.includes("openai")) {
    return ["gpt-4o-transcribe", "gpt-4o-mini-transcribe", "whisper-1"];
  }
  // Local server without /models: throw to fall back to the free-text field.
  throw new Error("models unavailable (local server)");
}

export const history = [
  {
    status: "done",
    text: "Schedule a meeting with the product team on Thursday at three in the afternoon to review the quarter's roadmap.",
    provider_id: "groq",
    latency_ms: 780,
    audio_duration_ms: 6200,
    recorded_duration_ms: 8100,
    created_at_ms: now - 4 * min,
    audio_path: "/home/user/.local/share/wren/recordings/2026-07-12-1.wav",
  },
  {
    status: "done",
    text: "Reminder: buy coffee, paper filters, and sweetener at the store later today.",
    provider_id: "groq",
    latency_ms: 540,
    audio_duration_ms: 3400,
    recorded_duration_ms: 3400,
    created_at_ms: now - 22 * min,
    audio_path: "/home/user/.local/share/wren/recordings/2026-07-12-2.wav",
  },
  {
    status: "failed",
    text: "",
    provider_id: "groq",
    latency_ms: 0,
    audio_duration_ms: 5100,
    recorded_duration_ms: 5100,
    created_at_ms: now - 51 * min,
    error: "429 Too Many Requests — provider quota exceeded",
    audio_path: "/home/user/.local/share/wren/recordings/2026-07-12-3.wav",
  },
  {
    status: "done",
    text: "Here's the summary of the call: the client approved the proposal but asked to move up the first phase's delivery by two weeks.",
    provider_id: "local",
    latency_ms: 2100,
    audio_duration_ms: 9800,
    recorded_duration_ms: 12400,
    created_at_ms: now - 3 * 60 * min,
    audio_path: "/home/user/.local/share/wren/recordings/2026-07-12-4.wav",
  },
  {
    status: "done",
    text: "Testing offline dictation with Parakeet — without sending any audio to the cloud.",
    provider_id: "embedded-parakeet-v3",
    latency_ms: 1450,
    audio_duration_ms: 4300,
    recorded_duration_ms: 4300,
    created_at_ms: now - 26 * 60 * min,
    audio_path: "/home/user/.local/share/wren/recordings/2026-07-11-9.wav",
  },
];

const TARGETS = [
  "wren",
  "wren::shortcut",
  "wren::audio",
  "wren::transcribe",
  "wren::telemetry",
  "wren::clipboard",
];
const MESSAGES: [string, string][] = [
  ["INFO", "Wren starting"],
  ["INFO", "global shortcut registered: ctrl+shift+space"],
  ["DEBUG", "capture started (device=Yeti Nano USB, 16kHz mono)"],
  ["DEBUG", "VAD: 8.1s dictated → 6.2s sent (trim + pauses)"],
  ["INFO", "transcription completed in 780ms (provider=groq)"],
  ["DEBUG", "text delivered via paste (Ctrl+V)"],
  ["WARN", "pause compression shortened 1 segment of 2.4s"],
  ["ERROR", "429 Too Many Requests — provider quota exceeded"],
  ["INFO", "clipboard restored after paste"],
  ["DEBUG", "session RSS peak: 148 MB"],
  ["WARN", "wtype not found in PATH — using fallback"],
  ["INFO", "session cancelled by user (Escape)"],
];

export const logs = Array.from({ length: 40 }, (_, i) => {
  const [level, message] = MESSAGES[i % MESSAGES.length];
  return {
    ts_ms: now - (40 - i) * 7000,
    level,
    target: TARGETS[i % TARGETS.length],
    message,
  };
});

export function filteredLogs(
  level: string | null,
  query: string | null,
): typeof logs {
  const order = ["ERROR", "WARN", "INFO", "DEBUG"];
  let out = logs;
  if (level) {
    const minIdx = order.indexOf(level);
    out = out.filter((l) => order.indexOf(l.level) <= minIdx);
  }
  if (query) {
    const q = query.toLowerCase();
    out = out.filter(
      (l) =>
        l.message.toLowerCase().includes(q) ||
        l.target.toLowerCase().includes(q),
    );
  }
  return out;
}

const MB = 1024 * 1024;

export const metrics = [
  {
    created_at_ms: now - 4 * min,
    outcome: "delivered",
    provider_id: "groq",
    model: "whisper-large-v3-turbo",
    recorded_duration_ms: 8100,
    sent_audio_duration_ms: 6200,
    total_ms: 1120,
    stages: [
      { stage: "capture_stop", duration_ms: 40 },
      { stage: "vad", duration_ms: 90, audio_bytes: 198_400 },
      { stage: "persist_audio", duration_ms: 60, audio_bytes: 198_400 },
      { stage: "transcribe", duration_ms: 780 },
      { stage: "deliver", duration_ms: 150 },
    ],
    rss_start_bytes: 96 * MB,
    rss_peak_bytes: 148 * MB,
  },
  {
    created_at_ms: now - 22 * min,
    outcome: "delivered",
    provider_id: "groq",
    model: "whisper-large-v3-turbo",
    recorded_duration_ms: 3400,
    sent_audio_duration_ms: 3400,
    total_ms: 690,
    stages: [
      { stage: "capture_stop", duration_ms: 30 },
      { stage: "vad", duration_ms: 55 },
      { stage: "persist_audio", duration_ms: 45 },
      { stage: "transcribe", duration_ms: 470 },
      { stage: "deliver", duration_ms: 90 },
    ],
    rss_start_bytes: 94 * MB,
    rss_peak_bytes: 121 * MB,
  },
  {
    created_at_ms: now - 51 * min,
    outcome: "failed",
    provider_id: "groq",
    model: "whisper-large-v3-turbo",
    recorded_duration_ms: 5100,
    sent_audio_duration_ms: 5100,
    total_ms: 5300,
    stages: [
      { stage: "capture_stop", duration_ms: 35 },
      { stage: "vad", duration_ms: 70 },
      { stage: "persist_audio", duration_ms: 50 },
      { stage: "transcribe", duration_ms: 5145 },
    ],
    rss_start_bytes: 95 * MB,
    rss_peak_bytes: 132 * MB,
  },
  {
    created_at_ms: now - 78 * min,
    outcome: "discarded_no_speech",
    provider_id: "local",
    model: "ggml-large-v3",
    recorded_duration_ms: 1200,
    sent_audio_duration_ms: 0,
    total_ms: 130,
    stages: [
      { stage: "capture_stop", duration_ms: 30 },
      { stage: "vad", duration_ms: 100 },
    ],
    rss_start_bytes: 88 * MB,
    rss_peak_bytes: 90 * MB,
  },
  {
    created_at_ms: now - 26 * 60 * min,
    outcome: "delivered",
    provider_id: "embedded-parakeet-v3",
    model: "parakeet-v3",
    recorded_duration_ms: 4300,
    sent_audio_duration_ms: 4300,
    total_ms: 1720,
    stages: [
      { stage: "capture_stop", duration_ms: 35 },
      { stage: "vad", duration_ms: 60 },
      { stage: "persist_audio", duration_ms: 55 },
      { stage: "transcribe", duration_ms: 1450 },
      { stage: "deliver", duration_ms: 120 },
    ],
    rss_start_bytes: 210 * MB,
    rss_peak_bytes: 690 * MB,
  },
  {
    created_at_ms: now - 30 * 60 * min,
    outcome: "cancelled",
    provider_id: "groq",
    model: "whisper-large-v3-turbo",
    recorded_duration_ms: 2100,
    sent_audio_duration_ms: 0,
    total_ms: 70,
    stages: [{ stage: "capture_stop", duration_ms: 40 }],
    rss_start_bytes: 92 * MB,
    rss_peak_bytes: 96 * MB,
  },
];

/** Embedded-engine catalog (camelCase, like the Rust DTO). */
export const embeddedCatalog = [
  {
    id: "parakeet-v3",
    label: "Parakeet V3 (NVIDIA)",
    language: "multi",
    sizeBytes: 2.4 * 1024 * MB,
  },
  {
    id: "whisper-base",
    label: "Whisper Base",
    language: "multi",
    sizeBytes: 142 * MB,
  },
  {
    id: "whisper-small-pt",
    label: "Whisper Small (Portuguese)",
    language: "pt",
    sizeBytes: 466 * MB,
  },
];

/** Ids of already-downloaded models. Parakeet downloaded (shows "Offline"). */
export const embeddedLocal = ["parakeet-v3"];

export const logFilePath = "/home/user/.local/share/wren/logs/wren.log";
