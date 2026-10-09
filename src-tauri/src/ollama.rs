//! Ollama proxy. The WebView origin (http://tauri.localhost) is rejected by
//! Ollama's CORS check with 403 — verified — so every model call (streaming chat,
//! embeddings, health) is routed through Rust where CORS does not apply.
use serde::Serialize;
use serde_json::{json, Value};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

const OLLAMA: &str = "http://127.0.0.1:11434";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub ok: bool,
    pub models: Vec<String>,
    pub error: Option<String>,
}

#[tauri::command(async)]
pub async fn ollama_health() -> Health {
    let client = reqwest::Client::new();
    match client.get(format!("{OLLAMA}/api/tags")).timeout(Duration::from_secs(4)).send().await {
        Ok(r) if r.status().is_success() => match r.json::<Value>().await {
            Ok(v) => {
                let models = v["models"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m["name"].as_str().map(String::from))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                Health { ok: true, models, error: None }
            }
            Err(e) => Health { ok: false, models: vec![], error: Some(e.to_string()) },
        },
        Ok(r) => Health { ok: false, models: vec![], error: Some(format!("HTTP {}", r.status())) },
        Err(e) => Health { ok: false, models: vec![], error: Some(e.to_string()) },
    }
}

/// Batch embedding via /api/embed — shared by the command and the indexer.
pub async fn embed_batch(model: &str, input: &[String]) -> Result<Vec<Vec<f32>>, String> {
    if input.is_empty() {
        return Ok(vec![]);
    }
    let v: Value = reqwest::Client::new()
        .post(format!("{OLLAMA}/api/embed"))
        .json(&json!({ "model": model, "input": input }))
        .timeout(Duration::from_secs(180))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let arr = v["embeddings"].as_array().ok_or_else(|| "no embeddings in response".to_string())?;
    let mut out = Vec::with_capacity(arr.len());
    for e in arr {
        let dims = e.as_array().ok_or_else(|| "malformed embedding".to_string())?;
        out.push(dims.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect::<Vec<f32>>());
    }
    Ok(out)
}

#[tauri::command(async)]
pub async fn ollama_embed(model: String, input: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
    embed_batch(&model, &input).await
}

/// Streaming chat. Emits `ollama-chunk` events { rid, token, done, text } and
/// also returns the full text, so the frontend can use either signal.
#[tauri::command(async)]
pub async fn ollama_chat(payload: Value, rid: String, app: AppHandle) -> Result<String, String> {
    use futures_util::StreamExt;
    let mut p = payload;
    p["stream"] = Value::Bool(true);
    let resp = reqwest::Client::new()
        .post(format!("{OLLAMA}/api/chat"))
        .json(&p)
        .timeout(Duration::from_secs(600))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("ollama HTTP {}", resp.status()));
    }
    let mut full = String::new();
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut ollama_err: Option<String> = None;
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk.map_err(|e| e.to_string())?);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            if let Some(e) = v["error"].as_str() {
                ollama_err = Some(e.to_string());
            }
            if let Some(tok) = v["message"]["content"].as_str() {
                if !tok.is_empty() {
                    full.push_str(tok);
                    let _ = app.emit(
                        "ollama-chunk",
                        json!({ "rid": rid, "token": tok, "done": false, "text": "" }),
                    );
                }
            }
        }
    }
    if let Some(e) = ollama_err {
        return Err(e);
    }
    let _ = app.emit(
        "ollama-chunk",
        json!({ "rid": rid, "token": "", "done": true, "text": full }),
    );
    Ok(full)
}
