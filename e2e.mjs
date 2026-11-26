// Drive the real Training Ground window over CDP (WebView2 remote debugging).
// Node 22+ has a built-in WebSocket client.
const CDP_HTTP = "http://127.0.0.1:9333";

const pages = await (await fetch(`${CDP_HTTP}/json/list`)).json();
const page = pages.find((p) => p.type === "page" && p.url.includes("localhost:1420"));
if (!page) {
  console.error("FAIL: app page not found in", pages.map((p) => p.url));
  process.exit(1);
}
const ws = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });

let seq = 0;
const pending = new Map();
ws.onmessage = (ev) => {
  const msg = JSON.parse(ev.data);
  if (msg.id && pending.has(msg.id)) {
    pending.get(msg.id)(msg);
    pending.delete(msg.id);
  }
};
function send(method, params = {}) {
  const id = ++seq;
  return new Promise((resolve) => {
    pending.set(id, resolve);
    ws.send(JSON.stringify({ id, method, params }));
  });
}
async function evaluate(expression) {
  const r = await send("Runtime.evaluate", {
    expression, awaitPromise: true, returnByValue: true,
  });
  if (r.result?.exceptionDetails) {
    throw new Error("page exception: " + JSON.stringify(r.result.exceptionDetails.exception?.description || r.result.exceptionDetails, null, 2).slice(0, 800));
  }
  return r.result?.result?.value;
}

const results = [];
function check(name, ok, detail = "") {
  results.push({ name, ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? "  -- " + detail : ""}`);
}

// 1. App shell rendered
const navText = await evaluate(`document.querySelector(".topbar")?.innerText || ""`);
const navLower = navText.toLowerCase();
check("topbar rendered", navLower.includes("learn") && navLower.includes("focus"), navText.replace(/\n/g, " | "));

// 2. Backend invoke: memory snapshot (focus engine)
const mem = await evaluate(`window.__TAURI__.core.invoke("memory_snapshot")`);
check("memory_snapshot returns data", mem && typeof mem.totalMb === "number" && typeof mem.availableMb === "number",
  JSON.stringify(mem).slice(0, 120));

// 3. Backend invoke: process list (focus engine)
const procs = await evaluate(`window.__TAURI__.core.invoke("list_processes", { extra: [] })`);
check("list_processes returns rows", Array.isArray(procs) && procs.length > 10, `${procs.length} processes`);
const sorted = procs.every((p, i) => i === 0 || (procs[i - 1].ramMb ?? 0) >= (p.ramMb ?? 0));
check("processes sorted by RAM desc", sorted);

// 4. DB: create + list a track
await evaluate(`window.__TAURI__.core.invoke("create_track", { name: "E2E Python", language: "python" })`);
const tracks = await evaluate(`window.__TAURI__.core.invoke("list_tracks")`);
const t = (tracks || []).find((x) => x.name === "E2E Python");
check("create_track + list_tracks roundtrip", !!t, JSON.stringify(tracks).slice(0, 150));

// 5. Lesson payload save/load roundtrip
const payload = {
  title: "E2E lesson", prompt: "compute sum", starter: "def solve():\n    pass",
  tests: "assert solve([1,2]) == 3", hints: ["h1", "h2"], solution: "def solve(a):\n    return sum(a)",
};
await evaluate(`window.__TAURI__.core.invoke("save_lesson", { trackId: ${t?.id ?? 0}, idx: 1, title: ${JSON.stringify(payload.title)}, payload: ${JSON.stringify(JSON.stringify(payload))} })`);
const lessons = await evaluate(`window.__TAURI__.core.invoke("list_lessons", { trackId: ${t?.id ?? 0} })`);
check("save_lesson + list_lessons", Array.isArray(lessons) && lessons.length > 0, `${lessons.length} lessons`);

const lesson = lessons[0];
const got = await evaluate(`window.__TAURI__.core.invoke("get_lesson", { trackId: ${t?.id ?? 0}, idx: 1 })`);
const gotPayload = typeof got?.payload === "string" ? JSON.parse(got.payload) : got?.payload;
check("get_lesson payload roundtrip", gotPayload?.title === payload.title && gotPayload?.hints?.length === 2,
  JSON.stringify(gotPayload).slice(0, 120));

// 6. Sandbox: run failing python code with tests
const runFail = await evaluate(`window.__TAURI__.core.invoke("run_code", { req: { language: "python", code: "def solve(a):\\n    return 0\\n\\n#TESTS\\n" + ${JSON.stringify(payload.tests)}, timeoutMs: 10000 } })`);
check("sandbox: failing test detected", runFail && runFail.ok === false && (runFail.stderr || "").includes("AssertionError"),
  JSON.stringify(runFail).slice(0, 160));

// 7. Sandbox: passing code
const runPass = await evaluate(`window.__TAURI__.core.invoke("run_code", { req: { language: "python", code: "def solve(a):\\n    return sum(a)\\n\\n#TESTS\\n" + ${JSON.stringify(payload.tests)}, timeoutMs: 10000 } })`);
check("sandbox: passing test ok", runPass && runPass.ok === true, JSON.stringify(runPass).slice(0, 120));

// 8. Sandbox: timeout kill (infinite loop)
const runLoop = await evaluate(`window.__TAURI__.core.invoke("run_code", { req: { language: "python", code: "while True:\\n    x = 1\\n", timeoutMs: 3000 } })`);
check("sandbox: infinite loop timed out", runLoop && runLoop.ok === false && runLoop.timedOut === true,
  JSON.stringify(runLoop).slice(0, 160));

// 9. Web tier: search via Rust (bypasses WebView CORS)
const search = await evaluate(`window.__TAURI__.core.invoke("web_search", { query: "python list comprehension" })`);
check("web_search returns results", Array.isArray(search) && search.length > 0, `${search.length} results: ${(search[0]?.title || "").slice(0, 60)}`);

// 10. Fetch tier
const fetched = await evaluate(`window.__TAURI__.core.invoke("fetch_url", { url: "https://example.com/?e2e=" + Date.now() })`);
check("fetch_url strips html", fetched && ((fetched.title || "") + (fetched.text || "")).includes("Example Domain"), JSON.stringify(fetched).slice(0, 100));

// 11. Ollama health via Rust proxy (origin is blocked from WebView)
const health = await evaluate(`window.__TAURI__.core.invoke("ollama_health")`);
check("ollama_health ok through Rust proxy", health !== undefined && health !== null, JSON.stringify(health).slice(0, 150));

// 12. UI nav: click first nav item, app still alive
await evaluate(`document.querySelector('.topbar a, .topbar button')?.click()`);
await new Promise((r) => setTimeout(r, 400));
const appAlive = await evaluate(`(document.querySelector("#app")?.innerText || document.body.innerText).length > 0`);
check("nav click renders a view", appAlive);

console.log("\n==== SUMMARY:", results.filter((r) => r.ok).length, "/", results.length, "passed ====");
ws.close();
process.exit(results.some((r) => !r.ok) ? 1 : 0);
