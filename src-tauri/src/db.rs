use rusqlite::{Connection, params};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use crate::config;

static DB: OnceLock<Mutex<Connection>> = OnceLock::new();

/// Todas las funciones de acá toman el lock con `.unwrap_or_else(|e| e.into_inner())`,
/// igual que el resto del repo (sync.rs, audio). Antes hacían `Err(_) => return` y eso
/// era una bomba: alcanza UN panic con el lock tomado (que main.rs además se traga con
/// catch_unwind) para que el mutex quede poisoned para siempre → `save_play_report` deja
/// de encolar y `get_pending_reports` devuelve vacío, sin un solo log. El equipo seguía
/// sonando y no reportaba nada nunca más. La conexión sigue usable después de un panic;
/// perder el reporting no.
pub fn db() -> &'static Mutex<Connection> {
    DB.get_or_init(|| {
        let path = db_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Mutex::new(open_or_recreate(&path))
    })
}

/// R-14: never panic on a bad cache DB. Open it; if it's corrupt/unreadable,
/// delete and recreate it (the DB is only a local cache — it's fully rebuilt
/// from the next sync, so dropping it is safe and far better than killing the
/// player). As an absolute last resort, fall back to an in-memory DB so music
/// never stops because of a storage problem.
fn open_or_recreate(path: &PathBuf) -> Connection {
    fn try_open(path: &PathBuf) -> Result<Connection, rusqlite::Error> {
        let conn = Connection::open(path)?;
        // Cheap readability probe: many corruptions open fine but fail a query.
        conn.query_row("SELECT 1", [], |_| Ok(()))?;
        init_tables(&conn)?;
        Ok(conn)
    }

    if let Ok(conn) = try_open(path) {
        return conn;
    }

    log::error!("SQLite cache DB is unusable — deleting and recreating it");
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));

    match try_open(path) {
        Ok(conn) => conn,
        Err(e) => {
            log::error!("Recreating cache DB failed ({e}) — using in-memory DB (no persistence)");
            let conn = Connection::open_in_memory()
                .expect("in-memory SQLite cannot fail to open");
            let _ = init_tables(&conn);
            conn
        }
    }
}

fn db_path() -> PathBuf {
    config::AppConfig::data_dir().join("millsonic.db")
}

fn init_tables(conn: &Connection) -> Result<(), rusqlite::Error> {
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
            sent INTEGER DEFAULT 0,
            completed INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS config_cache (
            key TEXT PRIMARY KEY,
            value TEXT
        );
        CREATE TABLE IF NOT EXISTS spot_schedules (
            id TEXT PRIMARY KEY,
            spot_id TEXT NOT NULL,
            name TEXT,
            audio_url TEXT,
            tts_text TEXT,
            days_of_week TEXT,
            start_time TEXT,
            end_time TEXT,
            frequency INTEGER DEFAULT 0,
            track_frequency INTEGER DEFAULT 4,
            start_date TEXT,
            end_date TEXT,
            file_path TEXT,
            synced_at TEXT DEFAULT (datetime('now'))
        );
    ")?;
    // Migración tolerante: el CREATE de arriba sólo corre en instalaciones nuevas, así que
    // los equipos que ya están en la calle tienen `pending_reports` sin `completed`. El
    // ALTER falla con "duplicate column name" en cuanto la columna existe — eso es lo
    // esperado y se ignora a propósito; cualquier otro error tampoco debe tumbar el boot,
    // porque sin DB el player no arranca y la música se corta.
    if let Err(e) = conn.execute(
        "ALTER TABLE pending_reports ADD COLUMN completed INTEGER NOT NULL DEFAULT 0",
        [],
    ) {
        log::debug!("pending_reports.completed ya existente o no migrable: {e}");
    }
    Ok(())
}

/// Save schedule slots from sync response
pub fn save_schedule(zone_id: &str, slots: &[serde_json::Value]) {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
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
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
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
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM schedule WHERE zone_id = ?1",
        params![zone_id],
        |row| row.get(0),
    ).unwrap_or(0);
    count > 0
}

/// Save/upsert a track record
pub fn upsert_track(id: &str, title: &str, artist: &str, artwork_url: Option<&str>, duration: f32, file_path: &str) {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    let _ = conn.execute(
        "INSERT INTO tracks (id, title, artist, artwork_url, duration, file_path) VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(id) DO UPDATE SET title=?2, artist=?3, artwork_url=?4, duration=?5, file_path=?6",
        params![id, title, artist, artwork_url, duration, file_path],
    );
}

/// Update last_played timestamp for a track
pub fn touch_track(id: &str) {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    let _ = conn.execute(
        "UPDATE tracks SET last_played = datetime('now') WHERE id = ?1",
        params![id],
    );
}

/// Get all cached tracks that have files on disk
pub fn get_all_cached_tracks() -> Vec<(String, String, String, Option<String>, f32, String)> {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
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

/// Save a play report for later batch sending.
/// `completed` viaja hasta el backend: sin él, PlayHistory queda con completed=false en
/// el 100% de los plays del desktop y los reportes de reproducción del cliente dan cero.
pub fn save_play_report(track_id: &str, zone_id: &str, started_at: &str, duration_secs: f64, completed: bool) {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    let _ = conn.execute(
        "INSERT INTO pending_reports (track_id, zone_id, started_at, duration_secs, completed) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![track_id, zone_id, started_at, duration_secs, completed as i32],
    );
}

/// Cuántos reportes se leen por lote. Lo expone el módulo para que el flusher sepa
/// cuándo el lote vino lleno (= queda cola) y vuelva a pedir, sin hardcodear el número
/// en dos lugares que después se desincronizan.
pub const PENDING_REPORTS_BATCH: usize = 100;

/// Get unsent play reports
pub fn get_pending_reports() -> Vec<(i64, String, String, String, f64, bool)> {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    let mut stmt = match conn.prepare(
        &format!("SELECT id, track_id, zone_id, started_at, duration_secs, completed FROM pending_reports WHERE sent = 0 ORDER BY id LIMIT {PENDING_REPORTS_BATCH}")
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
            row.get::<_, i64>(5)? != 0,
        ))
    }).ok()
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

/// Mark reports as sent
pub fn mark_reports_sent(ids: &[i64]) {
    if ids.is_empty() { return; }
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    for id in ids {
        let _ = conn.execute("UPDATE pending_reports SET sent = 1 WHERE id = ?1", params![id]);
    }
}

/// Días que retenemos un reporte que nunca se pudo enviar. Mismo criterio que el player
/// Android: pasada esa ventana el dato ya no le sirve a nadie y sólo engorda la cola.
const PENDING_REPORTS_RETENTION_DAYS: i64 = 7;

/// Delete old sent reports (cleanup).
/// Además vence las NO enviadas: sin esto la cola sólo se purgaba con sent=1, así que un
/// equipo con un trackId muerto (400 eterno) acumulaba filas para siempre y reintentaba
/// la misma basura en cada flush, tapando los reportes nuevos.
pub fn cleanup_sent_reports() {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    let _ = conn.execute("DELETE FROM pending_reports WHERE sent = 1", []);
    match conn.execute(
        // datetime(started_at) y no el string pelado: started_at es RFC3339 ("...T...+00:00")
        // y datetime('now') es "YYYY-MM-DD HH:MM:SS" — comparados como texto no coinciden.
        "DELETE FROM pending_reports WHERE sent = 0 AND datetime(started_at) < datetime('now', ?1)",
        params![format!("-{} days", PENDING_REPORTS_RETENTION_DAYS)],
    ) {
        Ok(n) if n > 0 => log::warn!(
            "Purgados {} play reports sin enviar de más de {} días",
            n, PENDING_REPORTS_RETENTION_DAYS
        ),
        _ => {}
    }
}

/// Minimum MB we always want free on the cache disk (R-03). Below this the LRU
/// cleanup runs until we're back above it (or there's nothing left to evict).
const CACHE_MIN_FREE_MB: u64 = 800;

/// Free space (MB) on the specific disk that holds the cache, NOT the sum of all
/// mounts. Summing every disk lets a big external drive mask a full cache
/// partition (R-03). We pick the mount whose path is the longest prefix of the
/// cache dir; fall back to the smallest free disk if none matches.
fn cache_disk_free_mb() -> u64 {
    use sysinfo::Disks;
    let cache = config::AppConfig::data_dir();
    let disks = Disks::new_with_refreshed_list();
    let mut best: Option<(usize, u64)> = None; // (mount len, free bytes)
    let mut smallest: Option<u64> = None;
    for d in disks.iter() {
        let free = d.available_space();
        smallest = Some(smallest.map_or(free, |s: u64| s.min(free)));
        let mp = d.mount_point();
        if cache.starts_with(mp) {
            let len = mp.to_string_lossy().len();
            if best.map_or(true, |(l, _)| len > l) {
                best = Some((len, free));
            }
        }
    }
    let bytes = best.map(|(_, f)| f).or(smallest).unwrap_or(u64::MAX);
    bytes / 1_048_576
}

/// LRU cache cleanup — evict oldest-played tracks until the cache disk has at
/// least CACHE_MIN_FREE_MB free again. Loops in batches (no silent 20-file cap)
/// and stops only when we're above the threshold or there's nothing left to
/// remove — so a single big download can't run the disk to 100%.
pub fn cleanup_cache() {
    let free_mb = cache_disk_free_mb();
    if free_mb >= CACHE_MIN_FREE_MB {
        return; // Plenty of space
    }
    log::warn!("Cache disk low: {}MB free (<{}MB) — running LRU cleanup", free_mb, CACHE_MIN_FREE_MB);

    let conn = db().lock().unwrap_or_else(|e| e.into_inner());

    let mut total_removed = 0;
    // Safety bound on iterations so a stuck filesystem can't spin forever.
    for _round in 0..50 {
        let batch: Vec<(String, String)> = {
            let mut stmt = match conn.prepare(
                "SELECT id, file_path FROM tracks WHERE file_path IS NOT NULL ORDER BY COALESCE(last_played, '2000-01-01') ASC LIMIT 20"
            ) {
                Ok(s) => s,
                Err(_) => return,
            };
            stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        };

        if batch.is_empty() {
            log::warn!("Cache cleanup: nothing left to evict ({} removed, still {}MB free)", total_removed, cache_disk_free_mb());
            break;
        }

        let mut removed_this_round = 0;
        for (id, path) in batch {
            if std::path::Path::new(&path).exists() {
                if let Err(e) = std::fs::remove_file(&path) {
                    log::error!("Failed to remove cached file {}: {}", path, e);
                } else {
                    removed_this_round += 1;
                    total_removed += 1;
                }
            }
            // Always clear the file_path so we don't re-select this row even if
            // the file was already gone.
            let _ = conn.execute("UPDATE tracks SET file_path = NULL WHERE id = ?1", params![id]);

            if cache_disk_free_mb() >= CACHE_MIN_FREE_MB {
                log::info!("Cache cleanup done: removed {} files, now {}MB free", total_removed, cache_disk_free_mb());
                return;
            }
        }
        if removed_this_round == 0 {
            break; // rows existed but no files to delete — avoid infinite loop
        }
    }
}

/// Save spot schedules from sync response
pub fn save_spot_schedules(spots: &[serde_json::Value]) {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    let _ = conn.execute("DELETE FROM spot_schedules", []);
    for spot in spots {
        let id = spot.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let spot_id = spot.get("spotId").and_then(|v| v.as_str()).unwrap_or("");
        let name = spot.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let audio_url = spot.get("audioUrl").and_then(|v| v.as_str()).unwrap_or("");
        let tts_text = spot.get("ttsText").and_then(|v| v.as_str()).unwrap_or("");
        let days_of_week = spot.get("daysOfWeek")
            .map(|v| v.to_string())
            .unwrap_or_else(|| "[]".to_string());
        let start_time = spot.get("startTime").and_then(|v| v.as_str()).unwrap_or("00:00");
        let end_time = spot.get("endTime").and_then(|v| v.as_str()).unwrap_or("23:59");
        let frequency = spot.get("frequency").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        // 0 = sin trackFrequency (el backend manda null en modo "minutos"). NO defaultear a
        // 4 acá: eso pisaba la señal de modo-minutos y forzaba cadencia por canciones.
        let track_frequency = spot.get("trackFrequency").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let start_date = spot.get("startDate").and_then(|v| v.as_str()).map(|s| s.to_string());
        let end_date = spot.get("endDate").and_then(|v| v.as_str()).map(|s| s.to_string());
        let file_path = spot.get("_filePath").and_then(|v| v.as_str()).unwrap_or("");
        let _ = conn.execute(
            "INSERT OR REPLACE INTO spot_schedules (id, spot_id, name, audio_url, tts_text, days_of_week, start_time, end_time, frequency, track_frequency, start_date, end_date, file_path) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![id, spot_id, name, audio_url, tts_text, days_of_week, start_time, end_time, frequency, track_frequency, start_date, end_date, file_path],
        );
    }
}

/// Load all spot schedules from DB
pub fn load_spot_schedules() -> Vec<(String, String, String, String, i32, i32, Option<String>, Option<String>, String)> {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    let mut stmt = match conn.prepare(
        "SELECT id, days_of_week, start_time, end_time, track_frequency, frequency, start_date, end_date, file_path FROM spot_schedules WHERE file_path IS NOT NULL AND file_path != ''"
    ) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i32>(4)?,
            row.get::<_, i32>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, String>(8)?,
        ))
    }).ok()
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

/// Save a config value
pub fn set_config(key: &str, value: &str) {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    let _ = conn.execute(
        "INSERT INTO config_cache (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=?2",
        params![key, value],
    );
}

/// Get a config value
pub fn get_config(key: &str) -> Option<String> {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    conn.query_row(
        "SELECT value FROM config_cache WHERE key = ?1",
        params![key],
        |row| row.get(0),
    ).ok()
}

#[cfg(test)]
mod pending_reports_migration_tests {
    use super::*;

    /// Los equipos que ya están en la calle tienen la tabla vieja, sin `completed`: el
    /// CREATE TABLE IF NOT EXISTS no los toca. Si el ALTER no fuera tolerante, el segundo
    /// boot fallaría con "duplicate column name" y el player se quedaría sin DB.
    #[test]
    fn migrates_a_legacy_pending_reports_table_and_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE pending_reports (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                track_id TEXT NOT NULL,
                zone_id TEXT NOT NULL,
                started_at TEXT NOT NULL,
                duration_secs REAL NOT NULL,
                sent INTEGER DEFAULT 0
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pending_reports (track_id, zone_id, started_at, duration_secs) VALUES ('t1','z1','2026-01-01T00:00:00+00:00', 42.0)",
            [],
        )
        .unwrap();

        init_tables(&conn).unwrap();
        init_tables(&conn).unwrap(); // segundo boot: el ALTER ya no aplica y no debe romper

        // La fila vieja sobrevive y queda con completed=0 (default), no NULL.
        let completed: i64 = conn
            .query_row("SELECT completed FROM pending_reports WHERE track_id = 't1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(completed, 0);

        // Y la columna acepta escrituras nuevas.
        conn.execute(
            "INSERT INTO pending_reports (track_id, zone_id, started_at, duration_secs, completed) VALUES ('t2','z1','2026-01-02T00:00:00+00:00', 200.0, 1)",
            [],
        )
        .unwrap();
        let completed: i64 = conn
            .query_row("SELECT completed FROM pending_reports WHERE track_id = 't2'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(completed, 1);
    }
}

#[cfg(test)]
mod db_recovery_tests {
    use super::*;

    #[test]
    fn recreates_a_corrupt_db_instead_of_panicking() {
        let path = std::env::temp_dir().join("millsonic_r14_recovery_test.db");
        let _ = std::fs::remove_file(&path);
        // Write garbage so it is NOT a valid SQLite file.
        std::fs::write(&path, b"definitely not a sqlite database header").unwrap();

        // Must recover (delete + recreate) and return a usable connection.
        let conn = open_or_recreate(&path);
        let one: i64 = conn.query_row("SELECT 1", [], |r| r.get(0)).unwrap();
        assert_eq!(one, 1);
        // Tables were initialised on the fresh DB.
        let n: i64 = conn.query_row("SELECT count(*) FROM tracks", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);

        drop(conn);
        let _ = std::fs::remove_file(&path);
    }
}
