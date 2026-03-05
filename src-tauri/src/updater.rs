use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::UpdaterExt;
use std::time::Duration;

pub async fn start_update_loop(app: AppHandle) {
    // Wait 30 seconds before first check
    tokio::time::sleep(Duration::from_secs(30)).await;

    loop {
        log::info!("Checking for updates...");
        match check_for_update(&app).await {
            Ok(true) => log::info!("Update available, notified frontend"),
            Ok(false) => log::info!("No update available"),
            Err(e) => log::error!("Update check failed: {}", e),
        }
        // Check every hour
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

async fn check_for_update(app: &AppHandle) -> Result<bool, String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    let update = updater.check().await.map_err(|e| e.to_string())?;

    match update {
        Some(update) => {
            let version = update.version.clone();
            let body = update.body.clone().unwrap_or_default();
            log::info!("Update found: v{}", version);

            let _ = app.emit("update-available", serde_json::json!({
                "version": version,
                "notes": body,
            }));

            // Store update handle for later installation
            // We'll re-check when user confirms
            Ok(true)
        }
        None => Ok(false),
    }
}

#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<String, String> {
    log::info!("User requested update installation");
    let updater = app.updater().map_err(|e| e.to_string())?;
    let update = updater.check().await.map_err(|e| e.to_string())?;

    match update {
        Some(update) => {
            let version = update.version.clone();
            log::info!("Downloading update v{}...", version);

            let _ = app.emit("update-progress", serde_json::json!({
                "phase": "downloading",
                "version": version,
            }));

            // Download and install
            let mut downloaded: u64 = 0;
            let mut last_pct: u64 = 0;
            update.download_and_install(
                |chunk_len, content_len| {
                    downloaded += chunk_len as u64;
                    if let Some(total) = content_len {
                        let pct = (downloaded * 100) / total;
                        if pct != last_pct {
                            last_pct = pct;
                            let _ = app.emit("update-progress", serde_json::json!({
                                "phase": "downloading",
                                "percent": pct,
                            }));
                        }
                    }
                },
                || {
                    let _ = app.emit("update-progress", serde_json::json!({
                        "phase": "installing",
                    }));
                },
            ).await.map_err(|e| {
                log::error!("Update install failed: {}", e);
                e.to_string()
            })?;

            log::info!("Update installed, restarting...");
            let _ = app.emit("update-progress", serde_json::json!({
                "phase": "restarting",
            }));

            // Restart the app
            app.restart();
        }
        None => {
            Err("No update available".to_string())
        }
    }
}
