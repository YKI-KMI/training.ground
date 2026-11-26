# Training Ground

Minimalist Tauri 2 desktop app that (a) frees RAM via an approving Focus Mode,
(b) teaches coding step-by-step with a local Ollama model (question → editor →
run → hint ladder → solution → select-to-explain), and (c) answers from a
3-tier resource engine: model knowledge → local indexed files (hybrid
vector + keyword search) → model-initiated web search/fetch.

## Features

- **Focus Mode** — live memory bar per-process, processes sorted by RAM,
  checkboxes + Close (frees RAM via TerminateProcess) / Suspend / Trim.
  A hard-coded protected list (system + the app itself + Ollama) is never
  touched. Closure approval always sits with the user.
- **Deep Focus** — one aggressive sweep over every non-essential process,
  in one of two reversible modes: *suspend* (pauses everything, restores
  instantly, RAM stays used) or *close* (ends them, frees RAM, relaunched on
  restore). Every touched process is recorded in a `focus_session` table, so
  "end deep focus & restore" brings everything back with one click — even
  after an app restart. No reboot needed; the shell (explorer) is an explicit
  opt-in.
- **Learn** — pick a track, generate a lesson from the local LLM (the model
  may request web results by emitting `[WEB: query]`; max 3 rounds),
  write code in CodeMirror, run it against injected tests inside a
  256 MB / 10 s sandbox, climb a hint ladder, view the streamed solution
  as markdown, select any span for a "explain this" popover.
- **Library** — index any folder (txt/pdf, up to 3000 files) into local
  embeddings (nomic-embed-text) + SQLite; hybrid 0.7 vector / 0.3 keyword
  search with at most 2 chunks per file.
- **Progress** — attempts and pass rates per track from SQLite.
- **Settings** — chat model choice, web tier toggle, extra protected
  processes for Focus.

## Prerequisites

- **Node 22+**, **Rust stable (MSVC toolchain + VS Build Tools)**, WebView2
  runtime (present on Windows 11 by default).
- **Ollama** running on `http://127.0.0.1:11434` with:

  ```sh
  ollama pull qwen2.5-coder:7b   # chat/lesson model (default)
  ollama pull nomic-embed-text   # embeddings for the Library tier
  ```

  Other installed chat models are selectable in Settings. `qwen2.5-coder:32b`
  exists in the catalog but needs ~19 GB RAM — too big for a 16 GB machine.
- Python 3 and an editor toolchain (gcc / rustc / javac) on PATH if you want
  the sandbox to run the corresponding languages. Python is the default track.

## Run in development

```sh
npm install
npm run tauri dev
```

The dev URL is `http://localhost:1420` (strictPort — see Troubleshooting).
All LLM and web traffic is proxied through Rust (reqwest) because Ollama
blocks the WebView2 origin by CORS; that's by design, not a bug.

## Production build

```sh
npm run tauri build
```

Artifacts land in `src-tauri/target/release/bundle/`. The app stores its
database at `%APPDATA%\com.yashv.training-ground\training.db` (SQLite, WAL).

## Automated E2E (against the real window)

Start the dev app with WebView2 remote debugging so the CDP driver can reach it:

```sh
npm run dev:cdp   # sets WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS and launches
```

In a second terminal:

```sh
npm run e2e
```

14 checks run against the live app: topbar, memory snapshot, process list,
track/lesson DB roundtrip, sandbox (failing test / passing test / infinite-loop
timeout), web search, URL fetch, Ollama health, and nav clicks. Requires Node
22+ (built-in WebSocket client); exits non-zero if any check fails.

## Rust tests

```sh
cargo test          # add -j 1 on a 16 GB machine (see Troubleshooting)
```

Unit tests cover process classification safety, test injection, sandbox
pass/fail/timeout/stdin/output caps, HTML stripping, search parsing, chunking,
and a live end-to-end index+search (needs Ollama up + network).

## Architecture

- `src-tauri/src/focus.rs` — process scan/classify, close (with approval),
  suspend, trim; protected list that is never touched. Deep Focus sweep
  (`deep_focus_apply`) plus the reversible session (`focus_session`,
  `focus_restore`: resume by pid with a name check, relaunch deduped by exe).
- `src-tauri/src/sandbox.rs` — Job Object (256 MB) + run/compile timeouts,
  interpreter fallback chains, `#TESTS` splice for java/rust/c.
- `src-tauri/src/db.rs` — SQLite (tracks / lessons / attempts / settings /
  chunks / fetch_cache / focus_session) + in-code migration for
  fetch_cache.title.
- `src-tauri/src/ollama.rs` — chat (NDJSON streamed via events) + embeddings.
- `src-tauri/src/web.rs` — DuckDuckGo → StackOverflow → Wikipedia search,
  URL fetch with 20 s timeout, title extraction, 7-day cache (title + text).
- `src-tauri/src/resources.rs` — chunk + embed (+ hybrid score) local files.
- `src-tauri/src/lib.rs` — state, DB bootstrap, all command registrations.
- `src/learn.ts`, `src/views.ts`, `src/api.ts`, `src/editor.ts` — UI:
  tracks, lesson generation, hint ladder, select-to-explain, focus table,
  library, progress, settings.
- `e2e.mjs`, `dev-cdp.mjs` — CDP driver + dev launcher for the E2E suite.

## Troubleshooting (Windows, 16 GB RAM machine)

- **`Error: Port 1420 is already in use`** — an old Vite dev server survived.
  Find and kill it: `netstat -ano | findstr :1420`, then `taskkill /F /PID <pid>`.
- **`failed to remove file ... training-ground.exe (os error 5)`** — the
  previous app instance is still running or a WebView2 host is attached.
  Close it first; `tauri dev` cannot overwrite a running exe.
- **rustc OOM / `STATUS_STACK_BUFFER_OVERRUN` (0xc0000409)** — full debuginfo
  on the `windows`/`tauri-utils` crates exhausts the 16 GB RAM / ~19 GB commit
  limit. This repo's `Cargo.toml` already sets lean `[profile.dev]`/`[profile.test]`
  debuginfo and `debug = false` for the windows crates. If you still hit it:
  close RAM-heavy apps, unload Ollama models (`curl -X POST :11434/api/generate
  -d '{"keep_alive":0,...}'`), and build with `cargo build/test -j 1`.
- **Ollama 403 from the app** — expected; Ollama rejects foreign origins. The
  app therefore never calls Ollama from the frontend; Rust proxies everything.
- **e2e refuses to connect (ECONNREFUSED 9333)** — the app wasn't launched via
  `npm run dev:cdp`, or an earlier instance (without the debugging port) is
  still up. Kill it and relaunch with `dev:cdp`.
- **fetch_url returns a stale page** — cached for 7 days keyed by exact URL;
  append a `?_=` query param to bypass.
