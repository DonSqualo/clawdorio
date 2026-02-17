// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[tauri::command]
fn engine_base_url() -> String {
    // For now, fixed port; later we can bind :0 and return the chosen port.
    "http://127.0.0.1:39333".to_string()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Start the local API server inside the desktop process.
    // This keeps the engine headless (same API used by non-Tauri clients).
    tauri::async_runtime::spawn(async move {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 39333));
        let db_path = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".clawdorio")
            .join("clawdorio.db");
        let _ = clawdorio_server::serve(addr, db_path).await;
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![greet, engine_base_url])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
