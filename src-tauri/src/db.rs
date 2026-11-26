//! SQLite persistence: curriculum ledger (tracks/lessons/attempts), settings,
//! the local-resource chunk index, and the web fetch cache. The ledger is what
//! keeps your course stable across sessions — the model never re-decides it.
use crate::AppState;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::path::Path;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn open_db(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| e.to_string())?;
    conn.pragma_update(None, "journal_mode", "WAL").map_err(|e| e.to_string())?;
    conn.pragma_update(None, "foreign_keys", "ON").map_err(|e| e.to_string())?;
    conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
    // Migration: fetch_cache gained a `title` column (cached hits used to drop the title).
    let has_title: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('fetch_cache') WHERE name='title'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(true);
    if !has_title {
        conn.execute_batch(
            "ALTER TABLE fetch_cache ADD COLUMN title TEXT NOT NULL DEFAULT '';
             UPDATE fetch_cache SET title = '' WHERE title IS NULL;",
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(conn)
}

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tracks(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  language TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS lessons(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
  idx INTEGER NOT NULL,
  title TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'available',
  payload TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  UNIQUE(track_id, idx)
);
CREATE TABLE IF NOT EXISTS attempts(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  lesson_id INTEGER NOT NULL REFERENCES lessons(id) ON DELETE CASCADE,
  passed INTEGER NOT NULL,
  hints_used INTEGER NOT NULL,
  code TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS chunks(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  path TEXT NOT NULL,
  chunk_idx INTEGER NOT NULL,
  text TEXT NOT NULL,
  embedding BLOB NOT NULL,
  UNIQUE(path, chunk_idx)
);
CREATE TABLE IF NOT EXISTS fetch_cache(
  url TEXT PRIMARY KEY,
  title TEXT NOT NULL DEFAULT '',
  text TEXT NOT NULL,
  fetched_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS focus_session(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  pid INTEGER NOT NULL,
  name TEXT NOT NULL,
  exe TEXT NOT NULL,
  mode TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
";

// ---- focus session (reversible deep focus) --------------------------------
// One row per process a sweep touched, so the app can bring it back after a
// restart of the app itself. `mode` is "suspended" (resume) or "closed" (relaunch).

pub fn session_clear(db: &Connection) -> Result<(), String> {
    db.execute("DELETE FROM focus_session", []).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn session_add(db: &Connection, pid: u32, name: &str, exe: &str, mode: &str) -> Result<(), String> {
    db.execute(
        "INSERT INTO focus_session(pid, name, exe, mode, created_at) VALUES(?1,?2,?3,?4,?5)",
        params![pid, name, exe, mode, now()],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn session_entries(db: &Connection) -> Result<Vec<(u32, String, String, String)>, String> {
    let mut stmt = db
        .prepare("SELECT pid, name, exe, mode FROM focus_session ORDER BY id")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| e.to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub id: i64,
    pub name: String,
    pub language: String,
    pub created_at: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackInfo {
    pub id: i64,
    pub name: String,
    pub language: String,
    pub lessons: i64,
    pub done: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Lesson {
    pub id: i64,
    pub track_id: i64,
    pub idx: i64,
    pub title: String,
    pub status: String,
    pub payload: String,
}

#[tauri::command(async)]
pub fn create_track(
    name: String,
    language: String,
    state: tauri::State<'_, AppState>,
) -> Result<Track, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    db.execute(
        "INSERT INTO tracks(name, language, created_at) VALUES(?1, ?2, ?3)",
        params![name, language, now()],
    )
    .map_err(|e| e.to_string())?;
    let id = db.last_insert_rowid();
    Ok(Track { id, name, language, created_at: now() })
}

#[tauri::command(async)]
pub fn list_tracks(state: tauri::State<'_, AppState>) -> Result<Vec<TrackInfo>, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let mut stmt = db
        .prepare(
            "SELECT t.id, t.name, t.language,
                    (SELECT COUNT(*) FROM lessons l WHERE l.track_id=t.id),
                    (SELECT COUNT(*) FROM lessons l WHERE l.track_id=t.id AND l.status='done')
             FROM tracks t ORDER BY t.id",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(TrackInfo {
                id: r.get(0)?,
                name: r.get(1)?,
                language: r.get(2)?,
                lessons: r.get(3)?,
                done: r.get(4)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| e.to_string())
}

#[tauri::command(async)]
pub fn save_lesson(
    track_id: i64,
    idx: i64,
    title: String,
    payload: String,
    state: tauri::State<'_, AppState>,
) -> Result<i64, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    db.execute(
        "INSERT INTO lessons(track_id, idx, title, payload, created_at)
         VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(track_id, idx) DO UPDATE SET title=excluded.title, payload=excluded.payload",
        params![track_id, idx, title, payload, now()],
    )
    .map_err(|e| e.to_string())?;
    let id = db
        .query_row(
            "SELECT id FROM lessons WHERE track_id=?1 AND idx=?2",
            params![track_id, idx],
            |r| r.get::<_, i64>(0),
        )
        .map_err(|e| e.to_string())?;
    Ok(id)
}

#[tauri::command(async)]
pub fn get_lesson(track_id: i64, idx: i64, state: tauri::State<'_, AppState>) -> Result<Option<Lesson>, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let mut stmt = db
        .prepare("SELECT id, track_id, idx, title, status, payload FROM lessons WHERE track_id=?1 AND idx=?2")
        .map_err(|e| e.to_string())?;
    let mut rows = stmt.query_map(params![track_id, idx], |r| {
        Ok(Lesson { id: r.get(0)?, track_id: r.get(1)?, idx: r.get(2)?, title: r.get(3)?, status: r.get(4)?, payload: r.get(5)? })
    }).map_err(|e| e.to_string())?;
    match rows.next() {
        Some(row) => Ok(Some(row.map_err(|e| e.to_string())?)),
        None => Ok(None),
    }
}

#[tauri::command(async)]
pub fn list_lessons(track_id: i64, state: tauri::State<'_, AppState>) -> Result<Vec<Lesson>, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let mut stmt = db
        .prepare("SELECT id, track_id, idx, title, status, payload FROM lessons WHERE track_id=?1 ORDER BY idx")
        .map_err(|e| e.to_string())?;
    let rows = stmt.query_map(params![track_id], |r| {
        Ok(Lesson { id: r.get(0)?, track_id: r.get(1)?, idx: r.get(2)?, title: r.get(3)?, status: r.get(4)?, payload: r.get(5)? })
    }).map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| e.to_string())
}

#[tauri::command(async)]
pub fn set_lesson_status(lesson_id: i64, status: String, state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let n = db.execute("UPDATE lessons SET status=?1 WHERE id=?2", params![status, lesson_id]).map_err(|e| e.to_string())?;
    Ok(n > 0)
}

#[tauri::command(async)]
pub fn add_attempt(lesson_id: i64, passed: bool, hints_used: i64, code: String, state: tauri::State<'_, AppState>) -> Result<i64, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    db.execute(
        "INSERT INTO attempts(lesson_id, passed, hints_used, code, created_at) VALUES(?1,?2,?3,?4,?5)",
        params![lesson_id, passed as i64, hints_used, code, now()],
    ).map_err(|e| e.to_string())?;
    Ok(db.last_insert_rowid())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub tracks: i64,
    pub lessons_done: i64,
    pub attempts: i64,
    pub passed: i64,
    pub chunks: i64,
}

#[tauri::command(async)]
pub fn stats(state: tauri::State<'_, AppState>) -> Result<Stats, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let scalar = |sql: &str| -> Result<i64, String> {
        db.query_row(sql, [], |r| r.get(0)).map_err(|e| e.to_string())
    };
    Ok(Stats {
        tracks: scalar("SELECT COUNT(*) FROM tracks")?,
        lessons_done: scalar("SELECT COUNT(*) FROM lessons WHERE status='done'")?,
        attempts: scalar("SELECT COUNT(*) FROM attempts")?,
        passed: scalar("SELECT COUNT(*) FROM attempts WHERE passed=1")?,
        chunks: scalar("SELECT COUNT(*) FROM chunks")?,
    })
}

// ---- settings -------------------------------------------------------------
#[tauri::command(async)]
pub fn get_settings(state: tauri::State<'_, AppState>) -> Result<Vec<(String, String)>, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let mut stmt = db.prepare("SELECT key, value FROM settings").map_err(|e| e.to_string())?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| e.to_string())
}

#[tauri::command(async)]
pub fn set_setting(key: String, value: String, state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    db.execute("INSERT INTO settings(key, value) VALUES(?1,?2)
                ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value])
        .map_err(|e| e.to_string())?;
    Ok(true)
}
