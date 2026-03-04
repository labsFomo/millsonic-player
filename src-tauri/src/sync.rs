use tauri::AppHandle;
use std::time::Duration;

pub async fn start_sync_loop(_handle: AppHandle) {
    loop {
        // TODO: check if paired, sync schedule, download tracks
        tokio::time::sleep(Duration::from_secs(300)).await; // 5 min
    }
}
