use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::UpdaterExt;
use tauri_plugin_updater::Update;
use chrono::{Local, Timelike};
use std::time::Duration;

pub async fn start_update_loop(app: AppHandle) {
    // Wait 30 seconds before first check
    tokio::time::sleep(Duration::from_secs(30)).await;

    loop {
        log::info!("Checking for updates...");
        match check_for_update(&app).await {
            Ok(true) => log::info!("Update flow handled (notified frontend or auto-installed)"),
            Ok(false) => log::info!("No update available"),
            Err(e) => log::error!("Update check failed: {}", e),
        }
        // Check every 30 minutes so we reliably hit the 04:00–04:59 local
        // auto-update window even on machines with no human to click.
        tokio::time::sleep(Duration::from_secs(1800)).await;
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

            // Auto-update window: 04:00–04:59 LOCAL machine time. Unattended
            // local PCs never get a human to click "Actualizar", so during this
            // window we install automatically (same flow as install_update).
            let hour = Local::now().hour();
            if hour == 4 {
                log::info!(
                    "Auto-update window (04:{:02} local): installing v{}",
                    Local::now().minute(),
                    version
                );
                // download_and_install + R-09 graceful flush + restart.
                // On success this never returns (app.restart()), so there is
                // no re-install loop: we reboot into the new version.
                download_and_install_update(app, update).await?;
                return Ok(true);
            }

            // Outside the window: keep the existing behaviour — show the modal
            // and let a human confirm via the install_update command.
            let _ = app.emit("update-available", serde_json::json!({
                "version": version,
                "notes": body,
            }));

            Ok(true)
        }
        None => Ok(false),
    }
}

/// Shared install path: download_and_install with progress callbacks, then the
/// R-09 graceful flush of pending play reports, then app.restart(). Used by both
/// the manual `install_update` command and the 04:xx auto-update window.
/// On success this does not return (the app restarts).
async fn download_and_install_update(app: &AppHandle, update: Update) -> Result<(), String> {
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

    log::info!("Update installed, flushing reports + stopping audio before restart...");
    let _ = app.emit("update-progress", serde_json::json!({
        "phase": "restarting",
    }));

    // R-09: graceful — flush pending play reports and stop playback
    // cleanly so the restart doesn't lose data or cut mid-buffer.
    crate::sync::flush_reports_before_exit().await;

    // Restart the app (does not return)
    app.restart();
}

#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<String, String> {
    log::info!("User requested update installation");
    let updater = app.updater().map_err(|e| e.to_string())?;
    let update = updater.check().await.map_err(|e| e.to_string())?;

    match update {
        Some(update) => {
            // Same path as the auto-update window: download + R-09 flush + restart.
            download_and_install_update(&app, update).await?;
            // Unreachable (app.restart() above), but keeps the type Result<String>.
            Ok("restarting".to_string())
        }
        None => {
            Err("No update available".to_string())
        }
    }
}
