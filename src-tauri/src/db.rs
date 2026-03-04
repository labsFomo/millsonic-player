use rusqlite::{Connection, params};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use crate::config;

static DB: OnceLock<Mutex<Connection>> = OnceLock::new();

pub fn db() -> &'static Mutex<Connection> {
    DB.get_or_init(|| {
        let path = db_path();
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let conn = Connection::open(&path).expect("Cannot open SQLite DB");
        init_tables(&conn);
        Mutex::new(conn)
    })
}

fn db_path() -> PathBuf {
    config::AppConfig::data_dir().join("millsonic.db")
}

fn init_tables(conn: &Connection) {
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS schedule (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            zone_id TEXT NOT NULL,
            day_of_week INTEGER NOT NULL,
            start_time TEXT NOT NULL,
            end_time TEXT NOT NULL,
            playlist_name TEXT,
            tracks_json TEXT,
            synced_at TEXT DEFAULT (datetime('now'))
        );
        CREATE TABLE IF NOT EXISTS tracks (
            id TEXT PRIMARY KEY,
            title TEXT,
            artist TEXT,
            artwork_url TEXT,
            duration REAL DEFAULT 0,
            file_path TEXT,
            downloaded_at TEXT DEFAULT (datetime('now')),
            last_played TEXT
        );
        CREATE TABLE IF NOT EXISTS pending_reports (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            track_id TEXT NOT NULL,
            zone_id TEXT NOT NULL,
            started_at TEXT NOT NULL,
            duration_secs REAL NOT NULL,
            sent INTEGER DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS config_cache (
            key TEXT PRIMARY KEY,
            value TEXT
        );
    ").expect("Cannot create tables");
}

/// Save schedule slots from sync response
pub fn save_schedule(zone_id: &str, slots: &[serde_json::Value]) {
    let conn = match db().lock() {
        Ok(c) => c,
        Err(_) => return,
    };
    // Clear old schedule for this zone
    let _ = conn.execute("DELETE FROM schedule WHERE zone_id = ?1", params![zone_id]);
    for slot in slots {
        let day = slot.get("dayOfWeek").and_then(|d| d.as_u64()).unwrap_or(0) as i32;
        let start = slot.get("startTime").and_then(|s| s.as_str()).unwrap_or("00:00");
        let end = slot.get("endTime").and_then(|s| s.as_str()).unwrap_or("23:59");
        let playlist_name = slot.get("playlist")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("");
        let tracks_json = slot.get("playlist")
            .and_then(|p| p.get("tracks"))
            .map(|t| t.to_string())
            .unwrap_or_else(|| "[]".to_string());
        let _ = conn.execute(
            "INSERT INTO schedule (zone_id, day_of_week, start_time, end_time, playlist_name, tracks_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![zone_id, day, start, end, playlist_name, tracks_json],
        );
    }
}

/// Load cached schedule for a zone and day
pub fn load_schedule(zone_id: &str, day_of_week: u32) -> Vec<serde_json::Value> {
    let conn = match db().lock() {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let mut stmt = match conn.prepare(
        "SELECT day_of_week, start_time, end_time, playlist_name, tracks_json FROM schedule WHERE zone_id = ?1 AND day_of_week = ?2"
    ) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let rows = stmt.query_map(params![zone_id, day_of_week], |row| {
        let day: i32 = row.get(0)?;
        let start: String = row.get(1)?;
        let end: String = row.get(2)?;
        let name: String = row.get(3)?;
        let tracks_str: String = row.get(4)?;
        let tracks: serde_json::Value = serde_json::from_str(&tracks_str).unwrap_or(serde_json::json!([]));
        Ok(serde_json::json!({
            "dayOfWeek": day,
            "startTime": start,
            "endTime": end,
            "playlist": {
                "name": name,
                "tracks": tracks
            }
        }))
    }).ok();
    match rows {
        Some(r) => r.filter_map(|r| r.ok()).collect(),
        None => vec![],
    }
}

/// Check if we have any cached schedule
pub fn has_cached_schedule(zone_id: &str) -> bool {
    let conn = match db().lock() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM schedule WHERE zone_id = ?1",
        params![zone_id],
        |row| row.get(0),
    ).unwrap_or(0);
    count > 0
}

/// Save/upsert a track record
pub fn upsert_track(id: &str, title: &str, artist: &str, artwork_url: Option<&str>, duration: f32, file_path: &str) {
    let conn = match db().lock() {
        Ok(c) => c,
        Err(_) => return,
    };
    let _ = conn.execute(
        "INSERT INTO tracks (id, title, artist, artwork_url, duration, file_path) VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(id) DO UPDATE SET title=?2, artist=?3, artwork_url=?4, duration=?5, file_path=?6",
        params![id, title, artist, artwork_url, duration, file_path],
    );
}

/// Update last_played timestamp for a track
pub fn touch_track(id: &str) {
    let conn = match db().lock() {
        Ok(c) => c,
        Err(_) => return,
    };
    let _ = conn.execute(
        "UPDATE tracks SET last_played = datetime('now') WHERE id = ?1",
        params![id],
    );
}

/// Get all cached tracks that have files on disk
pub fn get_all_cached_tracks() -> Vec<(String, String, String, Option<String>, f32, String)> {
    let conn = match db().lock() {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let mut stmt = match conn.prepare(
        "SELECT id, title, artist, artwork_url, duration, file_path FROM tracks WHERE file_path IS NOT NULL ORDER BY last_played DESC"
    ) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, f32>(4)?,
            row.get::<_, String>(5)?,
        ))
    }).ok()
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

/// Save a play report for later batch sending
pub fn save_play_report(track_id: &str, zone_id: &str, started_at: &str, duration_secs: f64) {
    let conn = match db().lock() {
        Ok(c) => c,
        Err(_) => return,
    };
    let _ = conn.execute(
        "INSERT INTO pending_reports (track_id, zone_id, started_at, duration_secs) VALUES (?1, ?2, ?3, ?4)",
        params![track_id, zone_id, started_at, duration_secs],
    );
}

/// Get unsent play reports
pub fn get_pending_reports() -> Vec<(i64, String, String, String, f64)> {
    let conn = match db().lock() {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let mut stmt = match conn.prepare(
        "SELECT id, track_id, zone_id, started_at, duration_secs FROM pending_reports WHERE sent = 0 ORDER BY id LIMIT 100"
    ) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, f64>(4)?,
        ))
    }).ok()
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

/// Mark reports as sent
pub fn mark_reports_sent(ids: &[i64]) {
    if ids.is_empty() { return; }
    let conn = match db().lock() {
        Ok(c) => c,
        Err(_) => return,
    };
    for id in ids {
        let _ = conn.execute("UPDATE pending_reports SET sent = 1 WHERE id = ?1", params![id]);
    }
}

/// Delete old sent reports (cleanup)
pub fn cleanup_sent_reports() {
    let conn = match db().lock() {
        Ok(c) => c,
        Err(_) => return,
    };
    let _ = conn.execute("DELETE FROM pending_reports WHERE sent = 1", []);
}

/// LRU cache cleanup - delete oldest-played tracks when disk < 500MB free
pub fn cleanup_cache() {
    use sysinfo::Disks;
    let disks = Disks::new_with_refreshed_list();
    let free_bytes: u64 = disks.iter().map(|d| d.available_space()).sum();
    let free_mb = free_bytes / 1_048_576;

    if free_mb >= 500 {
        return; // Plenty of space
    }

    log::warn!("Disk space low: {}MB free, running cache cleanup", free_mb);

    let conn = match db().lock() {
        Ok(c) => c,
        Err(_) => return,
    };

    // Get tracks ordered by oldest last_played, skip those without file_path
    let mut stmt = match conn.prepare(
        "SELECT id, file_path FROM tracks WHERE file_path IS NOT NULL ORDER BY COALESCE(last_played, '2000-01-01') ASC LIMIT 20"
    ) {
        Ok(s) => s,
        Err(_) => return,
    };

    let tracks: Vec<(String, String)> = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }).ok()
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default();

    for (id, path) in tracks {
        if std::path::Path::new(&path).exists() {
            if let Err(e) = std::fs::remove_file(&path) {
                log::error!("Failed to remove cached file {}: {}", path, e);
            } else {
                log::info!("LRU cleanup: removed {}", path);
                let _ = conn.execute("UPDATE tracks SET file_path = NULL WHERE id = ?1", params![id]);
            }
        }
        // Re-check free space
        let disks2 = Disks::new_with_refreshed_list();
        let new_free: u64 = disks2.iter().map(|d| d.available_space()).sum();
        if new_free / 1_048_576 >= 500 {
            break;
        }
    }
}

/// Save a config value
pub fn set_config(key: &str, value: &str) {
    let conn = match db().lock() {
        Ok(c) => c,
        Err(_) => return,
    };
    let _ = conn.execute(
        "INSERT INTO config_cache (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=?2",
        params![key, value],
    );
}

/// Get a config value
pub fn get_config(key: &str) -> Option<String> {
    let conn = match db().lock() {
        Ok(c) => c,
        Err(_) => return None,
    };
    conn.query_row(
        "SELECT value FROM config_cache WHERE key = ?1",
        params![key],
        |row| row.get(0),
    ).ok()
}
