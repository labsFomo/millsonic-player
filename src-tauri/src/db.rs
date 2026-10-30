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
            assets_json TEXT,
            rotation TEXT,
            synced_at TEXT DEFAULT (datetime('now'))
        );
    ")?;
    // Migración tolerante: el CREATE de arriba sólo corre en instalaciones nuevas, así que
    // los equipos que ya están en la calle tienen `pending_reports` sin `completed`. El
    // ALTER falla con "duplicate column name" en cuanto la columna existe — eso es lo
    // esperado y se ignora a propósito; cualquier otro error tampoco debe tumbar el boot,
    // porque sin DB el player no arranca y la música se corta.
    //
    // Mismo patrón para TODAS las columnas nuevas. Son sólo `ADD COLUMN`: nunca se reescribe
    // ni se dropea la tabla, así que una fila vieja encolada sobrevive la migración y se
    // sigue enviando como lo que es (un play de música).
    //
    // ⚠️ Por qué no se relaja `track_id NOT NULL` ni `duration_secs NOT NULL` (que una fila
    // de anuncio no tiene): SQLite no sabe hacerlo con un ALTER, exige reescribir la tabla
    // entera (create+copy+drop+rename). Hacer eso en el boot de ~120 equipos instalados, para
    // ganar dos NULLs, es desproporcionado. En su lugar las filas de spot guardan `track_id=''`
    // y `duration_secs < 0` como "no aplica", y el serializador OMITE esos campos del JSON
    // (ver `pending_report_json`): el backend recibe exactamente la ausencia que espera.
    for (col, ddl) in [
        ("completed", "ALTER TABLE pending_reports ADD COLUMN completed INTEGER NOT NULL DEFAULT 0"),
        // 'track' = todo lo que ya estaba encolado y todo lo que encola el camino de música.
        ("media_type", "ALTER TABLE pending_reports ADD COLUMN media_type TEXT NOT NULL DEFAULT 'track'"),
        ("spot_id", "ALTER TABLE pending_reports ADD COLUMN spot_id TEXT"),
        ("schedule_id", "ALTER TABLE pending_reports ADD COLUMN schedule_id TEXT"),
        ("asset_id", "ALTER TABLE pending_reports ADD COLUMN asset_id TEXT"),
        ("outcome", "ALTER TABLE pending_reports ADD COLUMN outcome TEXT"),
        ("failure_reason", "ALTER TABLE pending_reports ADD COLUMN failure_reason TEXT"),
        // Audios del box (Fase 5). `spot_schedules` se borra y reescribe completa en cada
        // sync, así que acá el ALTER sólo tiene que dejar la columna disponible.
        ("assets_json", "ALTER TABLE spot_schedules ADD COLUMN assets_json TEXT"),
        ("rotation", "ALTER TABLE spot_schedules ADD COLUMN rotation TEXT"),
    ] {
        if let Err(e) = conn.execute(ddl, []) {
            log::debug!("columna {col} ya existente o no migrable: {e}");
        }
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

// ── Reporte de emisión de anuncios ───────────────────────────────────────────

/// Cómo terminó una emisión. Espeja EXACTAMENTE el enum del backend
/// (`SPOT_OUTCOMES` en `src/devices/dto/batch-play-report.dto.ts`), que valida con `@IsIn`:
/// un literal que no esté en esa lista es un 400 del ValidationPipe y el 400 tumba el BATCH
/// ENTERO, o sea que la flota deja de reportar también la música (telemetry_dto_400_fleet_blackout).
/// Por eso los literales viven acá, en un solo lugar, y nunca se construyen a mano.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpotOutcome {
    Completed,
    Interrupted,
    Failed,
}

impl SpotOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            SpotOutcome::Completed => "COMPLETED",
            SpotOutcome::Interrupted => "INTERRUPTED",
            SpotOutcome::Failed => "FAILED",
        }
    }
}

/// `failureReason` tiene `@MaxLength(200)` en el DTO: un carácter más y es 400 + batch entero
/// perdido. Se recorta por `chars()` y no por bytes a propósito — un mensaje con acentos
/// partido al medio haría panic (misma lección que el recorte de errores de `api.rs`).
pub const FAILURE_REASON_MAX_CHARS: usize = 200;

fn clamp_failure_reason(reason: &str) -> String {
    reason.chars().take(FAILURE_REASON_MAX_CHARS).collect()
}

/// `media_type` de una fila de la cola. Literal del backend, no inventar. (El de música es
/// `'track'` y vive en el DEFAULT de la columna: una fila que no sea spot se serializa por el
/// camino de siempre, sin mandar el campo.)
const MEDIA_TYPE_SPOT: &str = "spot";

/// Encola la emisión de un anuncio. Mismo lugar y mismo flusher que los plays de música:
/// SQLite primero, red después, así una emisión sobrevive un corte de luz o un finde offline.
///
/// `duration_secs = None` ⇒ el backend guarda NULL, que es DISTINTO de "duró 0s" (el promedio
/// del reporte miente si se mezclan). Se persiste como -1 porque la columna es NOT NULL.
#[allow(clippy::too_many_arguments)]
pub fn save_spot_report(
    spot_id: &str,
    schedule_id: &str,
    asset_id: Option<&str>,
    zone_id: &str,
    started_at: &str,
    duration_secs: Option<f64>,
    outcome: SpotOutcome,
    failure_reason: Option<&str>,
) {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    save_spot_report_in(
        &conn, spot_id, schedule_id, asset_id, zone_id, started_at, duration_secs, outcome,
        failure_reason,
    );
}

/// El INSERT, separado de la conexión global para poder testear el round-trip completo
/// (encolar → leer → serializar) contra una DB temporal.
#[allow(clippy::too_many_arguments)]
fn save_spot_report_in(
    conn: &Connection,
    spot_id: &str,
    schedule_id: &str,
    asset_id: Option<&str>,
    zone_id: &str,
    started_at: &str,
    duration_secs: Option<f64>,
    outcome: SpotOutcome,
    failure_reason: Option<&str>,
) {
    if spot_id.is_empty() {
        // Sin spotId el backend descarta la fila (en silencio), así que ni se encola: una fila
        // que nunca se puede aceptar sólo tapa las buenas hasta la purga de 7 días.
        // El guard vive acá, en el INSERT, para que ningún llamador futuro lo pueda saltear.
        log::warn!("save_spot_report sin spotId (schedule={schedule_id}) — no se encola");
        return;
    }
    let reason = failure_reason.map(clamp_failure_reason);
    let _ = conn.execute(
        "INSERT INTO pending_reports \
         (track_id, zone_id, started_at, duration_secs, completed, media_type, spot_id, schedule_id, asset_id, outcome, failure_reason) \
         VALUES ('', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            zone_id,
            started_at,
            duration_secs.unwrap_or(-1.0),
            (outcome == SpotOutcome::Completed) as i32,
            MEDIA_TYPE_SPOT,
            spot_id,
            // Cadena vacía → NULL: el backend quiere ausencia, no "".
            if schedule_id.is_empty() { None } else { Some(schedule_id) },
            asset_id.filter(|s| !s.is_empty()),
            outcome.as_str(),
            reason,
        ],
    );
}

/// Una fila de la cola local, lista para serializar. Era una tupla de 6; con los anuncios
/// pasaba a 12 y ya nadie sabría qué es `r.9`.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingReport {
    pub id: i64,
    pub media_type: String,
    pub track_id: String,
    pub zone_id: String,
    pub started_at: String,
    /// `None` = el player no midió la duración (columna < 0 en disco).
    pub duration_secs: Option<f64>,
    pub completed: bool,
    pub spot_id: Option<String>,
    pub schedule_id: Option<String>,
    pub asset_id: Option<String>,
    pub outcome: Option<String>,
    pub failure_reason: Option<String>,
}

/// Serializa UNA fila de la cola al shape que espera `POST /devices/:id/play-report-batch`.
///
/// Función pura (sin DB, sin red) a propósito: es el contrato con el backend y es lo único
/// que puede tumbar el batch entero con un 400, así que tiene que poder testearse sola.
///
/// RETROCOMPATIBILIDAD, que es la parte sagrada: una fila de música serializa EXACTAMENTE los
/// mismos 5 campos que 0.9.16, sin `mediaType` ni nada nuevo. Un batch sin filas de spot viaja
/// byte-idéntico a hoy.
pub fn pending_report_json(r: &PendingReport) -> serde_json::Value {
    if r.media_type != MEDIA_TYPE_SPOT {
        return serde_json::json!({
            "trackId": r.track_id,
            "zoneId": r.zone_id,
            "startedAt": r.started_at,
            "durationSecs": r.duration_secs.unwrap_or(0.0),
            "completed": r.completed,
        });
    }

    // Camino de anuncio. Se usan los alias `startedAt`/`durationSecs` — los MISMOS que ya usa
    // el camino de música de este player — para no tener dos convenciones en el mismo body.
    let mut obj = serde_json::Map::new();
    obj.insert("mediaType".into(), serde_json::json!(MEDIA_TYPE_SPOT));
    obj.insert("spotId".into(), serde_json::json!(r.spot_id.clone().unwrap_or_default()));
    obj.insert("zoneId".into(), serde_json::json!(r.zone_id));
    obj.insert("startedAt".into(), serde_json::json!(r.started_at));
    obj.insert("completed".into(), serde_json::json!(r.completed));
    if let Some(sid) = r.schedule_id.as_deref().filter(|s| !s.is_empty()) {
        obj.insert("scheduleId".into(), serde_json::json!(sid));
    }
    if let Some(aid) = r.asset_id.as_deref().filter(|s| !s.is_empty()) {
        obj.insert("assetId".into(), serde_json::json!(aid));
    }
    // Ausente ⇒ NULL en la base, que es distinto de 0s. No se manda `null` explícito: el campo
    // simplemente no viaja (el DTO es @IsOptional, un `null` con @IsNumber sería 400).
    if let Some(d) = r.duration_secs {
        obj.insert("durationSecs".into(), serde_json::json!(d));
    }
    // ⚠️ Whitelist DURA: sólo se manda `outcome` si es uno de los tres literales que el
    // backend acepta. Una fila con la columna corrupta/desconocida viaja SIN el campo y el
    // backend lo deriva de `completed`/`failureReason` — perder el detalle de una emisión es
    // infinitamente mejor que un 400 que descarta el batch entero.
    if let Some(o) = r.outcome.as_deref() {
        if [
            SpotOutcome::Completed.as_str(),
            SpotOutcome::Interrupted.as_str(),
            SpotOutcome::Failed.as_str(),
        ]
        .contains(&o)
        {
            obj.insert("outcome".into(), serde_json::json!(o));
        } else {
            log::warn!("Fila de spot con outcome desconocido ({o}) — se manda sin outcome");
        }
    }
    if let Some(fr) = r.failure_reason.as_deref().filter(|s| !s.is_empty()) {
        obj.insert("failureReason".into(), serde_json::json!(clamp_failure_reason(fr)));
    }
    serde_json::Value::Object(obj)
}

/// Cuántos reportes se leen por lote. Lo expone el módulo para que el flusher sepa
/// cuándo el lote vino lleno (= queda cola) y vuelva a pedir, sin hardcodear el número
/// en dos lugares que después se desincronizan.
pub const PENDING_REPORTS_BATCH: usize = 100;

/// Get unsent play reports (música y anuncios, en orden de encolado).
pub fn get_pending_reports() -> Vec<PendingReport> {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    get_pending_reports_in(&conn)
}

fn get_pending_reports_in(conn: &Connection) -> Vec<PendingReport> {
    let mut stmt = match conn.prepare(
        &format!(
            "SELECT id, track_id, zone_id, started_at, duration_secs, completed, \
                    COALESCE(media_type, 'track'), spot_id, schedule_id, asset_id, outcome, failure_reason \
             FROM pending_reports WHERE sent = 0 ORDER BY id LIMIT {PENDING_REPORTS_BATCH}"
        )
    ) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    stmt.query_map([], |row| {
        let dur: f64 = row.get(4)?;
        Ok(PendingReport {
            id: row.get(0)?,
            track_id: row.get(1)?,
            zone_id: row.get(2)?,
            started_at: row.get(3)?,
            // -1 (o cualquier negativo) = el player no midió la duración → NULL en el backend.
            duration_secs: if dur < 0.0 { None } else { Some(dur) },
            completed: row.get::<_, i64>(5)? != 0,
            media_type: row.get(6)?,
            spot_id: row.get(7)?,
            schedule_id: row.get(8)?,
            asset_id: row.get(9)?,
            outcome: row.get(10)?,
            failure_reason: row.get(11)?,
        })
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
        // Audios del box (Fase 5). Se guarda la lista TAL COMO LA MANDÓ EL SERVER (el orden es
        // autoritativo y la rotación depende de él), con el path local que le agregó el
        // descargador. `_assets` ausente = box de 0/1 audio → camino legacy de `audio_url`.
        let assets_json = spot.get("_assets").map(|v| v.to_string());
        let rotation = spot.get("rotation").and_then(|v| v.as_str());
        let _ = conn.execute(
            "INSERT OR REPLACE INTO spot_schedules (id, spot_id, name, audio_url, tts_text, days_of_week, start_time, end_time, frequency, track_frequency, start_date, end_date, file_path, assets_json, rotation) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            params![id, spot_id, name, audio_url, tts_text, days_of_week, start_time, end_time, frequency, track_frequency, start_date, end_date, file_path, assets_json, rotation],
        );
    }
}

/// Un audio del box, ya resuelto a disco. `file_path` vacío = el audio existe en el server
/// pero NO se pudo bajar: igual queda en la lista (el contrato prohíbe filtrar) para que la
/// rotación no se desordene y para poder reportar el fallo.
#[derive(Debug, Clone, PartialEq)]
pub struct SpotAssetRow {
    pub id: String,
    pub file_path: String,
    /// Duración declarada por el server (segundos). 0 = desconocida. Sólo se usa para
    /// distinguir COMPLETED de INTERRUPTED en el reporte; nunca para decidir si suena.
    pub duration: f32,
}

/// Una fila de `spot_schedules`. Era una tupla de 9 y con los audios del box pasaba a 12.
#[derive(Debug, Clone)]
pub struct SpotScheduleRow {
    /// `SpotSchedule.id` — la clave del cursor de rotación y el `scheduleId` del reporte.
    pub id: String,
    /// `Spot.id` — lo que el reporte de emisión necesita.
    pub spot_id: String,
    pub days_of_week: String,
    pub start_time: String,
    pub end_time: String,
    pub track_frequency: i32,
    pub frequency: i32,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    /// Audio por defecto cacheado (camino legacy). Vacío = no está en disco.
    pub file_path: String,
    /// URL firmada del audio por defecto. Vacía = el box no tiene audio (TTS / todo apagado).
    pub audio_url: String,
    /// Audios del box en el ORDEN AUTORITATIVO del server. Vacío = box de 0/1 audio.
    pub assets: Vec<SpotAssetRow>,
    /// `rotation` tal como vino. Un valor que este player no conoce ⇒ camino legacy.
    pub rotation: Option<String>,
}

/// Load all spot schedules from DB.
///
/// ⚠️ Ya NO filtra por `file_path != ''`. Antes un box cuyo audio no se pudo bajar
/// desaparecía de la lista y el anuncio simplemente no sonaba, sin un solo rastro — que es
/// exactamente el pedido que originó el reporte de emisión ("el anuncio no suena y nadie se
/// entera"). Ahora la fila llega hasta la lógica de cadencia, que decide si hay algo
/// reproducible y, si no hay, encola un reporte `FAILED` (throttleado).
pub fn load_spot_schedules() -> Vec<SpotScheduleRow> {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    let mut stmt = match conn.prepare(
        "SELECT id, spot_id, days_of_week, start_time, end_time, track_frequency, frequency, \
                start_date, end_date, COALESCE(file_path, ''), COALESCE(audio_url, ''), \
                assets_json, rotation \
         FROM spot_schedules"
    ) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    stmt.query_map([], |row| {
        let assets_json: Option<String> = row.get(11)?;
        Ok(SpotScheduleRow {
            id: row.get(0)?,
            spot_id: row.get(1)?,
            days_of_week: row.get(2)?,
            start_time: row.get(3)?,
            end_time: row.get(4)?,
            track_frequency: row.get(5)?,
            frequency: row.get(6)?,
            start_date: row.get(7)?,
            end_date: row.get(8)?,
            file_path: row.get(9)?,
            audio_url: row.get(10)?,
            assets: parse_assets_json(assets_json.as_deref()),
            rotation: row.get(12)?,
        })
    }).ok()
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

/// Parsea `assets_json` preservando el orden. Un JSON corrupto devuelve lista vacía (= camino
/// legacy de `audio_url`), nunca un panic: la música no se corta por una columna rota.
fn parse_assets_json(raw: Option<&str>) -> Vec<SpotAssetRow> {
    let raw = match raw {
        Some(r) if !r.is_empty() => r,
        _ => return vec![],
    };
    let parsed: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("assets_json ilegible ({e}) — se usa el audio por defecto del box");
            return vec![];
        }
    };
    parsed
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let id = a.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    if id.is_empty() {
                        return None;
                    }
                    Some(SpotAssetRow {
                        id: id.to_string(),
                        file_path: a
                            .get("filePath")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        duration: a
                            .get("duration")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0) as f32,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Save a config value
pub fn set_config(key: &str, value: &str) {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    set_config_in(&conn, key, value);
}

/// Get a config value
pub fn get_config(key: &str) -> Option<String> {
    let conn = db().lock().unwrap_or_else(|e| e.into_inner());
    get_config_in(&conn, key)
}

// El SQL de `config_cache` separado de la conexión global: es donde viven el cursor de
// rotación y la cadencia de los anuncios, así que el round-trip tiene que poder testearse
// contra una DB temporal en vez de contra el `data_dir` del usuario.
fn set_config_in(conn: &Connection, key: &str, value: &str) {
    let _ = conn.execute(
        "INSERT INTO config_cache (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=?2",
        params![key, value],
    );
}

fn get_config_in(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM config_cache WHERE key = ?1",
        params![key],
        |row| row.get(0),
    ).ok()
}

// ── Estado persistido de los anuncios ────────────────────────────────────────
// Todo esto vivía SÓLO en memoria y se perdía en cada reinicio (un equipo que se reinicia a
// la noche arrancaba el día con la cadencia en cero y la rotación en el primer audio). Va a
// `config_cache`, que es una tabla que YA existe en los ~120 equipos instalados: cero
// migración, cero riesgo de boot.

/// Cursor de rotación del box, por `SpotSchedule.id`. Es el único dato que el contrato pide
/// persistir (`lastAssetId`).
fn asset_cursor_key(schedule_id: &str) -> String {
    format!("spot_last_asset:{schedule_id}")
}

pub fn get_spot_last_asset(schedule_id: &str) -> Option<String> {
    get_config(&asset_cursor_key(schedule_id)).filter(|s| !s.is_empty())
}

pub fn set_spot_last_asset(schedule_id: &str, asset_id: &str) {
    set_config(&asset_cursor_key(schedule_id), asset_id);
}

/// Último box que sonó (anti-repeat entre boxes distintos de la misma zona).
const KEY_LAST_SPOT_SCHEDULE: &str = "spot_last_schedule_id";
/// Epoch (segundos) de la última emisión EXITOSA — la cadencia por minutos.
const KEY_LAST_SPOT_AT: &str = "spot_last_played_at";

pub fn get_last_spot_schedule_id() -> Option<String> {
    get_config(KEY_LAST_SPOT_SCHEDULE).filter(|s| !s.is_empty())
}

pub fn set_last_spot_schedule_id(schedule_id: &str) {
    set_config(KEY_LAST_SPOT_SCHEDULE, schedule_id);
}

pub fn get_last_spot_at() -> Option<i64> {
    get_config(KEY_LAST_SPOT_AT).and_then(|v| v.parse::<i64>().ok())
}

pub fn set_last_spot_at(epoch: i64) {
    set_config(KEY_LAST_SPOT_AT, &epoch.to_string());
}

/// Helpers SÓLO para tests: dejan ejercitar el MISMO SQL que usa prod contra una DB temporal
/// en vez de contra el `data_dir` del usuario (la conexión global es un singleton y no se
/// puede apuntar a otro archivo). No se compilan en el binario de release.
#[cfg(test)]
pub mod test_support {
    use super::*;

    pub fn open(path: &PathBuf) -> Connection {
        open_or_recreate(path)
    }
    pub fn get_spot_last_asset(conn: &Connection, schedule_id: &str) -> Option<String> {
        get_config_in(conn, &asset_cursor_key(schedule_id)).filter(|s| !s.is_empty())
    }
    pub fn set_spot_last_asset(conn: &Connection, schedule_id: &str, asset_id: &str) {
        set_config_in(conn, &asset_cursor_key(schedule_id), asset_id);
    }
    pub fn get_last_spot_at(conn: &Connection) -> Option<i64> {
        get_config_in(conn, KEY_LAST_SPOT_AT).and_then(|v| v.parse().ok())
    }
    pub fn set_last_spot_at(conn: &Connection, epoch: i64) {
        set_config_in(conn, KEY_LAST_SPOT_AT, &epoch.to_string());
    }
    pub fn get_last_spot_schedule_id(conn: &Connection) -> Option<String> {
        get_config_in(conn, KEY_LAST_SPOT_SCHEDULE).filter(|s| !s.is_empty())
    }
    pub fn set_last_spot_schedule_id(conn: &Connection, id: &str) {
        set_config_in(conn, KEY_LAST_SPOT_SCHEDULE, id);
    }
    #[allow(clippy::too_many_arguments)]
    pub fn save_spot_report(
        conn: &Connection,
        spot_id: &str,
        schedule_id: &str,
        asset_id: Option<&str>,
        zone_id: &str,
        started_at: &str,
        duration_secs: Option<f64>,
        outcome: SpotOutcome,
        failure_reason: Option<&str>,
    ) {
        save_spot_report_in(
            conn, spot_id, schedule_id, asset_id, zone_id, started_at, duration_secs, outcome,
            failure_reason,
        );
    }
    pub fn save_play_report(
        conn: &Connection,
        track_id: &str,
        zone_id: &str,
        started_at: &str,
        duration_secs: f64,
        completed: bool,
    ) {
        let _ = conn.execute(
            "INSERT INTO pending_reports (track_id, zone_id, started_at, duration_secs, completed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![track_id, zone_id, started_at, duration_secs, completed as i32],
        );
    }
    pub fn pending(conn: &Connection) -> Vec<PendingReport> {
        get_pending_reports_in(conn)
    }
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

#[cfg(test)]
mod spot_report_serialization_tests {
    use super::*;

    fn track_row() -> PendingReport {
        PendingReport {
            id: 1,
            media_type: "track".into(),
            track_id: "tr-1".into(),
            zone_id: "z-1".into(),
            started_at: "2026-11-05T14:03:11+00:00".into(),
            duration_secs: Some(183.0),
            completed: true,
            spot_id: None,
            schedule_id: None,
            asset_id: None,
            outcome: None,
            failure_reason: None,
        }
    }

    fn spot_row() -> PendingReport {
        PendingReport {
            id: 2,
            media_type: "spot".into(),
            track_id: String::new(),
            zone_id: "z-1".into(),
            started_at: "2026-11-05T14:03:11.000Z".into(),
            duration_secs: Some(20.0),
            completed: true,
            spot_id: Some("sp-1".into()),
            schedule_id: Some("ss-1".into()),
            asset_id: Some("as-3".into()),
            outcome: Some("COMPLETED".into()),
            failure_reason: None,
        }
    }

    /// LO MÁS IMPORTANTE DE TODO EL ARCHIVO: un batch de sólo música tiene que viajar
    /// byte-idéntico a 0.9.16. Hay ~120 equipos reportando por este camino y un campo nuevo
    /// que el DTO no espere es un 400 que tumba el batch ENTERO.
    #[test]
    fn retrocompat_una_fila_de_musica_serializa_igual_que_0916() {
        let v = pending_report_json(&track_row());
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"completed":true,"durationSecs":183.0,"startedAt":"2026-11-05T14:03:11+00:00","trackId":"tr-1","zoneId":"z-1"}"#
        );
        // Exactamente los 5 campos de siempre: nada de mediaType/spotId/outcome.
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 5);
        for k in ["mediaType", "spotId", "scheduleId", "assetId", "outcome", "failureReason"] {
            assert!(obj.get(k).is_none(), "una fila de música no puede llevar {k}");
        }
    }

    #[test]
    fn retrocompat_media_type_desconocido_se_trata_como_musica() {
        // Una fila escrita por una versión futura y leída por esta: nunca se manda como spot.
        let mut r = track_row();
        r.media_type = "jingle".into();
        r.track_id = "tr-9".into();
        assert_eq!(pending_report_json(&r).as_object().unwrap().len(), 5);
    }

    #[test]
    fn spot_completed() {
        let v = pending_report_json(&spot_row());
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"assetId":"as-3","completed":true,"durationSecs":20.0,"mediaType":"spot","outcome":"COMPLETED","scheduleId":"ss-1","spotId":"sp-1","startedAt":"2026-11-05T14:03:11.000Z","zoneId":"z-1"}"#
        );
    }

    #[test]
    fn spot_interrupted() {
        let mut r = spot_row();
        r.outcome = Some("INTERRUPTED".into());
        r.completed = false;
        r.duration_secs = Some(7.0);
        r.failure_reason = Some("playback_cap_45s".into());
        let v = pending_report_json(&r);
        assert_eq!(v["mediaType"], "spot");
        assert_eq!(v["outcome"], "INTERRUPTED");
        assert_eq!(v["completed"], false);
        assert_eq!(v["durationSecs"], 7.0);
        assert_eq!(v["failureReason"], "playback_cap_45s");
    }

    #[test]
    fn spot_failed_sin_duracion() {
        let mut r = spot_row();
        r.outcome = Some("FAILED".into());
        r.completed = false;
        r.asset_id = None;
        r.duration_secs = None; // no sonó: NULL, que es distinto de 0s
        r.failure_reason = Some("audio_missing".into());
        let v = pending_report_json(&r);
        let obj = v.as_object().unwrap();
        assert!(obj.get("durationSecs").is_none(), "sin duración ⇒ el campo NO viaja (NULL != 0s)");
        assert!(obj.get("assetId").is_none(), "box de un audio ⇒ sin assetId");
        assert_eq!(v["outcome"], "FAILED");
        assert_eq!(v["failureReason"], "audio_missing");
    }

    #[test]
    fn spot_sin_schedule_ni_asset_no_manda_cadenas_vacias() {
        let mut r = spot_row();
        r.schedule_id = Some(String::new());
        r.asset_id = Some(String::new());
        let obj = pending_report_json(&r);
        let obj = obj.as_object().unwrap();
        assert!(obj.get("scheduleId").is_none());
        assert!(obj.get("assetId").is_none());
    }

    /// Un literal que el `@IsIn` del backend no acepta es un 400 y el 400 descarta el batch
    /// entero (música incluida). Antes que mandarlo, se manda SIN outcome y el server lo deriva.
    #[test]
    fn un_outcome_corrupto_viaja_sin_outcome_en_vez_de_tumbar_el_batch() {
        let mut r = spot_row();
        r.outcome = Some("medio_bien".into());
        let v = pending_report_json(&r);
        assert!(v.as_object().unwrap().get("outcome").is_none());
        // La emisión igual se reporta: se pierde el detalle, no la fila.
        assert_eq!(v["spotId"], "sp-1");
    }

    #[test]
    fn los_tres_literales_son_los_del_backend() {
        assert_eq!(SpotOutcome::Completed.as_str(), "COMPLETED");
        assert_eq!(SpotOutcome::Interrupted.as_str(), "INTERRUPTED");
        assert_eq!(SpotOutcome::Failed.as_str(), "FAILED");
    }

    /// `@MaxLength(200)`: un char más y es 400 + batch entero perdido.
    #[test]
    fn el_failure_reason_se_recorta_a_200_chars_sin_partir_acentos() {
        let largo = "ñ".repeat(5000);
        let mut r = spot_row();
        r.outcome = Some("FAILED".into());
        r.failure_reason = Some(largo);
        let v = pending_report_json(&r);
        let fr = v["failureReason"].as_str().unwrap();
        assert_eq!(fr.chars().count(), FAILURE_REASON_MAX_CHARS);
        assert!(fr.ends_with('ñ'), "se recortó por bytes y partió un char multibyte");
    }

    #[test]
    fn un_batch_mezclado_mantiene_el_orden_y_no_contamina_las_filas_de_musica() {
        let rows = vec![track_row(), spot_row(), track_row()];
        let out: Vec<serde_json::Value> = rows.iter().map(pending_report_json).collect();
        assert_eq!(out[0].as_object().unwrap().len(), 5);
        assert_eq!(out[1]["mediaType"], "spot");
        assert_eq!(out[2].as_object().unwrap().len(), 5);
    }
}

#[cfg(test)]
mod spot_queue_db_tests {
    use super::*;

    /// Los ~120 equipos instalados tienen `pending_reports` SIN ninguna de las columnas de
    /// anuncios. Este test es el que defiende el arranque de esos equipos: la migración no
    /// puede perder filas encoladas ni fallar en el segundo boot.
    #[test]
    fn migra_una_tabla_vieja_sin_perder_filas_y_es_idempotente() {
        let conn = Connection::open_in_memory().unwrap();
        // Tabla EXACTA de un equipo pre-0.9.16 (sin `completed` tampoco).
        conn.execute_batch(
            "CREATE TABLE pending_reports (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                track_id TEXT NOT NULL,
                zone_id TEXT NOT NULL,
                started_at TEXT NOT NULL,
                duration_secs REAL NOT NULL,
                sent INTEGER DEFAULT 0
            );
            CREATE TABLE spot_schedules (
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
                synced_at TEXT
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pending_reports (track_id, zone_id, started_at, duration_secs) VALUES ('viejo','z1','2026-01-01T00:00:00+00:00', 42.0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO spot_schedules (id, spot_id, audio_url, days_of_week, start_time, end_time, file_path) VALUES ('ss-old','sp-old','https://s3/a.mp3','[]','00:00','23:59','/tmp/x.mp3')",
            [],
        )
        .unwrap();

        init_tables(&conn).unwrap();
        init_tables(&conn).unwrap(); // segundo boot: los ALTER ya no aplican y no deben romper
        init_tables(&conn).unwrap(); // tercero, por si acaso

        // 1) La fila encolada SOBREVIVE y sigue siendo un play de música.
        let (mt, tid): (String, String) = conn
            .query_row(
                "SELECT COALESCE(media_type,'track'), track_id FROM pending_reports WHERE track_id='viejo'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(mt, "track", "una fila vieja NO puede volverse un anuncio");
        assert_eq!(tid, "viejo");
        let n: i64 = conn.query_row("SELECT count(*) FROM pending_reports", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "la migración perdió filas");

        // 2) Las columnas nuevas aceptan escrituras de anuncio.
        conn.execute(
            "INSERT INTO pending_reports (track_id, zone_id, started_at, duration_secs, completed, media_type, spot_id, schedule_id, asset_id, outcome, failure_reason) \
             VALUES ('', 'z1', '2026-11-05T14:03:11+00:00', -1.0, 0, 'spot', 'sp-1', 'ss-1', 'as-1', 'FAILED', 'audio_missing')",
            [],
        )
        .unwrap();

        // 3) `spot_schedules` migró y su fila vieja sigue ahí, sin assets (camino legacy).
        let (assets, rot): (Option<String>, Option<String>) = conn
            .query_row("SELECT assets_json, rotation FROM spot_schedules WHERE id='ss-old'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(assets, None);
        assert_eq!(rot, None);
    }

    /// `duration_secs` es NOT NULL en la tabla vieja y no se puede relajar con un ALTER, así
    /// que "sin duración" se persiste como negativo y se lee como `None`.
    #[test]
    fn el_sentinel_negativo_de_duracion_se_lee_como_none() {
        let r = PendingReport {
            id: 1, media_type: "spot".into(), track_id: String::new(), zone_id: "z".into(),
            started_at: "x".into(), duration_secs: None, completed: false,
            spot_id: Some("sp".into()), schedule_id: None, asset_id: None,
            outcome: Some("FAILED".into()), failure_reason: Some("audio_missing".into()),
        };
        assert!(pending_report_json(&r).as_object().unwrap().get("durationSecs").is_none());
    }

    #[test]
    fn assets_json_se_parsea_en_orden_y_un_json_roto_no_rompe_nada() {
        let ok = parse_assets_json(Some(
            r#"[{"id":"a1","filePath":"/tmp/a1.mp3","duration":20},{"id":"a2","filePath":"","duration":0}]"#,
        ));
        assert_eq!(ok.len(), 2);
        assert_eq!(ok[0].id, "a1");
        assert_eq!(ok[0].duration, 20.0);
        assert_eq!(ok[1].id, "a2");
        assert_eq!(ok[1].file_path, "", "el audio que no bajó se conserva en la lista");

        // Defensivo: nada de esto puede hacer panic ni cortar la música.
        assert!(parse_assets_json(None).is_empty());
        assert!(parse_assets_json(Some("")).is_empty());
        assert!(parse_assets_json(Some("{no es json")).is_empty());
        assert!(parse_assets_json(Some("{}")).is_empty());
        assert!(parse_assets_json(Some(r#"[{"sin":"id"}]"#)).is_empty());
    }
}

#[cfg(test)]
mod spot_queue_roundtrip_tests {
    use super::*;

    /// END-TO-END de la cola local, contra SQLite de verdad: encolar → leer → serializar.
    /// Es el JSON EXACTO que el player postea a `POST /devices/:id/play-report-batch`.
    #[test]
    fn roundtrip_completo_cola_a_json() {
        let conn = Connection::open_in_memory().unwrap();
        init_tables(&conn).unwrap();

        // Un play de música (como siempre) y tres emisiones de anuncio.
        test_support::save_play_report(&conn, "tr-1", "z-1", "2026-11-05T14:00:00+00:00", 183.0, true);
        test_support::save_spot_report(
            &conn, "sp-1", "ss-1", Some("as-3"), "z-1", "2026-11-05T14:03:11+00:00",
            Some(20.0), SpotOutcome::Completed, None,
        );
        test_support::save_spot_report(
            &conn, "sp-1", "ss-1", Some("as-4"), "z-1", "2026-11-05T14:33:11+00:00",
            Some(31.0), SpotOutcome::Interrupted, Some("playback_cap_45s"),
        );
        test_support::save_spot_report(
            &conn, "sp-2", "ss-2", None, "z-1", "2026-11-05T15:03:11+00:00",
            None, SpotOutcome::Failed, Some("download_failed"),
        );
        // Sin spotId no se encola (el backend la descartaría en silencio).
        test_support::save_spot_report(
            &conn, "", "ss-9", None, "z-1", "2026-11-05T15:04:00+00:00",
            None, SpotOutcome::Failed, Some("audio_missing"),
        );

        let rows = test_support::pending(&conn);
        assert_eq!(rows.len(), 4, "la fila sin spotId no debe encolarse");

        let batch: Vec<serde_json::Value> = rows.iter().map(pending_report_json).collect();
        let json = serde_json::to_string_pretty(&batch).unwrap();
        println!("--- reports[] que postea el player ---\n{json}");

        assert_eq!(
            serde_json::to_string(&batch).unwrap(),
            concat!(
                r#"[{"completed":true,"durationSecs":183.0,"startedAt":"2026-11-05T14:00:00+00:00","trackId":"tr-1","zoneId":"z-1"},"#,
                r#"{"assetId":"as-3","completed":true,"durationSecs":20.0,"mediaType":"spot","outcome":"COMPLETED","scheduleId":"ss-1","spotId":"sp-1","startedAt":"2026-11-05T14:03:11+00:00","zoneId":"z-1"},"#,
                r#"{"assetId":"as-4","completed":false,"durationSecs":31.0,"failureReason":"playback_cap_45s","mediaType":"spot","outcome":"INTERRUPTED","scheduleId":"ss-1","spotId":"sp-1","startedAt":"2026-11-05T14:33:11+00:00","zoneId":"z-1"},"#,
                r#"{"completed":false,"failureReason":"download_failed","mediaType":"spot","outcome":"FAILED","scheduleId":"ss-2","spotId":"sp-2","startedAt":"2026-11-05T15:03:11+00:00","zoneId":"z-1"}]"#,
            )
        );
    }
}
