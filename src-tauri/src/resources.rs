//! Tier 2: the local resource index. Your books/PDFs/notes/code are chunked,
//! embedded via nomic-embed-text, stored in SQLite, and searched with hybrid
//! vector + keyword scoring. Fully offline; never leaves your machine.
use crate::{ollama, AppState};
use serde::Serialize;

pub const EMBED_MODEL: &str = "nomic-embed-text";
const EXT_TEXT: &[&str] = &[
    "md", "txt", "rst", "py", "js", "ts", "jsx", "tsx", "rs", "c", "cpp", "cc", "h", "hpp",
    "java", "go", "json", "toml", "yaml", "yml", "sh", "sql", "html", "css", "csv", "org", "adoc", "tex",
];
const SKIP_DIRS: &[&str] = &[
    ".git", "node_modules", "target", "dist", "build", "__pycache__", ".venv", "venv",
    ".idea", ".vscode", ".next", ".cache", "coverage", ".rustup", ".cargo",
];
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_FILES: usize = 3000;
const CHUNK_TARGET: usize = 1200;
const CHUNK_OVERLAP: usize = 200;
const EMBED_BATCH: usize = 8;

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct IndexReport {
    pub files: usize,
    pub chunks: usize,
    pub skipped: usize,
    pub errors: usize,
    pub messages: Vec<String>,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ResourceHit {
    pub path: String,
    pub text: String,
    pub score: f32,
}

fn emit(out: &mut Vec<String>, cur: &mut String, keep_tail: bool) {
    let c = cur.trim().to_string();
    cur.clear();
    if c.is_empty() {
        return;
    }
    if keep_tail && c.chars().count() > CHUNK_OVERLAP {
        let tail: String = c.chars().skip(c.chars().count() - CHUNK_OVERLAP).collect();
        out.push(c);
        cur.push_str(&tail);
    } else {
        out.push(c);
    }
}

/// Split text into ~CHUNK_TARGET-char pieces at paragraph boundaries, with
/// overlap across boundaries and hard word-safe splits for oversized blocks.
pub fn chunk_text(text: &str) -> Vec<String> {
    let norm = text.replace("\r\n", "\n");
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for block in norm.split("\n\n") {
        let b = block.trim();
        if b.is_empty() {
            continue;
        }
        let blen = b.chars().count();
        if blen > CHUNK_TARGET * 2 {
            if !cur.is_empty() {
                emit(&mut out, &mut cur, true);
            }
            let chars: Vec<char> = b.chars().collect();
            let mut s = 0usize;
            while s < chars.len() {
                let e = (s + CHUNK_TARGET).min(chars.len());
                out.push(chars[s..e].iter().collect());
                if e >= chars.len() {
                    break;
                }
                s = e.saturating_sub(CHUNK_OVERLAP);
            }
            continue;
        }
        if !cur.is_empty() && cur.chars().count() + blen + 2 > CHUNK_TARGET {
            emit(&mut out, &mut cur, true);
            if !cur.is_empty() {
                cur.push_str("\n\n");
            }
        } else if !cur.is_empty() {
            cur.push_str("\n\n");
        }
        cur.push_str(b);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn ext_of(path: &std::path::Path) -> String {
    path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).unwrap_or_default()
}

/// Recursively collect indexable files (text + pdf), skipping junk dirs.
pub fn collect_files(folder: &str) -> (Vec<std::path::PathBuf>, usize) {
    let mut files = Vec::new();
    let mut skipped = 0usize;
    let mut stack = vec![std::path::PathBuf::from(folder)];
    while let Some(dir) = stack.pop() {
        if files.len() >= MAX_FILES {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_lowercase();
                if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
                    skipped += 1;
                } else {
                    stack.push(path);
                }
                continue;
            }
            let ext = ext_of(&path);
            if ext == "pdf" || EXT_TEXT.contains(&ext.as_str()) {
                if entry.metadata().map(|m| m.len() <= MAX_FILE_BYTES).unwrap_or(false) {
                    files.push(path);
                } else {
                    skipped += 1;
                }
            }
        }
    }
    files.sort();
    (files, skipped)
}

fn extract_text(path: &std::path::Path) -> Result<String, String> {
    if ext_of(path) == "pdf" {
        pdf_extract::extract_text(path).map_err(|e| format!("pdf extract: {e}"))
    } else {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

pub fn normalize(v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.into_iter().map(|x| x / n).collect()
    } else {
        v
    }
}

struct Row {
    path: String,
    idx: i64,
    text: String,
    embedding: Vec<f32>,
}

/// Walk → extract → chunk → embed. No DB access, so it can run across awaits.
async fn index_core(folder: &str) -> Result<(Vec<Row>, IndexReport), String> {
    let (files, skipped) = collect_files(folder);
    let mut report = IndexReport { skipped, ..Default::default() };
    let mut pending: Vec<(String, i64, String)> = Vec::new();
    for f in &files {
        match extract_text(f) {
            Ok(text) => {
                let path = f.to_string_lossy().replace('/', "\\");
                for (i, c) in chunk_text(&text).into_iter().enumerate() {
                    pending.push((path.clone(), i as i64, c));
                }
                report.files += 1;
            }
            Err(e) => {
                report.errors += 1;
                report.messages.push(format!("{}: {e}", f.display()));
            }
        }
    }
    report.chunks = pending.len();
    let mut rows: Vec<Row> = Vec::with_capacity(pending.len());
    for batch in pending.chunks(EMBED_BATCH) {
        let texts: Vec<String> = batch.iter().map(|r| r.2.clone()).collect();
        match ollama::embed_batch(EMBED_MODEL, &texts).await {
            Ok(vecs) if vecs.len() == texts.len() => {
                for (r, v) in batch.iter().zip(vecs) {
                    rows.push(Row { path: r.0.clone(), idx: r.1, text: r.2.clone(), embedding: normalize(v) });
                }
            }
            Ok(_) => {
                report.errors += batch.len();
                report.messages.push("embedding count mismatch".into());
            }
            Err(e) => {
                report.errors += batch.len();
                report.messages.push(format!("embedding: {e}"));
                break;
            }
        }
    }
    Ok((rows, report))
}

/// Replace this folder's rows in one transaction (re-index is idempotent).
fn write_index(conn: &mut rusqlite::Connection, folder: &str, rows: &[Row]) -> Result<usize, String> {
    use rusqlite::params;
    let prefix = folder.replace('/', "\\");
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM chunks WHERE substr(path, 1, length(?1)) = ?1",
        params![prefix],
    )
    .map_err(|e| e.to_string())?;
    let mut n = 0usize;
    for r in rows {
        let blob: Vec<u8> = r.embedding.iter().flat_map(|f| f.to_le_bytes()).collect();
        tx.execute(
            "INSERT OR REPLACE INTO chunks(path, chunk_idx, text, embedding) VALUES(?1,?2,?3,?4)",
            rusqlite::params![r.path, r.idx, r.text, blob],
        )
        .map_err(|e| e.to_string())?;
        n += 1;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(n)
}

#[tauri::command(async)]
pub async fn index_folder(folder: String, state: tauri::State<'_, AppState>) -> Result<IndexReport, String> {
    if !std::path::Path::new(&folder).is_dir() {
        return Err(format!("not a folder: {folder}"));
    }
    let (rows, report) = index_core(&folder).await?;
    if report.chunks > 0 && rows.is_empty() && report.errors > 0 {
        return Err(format!(
            "embedding failed — is `{EMBED_MODEL}` pulled? ({})",
            report.messages.join("; ")
        ));
    }
    let mut db = state.db.lock().map_err(|e| e.to_string())?;
    write_index(&mut db, &folder, &rows)?;
    Ok(report)
}

/// Hybrid scoring: cosine over normalized embeddings (0.7) + keyword overlap (0.3).
/// With no query vector, keyword-only. Max 2 chunks per file so one book can't dominate.
fn search_rows(query: &str, qvec: Option<&[f32]>, conn: &rusqlite::Connection, top_k: usize) -> Vec<ResourceHit> {
    let words: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() >= 3)
        .collect();
    let mut stmt = conn
        .prepare("SELECT path, text, embedding FROM chunks")
        .expect("chunks table exists");
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Vec<u8>>(2)?))
        })
        .expect("query ok");
    let mut hits: Vec<ResourceHit> = Vec::new();
    for (path, text, blob) in rows.flatten() {
        let mut vec_score = 0f32;
        if let Some(qv) = qvec {
            if blob.len() % 4 == 0 && blob.len() / 4 == qv.len() {
                let v: Vec<f32> = blob
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect();
                vec_score = v.iter().zip(qv).map(|(a, b)| a * b).sum();
            }
        }
        let mut kw_score = 0f32;
        if !words.is_empty() {
            let tl = text.to_lowercase();
            let kw: f32 = words.iter().map(|w| tl.matches(w.as_str()).count() as f32).sum();
            kw_score = kw / (1.0 + kw);
        }
        let combined = if qvec.is_some() { 0.7 * vec_score + 0.3 * kw_score } else { kw_score };
        if combined > 0.0 {
            hits.push(ResourceHit {
                path,
                text: text.chars().take(700).collect(),
                score: combined,
            });
        }
    }
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    let mut per_path: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut out = Vec::new();
    for h in hits {
        let c = per_path.entry(h.path.clone()).or_insert(0);
        if *c < 2 {
            *c += 1;
            out.push(h);
            if out.len() >= top_k {
                break;
            }
        }
    }
    out
}

#[tauri::command(async)]
pub async fn search_resources(
    query: String,
    top_k: usize,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<ResourceHit>, String> {
    let qvec = match ollama::embed_batch(EMBED_MODEL, &[query.clone()]).await {
        Ok(mut v) if !v.is_empty() => Some(normalize(v.remove(0))),
        _ => None,
    };
    let db = state.db.lock().map_err(|e| e.to_string())?;
    Ok(search_rows(&query, qvec.as_deref(), &db, top_k.clamp(1, 10)))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceStats {
    pub files: i64,
    pub chunks: i64,
}

#[tauri::command(async)]
pub fn resource_stats(state: tauri::State<'_, AppState>) -> Result<ResourceStats, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let files = db.query_row("SELECT COUNT(DISTINCT path) FROM chunks", [], |r| r.get(0)).unwrap_or(0);
    let chunks = db.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0)).unwrap_or(0);
    Ok(ResourceStats { files, chunks })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("tg-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn chunking_covers_text_and_bounds_size() {
        let paras: Vec<String> = (0..100)
            .map(|i| format!("Paragraph {i} about list comprehensions and generators."))
            .collect();
        let text = paras.join("\n\n");
        let chunks = chunk_text(&text);
        assert!(chunks.len() > 3, "only {} chunks", chunks.len());
        for c in &chunks {
            assert!(!c.is_empty());
            assert!(c.chars().count() <= CHUNK_TARGET * 3, "chunk too big: {}", c.chars().count());
        }
        for p in &paras {
            assert!(chunks.iter().any(|c| c.contains(p)), "missing: {p}");
        }
    }

    #[test]
    fn oversized_block_splits_with_overlap() {
        let block: String = "word ".repeat(2000); // 10k chars, no blank lines
        let chunks = chunk_text(&block);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.chars().count() <= CHUNK_TARGET, "{}", c.chars().count());
        }
        let tail: String = chunks[0].chars().rev().take(50).collect::<Vec<_>>().into_iter().rev().collect();
        assert!(chunks[1].starts_with(&tail), "hard splits must overlap");
    }

    #[test]
    fn collect_files_skips_junk_dirs() {
        let tmp = tmp_dir("scan");
        std::fs::create_dir_all(tmp.join("node_modules")).unwrap();
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("node_modules").join("junk.js"), "junk").unwrap();
        std::fs::write(tmp.join("src").join("ok.py"), "def f(): return 1").unwrap();
        let (files, _) = collect_files(tmp.to_str().unwrap());
        assert_eq!(files.len(), 1, "{files:?}");
        assert!(files[0].to_string_lossy().ends_with("ok.py"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn index_and_search_end_to_end() {
        let tmp = tmp_dir("idx");
        std::fs::write(
            tmp.join("notes.md"),
            "The zipper pattern pushes onto stack A and pops from stack B to yield FIFO order. \
             Amortized cost stays constant across operations.",
        )
        .unwrap();
        let (rows, report) = index_core(tmp.to_str().unwrap()).await.expect("index core");
        assert_eq!(report.files, 1, "{:?}", report.messages);
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|r| r.embedding.iter().all(|v| v.is_finite())));
        let dim = rows[0].embedding.len();

        let q = ollama::embed_batch(EMBED_MODEL, &["zipper queue via two stacks".to_string()])
            .await
            .expect("embed query");
        let qv = normalize(q[0].clone());
        assert_eq!(qv.len(), dim);

        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        write_index(&mut conn, tmp.to_str().unwrap(), &rows).unwrap();

        let hits = search_rows("zipper queue via two stacks", Some(&qv), &conn, 5);
        assert!(!hits.is_empty());
        assert!(hits[0].path.contains("notes.md"), "{:?}", hits[0]);

        let kw = search_rows("zipper pattern", None, &conn, 5);
        assert!(!kw.is_empty(), "keyword-only path must work");

        // idempotent re-index with a different folder must not delete other rows
        let n = write_index(&mut conn, tmp.to_str().unwrap(), &rows).unwrap();
        assert_eq!(n, rows.len());
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0)).unwrap();
        assert_eq!(count as usize, rows.len(), "re-index must not duplicate rows");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
