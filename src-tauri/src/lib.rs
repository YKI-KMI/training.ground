mod db;
mod focus;
mod ollama;
mod resources;
mod sandbox;
mod web;

use std::sync::Mutex;
use tauri::Manager;

/// Shared app state: one SQLite connection behind a mutex. Deliberately held
/// only across synchronous SQL — never across an .await (keeps futures Send).
pub struct AppState {
    pub db: Mutex<rusqlite::Connection>,
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            let conn = db::open_db(&dir.join("training.db"))?;
            app.manage(AppState { db: Mutex::new(conn) });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // focus mode
            focus::list_processes,
            focus::memory_snapshot,
            focus::close_process,
            focus::suspend_process,
            focus::resume_process,
            focus::trim_process,
            focus::deep_focus_scan,
            focus::deep_focus_apply,
            focus::focus_session,
            focus::focus_restore,
            // sandbox
            sandbox::run_code,
            // curriculum ledger
            db::create_track,
            db::list_tracks,
            db::save_lesson,
            db::get_lesson,
            db::list_lessons,
            db::set_lesson_status,
            db::add_attempt,
            db::stats,
            db::get_settings,
            db::set_setting,
            // local model proxy
            ollama::ollama_health,
            ollama::ollama_embed,
            ollama::ollama_chat,
            // local resource index
            resources::index_folder,
            resources::search_resources,
            resources::resource_stats,
            // web tier
            web::fetch_url,
            web::web_search,
        ])
        .run(tauri::generate_context!())
        .expect("error while running training-ground");
}
