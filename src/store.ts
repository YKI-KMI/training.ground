export type View = "learn" | "lesson" | "focus" | "library" | "progress" | "settings";

export interface ProcInfo { pid: number; name: string; memMb: number; exe: string; protected: boolean; reason: string }
export interface MemInfo { totalMb: number; usedMb: number; availableMb: number }
export interface RunResult { ok: boolean; exitCode: number | null; stdout: string; stderr: string; timedOut: boolean; durationMs: number; interpreter: string }
export interface TrackInfo { id: number; name: string; language: string; lessons: number; done: number }
export interface Lesson { id: number; trackId: number; idx: number; title: string; status: string; payload: string }
export interface LessonPayload { title: string; prompt: string; starter: string; tests: string; hints: string[]; solution: string; sources?: string[] }
export interface Health { ok: boolean; models: string[]; error: string | null }
export interface ResourceHit { path: string; text: string; score: number }
export interface Hit { title: string; url: string; snippet: string }
export interface IndexReport { files: number; chunks: number; skipped: number; errors: number; messages: string[] }
export interface AppStats { tracks: number; lessonsDone: number; attempts: number; passed: number; chunks: number }

export const state = {
  view: "learn" as View,
  health: { ok: false, models: [], error: null } as Health,
  settings: {} as Record<string, string>,
  tracks: [] as TrackInfo[],
  trackId: 0,
  lessons: [] as Lesson[],
  lesson: null as Lesson | null,
  payload: null as LessonPayload | null,
  hintsUsed: 0,
  running: false,
  generating: false,
  procs: [] as ProcInfo[],
  focusSel: new Set<number>(),
};

export const model = () => state.settings.model || "qwen2.5-coder:7b";
export const webAllowed = () => state.settings.webAllowed !== "0";
export const extraProtected = () =>
  (state.settings.protectedExtra || "").split(",").map((s) => s.trim()).filter(Boolean);

export function el(html: string): HTMLElement {
  const t = document.createElement("template");
  t.innerHTML = html.trim();
  return t.content.firstElementChild as HTMLElement;
}

export function toast(msg: string, kind: "" | "err" | "ok" = "") {
  const t = document.getElementById("toast")!;
  t.className = kind;
  t.textContent = msg;
  t.hidden = false;
  clearTimeout((t as any)._timer);
  (t as any)._timer = setTimeout(() => (t.hidden = true), 3500);
}

export function esc(s: string): string {
  return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]!));
}
