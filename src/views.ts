import { api } from "./api";
import { esc, extraProtected, state, toast } from "./store";
import type { ProcInfo } from "./store";

const REASON_LABEL: Record<string, string> = {
  you: "this app", always: "protected", system: "system", extra: "yours",
};

function procRow(p: ProcInfo): string {
  const locked = p.protected;
  const reason = locked ? `<span class="lock" title="protected: ${REASON_LABEL[p.reason] || p.reason}">🔒 ${REASON_LABEL[p.reason] || p.reason}</span>` : "";
  return `<tr data-pid="${p.pid}">
    <td>${locked ? "" : `<input type="checkbox" data-check="${p.pid}" ${state.focusSel.has(p.pid) ? "checked" : ""}>`}</td>
    <td>${esc(p.name)} ${reason}</td>
    <td class="pid">${p.pid}</td>
    <td class="mb">${p.memMb} MB</td>
    <td style="text-align:right">${locked ? "" : `
      <button class="btn small ghost" data-suspend="${p.pid}" title="pause (frees CPU, not RAM)">‖</button>
      <button class="btn small ghost" data-trim="${p.pid}" title="ask Windows to page out its working set">⌫</button>`}</td>
  </tr>`;
}

export async function renderFocus(app: HTMLElement): Promise<void> {
  app.innerHTML = `<div class="center">
    <h1 class="title">focus</h1>
    <div class="card">
      <div class="spread">
        <div><b id="mem-used">…</b> <span class="muted" id="mem-total"></span></div>
        <button class="btn small" id="mem-refresh">↻ refresh</button>
      </div>
      <div class="membar"><i id="mem-fill" style="width:0%"></i></div>
      <div class="muted" style="font-size:13px">
        closing a process frees RAM · suspending frees only CPU · trimming pages out its working set.
        protected processes (your tools, OS, this app) can never be closed.
      </div>
    </div>
    <div class="card">
      <div class="spread" style="margin-bottom:10px">
        <div class="row">
          <button class="btn small" id="sel-top">select top 5</button>
          <button class="btn small ghost" id="sel-none">clear</button>
        </div>
        <button class="btn primary" id="close-sel" disabled>close selected</button>
      </div>
      <table class="procs">
        <thead><tr><th style="width:28px"></th><th>process</th><th style="width:72px">pid</th>
        <th style="width:86px;text-align:right">RAM</th><th style="width:110px"></th></tr></thead>
        <tbody id="proc-rows"><tr><td colspan="5" class="muted">scanning…</td></tr></tbody>
      </table>
    </div>
  </div>`;

  const rows = () => app.querySelector<HTMLDivElement>("#proc-rows")!;
  const refresh = async () => {
    try {
      const [mem, procs] = await Promise.all([api.memory(), api.listProcesses(extraProtected())]);
      state.procs = procs;
      const used = app.querySelector("#mem-used");
      const total = app.querySelector("#mem-total");
      const fill = app.querySelector<HTMLDivElement>("#mem-fill");
      if (used) used.textContent = `${mem.usedMb} MB used`;
      if (total) total.textContent = `of ${mem.totalMb} MB · ${mem.availableMb} MB available`;
      if (fill) fill.style.width = `${Math.min(100, Math.round((mem.usedMb / mem.totalMb) * 100))}%`;
      rows().innerHTML = procs.map(procRow).join("") || `<tr><td colspan="5" class="muted">no processes found</td></tr>`;
      bind();
      updateCloseBtn();
    } catch (e) {
      rows().innerHTML = `<tr><td colspan="5" class="muted">scan failed: ${esc(String(e))}</td></tr>`;
    }
  };

  const updateCloseBtn = () => {
    const btn = app.querySelector<HTMLButtonElement>("#close-sel")!;
    const sel = state.procs.filter((p) => state.focusSel.has(p.pid));
    const mb = sel.reduce((a, p) => a + p.memMb, 0);
    btn.disabled = sel.length === 0;
    btn.textContent = sel.length ? `close ${sel.length} · ~${mb} MB` : "close selected";
  };

  const bind = () => {
    app.querySelectorAll<HTMLInputElement>("input[data-check]").forEach((cb) => {
      cb.onchange = () => {
        const pid = Number(cb.dataset.check);
        cb.checked ? state.focusSel.add(pid) : state.focusSel.delete(pid);
        updateCloseBtn();
      };
    });
    app.querySelectorAll<HTMLButtonElement>("button[data-suspend]").forEach((b) => {
      b.onclick = async () => {
        const pid = Number(b.dataset.suspend);
        const isSuspended = b.dataset.on === "1";
        try {
          if (isSuspended) { await api.resume(pid, extraProtected()); b.dataset.on = "0"; b.textContent = "‖"; }
          else { await api.suspend(pid, extraProtected()); b.dataset.on = "1"; b.textContent = "▶"; toast(`pid ${pid} ${b.dataset.on === "1" ? "suspended (CPU freed, RAM kept)" : "resumed"}`);
            b.textContent = b.dataset.on === "1" ? "▶" : "‖"; }
        } catch (e) { toast(String(e).replace(/^Error: /, ""), "err"); }
      };
    });
    app.querySelectorAll<HTMLButtonElement>("button[data-trim]").forEach((b) => {
      b.onclick = async () => {
        try {
          await api.trim(Number(b.dataset.trim), extraProtected());
          toast("working set paged out", "ok");
          refresh();
        } catch (e) { toast(String(e).replace(/^Error: /, ""), "err"); }
      };
    });
  };

  app.querySelector<HTMLElement>("#mem-refresh")!.onclick = refresh;
  app.querySelector<HTMLButtonElement>("#sel-top")!.onclick = () => {
    state.focusSel.clear();
    state.procs.filter((p) => !p.protected).slice(0, 5).forEach((p) => state.focusSel.add(p.pid));
    refresh();
  };
  app.querySelector<HTMLButtonElement>("#sel-none")!.onclick = () => { state.focusSel.clear(); refresh(); };
  app.querySelector<HTMLButtonElement>("#close-sel")!.onclick = async () => {
    const sel = state.procs.filter((p) => state.focusSel.has(p.pid));
    const btn = app.querySelector<HTMLButtonElement>("#close-sel")!;
    btn.disabled = true;
    let freed = 0, closed = 0;
    for (const p of sel) {
      try { await api.close(p.pid, extraProtected()); freed += p.memMb; closed++; }
      catch (e) { toast(`${p.name}: ${String(e).replace(/^Error: /, "")}`, "err"); }
    }
    state.focusSel.clear();
    toast(closed ? `closed ${closed} process${closed > 1 ? "es" : ""} · ~${freed} MB reported` : "nothing closed", closed ? "ok" : "");
    await refresh();
  };
  await refresh();
}

export async function renderLibrary(app: HTMLElement): Promise<void> {
  const stats = await api.resourceStats().catch(() => ({ files: 0, chunks: 0 }));
  app.innerHTML = `<div class="center">
    <h1 class="title">library</h1>
    <div class="card">
      <h3>index a folder</h3>
      <p class="muted" style="margin:4px 0 10px;font-size:13.5px">
        books, PDFs, notes, code — chunked, embedded (nomic-embed-text) and kept on this machine.
        text PDFs only; scanned PDFs need OCR (not supported yet).
      </p>
      <div class="row">
        <input type="text" id="folder" placeholder="C:\Users\you\Documents\books" style="flex:1;min-width:260px">
        <button class="btn primary" id="index-btn">index</button>
      </div>
      <div id="index-report" class="muted" style="font-size:13.5px;margin-top:8px"></div>
      <div class="muted" style="font-size:13px;margin-top:6px">currently indexed: <b>${stats.files}</b> files · <b>${stats.chunks}</b> chunks</div>
    </div>
    <div class="card">
      <h3>test retrieval</h3>
      <div class="row">
        <input type="text" id="q" placeholder="e.g. how does a hash map handle collisions" style="flex:1">
        <button class="btn" id="q-btn">search</button>
      </div>
      <div id="hits" style="margin-top:6px"></div>
    </div>
    <div class="card">
      <h3>the web</h3>
      <p class="muted" style="margin:4px 0 10px;font-size:13.5px">
        tier 3 — used only when the model admits it doesn't know (it emits a [WEB: …] marker, we fetch, it synthesizes).
        you can also poke it manually here.
      </p>
      <div class="row">
        <input type="text" id="wq" placeholder="search the live web" style="flex:1">
        <button class="btn" id="w-btn">search</button>
      </div>
      <div id="whits" style="margin-top:6px"></div>
    </div>
  </div>`;

  const report = app.querySelector("#index-report")!;
  app.querySelector<HTMLButtonElement>("#index-btn")!.onclick = async () => {
    const folder = (app.querySelector("#folder") as HTMLInputElement).value.trim();
    if (!folder) return toast("enter a folder path", "err");
    const btn = app.querySelector<HTMLButtonElement>("#index-btn")!;
    btn.disabled = true;
    report.textContent = "walking, chunking and embedding… (first run can take a while)";
    try {
      const r = await api.indexFolder(folder);
      const extra = r.messages.length ? `\n${r.messages.slice(0, 5).join("\n")}` : "";
      report.innerHTML = `indexed <b>${r.files}</b> files → <b>${r.chunks}</b> chunks ` +
        `(${r.skipped} skipped, ${r.errors} errors)${extra ? `<br><span class="muted">${esc(extra)}</span>` : ""}`;
      toast("index updated", "ok");
    } catch (e) {
      report.innerHTML = `<span class="err" style="color:var(--danger)">${esc(String(e).replace(/^Error: /, ""))}</span>`;
    } finally {
      btn.disabled = false;
    }
  };

  app.querySelector<HTMLButtonElement>("#q-btn")!.onclick = async () => {
    const q = (app.querySelector("#q") as HTMLInputElement).value.trim();
    const box = app.querySelector("#hits")!;
    if (!q) return;
    box.innerHTML = `<div class="muted">searching…</div>`;
    try {
      const hits = await api.searchResources(q, 5);
      box.innerHTML = hits.length
        ? hits.map((h) => `<div class="hit"><div class="p">${esc(h.path)} <span class="muted">(score ${h.score.toFixed(3)})</span></div><div class="t">${esc(h.text.slice(0, 260))}…</div></div>`).join("")
        : `<div class="muted">no local hits — index a folder first</div>`;
    } catch (e) {
      box.innerHTML = `<div class="muted">${esc(String(e).replace(/^Error: /, ""))}</div>`;
    }
  };

  app.querySelector<HTMLButtonElement>("#w-btn")!.onclick = async () => {
    const q = (app.querySelector("#wq") as HTMLInputElement).value.trim();
    const box = app.querySelector("#whits")!;
    if (!q) return;
    box.innerHTML = `<div class="muted">searching the web…</div>`;
    try {
      const hits = await api.webSearch(q);
      box.innerHTML = hits.map((h) =>
        `<div class="hit"><a class="p" href="${esc(h.url)}" target="_blank" rel="noreferrer">${esc(h.title)}</a><div class="t">${esc(h.snippet)}</div></div>`).join("");
    } catch (e) {
      box.innerHTML = `<div class="muted">${esc(String(e).replace(/^Error: /, ""))}</div>`;
    }
  };
}

export async function renderProgress(app: HTMLElement): Promise<void> {
  const s = await api.stats();
  const rate = s.attempts ? Math.round((s.passed / s.attempts) * 100) : 0;
  app.innerHTML = `<div class="center">
    <h1 class="title">progress</h1>
    <div class="grid">
      <div class="stat"><div class="n">${s.tracks}</div><div class="l">tracks</div></div>
      <div class="stat"><div class="n">${s.lessonsDone}</div><div class="l">lessons solved</div></div>
      <div class="stat"><div class="n">${s.attempts}</div><div class="l">attempts</div></div>
      <div class="stat"><div class="n">${rate}%</div><div class="l">pass rate</div></div>
      <div class="stat"><div class="n">${s.chunks}</div><div class="l">indexed chunks</div></div>
    </div>
    <p class="muted" style="margin-top:16px;font-size:13.5px">
      every attempt, hint used and result is kept in a local SQLite ledger — your course does not
      re-invent itself between sessions.
    </p>
  </div>`;
}

export async function renderSettings(app: HTMLElement): Promise<void> {
  const h = state.health;
  const current = state.settings.model || "qwen2.5-coder:7b";
  const opts = (h.models.length ? h.models : [current])
    .map((m) => `<option value="${esc(m)}" ${m === current ? "selected" : ""}>${esc(m)}</option>`).join("");
  app.innerHTML = `<div class="center">
    <h1 class="title">settings</h1>
    <div class="card">
      <h3>local model</h3>
      <div class="row" style="margin:8px 0">
        <select id="model-sel" style="max-width:320px">${opts}</select>
        <span class="pill ${h.ok ? "ok" : "warn"}">${h.ok ? "ollama connected" : esc(h.error || "ollama down")}</span>
        <button class="btn small" id="hs-retry">retry</button>
      </div>
      <div class="muted" style="font-size:13px">small models trade depth for speed — qwen2.5-coder:7b fits your 16 GB; :32b will not.</div>
    </div>
    <div class="card">
      <h3>resources</h3>
      <label class="check" style="margin:8px 0">
        <input type="checkbox" id="web-on" ${state.settings.webAllowed !== "0" ? "checked" : ""}>
        allow the model to fetch the web when it doesn't know something (tier 3)
      </label>
      <div class="muted" style="font-size:13px;margin-bottom:12px">turning this off makes lessons fully offline (tier 1 + your indexed files only).</div>
      <div class="muted" style="font-size:13px;margin-bottom:6px">extra protected processes in focus mode (comma separated, e.g. slack, spotify):</div>
      <input type="text" id="prot" placeholder="slack, spotify" value="${esc(state.settings.protectedExtra || "")}">
      <div class="row" style="margin-top:10px"><button class="btn primary" id="save">save</button></div>
    </div>
  </div>`;

  app.querySelector<HTMLSelectElement>("#model-sel")!.onchange = async (ev) => {
    const v = (ev.target as HTMLSelectElement).value;
    state.settings.model = v;
    await api.setSetting("model", v);
    toast(`model → ${v}`, "ok");
  };
  app.querySelector<HTMLInputElement>("#web-on")!.onchange = async (ev) => {
    const v = (ev.target as HTMLInputElement).checked ? "1" : "0";
    state.settings.webAllowed = v;
    await api.setSetting("webAllowed", v);
  };
  app.querySelector<HTMLButtonElement>("#save")!.onclick = async () => {
    const v = (app.querySelector("#prot") as HTMLInputElement).value.trim();
    state.settings.protectedExtra = v;
    await api.setSetting("protectedExtra", v);
    toast("saved", "ok");
  };
  app.querySelector<HTMLButtonElement>("#hs-retry")!.onclick = async () => {
    state.health = await api.health().catch((e) => ({ ok: false, models: [], error: String(e) }));
    toast(state.health.ok ? "connected" : state.health.error || "down", state.health.ok ? "ok" : "err");
    import("./main").then((m) => m.refreshStatusDot());
  };
}
