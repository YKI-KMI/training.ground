/// App shell: boots settings/model health, routes the top nav, and owns the
/// status dot in the header. Screens are lazily re-rendered through `go`.
import { api } from "./api";
import { esc, state } from "./store";
import type { View } from "./store";
import { renderFocus, renderLibrary, renderProgress, renderSettings } from "./views";
import { renderLearn, renderLesson } from "./learn";

const app = (): HTMLElement => document.getElementById("app")!;

/** Header dot: green when the local Ollama proxy answers, red with the reason. */
export function refreshStatusDot(): void {
  const dot = document.getElementById("llm-dot");
  const label = document.getElementById("llm-label");
  if (!dot || !label) return;
  const h = state.health;
  dot.className = `dot ${h.ok ? "ok" : "err"}`;
  label.textContent = h.ok
    ? state.settings.model || "qwen2.5-coder:7b"
    : h.error || "ollama down";
}

/** Route to a screen. The nav highlights the parent item (lesson lives under learn). */
export async function go(view: View): Promise<void> {
  state.view = view;
  const navKey = view === "lesson" ? "learn" : view;
  document.querySelectorAll<HTMLButtonElement>("#nav button").forEach((b) => {
    b.classList.toggle("active", b.dataset.view === navKey);
  });
  try {
    switch (view) {
      case "learn":
        await renderLearn(app());
        break;
      case "lesson":
        await renderLesson(app());
        break;
      case "focus":
        await renderFocus(app());
        break;
      case "library":
        await renderLibrary(app());
        break;
      case "progress":
        await renderProgress(app());
        break;
      case "settings":
        await renderSettings(app());
        break;
    }
  } catch (e) {
    app().innerHTML = `<div class="center muted">view failed: ${esc(String(e).replace(/^Error: /, ""))}</div>`;
  }
}

async function boot(): Promise<void> {
  document.querySelectorAll<HTMLButtonElement>("#nav button").forEach((b) => {
    b.onclick = () => void go(b.dataset.view as View);
  });
  app().innerHTML = `<div class="center muted">connecting to the local backend…</div>`;

  state.settings = await api.getSettings().catch(() => ({}) as Record<string, string>);
  state.health = await api
    .health()
    .catch((e) => ({ ok: false, models: [] as string[], error: String(e).replace(/^Error: /, "") }));
  state.tracks = await api.listTracks().catch(() => []);
  refreshStatusDot();
  await go("learn");

  // keep the dot honest even if ollama comes up/down while we work
  setInterval(async () => {
    state.health = await api
      .health()
      .catch((e) => ({ ok: false, models: [] as string[], error: String(e).replace(/^Error: /, "") }));
    refreshStatusDot();
  }, 30_000);
}

window.addEventListener("DOMContentLoaded", () => void boot());
