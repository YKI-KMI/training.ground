import { marked } from "marked";
import type { EditorView } from "@codemirror/view";
import { api, askWithWeb } from "./api";
import type { Msg } from "./api";
import { createEditor } from "./editor";
import { el, esc, model, state, toast, webAllowed } from "./store";
import type { Lesson, LessonPayload, TrackInfo } from "./store";

/** listeners/handles left behind by the previous screen, dropped on re-render */
let disposers: (() => void)[] = [];
function dispose(): void {
  disposers.forEach((d) => d());
  disposers = [];
}

const md = (s: string): string => {
  try {
    return marked.parse(s) as string;
  } catch {
    return esc(s);
  }
};

const errText = (e: unknown): string => String(e).replace(/^Error: /, "");

/** tier 2: pull passages from the user's own indexed files for a query. */
async function localContext(query: string): Promise<string> {
  try {
    const hits = await api.searchResources(query.slice(0, 400), 4);
    if (!hits.length) return "";
    return (
      "\n\nRelevant passages from the user's indexed local library:\n" +
      hits.map((h) => `- (${h.path}) ${h.text.slice(0, 600)}`).join("\n")
    );
  } catch {
    return "";
  }
}

// ---------------------------------------------------------------------------
// learn: tracks
// ---------------------------------------------------------------------------

export async function renderLearn(app: HTMLElement): Promise<void> {
  dispose();
  state.tracks = await api.listTracks().catch(() => state.tracks);
  state.lessons = state.trackId
    ? await api.listLessons(state.trackId).catch(() => [] as Lesson[])
    : [];

  const tracksHtml = state.tracks.length
    ? state.tracks
        .map((t) => {
          const open = state.trackId === t.id;
          return `<div class="card track" data-tid="${t.id}">
      <div class="spread">
        <div><b>${esc(t.name)}</b> <span class="pill">${esc(t.language)}</span></div>
        <div class="row">
          <span class="muted" style="font-size:13px">${t.done}/${t.lessons} done</span>
          <button class="btn small" data-gen="${t.id}">+ new lesson</button>
        </div>
      </div>
      ${open ? lessonsList(t) : ""}
    </div>`;
        })
        .join("")
    : `<div class="card muted" style="font-size:14px">
        no tracks yet. a track is a course the local model writes for you, one lesson at a time —
        question, starter code, tests, hints. create one above.
      </div>`;

  app.innerHTML = `<div class="center">
    <h1 class="title">learn</h1>
    <div class="card">
      <h3>start a track</h3>
      <div class="row" style="margin-top:8px">
        <input type="text" id="tname" placeholder="e.g. python fundamentals" style="flex:1;min-width:220px">
        <select id="tlang" style="width:auto">
          <option value="python">python</option>
          <option value="javascript">javascript</option>
        </select>
        <button class="btn primary" id="tcreate">create</button>
      </div>
    </div>
    ${tracksHtml}
  </div>`;

  app.querySelector<HTMLButtonElement>("#tcreate")!.onclick = async () => {
    const name = (app.querySelector("#tname") as HTMLInputElement).value.trim();
    if (!name) return toast("name your track first", "err");
    const lang = (app.querySelector("#tlang") as HTMLSelectElement).value;
    try {
      const t = await api.createTrack(name, lang);
      state.tracks.push({ id: t.id, name: t.name, language: t.language, lessons: 0, done: 0 });
      state.trackId = t.id;
      toast(`track “${name}” created`, "ok");
      renderLearn(app);
    } catch (e) {
      toast(errText(e), "err");
    }
  };

  app.querySelectorAll<HTMLElement>(".track[data-tid]").forEach((card) => {
    card.onclick = (ev) => {
      const target = ev.target as HTMLElement;
      if (target.closest("button")) return; // keep +new-lesson clickable
      if (target.closest(".lessons-list div")) return; // lesson rows handle themselves
      const id = Number(card.dataset.tid);
      state.trackId = state.trackId === id ? 0 : id;
      renderLearn(app);
    };
  });

  app.querySelectorAll<HTMLButtonElement>("button[data-gen]").forEach((b) => {
    b.onclick = async (ev) => {
      ev.stopPropagation();
      const t = state.tracks.find((x) => x.id === Number(b.dataset.gen));
      if (t) await generateLesson(app, t);
    };
  });

  app.querySelectorAll<HTMLElement>(".lessons-list div[data-idx]").forEach((row) => {
    row.onclick = async (ev) => {
      ev.stopPropagation();
      const tid = Number((row.closest(".track") as HTMLElement).dataset.tid);
      await openLesson(tid, Number(row.dataset.idx));
    };
  });
}

function lessonsList(t: TrackInfo): string {
  if (!t.lessons) {
    return `<div class="lessons-list"><div class="muted">empty — hit “+ new lesson” to let the model write lesson 1.</div></div>`;
  }
  return `<div class="lessons-list">${state.lessons
    .filter((l) => l.trackId === t.id)
    .map(
      (l) => `<div data-idx="${l.idx}" class="${l.status === "done" ? "done" : ""}" style="cursor:pointer">
      <span class="muted">${l.idx + 1}.</span>
      <span style="flex:1">${esc(l.title)}</span>
      <span>${l.status === "done" ? "✓" : "·"}</span>
    </div>`,
    )
    .join("")}</div>`;
}

// ---------------------------------------------------------------------------
// lesson generation (model writes the lesson, we persist it in the ledger)
// ---------------------------------------------------------------------------

function parseLessonJson(raw: string): LessonPayload {
  let text = raw.trim().replace(/^```(?:json)?\s*/i, "").replace(/```\s*$/, "");
  const start = text.indexOf("{");
  const end = text.lastIndexOf("}");
  if (start >= 0 && end > start) text = text.slice(start, end + 1);
  const obj = JSON.parse(text) as Partial<LessonPayload>;
  if (!obj.prompt || !obj.title) throw new Error("model did not return a lesson object");
  const payload: LessonPayload = {
    title: String(obj.title),
    prompt: String(obj.prompt),
    starter: typeof obj.starter === "string" ? obj.starter : "",
    tests: typeof obj.tests === "string" ? obj.tests : "",
    hints: Array.isArray(obj.hints) ? obj.hints.map(String).slice(0, 5) : [],
    solution: typeof obj.solution === "string" ? obj.solution : "",
    sources: Array.isArray(obj.sources) ? obj.sources.map(String) : undefined,
  };
  return payload;
}

async function generateLesson(app: HTMLElement, track: TrackInfo): Promise<void> {
  if (state.generating) return;
  state.generating = true;
  dispose();

  const idx = track.lessons;
  const level = idx < 3 ? "beginner" : idx < 8 ? "intermediate" : "advanced";
  const prev = await api.listLessons(track.id).catch(() => [] as Lesson[]);
  const prevTitles = prev.map((l) => `${l.idx + 1}. ${l.title}`).join("\n");

  app.innerHTML = `<div class="center"><div class="gen">
    <div class="spin"></div>
    <div class="big">writing lesson ${idx + 1}…</div>
    <div class="muted">drafting the question, starter code, tests and hint ladder —
      web tier ${webAllowed() ? "on" : "off"}</div>
    <pre id="genraw" class="out" style="text-align:left;max-width:660px;margin:18px auto 0;max-height:200px"></pre>
    <div style="margin-top:14px"><button class="btn ghost" id="gcancel">cancel</button></div>
  </div></div>`;

  let cancelled = false;
  app.querySelector<HTMLButtonElement>("#gcancel")!.onclick = () => {
    cancelled = true;
    state.generating = false;
    renderLearn(app);
  };

  const rawEl = app.querySelector("#genraw");
  const system =
    `You are a precise programming tutor. You write ONE self-contained coding lesson for the track ` +
    `"${track.name}" in ${track.language}. Level: ${level}.\n` +
    `Output ONLY a single JSON object — no prose, no markdown fences — with exactly these keys:\n` +
    `{"title": "...", ` +
    `"prompt": "the exercise in markdown: context, what to build, concrete requirements, and 1-2 worked input/output examples", ` +
    `"starter": "the code the student opens in the editor (a skeleton with a TODO, not the answer)", ` +
    `"tests": "verification code that is CONCATENATED AFTER the student's code when they press run. ` +
    `For python: top-level asserts using only stdlib that raise AssertionError when wrong. ` +
    `For javascript: statements in the same file that throw when wrong.", ` +
    `"hints": ["nudge", "point at the concept", "near-solution approach"], ` +
    `"solution": "the complete reference solution as a step-by-step markdown walkthrough", ` +
    `"sources": ["optional doc urls"]}\n` +
    `Rules: a correct student solution MUST make the tests exit 0, a wrong one must not. ` +
    `Never put the answer in starter or prompt. Keep it completable in ~10 minutes.`;

  const user =
    `Write lesson ${idx + 1} for this track.\n` +
    (prevTitles ? `Lessons already covered:\n${prevTitles}\nPick a topic that follows on from those.\n` : "This is lesson 1 — start from the fundamentals.\n") +
    `Return the JSON object now.`;

  const messages: Msg[] = [
    { role: "system", content: system },
    { role: "user", content: user },
  ];

  let raw = "";
  try {
    const full = await askWithWeb(
      { model: model(), messages, options: { temperature: 0.7 } },
      (t) => {
        raw += t;
        if (rawEl && !cancelled) rawEl.textContent = raw.slice(-1600);
      },
      webAllowed(),
    );
    if (cancelled) return;
    const payload = parseLessonJson(full);
    await api.saveLesson(track.id, idx, payload.title, JSON.stringify(payload));
    toast(`lesson ${idx + 1} ready`, "ok");
    state.trackId = track.id;
    await refreshTracks();
    await openLesson(track.id, idx);
  } catch (e) {
    if (!cancelled) {
      toast(`could not write the lesson: ${errText(e)}`, "err");
      await renderLearn(app);
    }
  } finally {
    state.generating = false;
  }
}

async function refreshTracks(): Promise<void> {
  state.tracks = await api.listTracks().catch(() => state.tracks);
  state.lessons = await api.listLessons(state.trackId).catch(() => []);
}

export async function openLesson(trackId: number, idx: number): Promise<void> {
  const lesson = await api.getLesson(trackId, idx);
  if (!lesson) return toast("lesson not found", "err");
  let payload: LessonPayload;
  try {
    payload = JSON.parse(lesson.payload) as LessonPayload;
  } catch {
    return toast("lesson payload is corrupt", "err");
  }
  state.trackId = trackId;
  state.lesson = lesson;
  state.payload = payload;
  state.hintsUsed = 0;
  state.lessons = await api.listLessons(trackId).catch(() => state.lessons);
  const m = await import("./main");
  await m.go("lesson");
}

// ---------------------------------------------------------------------------
// lesson screen: question → edit → run → hints → solution → explain
// ---------------------------------------------------------------------------

export async function renderLesson(app: HTMLElement): Promise<void> {
  dispose();
  const lesson = state.lesson;
  const p = state.payload;
  if (!lesson || !p) {
    state.trackId = 0;
    const m = await import("./main");
    return m.go("learn");
  }
  const track = state.tracks.find((t) => t.id === lesson.trackId);
  const lang = track?.language || "python";
  const hints = p.hints || [];
  const hasNext = (track?.lessons ?? 0) > lesson.idx + 1;

  app.innerHTML = `<div class="center">
    <div class="lesson-head">
      <a href="#" id="back">← ${esc(track?.name ?? "track")}</a>
      <span>lesson ${lesson.idx + 1}</span>
      <span class="pill ${lesson.status === "done" ? "ok" : ""}" id="status-pill">${esc(lesson.status)}</span>
      <span style="flex:1"></span>
      <span>hints used: <b id="hintcount">${state.hintsUsed}</b>/${hints.length}</span>
    </div>
    <div class="card"><div class="prompt-md" id="prompt"></div></div>
    <div class="editor-wrap" id="editor"></div>
    <div class="actions">
      <button class="btn primary" id="run">run <kbd>ctrl+enter</kbd></button>
      <button class="btn" id="hint-btn">hint</button>
      <button class="btn ghost" id="sol-btn">show solution</button>
      <button class="btn ghost" id="reset">reset</button>
      <span class="grow"></span>
      <button class="btn ghost" id="next" hidden>next lesson →</button>
    </div>
    <div class="out" id="out" hidden></div>
    <div id="hints"></div>
    <div id="solution" hidden></div>
    <div id="notes"></div>
  </div>`;

  const promptEl = app.querySelector("#prompt")!;
  promptEl.innerHTML = md(p.prompt);

  const out = app.querySelector("#out") as HTMLElement;
  const hintsBox = app.querySelector("#hints") as HTMLElement;
  const solBox = app.querySelector("#solution") as HTMLElement;
  const notesBox = app.querySelector("#notes") as HTMLElement;
  const runBtn = app.querySelector<HTMLButtonElement>("#run")!;
  const hintBtn = app.querySelector<HTMLButtonElement>("#hint-btn")!;
  const solBtn = app.querySelector<HTMLButtonElement>("#sol-btn")!;
  const nextBtn = app.querySelector<HTMLButtonElement>("#next")!;
  const hintCount = app.querySelector("#hintcount")!;
  nextBtn.hidden = !hasNext;

  let editor: EditorView | null = null;
  const getCode = () => (editor ? editor.state.doc.toString() : p!.starter);

  const run = async () => {
    if (state.running) return;
    state.running = true;
    runBtn.disabled = true;
    runBtn.textContent = "running…";
    out.hidden = false;
    out.className = "out";
    out.textContent = "running…";
    const code = getCode();
    try {
      const r = await api.runCode(lang, code, p.tests || undefined);
      const pass = r.ok;
      out.className = `out ${pass ? "pass" : "fail"}`;
      const head = pass
        ? `<span class="okc">✓ tests passed</span>`
        : `<span class="err">✗ ${r.timedOut ? "timed out" : `failed (exit ${r.exitCode ?? "?"})`}</span>`;
      out.innerHTML =
        `${head} <span class="muted">· ${r.durationMs} ms · ${esc(r.interpreter)}</span>` +
        `\n${r.stdout ? esc(r.stdout) : '<span class="muted">(no output)</span>'}` +
        (r.stderr ? `\n<span class="err">${esc(r.stderr)}</span>` : "");
      out.scrollTop = out.scrollHeight;
      if (state.lesson) {
        await api.addAttempt(state.lesson.id, pass, state.hintsUsed, code).catch(() => 0);
        if (pass && state.lesson.status !== "done") {
          await api.setLessonStatus(state.lesson.id, "done").catch(() => false);
          state.lesson.status = "done";
          const pill = app.querySelector("#status-pill")!;
          pill.textContent = "done";
          pill.className = "pill ok";
          state.lessons = await api.listLessons(lesson.trackId).catch(() => state.lessons);
          toast("lesson complete ✓", "ok");
        }
      }
    } catch (e) {
      out.className = "out fail";
      out.innerHTML = `<span class="err">${esc(errText(e))}</span>`;
    } finally {
      state.running = false;
      runBtn.disabled = false;
      runBtn.innerHTML = `run <kbd>ctrl+enter</kbd>`;
    }
  };

  editor = createEditor(app.querySelector<HTMLElement>("#editor")!, p.starter, lang, run);

  runBtn.onclick = run;

  app.querySelector<HTMLButtonElement>("#reset")!.onclick = () => {
    if (!editor) return;
    editor.dispatch({ changes: { from: 0, to: editor.state.doc.length, insert: p.starter } });
    toast("starter restored", "");
  };

  hintBtn.onclick = () => {
    if (state.hintsUsed >= hints.length) return;
    const h = hints[state.hintsUsed];
    state.hintsUsed += 1;
    hintsBox.appendChild(
      el(`<div class="hint"><div class="who">hint ${state.hintsUsed}/${hints.length}</div>${md(h)}</div>`),
    );
    hintCount.textContent = String(state.hintsUsed);
    hintBtn.textContent =
      state.hintsUsed >= hints.length ? "no more hints" : `hint ${state.hintsUsed + 1}/${hints.length}`;
    if (state.hintsUsed >= hints.length) hintBtn.disabled = true;
  };
  if (!hints.length) {
    hintBtn.disabled = true;
    hintBtn.textContent = "no hints";
  } else {
    hintBtn.textContent = `hint 1/${hints.length}`;
  }

  let solShown = false;
  solBtn.onclick = async () => {
    if (solShown) return;
    solShown = true;
    solBtn.disabled = true;
    solBox.hidden = false;
    solBox.innerHTML = `<div class="md" id="solmd"><span class="muted">writing the step-by-step solution…</span></div>`;
    const solMd = solBox.querySelector("#solmd")!;
    const code = getCode();
    const local = await localContext(p.title + " " + p.prompt.slice(0, 200));
    const messages: Msg[] = [
      { role: "system", content: "You are a patient tutor. Markdown only, no preamble, be concrete." },
      {
        role: "user",
        content:
          `Lesson “${p.title}”:\n${p.prompt}\n\nThe student's current attempt:\n` +
          "```" + lang + "\n" + code + "```\n\n" +
          `Walk through the solution step by step: what to do first and why, then the next step, ` +
          `ending with the final code in a fenced block. If their attempt is on the right track, say what to keep.` +
          local,
      },
    ];
    let buf = "";
    let last = 0;
    try {
      const full = await askWithWeb(
        { model: model(), messages, options: { temperature: 0.4 } },
        (t) => {
          buf += t;
          const now = Date.now();
          if (now - last > 220) {
            last = now;
            solMd.innerHTML = md(buf);
          }
        },
        webAllowed(),
      );
      solMd.innerHTML = md(full);
    } catch (e) {
      solMd.innerHTML = `<span class="err">${esc(errText(e))}</span>`;
      solBtn.disabled = false;
      solShown = false;
    }
  };

  nextBtn.onclick = async () => {
    await openLesson(lesson.trackId, lesson.idx + 1);
  };

  app.querySelector<HTMLAnchorElement>("#back")!.onclick = async (ev) => {
    ev.preventDefault();
    state.trackId = lesson.trackId;
    state.lessons = await api.listLessons(lesson.trackId).catch(() => state.lessons);
    const m = await import("./main");
    await m.go("learn");
  };

  // --- select-to-explain: highlight anything in the lesson, ask what it means
  let selBtn: HTMLElement | null = null;
  const killSelBtn = () => {
    if (selBtn) {
      selBtn.remove();
      selBtn = null;
    }
  };
  const onMouseUp = () => {
    const sel = window.getSelection();
    const text = sel ? sel.toString().trim() : "";
    killSelBtn();
    if (!sel || !text || text.length < 3 || !sel.rangeCount) return;
    const rect = sel.getRangeAt(0).getBoundingClientRect();
    if (!app.contains(sel.anchorNode)) return;
    selBtn = el(`<button class="sel-btn">explain ✦</button>`);
    selBtn.style.position = "fixed";
    selBtn.style.left = `${Math.max(8, rect.left)}px`;
    selBtn.style.top = `${Math.max(8, rect.bottom + 8)}px`;
    const chosen = text;
    selBtn.onclick = () => {
      killSelBtn();
      sel!.removeAllRanges();
      void explain(chosen);
    };
    document.body.appendChild(selBtn);
  };
  document.addEventListener("mouseup", onMouseUp);
  disposers.push(() => {
    document.removeEventListener("mouseup", onMouseUp);
    killSelBtn();
  });

  async function explain(text: string): Promise<void> {
    notesBox.hidden = false;
    notesBox.innerHTML = `<div class="who">explain</div><div class="md" id="exmd"><span class="muted">thinking…</span></div>`;
    const exMd = notesBox.querySelector("#exmd")!;
    const local = await localContext(text);
    const messages: Msg[] = [
      { role: "system", content: "You are a patient tutor. Markdown only, concise but complete." },
      {
        role: "user",
        content:
          `We are on the lesson “${p!.title}”. The student highlighted this passage:\n"""\n${text}\n"""\n\n` +
          `Explain what it means, why it matters, and give one tiny illustrative example.` +
          local,
      },
    ];
    let buf = "";
    let last = 0;
    try {
      const full = await askWithWeb(
        { model: model(), messages, options: { temperature: 0.3 } },
        (t) => {
          buf += t;
          const now = Date.now();
          if (now - last > 220) {
            last = now;
            exMd.innerHTML = md(buf);
          }
        },
        webAllowed(),
      );
      exMd.innerHTML = md(full);
    } catch (e) {
      exMd.innerHTML = `<span class="err">${esc(errText(e))}</span>`;
    }
  }
}
