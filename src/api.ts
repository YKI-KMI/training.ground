import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  AppStats, Health, Hit, IndexReport, Lesson, MemInfo, ProcInfo, ResourceHit, RunResult, TrackInfo,
} from "./store";

export const api = {
  // focus
  listProcesses: (extra: string[]) => invoke<ProcInfo[]>("list_processes", { extra }),
  memory: () => invoke<MemInfo>("memory_snapshot"),
  close: (pid: number, extra: string[]) => invoke<boolean>("close_process", { pid, extra }),
  suspend: (pid: number, extra: string[]) => invoke<number>("suspend_process", { pid, extra }),
  resume: (pid: number, extra: string[]) => invoke<number>("resume_process", { pid, extra }),
  trim: (pid: number, extra: string[]) => invoke<boolean>("trim_process", { pid, extra }),
  // sandbox
  runCode: (language: string, code: string, tests?: string, timeoutMs?: number) =>
    invoke<RunResult>("run_code", { req: { language, code, tests, timeoutMs } }),
  // ledger
  createTrack: (name: string, language: string) => invoke<TrackInfo>("create_track", { name, language }),
  listTracks: () => invoke<TrackInfo[]>("list_tracks"),
  saveLesson: (trackId: number, idx: number, title: string, payload: string) =>
    invoke<number>("save_lesson", { trackId, idx, title, payload }),
  getLesson: (trackId: number, idx: number) => invoke<Lesson | null>("get_lesson", { trackId, idx }),
  listLessons: (trackId: number) => invoke<Lesson[]>("list_lessons", { trackId }),
  setLessonStatus: (lessonId: number, status: string) =>
    invoke<boolean>("set_lesson_status", { lessonId, status }),
  addAttempt: (lessonId: number, passed: boolean, hintsUsed: number, code: string) =>
    invoke<number>("add_attempt", { lessonId, passed, hintsUsed, code }),
  stats: () => invoke<AppStats>("stats"),
  getSettings: async () => {
    const pairs = await invoke<[string, string][]>("get_settings");
    return Object.fromEntries(pairs);
  },
  setSetting: (key: string, value: string) => invoke<boolean>("set_setting", { key, value }),
  // model proxy
  health: () => invoke<Health>("ollama_health"),
  embed: (m: string, input: string[]) => invoke<number[][]>("ollama_embed", { model: m, input }),
  // resources
  indexFolder: (folder: string) => invoke<IndexReport>("index_folder", { folder }),
  searchResources: (query: string, topK: number) =>
    invoke<ResourceHit[]>("search_resources", { query, topK }),
  resourceStats: () => invoke<{ files: number; chunks: number }>("resource_stats"),
  // web
  fetchUrl: (url: string) => invoke<{ url: string; title: string; text: string; cached: boolean }>("fetch_url", { url }),
  webSearch: (query: string) => invoke<Hit[]>("web_search", { query }),
};

export interface Msg { role: "system" | "user" | "assistant"; content: string }

/** Stream a chat completion through the Rust proxy; resolves with the full text. */
export async function chatStream(
  payload: Record<string, unknown>,
  onToken: (t: string) => void,
): Promise<string> {
  const rid = crypto.randomUUID();
  const un = await listen<{ rid: string; token: string }>("ollama-chunk", (e) => {
    if (e.payload.rid === rid && e.payload.token) onToken(e.payload.token);
  });
  try {
    return await invoke<string>("ollama_chat", { payload, rid });
  } finally {
    un();
  }
}

/**
 * Chat with the model-initiated web tier: if the model answers with a
 * `[WEB: query]` line it is admitting it doesn't know — we search, fetch the
 * top page, feed it back, and re-ask (max 3 rounds).
 */
export async function askWithWeb(
  payload: { model: string; messages: Msg[]; options?: Record<string, unknown> },
  onToken: (t: string) => void,
  allowed: boolean,
): Promise<string> {
  let messages = [...payload.messages];
  let full = "";
  for (let round = 0; round <= 3; round++) {
    full = await chatStream({ ...payload, messages }, onToken);
    const m = full.match(/^\s*\[WEB:\s*(.+?)\s*\]\s*$/m);
    if (!allowed || !m || round === 3) return full;
    const query = m[1];
    onToken(`\n\n› searching the web for “${query}”…\n`);
    try {
      const hits = await api.webSearch(query);
      let ctx = hits.map((h) => `- ${h.title}\n  ${h.url}\n  ${h.snippet}`).join("\n");
      if (hits[0]) {
        try {
          const f = await api.fetchUrl(hits[0].url);
          ctx += `\n\nTop result — ${f.title} (${f.url}):\n${f.text.slice(0, 5000)}`;
        } catch { /* snippet-only is fine */ }
      }
      onToken("› synthesizing with sources…\n\n");
      messages = [
        ...payload.messages,
        { role: "assistant", content: full },
        {
          role: "user",
          content: `Web research results:\n${ctx}\n\nNow answer the original request using this. Do NOT emit the [WEB: ...] marker again.`,
        },
      ];
    } catch (e) {
      onToken(`\n(web search unavailable: ${String(e).replace(/^Error: /, "")})\n`);
      return full;
    }
  }
  return full;
}
