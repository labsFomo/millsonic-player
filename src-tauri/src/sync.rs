use crate::{api, audio, config, db};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use chrono::{Datelike, Timelike, Utc};
use chrono_tz;
use tokio::sync::Notify;
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, AtomicI64, Ordering};
use std::collections::HashMap;

// ── R-11: server clock offset ────────────────────────────────────────────────
// The schedule position depends on time. If the box's system clock is wrong (no
// NTP, DST mishap) the wrong song/second plays. The server's now-playing returns
// `syncTimestamp` (ms epoch) — we treat that as the source of truth and keep an
// offset so local time calcs (spot windows, offline fallback) use SERVER time.
static SERVER_CLOCK_OFFSET_MS: AtomicI64 = AtomicI64::new(0);
static SERVER_CLOCK_SYNCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Update the offset from a server `syncTimestamp` (ms since epoch).
fn update_clock_offset(server_ts_ms: i64) {
    if server_ts_ms <= 0 { return; }
    let local_ms = Utc::now().timestamp_millis();
    let offset = server_ts_ms - local_ms;
    SERVER_CLOCK_OFFSET_MS.store(offset, Ordering::Relaxed);
    SERVER_CLOCK_SYNCED.store(true, Ordering::Relaxed);
    if offset.abs() > 5000 {
        log::warn!("R-11: device clock off by {}ms vs server — using server time", offset);
    }
}

/// Current time corrected to the server clock (falls back to local if never synced).
fn server_now() -> chrono::DateTime<Utc> {
    let off = SERVER_CLOCK_OFFSET_MS.load(Ordering::Relaxed);
    Utc::now() + chrono::Duration::milliseconds(off)
}

/// R-11: last known server clock offset (ms). Positive = local clock is behind server.
pub fn clock_offset_ms() -> i64 {
    SERVER_CLOCK_OFFSET_MS.load(Ordering::Relaxed)
}

// ── R-18: device token refresh ───────────────────────────────────────────────
/// Decode the JWT `exp` (seconds epoch) WITHOUT verifying the signature.
fn jwt_exp_secs(token: &str) -> Option<i64> {
    use base64::Engine;
    let payload_b64 = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    json.get("exp").and_then(|v| v.as_i64())
}

/// R-18: refresh the device token when it's within 30 days of expiring, so a
/// box in the field never silently dies at the 365-day mark.
async fn maybe_refresh_token() {
    let cfg = config::AppConfig::load();
    let (id, token) = match (cfg.device_id.clone(), cfg.device_token.clone()) {
        (Some(i), Some(t)) => (i, t),
        _ => return,
    };
    let exp = match jwt_exp_secs(&token) {
        Some(e) => e,
        None => return,
    };
    let days_left = (exp - Utc::now().timestamp()) / 86400;
    if days_left > 30 {
        return;
    }
    log::info!("R-18: device token expires in {} days — refreshing", days_left);
    match api::refresh_device_token(&id, &token).await {
        Ok(new_token) => {
            let _ = config::AppConfig::update_and_save(|c| {
                c.device_token = Some(new_token.clone());
            });
            log::info!("R-18: device token refreshed (fresh 365d)");
        }
        Err(e) => log::error!("R-18: token refresh failed: {}", e),
    }
}

// ── R-10 v3: re-anclaje periódico al now-playing del server ──────────────────
// Mantiene a todos los equipos de la zona en la misma LÍNEA de temas: cada
// ANCHOR_EVERY_N_TRACKS temas reproducidos, re-alinea a la canción que el server
// dice que va ahora. NO sincroniza por tema ni corta a la mitad (resync_to
// crossfadea y es no-op si ya está alineado). Sin rate-limit: el contador de
// temas ES el throttle. Offline → no hace nada (sigue la reproducción local).
const RESYNC_POLL_SECS: u64 = 30;
const ANCHOR_EVERY_N_TRACKS: u32 = 4;

pub async fn start_resync_loop(handle: AppHandle) {
    let mut last_seen_track = String::new();
    let mut tracks_played: u32 = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(RESYNC_POLL_SECS)).await;

        let cfg = config::AppConfig::load();
        let zone_id = match cfg.zone_id.clone() {
            Some(z) if !z.is_empty() => z,
            _ => continue,
        };
        let np = match api::fetch_zone_now_playing(&zone_id).await {
            Ok(j) => j,
            Err(_) => continue, // offline → leave local playback alone
        };
        // Refresh del reloj corregido (R-11) en cada poll.
        if let Some(ts) = np.get("syncTimestamp").and_then(|v| v.as_i64()) {
            update_clock_offset(ts);
        }
        let ct = match np.get("currentTrack") {
            Some(c) => c,
            None => continue,
        };
        let server_is_spot = ct.get("isSpot").and_then(|v| v.as_bool()).unwrap_or(false);
        let server_id = ct.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let seek = ct.get("seekPosition").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;

        let mut p = match audio::player().lock() {
            Ok(p) => p,
            Err(e) => e.into_inner(),
        };
        if !p.is_playing() {
            continue;
        }
        // Detectar cambio de tema local para contar temas reproducidos.
        let local_id = p.current_track_id().unwrap_or_default();
        if !local_id.is_empty() && local_id != last_seen_track {
            if !last_seen_track.is_empty() {
                tracks_played += 1;
            }
            last_seen_track = local_id.clone();
        }

        // Todavía no toca re-anclar.
        if tracks_played < ANCHOR_EVERY_N_TRACKS {
            continue;
        }

        // Llegó la hora de re-anclar. Guardas (no se resetea el contador → se
        // reintenta en el próximo poll si una guarda transitoria bloquea):
        //  - nunca durante un voto SonicBox sonando o encolado;
        //  - el server está en un spot → no re-anclar a un spot;
        //  - tema recién arrancado (<20s) → no pisarlo (espejo del cuidado R-10).
        if p.playing_sonicbox || p.has_sonicbox_next() {
            continue;
        }
        if server_is_spot || server_id.is_empty() {
            continue;
        }
        if p.get_position() < 20.0 {
            continue;
        }

        // Ciclo de anclaje cumplido. resync_to es no-op si ya estamos alineados.
        tracks_played = 0;
        if local_id == server_id {
            continue; // ya alineado — nada que hacer
        }
        if p.resync_to(&server_id, seek) {
            log::info!(
                "R-10 v3: re-anclado al server tras {} temas — crossfade a {} (desde 0)",
                ANCHOR_EVERY_N_TRACKS, server_id,
            );
            last_seen_track = server_id.clone();
            if let Some(t) = p.current_track() {
                let _ = handle.emit("now-playing", serde_json::json!({
                    "title": t.title,
                    "artist": t.artist,
                    "duration": t.duration,
                    "position": 0.0,
                    "artworkUrl": t.artwork_url,
                }));
            }
        }
    }
}

// ── SonicBox anti-repeat (R-12) ─────────────────────────────────────────────
// A voted track must NEVER loop back-to-back. It may play again, but only after
// at least SONICBOX_MIN_GAP_TRACKS other tracks have played. This guard is the
// HARD guarantee: it holds even fully offline and even if the play-report (and
// therefore the server-side markPlayed) never succeeds and the API keeps
// returning the same vote/trackId.
const SONICBOX_MIN_GAP_TRACKS: u64 = 4;

// Monotonic count of tracks that have finished playing (grid + spot + sonicbox).
static SB_TRACKS_PLAYED: AtomicU64 = AtomicU64::new(0);
// trackId -> value of SB_TRACKS_PLAYED at the moment it last played via SonicBox.
static SB_LAST_PLAYED: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();

fn sb_last_played() -> &'static Mutex<HashMap<String, u64>> {
    SB_LAST_PLAYED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Increment the global "tracks played" counter (call once per finished track).
fn sb_bump_tracks_played() -> u64 {
    SB_TRACKS_PLAYED.fetch_add(1, Ordering::Relaxed) + 1
}

/// Remember that `track_id` just played via SonicBox (at the current count).
fn sb_note_played(track_id: &str) {
    let now = SB_TRACKS_PLAYED.load(Ordering::Relaxed);
    let mut map = match sb_last_played().lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    };
    map.insert(track_id.to_string(), now);
    // Prune entries well past the gap window to keep the map tiny.
    map.retain(|_, &mut last| now.saturating_sub(last) <= SONICBOX_MIN_GAP_TRACKS * 4);
}

/// Pure decision: should we skip staging this track because it played too
/// recently via SonicBox? `last` is the recorded count, `now` the current.
fn sb_should_skip_repeat(last: Option<u64>, now: u64, gap: u64) -> bool {
    match last {
        Some(last) => now.saturating_sub(last) < gap,
        None => false,
    }
}

/// Has `track_id` played via SonicBox within the last SONICBOX_MIN_GAP_TRACKS?
fn sb_recently_played(track_id: &str) -> bool {
    let now = SB_TRACKS_PLAYED.load(Ordering::Relaxed);
    let map = match sb_last_played().lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    };
    sb_should_skip_repeat(map.get(track_id).copied(), now, SONICBOX_MIN_GAP_TRACKS)
}

// ── Spot anti-repeat (R-06) ──────────────────────────────────────────────────
// Remember the last spot we played so we don't fire the same announcement twice
// in a row when several are eligible. People hate hearing the same ad on loop.
//
// PERSISTIDO (0.9.17): antes vivía sólo en memoria. Un equipo que se reinicia (update,
// corte de luz, el local que apaga la PC a la noche) volvía con el estado en cero.
static LAST_SPOT_ID: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn last_spot_id() -> &'static Mutex<Option<String>> {
    LAST_SPOT_ID.get_or_init(|| Mutex::new(db::get_last_spot_schedule_id()))
}

// Epoch (segundos, server-time) en que sonó el último spot — para la cadencia POR MINUTOS
// (`frequency`). Se inicializa a "ahora" en el primer chequeo (baseline) para que el primer
// anuncio por tiempo espere el intervalo completo, y se actualiza cada vez que suena un spot.
//
// PERSISTIDO (0.9.17): sin esto, cada reinicio reseteaba el baseline a "ahora" y el primer
// anuncio por minutos esperaba el intervalo completo OTRA VEZ. Un equipo que se reinicia
// seguido con cadencia de 60 min podía no emitir nunca.
static LAST_SPOT_AT: OnceLock<Mutex<Option<i64>>> = OnceLock::new();

fn last_spot_at() -> &'static Mutex<Option<i64>> {
    LAST_SPOT_AT.get_or_init(|| Mutex::new(db::get_last_spot_at()))
}

/// Pick a spot id from the eligible list that isn't the one we just played.
/// Pure + testable: rotates past `last`; if the only eligible spot IS the last
/// one, plays it anyway (can't leave dead air / skip a due ad).
fn pick_spot<'a>(eligible: &'a [(String, String)], last: Option<&str>) -> Option<&'a (String, String)> {
    if eligible.is_empty() {
        return None;
    }
    eligible
        .iter()
        .find(|(id, _)| Some(id.as_str()) != last)
        .or_else(|| eligible.first())
}

// ── Audios del box: rotación ROUND_ROBIN (Fase 5) ────────────────────────────
// CONTRATO v1 — la especificación está en el backend, en el docblock de
// `millsonic/src/spots/spot-assets.util.ts`, y la implementación de referencia ejecutable en
// `spot-assets.util.spec.ts`. Android implementa LO MISMO: si algo de acá cambia, los dos
// players rotan distinto y el reporte de emisión deja de ser reproducible.

/// Único valor de `rotation` que este player entiende. Un valor FUTURO desconocido NO se
/// adivina: se cae al audio por defecto (`audioUrl`), que es el paso 7 del contrato.
const SPOT_ROTATION_ROUND_ROBIN: &str = "ROUND_ROBIN";

/// Qué se va a reproducir en este turno del box, y con qué datos se reporta.
#[derive(Debug, Clone, PartialEq)]
struct SpotPick {
    /// `SpotSchedule.id` (el `id` de la entrada de `spots[]`). Clave del cursor y `scheduleId`.
    schedule_id: String,
    /// `Spot.id` — el box. Es lo que identifica al anuncio en el reporte.
    spot_id: String,
    /// `SpotAsset.id` del audio que SUENA, que es lo que se reporta como `assetId`.
    /// `None` = camino legacy (`audioUrl`), incluido el caso de la opción A en que la
    /// rotación eligió un audio que no estaba bajado y suena el default.
    asset_id: Option<String>,
    /// `SpotAsset.id` que ELIGIÓ la rotación, independientemente de si pudo sonar.
    /// El cursor avanza a éste SIEMPRE (paso 5): si avanzara sólo cuando suena, un audio
    /// roto dejaría al box pegado en él para siempre.
    cursor_asset_id: Option<String>,
    /// Path local del audio a reproducir. `None` = no hay nada que suene → se reporta FAILED.
    file_path: Option<String>,
    /// Motivo del fallo cuando `file_path` es `None`. Literales cortos y estables para que el
    /// reporte se pueda agrupar por causa.
    failure_reason: Option<&'static str>,
    /// Falla que hay que reportar **aunque el box igual suene** (opción A): el audio que le
    /// tocaba por rotación no estaba bajado. Se reporta contra `cursor_asset_id`, no contra
    /// `asset_id`, que en ese caso es `None`.
    pending_failure: Option<&'static str>,
    /// Duración declarada por el server, para poder distinguir COMPLETED de INTERRUPTED.
    /// 0 = desconocida (el camino legacy no la trae).
    expected_duration: f32,
}

/// PASO 4 del contrato, puro y aislado: elegir el audio del turno.
///
/// `i = position(id == last_asset_id)` (`None` si no está: primera vez, cursor perdido en un
/// reinstall, o asset borrado/deshabilitado) → `elegido = A[(i + 1) % n]`. Con `n >= 2` esto
/// garantiza `elegido.id != last_asset_id`: ANTI-REPEAT.
///
/// La lista llega TAL COMO LA MANDÓ EL SERVER (paso 1): no se reordena, no se baraja y no se
/// filtra — ni siquiera los audios que no se pudieron bajar, porque sacarlos correría el
/// índice y los dos players dejarían de coincidir.
fn pick_rotation_asset<'a>(
    assets: &'a [db::SpotAssetRow],
    last_asset_id: Option<&str>,
) -> Option<&'a db::SpotAssetRow> {
    if assets.is_empty() {
        return None; // paso 2: camino legacy
    }
    if assets.len() == 1 {
        return Some(&assets[0]); // paso 3: defensivo, hoy el server no manda listas de 1
    }
    let next = match assets.iter().position(|a| Some(a.id.as_str()) == last_asset_id) {
        Some(i) => (i + 1) % assets.len(),
        None => 0,
    };
    Some(&assets[next])
}

/// Resuelve QUÉ audio le toca a este box, aplicando los pasos 1-4 y 7 del contrato.
///
/// Puro: recibe el cursor y lo devuelve en el `asset_id` del pick. El paso 5 (persistir el
/// cursor ANTES de reproducir) y el 6 (reportar) los hace el llamador, que es el que tiene
/// efectos; así todo el algoritmo se puede testear sin DB ni audio.
fn resolve_spot_audio(row: &db::SpotScheduleRow, last_asset_id: Option<&str>) -> SpotPick {
    let rotation_known = row
        .rotation
        .as_deref()
        .map(|r| r == SPOT_ROTATION_ROUND_ROBIN)
        .unwrap_or(false);

    // Paso 7: `rotation` desconocido → nunca inventar, caer al audio por defecto.
    if !row.assets.is_empty() && !rotation_known {
        log::warn!(
            "Box {} manda rotation={:?}, que este player no entiende — se usa el audio por defecto",
            row.id, row.rotation
        );
    }

    // El audio que ELIGIÓ la rotación. El cursor avanza a éste SIEMPRE, incluso cuando el
    // archivo no está en disco y termina sonando el default: si no avanzara, un audio que
    // falla siempre dejaría al box pegado en él para toda la vida (paso 5 del contrato).
    let mut cursor_asset_id = None;
    let mut pending_failure = None;

    if rotation_known {
        if let Some(asset) = pick_rotation_asset(&row.assets, last_asset_id) {
            cursor_asset_id = Some(asset.id.clone());
            let playable = !asset.file_path.is_empty()
                && std::path::Path::new(&asset.file_path).exists();
            if playable {
                return SpotPick {
                    schedule_id: row.id.clone(),
                    spot_id: row.spot_id.clone(),
                    asset_id: Some(asset.id.clone()),
                    cursor_asset_id,
                    file_path: Some(asset.file_path.clone()),
                    failure_reason: None,
                    pending_failure: None,
                    expected_duration: asset.duration,
                };
            }
            // OPCIÓN A (decisión de Pablo, 2026-10-30) — ESTE COMPORTAMIENTO ESTÁ ALINEADO CON
            // ANDROID, no lo cambies en un solo player: el audio que le tocaba por rotación no
            // está bajado (descarga fallida, cache limpiado, o todavía no sincronizó), así que
            // se REPORTA la falla con su assetId y el box **suena igual** con su audio por
            // defecto. Un anuncio al aire vale más que la exactitud del balance de la rotación;
            // la contra conocida es que el default suena de más mientras falte la descarga.
            // La alternativa que se descartó era dejar el turno MUDO.
            // El server sólo manda assets con URL firmada, así que si no está en disco no es
            // "el box no tiene audio": es que la descarga no llegó.
            pending_failure = Some(FAIL_ASSET_NOT_CACHED);
        }
    }

    // Paso 2: `assets` ausente/vacío (box de 1 audio, box TTS, box con todo apagado) →
    // camino legacy con `audioUrl`. ESTE CASO ES EL FRECUENTE, no la excepción.
    // También se llega acá por la opción A de arriba, con `pending_failure` ya cargado.
    let legacy_playable =
        !row.file_path.is_empty() && std::path::Path::new(&row.file_path).exists();
    SpotPick {
        schedule_id: row.id.clone(),
        spot_id: row.spot_id.clone(),
        // `None` a propósito aunque la rotación haya elegido uno: lo que suena es el default
        // del box, y la emisión se tiene que reportar SIN assetId para no atribuirle al audio
        // roto una emisión que nunca tuvo (Android hace exactamente esto: `rotated?.assetId`).
        asset_id: None,
        cursor_asset_id,
        file_path: if legacy_playable { Some(row.file_path.clone()) } else { None },
        failure_reason: if legacy_playable {
            None
        } else if row.audio_url.is_empty() {
            // El box no tiene audio ninguno (TTS, o todos los audios deshabilitados).
            Some(FAIL_AUDIO_MISSING)
        } else {
            Some(FAIL_DOWNLOAD)
        },
        pending_failure,
        expected_duration: 0.0,
    }
}

/// Motivos de fallo. Cortos, estables y en inglés como los del Android, para que el reporte
/// agrupe igual en los dos players.
const FAIL_AUDIO_MISSING: &str = "audio_missing";
/// Mismo literal que `SPOT_FAILURE_ASSET_NOT_CACHED` del Android, para que el reporte de
/// emisión agrupe por causa sin importar la plataforma. No lo cambies de un lado solo.
const FAIL_ASSET_NOT_CACHED: &str = "asset_audio_not_cached";
const FAIL_DOWNLOAD: &str = "download_failed";
const FAIL_DEVICE: &str = "audio_device_error";
const FAIL_CAP: &str = "playback_cap_45s";
const FAIL_SHORT: &str = "short_playback";

// ── Reporte de emisión: throttle de fallos ───────────────────────────────────
// Un box roto se evalúa en CADA fin de tema (y ahora su turno no se consume, justamente para
// que no le queme el turno a los demás), así que sin freno encolaría un FAILED cada 3 minutos
// para siempre: ruido en el certificado y cola inflada. Se reporta el fallo una vez por
// (schedule, motivo) y por ventana — alcanza de sobra para que soporte lo vea.
const SPOT_FAILURE_THROTTLE_SECS: i64 = 30 * 60;

static SPOT_FAILURE_LAST: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();

fn spot_failure_last() -> &'static Mutex<HashMap<String, i64>> {
    SPOT_FAILURE_LAST.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Decisión pura del throttle, para poder testearla sin el global.
fn failure_throttle_allows(last: Option<i64>, now: i64, window: i64) -> bool {
    match last {
        // `now < last` = el reloj se fue para atrás; dejar pasar es mejor que callarse para siempre.
        Some(t) => now < t || now - t >= window,
        None => true,
    }
}

/// Encola una emisión FALLIDA (throttleada). `now_epoch` es server-time.
///
/// Reporta contra el audio que SONABA (`pick.asset_id`). Para la falla de la opción A —el
/// audio que le tocaba no estaba bajado y suena el default— hay que reportar contra el que
/// ELIGIÓ la rotación: para eso está [`report_spot_asset_failure`].
fn report_spot_failure(pick: &SpotPick, zone_id: &str, reason: &str, now_epoch: i64) {
    report_spot_asset_failure(pick, zone_id, pick.asset_id.as_deref(), reason, now_epoch)
}

/// Igual que [`report_spot_failure`] pero con el `assetId` explícito.
fn report_spot_asset_failure(
    pick: &SpotPick,
    zone_id: &str,
    asset_id: Option<&str>,
    reason: &str,
    now_epoch: i64,
) {
    // El asset entra en la clave del throttle: un box con 5 audios y 3 sin bajar tiene que
    // poder reportar los 3, no sólo el primero que cayó en la ventana. Para el camino legacy
    // (`asset_id: None`) la clave queda igual que antes.
    let key = format!("{}|{}|{}", pick.schedule_id, asset_id.unwrap_or(""), reason);
    {
        let mut map = spot_failure_last().lock().unwrap_or_else(|e| e.into_inner());
        if !failure_throttle_allows(map.get(&key).copied(), now_epoch, SPOT_FAILURE_THROTTLE_SECS) {
            return;
        }
        map.insert(key, now_epoch);
        // El mapa es minúsculo (una entrada por box×motivo), pero se poda igual para que un
        // equipo con meses de uptime y muchos boxes no lo deje crecer.
        map.retain(|_, &mut t| now_epoch < t || now_epoch - t <= SPOT_FAILURE_THROTTLE_SECS * 48);
    }
    log::error!(
        "Anuncio NO emitido: spot={} schedule={} asset={:?} motivo={}",
        pick.spot_id, pick.schedule_id, asset_id, reason
    );
    db::save_spot_report(
        &pick.spot_id,
        &pick.schedule_id,
        asset_id,
        zone_id,
        // server_now(), NO Utc::now(): el backend DESCARTA (contando) toda emisión con
        // playedAt fuera de [now-400d, now+24h], y el reloj de una PC de local puede estar
        // corrido. R-11 ya nos da el offset del server — usarlo es la diferencia entre una
        // emisión que figura en el certificado y una que se tira.
        &server_now().to_rfc3339(),
        None, // sin duración: no sonó. NULL != 0s.
        db::SpotOutcome::Failed,
        Some(reason),
    );
}

// ── Reporte de emisión: el anuncio que está al aire ──────────────────────────
// Qué spot está sonando AHORA, para poder cerrar el reporte cuando termine. Lo setea el
// arranque del spot (los dos caminos de avance pasan por `start_spot_playback`) y lo consume
// el fin del spot, que es un único lugar (`check_track_advancement`, rama `playing_spot`).
#[derive(Debug, Clone)]
struct OnAirSpot {
    schedule_id: String,
    spot_id: String,
    asset_id: Option<String>,
    zone_id: String,
    started_at: String,
    expected_duration: f32,
}

static ON_AIR_SPOT: OnceLock<Mutex<Option<OnAirSpot>>> = OnceLock::new();

fn on_air_spot() -> &'static Mutex<Option<OnAirSpot>> {
    ON_AIR_SPOT.get_or_init(|| Mutex::new(None))
}

/// Clasificación pura del cierre de una emisión.
/// `position` = segundos realmente reproducidos; `expected` = duración declarada (0 = no se sabe).
fn classify_spot_end(position: f32, expected: f32) -> (db::SpotOutcome, Option<&'static str>) {
    if position >= audio::MAX_SPOT_PLAYBACK_SECONDS {
        // Lo cortó el cap anti-freeze: o el audio es más largo de lo que un anuncio debería,
        // o el decode se trabó. En los dos casos el cliente NO escuchó el anuncio completo.
        return (db::SpotOutcome::Interrupted, Some(FAIL_CAP));
    }
    if expected > 1.0 && position < expected * 0.8 {
        // mp3 truncado/corrupto: el sink dijo "terminé" mucho antes de la duración real.
        return (db::SpotOutcome::Interrupted, Some(FAIL_SHORT));
    }
    (db::SpotOutcome::Completed, None)
}

/// Cierra la emisión que estaba al aire (si había) y la encola.
fn close_on_air_spot(position: f32, forced_reason: Option<&'static str>) {
    let on_air = {
        let mut g = on_air_spot().lock().unwrap_or_else(|e| e.into_inner());
        g.take()
    };
    let Some(spot) = on_air else { return };
    let (outcome, reason, duration) = match forced_reason {
        // Cierre forzado (una emisión que quedó abierta por un camino raro): no se sabe cuánto
        // sonó de verdad, así que la duración va NULL en vez de inventar un número — un
        // promedio mentiroso es peor que un dato ausente.
        Some(r) => (db::SpotOutcome::Interrupted, Some(r), None),
        None => {
            let (o, r) = classify_spot_end(position, spot.expected_duration);
            (o, r, Some(position.max(0.0) as f64))
        }
    };
    log::info!(
        "Emisión cerrada: spot={} asset={:?} {:?}s outcome={}",
        spot.spot_id, spot.asset_id, duration, outcome.as_str()
    );
    db::save_spot_report(
        &spot.spot_id,
        &spot.schedule_id,
        spot.asset_id.as_deref(),
        &spot.zone_id,
        &spot.started_at,
        duration,
        outcome,
        reason,
    );
}

/// Arranca el anuncio elegido. ÚNICO punto de reproducción de spots del player: los DOS
/// caminos de avance (crossfade y finish) llaman acá, así que la rotación, la persistencia
/// del cursor, la cadencia y el reporte no pueden quedar sólo en uno de los dos — que es
/// exactamente el bug que tuvo este repo cuando el crossfade no disparaba spots
/// (player_two_advance_paths).
///
/// Devuelve `true` sólo si el audio realmente arrancó.
fn start_spot_playback(
    player: &mut audio::AudioPlayer,
    pick: &SpotPick,
    zone_id: &str,
    now_epoch: i64,
) -> bool {
    // OPCIÓN A: el audio que le tocaba por rotación no estaba bajado. La falla se reporta
    // igual (contra el asset que eligió la rotación) y abajo sigue sonando el default. Va
    // ANTES del guard de `file_path` a propósito: si el default TAMPOCO está, se reportan las
    // dos cosas —el audio que faltó y que el box quedó mudo—, que es lo que hace Android.
    if let Some(reason) = pick.pending_failure {
        report_spot_asset_failure(
            pick,
            zone_id,
            pick.cursor_asset_id.as_deref(),
            reason,
            now_epoch,
        );
    }

    let Some(path) = pick.file_path.as_deref() else {
        report_spot_failure(
            pick,
            zone_id,
            pick.failure_reason.unwrap_or(FAIL_AUDIO_MISSING),
            now_epoch,
        );
        return false;
    };

    // Si por cualquier razón quedó una emisión abierta, se cierra como INTERRUPTED antes de
    // abrir la nueva: así una emisión nunca se le atribuye al anuncio siguiente.
    // (duración 0 porque acá `get_position()` es la del TEMA, no la del spot viejo; el cierre
    // forzado manda la duración en NULL de todas formas.)
    close_on_air_spot(0.0, Some("superseded"));

    match player.play_spot_file(path) {
        Ok(_) => {
            {
                let mut g = on_air_spot().lock().unwrap_or_else(|e| e.into_inner());
                *g = Some(OnAirSpot {
                    schedule_id: pick.schedule_id.clone(),
                    spot_id: pick.spot_id.clone(),
                    asset_id: pick.asset_id.clone(),
                    zone_id: zone_id.to_string(),
                    // Idem: server-time, para que el `@@unique (deviceId, spotId, playedAt)`
                    // y la ventana de cordura del backend trabajen sobre el reloj correcto.
                    started_at: server_now().to_rfc3339(),
                    expected_duration: pick.expected_duration,
                });
            }
            // ⚠️ ACÁ, y no antes: la cadencia por minutos se consume cuando el anuncio
            // EMPEZÓ A SONAR DE VERDAD. Antes se consumía al elegirlo, así que un box roto
            // reseteaba el reloj compartido de la zona y les quemaba el turno a los demás
            // (ANUNCIOS-REPORTE-Y-BOXES.md). Se persiste para sobrevivir el reinicio.
            if let Ok(mut g) = last_spot_at().lock() {
                *g = Some(now_epoch);
            }
            db::set_last_spot_at(now_epoch);
            true
        }
        Err(e) => {
            // El cap de 45s y `playing_spot` quedan sanos (ver `play_spot_file`), así que la
            // música sigue por el fall-through del llamador.
            report_spot_failure(
                pick,
                zone_id,
                &format!("{}: {}", FAIL_DEVICE, e),
                now_epoch,
            );
            false
        }
    }
}

/// Connection status: Online, Offline (cached), Emergency
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConnectionStatus {
    Online,
    Offline,
    Emergency,
}

static CONNECTION_STATUS: OnceLock<Mutex<ConnectionStatus>> = OnceLock::new();

fn conn_status() -> &'static Mutex<ConnectionStatus> {
    CONNECTION_STATUS.get_or_init(|| Mutex::new(ConnectionStatus::Online))
}

pub fn get_connection_status() -> ConnectionStatus {
    *conn_status().lock().unwrap_or_else(|e| e.into_inner())
}

pub fn set_connection_status(status: ConnectionStatus, handle: &AppHandle) {
    let mut s = conn_status().lock().unwrap_or_else(|e| e.into_inner());
    if *s != status {
        log::info!("Connection status: {:?} → {:?}", *s, status);
        *s = status;
    }
    let status_str = match status {
        ConnectionStatus::Online => "online",
        ConnectionStatus::Offline => "offline",
        ConnectionStatus::Emergency => "emergency",
    };
    let _ = handle.emit("connection-status", serde_json::json!({ "status": status_str }));
}

/// Hash a string to a positive integer (same as web player's hashCode)
fn hash_code(s: &str) -> u32 {
    let mut hash: i32 = 0;
    for c in s.chars() {
        hash = ((hash << 5).wrapping_sub(hash)).wrapping_add(c as i32);
        hash |= 0;
    }
    hash.unsigned_abs()
}

/// Deterministic seeded shuffle matching the web player's seededShuffle
fn seeded_shuffle<T: Clone>(arr: &[T], seed: &str) -> Vec<T> {
    let mut result: Vec<T> = arr.to_vec();
    let mut s = hash_code(seed) as u64;
    for i in (1..result.len()).rev() {
        s = (s.wrapping_mul(1664525).wrapping_add(1013904223)) & 0x7fffffff;
        let j = (s % (i as u64 + 1)) as usize;
        result.swap(i, j);
    }
    result
}

/// Interleave ponderado por bloque — port BIT-EXACTO de `weighted-interleave.ts` del
/// backend (modo bloques: ROCK×3, POP×2 → 3 rock, 2 pop, repite, con wrap-around).
/// Recibe listas YA barajadas con su peso. Determinístico: generar N da el mismo prefijo.
fn weighted_interleave<T: Clone>(lists: &[(Vec<T>, usize)], count: usize) -> Vec<T> {
    let active: Vec<&(Vec<T>, usize)> = lists
        .iter()
        .filter(|(items, w)| *w > 0 && !items.is_empty())
        .collect();
    if active.is_empty() || count == 0 {
        return Vec::new();
    }
    if active.len() == 1 {
        let (items, _) = active[0];
        return (0..count).map(|i| items[i % items.len()].clone()).collect();
    }
    let mut cursors = vec![0usize; active.len()];
    let mut out: Vec<T> = Vec::with_capacity(count);
    while out.len() < count {
        for (pi, (items, w)) in active.iter().enumerate() {
            let mut k = 0;
            while k < *w && out.len() < count {
                out.push(items[(cursors[pi] + k) % items.len()].clone());
                k += 1;
            }
            cursors[pi] += *w;
            if out.len() >= count {
                break;
            }
        }
    }
    out
}

static SYNC_TRIGGER: OnceLock<Notify> = OnceLock::new();

fn sync_trigger() -> &'static Notify {
    SYNC_TRIGGER.get_or_init(|| Notify::new())
}

// ── Health telemetry: last sync ──────────────────────────────────────────────
// Records the outcome of the most recent sync attempt so the device "Health"
// view can show when/how/whether content last synced. Tuple:
// (at ISO8601, status "success"|"error", trigger "manual"|"auto").
static LAST_SYNC: OnceLock<Mutex<Option<(String, String, String)>>> = OnceLock::new();

fn last_sync_cell() -> &'static Mutex<Option<(String, String, String)>> {
    LAST_SYNC.get_or_init(|| Mutex::new(None))
}

/// Outcome of the most recent sync, if any has run this session.
pub fn last_sync() -> Option<(String, String, String)> {
    last_sync_cell().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn record_sync(status: &str, trigger: &str) {
    let at = Utc::now().to_rfc3339();
    let mut g = last_sync_cell().lock().unwrap_or_else(|e| e.into_inner());
    *g = Some((at, status.to_string(), trigger.to_string()));
}

// Trigger that woke the NEXT sync. A manual/forceSync request sets this to
// "manual"; the periodic 300s wake leaves it "auto". Read+reset once per sync.
static NEXT_SYNC_TRIGGER: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn next_sync_trigger() -> &'static Mutex<Option<String>> {
    NEXT_SYNC_TRIGGER.get_or_init(|| Mutex::new(None))
}

/// Read and clear the reason that triggered the upcoming sync (defaults "auto").
fn take_sync_trigger() -> String {
    let mut g = next_sync_trigger().lock().unwrap_or_else(|e| e.into_inner());
    g.take().unwrap_or_else(|| "auto".to_string())
}

/// Content manifest version = fingerprint of the currently-loaded track list.
/// Closest thing the player has to a "manifest version" of its content.
pub fn current_manifest_version() -> Option<String> {
    use std::hash::{Hash, Hasher};
    current_tracks_fp()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .filter(|s| !s.is_empty())
        // The raw fingerprint is the full list of track ids (can be >1KB). Report
        // a compact, stable 16-hex hash as the manifest "version" instead.
        .map(|fp| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            fp.hash(&mut h);
            format!("{:016x}", h.finish())
        })
}

/// Call this to trigger an immediate sync (e.g. after pairing)
pub fn trigger_sync() {
    log::info!("Sync triggered manually");
    *next_sync_trigger().lock().unwrap_or_else(|e| e.into_inner()) = Some("manual".to_string());
    sync_trigger().notify_one();
}

/// R-09: called right before an update restart so the upcoming hard restart
/// doesn't lose queued play reports or cut audio mid-buffer. Flushes pending
/// reports and stops playback cleanly.
pub async fn flush_reports_before_exit() {
    let cfg = config::AppConfig::load();
    if let (Some(id), Some(token)) = (cfg.device_id.clone(), cfg.device_token.clone()) {
        flush_pending_reports(&id, &token).await;
    }
    if let Ok(mut p) = audio::player().lock() {
        p.stop();
    }
    // Health: this is a KNOWN clean exit (graceful restart for updates etc.) —
    // remove the run marker so the next boot doesn't flag it as a crash.
    crate::telemetry::clear_running_marker();
}

/// R-04: remove any leftover partial download files (`*.part`) from the cache
/// on startup, so a download interrupted by a crash/power-loss can't linger.
pub fn sweep_partial_downloads() {
    let dir = config::AppConfig::cache_dir();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("part") {
                let _ = std::fs::remove_file(&p);
                log::info!("Swept partial download: {}", p.display());
            }
        }
    }
}

/// Track the current playlist ID to avoid restarting playback on re-sync
static CURRENT_PLAYLIST_ID: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn current_playlist_id() -> &'static Mutex<Option<String>> {
    CURRENT_PLAYLIST_ID.get_or_init(|| Mutex::new(None))
}

// U6: fingerprint of the currently-loaded track list (ordered track ids). Lets
// us detect when the SONGS of a playlist changed even though the playlist_id is
// the same (admin edited its contents), so we reload instead of ignoring it.
static CURRENT_TRACKS_FP: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn current_tracks_fp() -> &'static Mutex<Option<String>> {
    CURRENT_TRACKS_FP.get_or_init(|| Mutex::new(None))
}

fn tracks_fingerprint(tracks: &[serde_json::Value]) -> String {
    let ids: Vec<&str> = tracks
        .iter()
        .map(|t| t.get("id").and_then(|v| v.as_str()).unwrap_or(""))
        .collect();
    ids.join(",")
}

pub async fn start_sync_loop(handle: AppHandle) {
    log::info!("Sync loop started");

    // R-04: clean any partial downloads left by a previous crash/power-loss.
    sweep_partial_downloads();

    // Init DB on first run
    let _ = db::db();

    loop {
        let cfg = config::AppConfig::load();
        if cfg.is_paired() {
            let device_id = cfg.device_id.clone().unwrap();
            let device_token = cfg.device_token.clone().unwrap();

            // Health telemetry: capture WHY this sync ran (manual/forceSync vs
            // the periodic auto wake) and its outcome.
            let trigger = take_sync_trigger();
            match do_sync(&handle, &device_id, &device_token).await {
                Ok(_) => {
                    log::info!("Sync completed successfully");
                    record_sync("success", &trigger);
                }
                Err(e) => {
                    log::error!("Sync error: {}", e);
                    record_sync("error", &trigger);
                    // Try offline fallback
                    handle_offline_fallback(&handle, &cfg);
                }
            }

            // After sync, try to flush pending play reports
            flush_pending_reports(&device_id, &device_token).await;

            // R-18: keep the device token from expiring at the 365-day mark.
            maybe_refresh_token().await;

            // Run LRU cache cleanup
            db::cleanup_cache();
            db::cleanup_sent_reports();
        } else {
            log::info!("Not paired, waiting...");
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }

        // Wait for either 300s or a manual trigger
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(300)) => {},
            _ = sync_trigger().notified() => {
                log::info!("Sync woken by trigger");
            },
        }
    }
}

/// Start batch report flusher (every 5 minutes)
pub async fn start_report_flusher() {
    loop {
        tokio::time::sleep(Duration::from_secs(300)).await;
        let cfg = config::AppConfig::load();
        if cfg.is_paired() {
            let device_id = cfg.device_id.clone().unwrap();
            let device_token = cfg.device_token.clone().unwrap();
            flush_pending_reports(&device_id, &device_token).await;
        }
    }
}

/// Guard in-flight del flusher. Se dispara desde tres lados (la task dedicada cada 300s,
/// el final de cada sync y el flush previo a un restart) y todos leen las MISMAS filas
/// `sent=0`: sin guard dos flushes solapados postean el mismo lote y el backend lo inserta
/// dos veces — `play_history` no tiene unique, así que el cliente ve los plays duplicados.
/// Con `try_lock` el segundo se va sin hacer nada; lo que quedó en la cola lo levanta el
/// flush siguiente (está en SQLite, no se pierde).
static FLUSH_IN_FLIGHT: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

fn flush_in_flight() -> &'static tokio::sync::Mutex<()> {
    FLUSH_IN_FLIGHT.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Tope de lotes por flush. Después de un finde offline hay miles de reportes encolados y
/// de a 100 cada 300s el equipo tardaba ~1h en drenar (y los más viejos se vencían antes
/// de salir). Loopeamos, pero acotado: esto corre dentro de la task de sync y no puede
/// quedarse una hora posteando.
const MAX_FLUSH_ROUNDS: usize = 20;

async fn flush_pending_reports(device_id: &str, device_token: &str) {
    let _guard = match flush_in_flight().try_lock() {
        Ok(g) => g,
        Err(_) => {
            log::debug!("Flush de play reports ya en curso — se saltea esta corrida");
            return;
        }
    };

    for round in 1..=MAX_FLUSH_ROUNDS {
        let reports = db::get_pending_reports();
        if reports.is_empty() { return; }
        // Lote lleno = casi seguro queda cola atrás; pedimos otro en la vuelta siguiente.
        let full_batch = reports.len() >= db::PENDING_REPORTS_BATCH;

        log::info!("Flushing {} pending play reports (vuelta {})", reports.len(), round);
        let ids: Vec<i64> = reports.iter().map(|r| r.id).collect();
        // El serializado vive en db.rs y es puro: es el contrato con el backend (una fila mal
        // formada = 400 = batch ENTERO perdido) y tiene que poder testearse sin red.
        // Una fila de música serializa exactamente igual que en 0.9.16.
        let report_data: Vec<serde_json::Value> =
            reports.iter().map(db::pending_report_json).collect();

        match api::report_plays_batch(device_id, device_token, report_data).await {
            Ok(_) => {
                db::mark_reports_sent(&ids);
                log::info!("Flushed {} play reports successfully", ids.len());
            }
            Err(e) => {
                // Sin marcar nada como enviado: las filas quedan en `sent=0` y se
                // reintentan solas en el próximo ciclo.
                log::warn!("Failed to flush play reports (will retry): {}", e);
                return;
            }
        }

        if !full_batch { return; }
    }
    log::warn!("Flush cortado por el tope de {} vueltas — sigue en el próximo ciclo", MAX_FLUSH_ROUNDS);
}

fn handle_offline_fallback(handle: &AppHandle, cfg: &config::AppConfig) {
    let zone_id = cfg.zone_id.as_deref().unwrap_or("");

    // Check if we already have a playlist playing
    if let Ok(player) = audio::player().lock() {
        if player.is_playing() && player.playlist_len() > 0 {
            // Already playing, just mark offline
            set_connection_status(ConnectionStatus::Offline, handle);
            return;
        }
    }

    // Try to load cached schedule
    let tz_str = cfg.timezone.as_deref().unwrap_or("America/Montevideo");
    let tz: chrono_tz::Tz = tz_str.parse().unwrap_or(chrono_tz::America::Montevideo);
    // R-11: server-corrected time so day/slot match the server even if this clock is off.
    let now = server_now().with_timezone(&tz);
    // Source of truth: backend uses JS getDay() = 0=Sun..6=Sat.
    // chrono num_days_from_sunday() gives 0=Sun..6=Sat — matches.
    let day_of_week = now.weekday().num_days_from_sunday();

    let cached_slots = db::load_schedule(zone_id, day_of_week);
    let current_time = now.format("%H:%M").to_string();

    // Find matching slot
    let mut found_tracks = false;
    for slot in &cached_slots {
        let start = slot.get("startTime").and_then(|s| s.as_str()).unwrap_or("00:00");
        let end = slot.get("endTime").and_then(|s| s.as_str()).unwrap_or("23:59");
        if current_time.as_str() >= start && current_time.as_str() < end {
            if let Some(tracks) = slot.get("playlist").and_then(|p| p.get("tracks")).and_then(|t| t.as_array()) {
                if !tracks.is_empty() {
                    log::info!("Offline: using cached schedule with {} tracks", tracks.len());
                    set_connection_status(ConnectionStatus::Offline, handle);
                    // Load cached tracks into player
                    load_cached_tracks_into_player(tracks, zone_id, &now);
                    found_tracks = true;
                    break;
                }
            }
        }
    }

    if !found_tracks {
        // Emergency mode: shuffle all cached tracks
        enter_emergency_mode(handle);
    }
}

fn load_cached_tracks_into_player(tracks: &[serde_json::Value], zone_id: &str, now: &chrono::DateTime<chrono_tz::Tz>) {
    let cache_dir = config::AppConfig::cache_dir();
    let mut playlist: Vec<audio::TrackInfo> = Vec::new();

    for track_val in tracks {
        let track_id = track_val.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
        let file_path = cache_dir.join(format!("{}.mp3", track_id));
        if file_path.exists() {
            playlist.push(audio::TrackInfo {
                track_id: track_id.to_string(),
                title: track_val.get("title").and_then(|v| v.as_str()).unwrap_or("Unknown").to_string(),
                artist: track_val.get("artist").and_then(|v| v.as_str()).unwrap_or("Unknown").to_string(),
                file_path: file_path.to_string_lossy().to_string(),
                duration: track_val.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32,
                artwork_url: track_val.get("artworkUrl").and_then(|v| v.as_str()).map(|s| s.to_string()),
            });
        }
    }

    if !playlist.is_empty() {
        // Apply seeded shuffle
        let date_str = now.format("%Y-%m-%d").to_string();
        let seed = format!("{}-{}-00:00", zone_id, date_str);
        playlist = seeded_shuffle(&playlist, &seed);

        if let Ok(mut player) = audio::player().lock() {
            player.set_playlist(playlist);
            let _ = player.play_current();
        }
    }
}

fn enter_emergency_mode(handle: &AppHandle) {
    log::warn!("Entering EMERGENCY mode — shuffling all cached tracks");
    set_connection_status(ConnectionStatus::Emergency, handle);

    let cached = db::get_all_cached_tracks();
    let mut playlist: Vec<audio::TrackInfo> = Vec::new();

    for (id, title, artist, artwork_url, duration, file_path) in &cached {
        if std::path::Path::new(file_path).exists() {
            playlist.push(audio::TrackInfo {
                track_id: id.clone(),
                title: title.clone(),
                artist: artist.clone(),
                file_path: file_path.clone(),
                duration: *duration,
                artwork_url: artwork_url.clone(),
            });
        }
    }

    if playlist.is_empty() {
        // R-01: nothing cached at all (e.g. paired but never synced, then went
        // offline). We can't conjure audio, but we must NOT give up silently:
        // keep nudging a sync so the instant any content arrives, music starts.
        log::error!("Emergency mode: NO cached tracks available — forcing sync to recover ASAP");
        trigger_sync();
        return;
    }

    // Random shuffle for emergency
    use rand::seq::SliceRandom;
    let mut rng = rand::thread_rng();
    playlist.shuffle(&mut rng);

    log::info!("Emergency mode: playing {} cached tracks on shuffle", playlist.len());
    if let Ok(mut player) = audio::player().lock() {
        player.set_playlist(playlist);
        let _ = player.play_current();
    }
}

async fn do_sync(
    handle: &AppHandle,
    device_id: &str,
    device_token: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let sync_data = api::sync_device(device_id, device_token).await?;

    // Check for API error
    if sync_data.get("statusCode").is_some() {
        let msg = sync_data.get("message").and_then(|m| m.as_str()).unwrap_or("Unknown error");
        return Err(format!("Sync API error: {}", msg).into());
    }

    // We're online!
    set_connection_status(ConnectionStatus::Online, handle);

    // Apply volume from server if present
    if let Some(device) = sync_data.get("device") {
        if let Some(vol) = device.get("volume").and_then(|v| v.as_u64()) {
            let _ = audio::set_volume(vol as u8);
        }
    }

    // Extract zone name and location name from sync response
    let zone_name = sync_data.get("zone")
        .and_then(|z| z.get("name"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let location_name = sync_data.get("zone")
        .and_then(|z| z.get("location"))
        .and_then(|l| l.get("name"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| sync_data.get("locationName").and_then(|v| v.as_str()).map(|s| s.to_string()));

    if zone_name.is_some() || location_name.is_some() {
        config::update_and_save_global(|c| {
            if let Some(ref zn) = zone_name { c.zone_name = Some(zn.clone()); }
            if let Some(ref ln) = location_name { c.location_name = Some(ln.clone()); }
        });
        // Emit event so frontend updates immediately
        let _ = handle.emit("zone-info", serde_json::json!({
            "zoneName": zone_name,
            "locationName": location_name,
        }));
    }

    // Get timezone from sync response
    let tz_str = sync_data.get("timezone").and_then(|v| v.as_str()).unwrap_or("America/Montevideo");
    let tz: chrono_tz::Tz = tz_str.parse().unwrap_or(chrono_tz::America::Montevideo);
    // R-11: server-corrected time so the shuffle seed (date) and schedule
    // position match the server / other branches even if this clock is off.
    let now = server_now().with_timezone(&tz);

    // Save timezone in config for offline use
    let tz_owned = tz_str.to_string();

    // Parse crossfade config from zone
    let crossfade_enabled = sync_data.get("zone")
        .and_then(|z| z.get("crossfadeEnabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let crossfade_duration = sync_data.get("zone")
        .and_then(|z| z.get("crossfadeDuration"))
        .and_then(|v| v.as_u64())
        .unwrap_or(3) as u32;

    config::update_and_save_global(|c| {
        c.timezone = Some(tz_owned.clone());
        c.crossfade_enabled = crossfade_enabled;
        c.crossfade_duration = crossfade_duration;
    });

    // Apply crossfade duration to audio player
    if let Ok(mut player) = audio::player().lock() {
        player.set_crossfade_duration(crossfade_duration as f32);
    }

    // Parse and download spots
    let spots = sync_data.get("spots").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if !spots.is_empty() {
        log::info!("Sync contains {} spots", spots.len());
        download_and_save_spots(&spots).await;
    } else {
        // Clear spots if none in sync
        db::save_spot_schedules(&[]);
        // Also clean spot cache files. `remove_dir_all` para el subdirectorio `assets/`:
        // `remove_file` sobre un directorio falla en silencio y los audios de box viejos
        // quedarían en disco para siempre.
        let spots_cache = config::AppConfig::data_dir().join("cache").join("spots");
        if let Ok(entries) = std::fs::read_dir(&spots_cache) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    let _ = std::fs::remove_dir_all(&p);
                } else {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        log::info!("Cleared spot cache (no spots in sync)");
    }

    // Parse schedule and save to SQLite
    let schedule = sync_data.get("schedule").cloned().unwrap_or(serde_json::json!([]));
    let slots = schedule.as_array().cloned().unwrap_or_default();

    let zone_id = config::AppConfig::load().zone_id.clone().unwrap_or_default();
    db::save_schedule(&zone_id, &slots);

    // Source of truth: backend uses JS getDay() = 0=Sun..6=Sat.
    // chrono num_days_from_sunday() gives 0=Sun..6=Sat — matches.
    let day_of_week = now.weekday().num_days_from_sunday();
    let current_time = now.format("%H:%M").to_string();

    log::info!(
        "Looking for schedule: dayOfWeek={} time={} tz={} clock_offset_ms={}",
        day_of_week, current_time, tz, clock_offset_ms()
    );

    let mut current_tracks: Vec<serde_json::Value> = Vec::new();
    let mut playlist_id: Option<String> = None;
    let mut slot_start_time: Option<String> = None;
    // multi-playlist ponderado: current_tracks ya viene barajado+intercalado → no re-shufflear.
    let mut already_sequenced = false;

    for slot in &slots {
        let slot_day = slot.get("dayOfWeek").and_then(|d| d.as_u64()).unwrap_or(99) as u32;
        let start = slot.get("startTime").and_then(|s| s.as_str()).unwrap_or("00:00");
        let end = slot.get("endTime").and_then(|s| s.as_str()).unwrap_or("23:59");

        if slot_day == day_of_week && current_time.as_str() >= start && current_time.as_str() < end {
            log::info!("Matched slot: day={} {}-{}", slot_day, start, end);
            slot_start_time = Some(start.to_string());
            // MULTI-playlist ponderado: si el slot trae `playlists` (>1), intercalar
            // bit-exacto con el backend (cada playlist barajada con su seed + interleave).
            let multi = slot.get("playlists").and_then(|p| p.as_array()).filter(|a| a.len() > 1).cloned();
            if let Some(pls) = multi {
                let date_str = now.format("%Y-%m-%d").to_string();
                let lists: Vec<(Vec<serde_json::Value>, usize)> = pls
                    .iter()
                    .map(|pl| {
                        let pid = pl.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let weight = pl.get("weight").and_then(|w| w.as_u64()).unwrap_or(1) as usize;
                        let tracks = pl.get("tracks").and_then(|t| t.as_array()).cloned().unwrap_or_default();
                        let seed = format!("{}-{}-{}-{}", zone_id, date_str, start, pid);
                        (seeded_shuffle(&tracks, &seed), weight)
                    })
                    .collect();
                current_tracks = weighted_interleave(&lists, 3500);
                playlist_id = pls.first().and_then(|p| p.get("id")).and_then(|v| v.as_str()).map(|s| s.to_string());
                already_sequenced = true;
                log::info!("Matched MULTI slot: {} playlists → interleaved {} tracks", pls.len(), current_tracks.len());
            } else if let Some(playlist) = slot.get("playlist") {
                playlist_id = playlist.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
                if let Some(tracks) = playlist.get("tracks").and_then(|t| t.as_array()) {
                    current_tracks = tracks.clone();
                    log::info!("Found {} tracks in playlist '{}'",
                        tracks.len(),
                        playlist.get("name").and_then(|n| n.as_str()).unwrap_or("?"));
                }
            }
            break;
        }
    }

    // Apply seeded shuffle to match web player order (single-playlist).
    // Multi-playlist ya viene barajado+intercalado bit-exacto → NO re-shufflear.
    if !already_sequenced && !current_tracks.is_empty() {
        let date_str = now.format("%Y-%m-%d").to_string();
        let start_t = slot_start_time.as_deref().unwrap_or("00:00");
        let seed = format!("{}-{}-{}", zone_id, date_str, start_t);
        log::info!("Shuffling {} tracks with seed: {}", current_tracks.len(), seed);
        current_tracks = seeded_shuffle(&current_tracks, &seed);
    }

    if current_tracks.is_empty() {
        log::info!("No tracks scheduled for current time slot");
        let _ = handle.emit("now-playing", serde_json::json!({
            "title": "Sin programación",
            "artist": "No hay música programada en este horario",
            "duration": 0,
            "position": 0,
            "artworkUrl": null,
        }));
        if let Ok(mut player) = audio::player().lock() {
            if player.is_playing() {
                player.stop();
            }
        }
        *current_playlist_id().lock().unwrap() = None;
        return Ok(());
    }

    // Check if the playlist changed. U6: "same" requires BOTH the same
    // playlist_id AND the same track list — if the songs were edited (added/
    // removed/reordered) under the same id, we must reload, not just re-cache.
    let new_fp = tracks_fingerprint(&current_tracks);
    let same_playlist = {
        let cur_id = current_playlist_id().lock().unwrap();
        let cur_fp = current_tracks_fp().lock().unwrap();
        cur_id.as_deref() == playlist_id.as_deref()
            && cur_fp.as_deref() == Some(new_fp.as_str())
    };

    if same_playlist {
        log::info!("Same playlist + same tracks, refreshing cache only");
        refresh_track_cache(&current_tracks).await;
        return Ok(());
    }
    if current_playlist_id().lock().unwrap().as_deref() == playlist_id.as_deref() {
        log::info!("Same playlist id but TRACK LIST CHANGED — reloading playlist (U6)");
    }

    // Different playlist (first sync / new slot / U6 content change) — (re)build
    // the playlist and start playing. U1: download only the start track first,
    // play it, then download the rest in the background.
    let cache_dir = config::AppConfig::cache_dir();
    std::fs::create_dir_all(&cache_dir)?;

    let total_tracks = current_tracks.len();
    let mut first_track_started = false;

    // Build the full playlist metadata up front. file_path is the predetermined
    // cache location even if the file isn't downloaded yet (progressive).
    let mut playlist: Vec<audio::TrackInfo> = Vec::new();
    let mut dl_info: Vec<(String, String, std::path::PathBuf)> = Vec::new(); // (id, stream_url, path)
    for track_val in current_tracks.iter() {
        let track_id = track_val.get("id").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
        let title = track_val.get("title").and_then(|v| v.as_str()).unwrap_or("Unknown").to_string();
        let artist = track_val.get("artist").and_then(|v| v.as_str()).unwrap_or("Unknown").to_string();
        let duration = track_val.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
        let artwork_url = track_val.get("artworkUrl").and_then(|v| v.as_str()).map(|s| s.to_string());
        let stream_url = track_val.get("streamUrl").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let file_path = cache_dir.join(format!("{}.mp3", track_id));
        db::upsert_track(&track_id, &title, &artist, artwork_url.as_deref(), duration, &file_path.to_string_lossy());
        dl_info.push((track_id.clone(), stream_url, file_path.clone()));
        playlist.push(audio::TrackInfo {
            track_id,
            title,
            artist,
            file_path: file_path.to_string_lossy().to_string(),
            duration,
            artwork_url,
        });
    }
    if playlist.is_empty() {
        return Ok(());
    }

    // ── U5: decide WHERE to start — which track AND which second. Authoritative
    // source = server now-playing (currentTrack.id + seekPosition), exactly like
    // the web player. Offline / on failure, fall back to a local time-based calc.
    let mut start_index = 0usize;
    let mut seek_secs = 0.0f32;
    let mut from_server = false;
    if let Ok(np) = api::fetch_zone_now_playing(&zone_id).await {
        // R-11: sync our clock to the server.
        if let Some(ts) = np.get("syncTimestamp").and_then(|v| v.as_i64()) {
            update_clock_offset(ts);
        }
        if let Some(ct) = np.get("currentTrack") {
            let is_spot = ct.get("isSpot").and_then(|v| v.as_bool()).unwrap_or(false);
            let cur_id = ct.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if !is_spot && !cur_id.is_empty() {
                if let Some(idx) = playlist.iter().position(|t| t.track_id == cur_id) {
                    start_index = idx;
                    seek_secs = ct.get("seekPosition").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                    from_server = true;
                    log::info!("Start from server now-playing: track {} of {}, seek {:.0}s (U5)",
                        idx, playlist.len(), seek_secs);
                }
            }
        }
    }
    if !from_server {
        let start_t = slot_start_time.as_deref().unwrap_or("00:00");
        let parts: Vec<&str> = start_t.split(':').collect();
        let start_h: u32 = parts.get(0).and_then(|s| s.parse().ok()).unwrap_or(0);
        let start_m: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let slot_start_secs = (start_h * 3600 + start_m * 60) as f64;
        let now_secs = (now.hour() * 3600 + now.minute() * 60 + now.second()) as f64;
        let elapsed_secs = if now_secs >= slot_start_secs { now_secs - slot_start_secs } else { 0.0 };
        let total_duration: f64 = playlist.iter().map(|t| t.duration as f64).sum();
        if total_duration > 0.0 {
            let looped = elapsed_secs % total_duration;
            let mut acc = 0.0f64;
            for (i, t) in playlist.iter().enumerate() {
                let next = acc + t.duration as f64;
                if next > looped {
                    start_index = i;
                    seek_secs = (looped - acc) as f32;
                    break;
                }
                acc = next;
            }
        }
        log::info!("Start from local fallback: track {} of {}, seek {:.0}s", start_index, playlist.len(), seek_secs);
    }

    // ── U1: download ONLY the start track, begin playing, then background the
    // rest. If the start track can't be cached, fall back to the first cached
    // one so we NEVER block into silence.
    {
        let start = &dl_info[start_index];
        if !start.2.exists() && !start.1.is_empty() {
            log::info!("Downloading start track first (progressive): {}", playlist[start_index].title);
            if let Err(e) = api::download_track(&start.1, &start.2).await {
                log::error!("Start track download failed: {}", e);
            }
        }
    }
    if !dl_info[start_index].2.exists() {
        if let Some(idx) = dl_info.iter().position(|d| d.2.exists()) {
            log::warn!("Start track not cached — falling back to first cached track");
            start_index = idx;
            seek_secs = 0.0;
        }
    }

    {
        let mut player = audio::player().lock().map_err(|e| e.to_string())?;
        player.set_playlist(playlist);
        player.current_index = start_index;
        if let Err(e) = player.play_current_at(seek_secs) {
            log::error!("Failed to start playback: {}", e);
        } else {
            first_track_started = true;
        }
    }
    *current_playlist_id().lock().unwrap() = playlist_id;
    *current_tracks_fp().lock().unwrap() = Some(new_fp.clone());
    let _ = handle.emit("status-change", serde_json::json!({"playing": true}));

    // Background-download the remaining tracks in playhead order (next, then the
    // one after, wrapping around) so each is ready before it's needed (U1).
    {
        let n = dl_info.len();
        let order: Vec<(String, String, std::path::PathBuf)> =
            (1..n).map(|k| dl_info[(start_index + k) % n].clone()).collect();
        tauri::async_runtime::spawn(async move {
            for (id, url, path) in order {
                if path.exists() || url.is_empty() {
                    continue;
                }
                if let Err(e) = api::download_track(&url, &path).await {
                    log::error!("Background download failed for {}: {}", id, e);
                }
            }
            log::info!("Background download of remaining tracks complete");
        });
    }

    if first_track_started {
        let _ = handle.emit("sync-progress", serde_json::json!({
            "phase": "ready",
            "current": total_tracks,
            "total": total_tracks,
            "trackName": "",
            "percent": 100
        }));
    }

    Ok(())
}

async fn refresh_track_cache(tracks: &[serde_json::Value]) {
    let cache_dir = config::AppConfig::cache_dir();
    let _ = std::fs::create_dir_all(&cache_dir);

    for track_val in tracks {
        let track_id = track_val.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
        let stream_url = track_val.get("streamUrl").and_then(|v| v.as_str()).unwrap_or("");
        let file_path = cache_dir.join(format!("{}.mp3", track_id));

        if !file_path.exists() && !stream_url.is_empty() {
            let title = track_val.get("title").and_then(|v| v.as_str()).unwrap_or("?");
            log::info!("Downloading missing track: {}", title);
            if let Err(e) = api::download_track(stream_url, &file_path).await {
                log::error!("Failed to download {}: {}", title, e);
            }
        }
    }
}

async fn download_and_save_spots(spots: &[serde_json::Value]) {
    let cache_dir = config::AppConfig::data_dir().join("cache").join("spots");
    let _ = std::fs::create_dir_all(&cache_dir);
    // Un subdirectorio propio para los audios del box: la ruta vieja era
    // `cache/spots/{scheduleId}.mp3`, o sea UNA sola por box — con varios audios se pisaban
    // entre ellos. Ahora la key del cache es el `SpotAsset.id`, que es único por audio.
    let assets_dir = cache_dir.join("assets");
    let _ = std::fs::create_dir_all(&assets_dir);

    let mut spots_with_paths: Vec<serde_json::Value> = Vec::new();

    for spot in spots {
        let id = spot.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
        let audio_url = spot.get("audioUrl").and_then(|v| v.as_str()).unwrap_or("");
        let name = spot.get("name").and_then(|v| v.as_str()).unwrap_or("?");

        let file_path = cache_dir.join(format!("{}.mp3", id));

        let mut s = spot.clone();

        // ── Audios del box (Fase 5) ──
        // Se recorre `assets` EN EL ORDEN EN QUE VINO y se conserva la entrada incluso si la
        // descarga falla (con `filePath: ""`). Filtrar los que no bajaron correría el índice
        // de la rotación y este player dejaría de coincidir con Android — y además el fallo
        // se volvería invisible, que es justo lo que vinimos a arreglar.
        let mut asset_rows: Vec<serde_json::Value> = Vec::new();
        if let Some(assets) = spot.get("assets").and_then(|v| v.as_array()) {
            for a in assets {
                let aid = a.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if aid.is_empty() {
                    continue;
                }
                let aurl = a.get("audioUrl").and_then(|v| v.as_str()).unwrap_or("");
                let duration = a.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let apath = assets_dir.join(format!("{}.mp3", aid));
                if !apath.exists() && !aurl.is_empty() {
                    log::info!("Downloading spot asset {} del box {}", aid, name);
                    if let Err(e) = api::download_track(aurl, &apath).await {
                        // No se corta el loop ni se pierde el box: el audio queda marcado como
                        // no descargado y el reporte de emisión lo va a decir cuando le toque
                        // el turno (`download_failed`), que es cuando importa.
                        log::error!("Failed to download spot asset {} ({}): {}", aid, name, e);
                    }
                }
                asset_rows.push(serde_json::json!({
                    "id": aid,
                    "filePath": if apath.exists() { apath.to_string_lossy().to_string() } else { String::new() },
                    "duration": duration,
                }));
            }
        }

        if !file_path.exists() && !audio_url.is_empty() {
            log::info!("Downloading spot: {} - {}", id, name);
            match api::download_track(audio_url, &file_path).await {
                Ok(_) => log::info!("Downloaded spot: {}", name),
                Err(e) => {
                    log::error!("Failed to download spot {}: {}", name, e);
                    // Still save without file_path (pero CON los assets, si bajaron).
                    if !asset_rows.is_empty() {
                        if let Some(obj) = s.as_object_mut() {
                            obj.insert("_assets".to_string(), serde_json::json!(asset_rows));
                        }
                    }
                    spots_with_paths.push(s);
                    continue;
                }
            }
        }

        if let Some(obj) = s.as_object_mut() {
            if file_path.exists() {
                obj.insert("_filePath".to_string(), serde_json::json!(file_path.to_string_lossy().to_string()));
            }
            if !asset_rows.is_empty() {
                obj.insert("_assets".to_string(), serde_json::json!(asset_rows));
            }
        }
        spots_with_paths.push(s);
    }

    db::save_spot_schedules(&spots_with_paths);
    cleanup_spot_asset_cache(&assets_dir);
}

/// Borra del cache los audios de box que ya no están en ninguna grilla. Sin esto, cada cuña
/// vieja quedaría en disco para siempre y el cache de spots sólo crecería (el LRU de
/// `cleanup_cache` sólo conoce la tabla `tracks`).
fn cleanup_spot_asset_cache(assets_dir: &std::path::Path) {
    let live: std::collections::HashSet<String> = db::load_spot_schedules()
        .iter()
        .flat_map(|r| r.assets.iter().map(|a| a.id.clone()).collect::<Vec<_>>())
        .collect();
    if let Ok(entries) = std::fs::read_dir(assets_dir) {
        for e in entries.flatten() {
            let p = e.path();
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if !stem.is_empty() && !live.contains(stem) {
                let _ = std::fs::remove_file(&p);
                log::info!("Spot asset fuera de grilla borrado del cache: {}", stem);
            }
        }
    }
}

/// Check if a spot should play based on schedule rules.
///
/// Devuelve el `SpotPick` COMPLETO (qué box, qué audio y con qué datos reportarlo), no sólo
/// el path: sin `spotId` no hay reporte de emisión posible, que es de lo que se trata 0.9.17.
/// También devuelve un pick cuando el box NO tiene nada reproducible — el llamador encola el
/// `FAILED` (throttleado) en vez de que el anuncio no suene y nadie se entere.
fn find_eligible_spot(tz: &chrono_tz::Tz, tracks_since_last_spot: usize) -> Option<SpotPick> {
    let now = server_now().with_timezone(tz); // R-11: server-corrected time
    // Source of truth: backend uses JS getDay() = 0=Sun..6=Sat.
    let day_of_week = now.weekday().num_days_from_sunday();
    let current_time = now.format("%H:%M").to_string();
    let current_date = now.format("%Y-%m-%d").to_string();

    let spots = db::load_spot_schedules();
    if spots.is_empty() {
        return None;
    }

    // Cadencia por MINUTOS (`frequency`): epoch del último spot. Baseline = ahora en el
    // primer chequeo, así el primer anuncio por tiempo espera el intervalo completo.
    // El valor PERSISTE entre reinicios (db::get_last_spot_at), así que "primer chequeo" es
    // sólo el de una instalación nueva. Si el valor guardado está en el FUTURO (reloj que se
    // movió para atrás) se rebasea a ahora, o si no la cadencia quedaría trabada.
    let now_epoch = now.timestamp();
    let last_spot_epoch: Option<i64> = if let Ok(mut g) = last_spot_at().lock() {
        match *g {
            None => {
                *g = Some(now_epoch);
                db::set_last_spot_at(now_epoch);
            }
            Some(t) if t > now_epoch => {
                log::warn!("Cadencia de anuncios con timestamp futuro ({t} > {now_epoch}) — rebase a ahora");
                *g = Some(now_epoch);
                db::set_last_spot_at(now_epoch);
            }
            Some(_) => {}
        }
        *g
    } else {
        Some(now_epoch)
    };

    // R-06: collect ALL eligible spots, then rotate past the last one played so
    // the same announcement doesn't repeat back-to-back.
    //
    // ⚠️ Ya NO se descarta acá el box cuyo audio no está en disco. Ese `continue` silencioso
    // era el agujero del pedido comercial: el anuncio no sonaba y no quedaba ni un rastro.
    // Ahora el box llega al pick y el llamador reporta `FAILED`/`download_failed`.
    let mut eligible: Vec<(String, String)> = Vec::new();
    let mut by_id: HashMap<String, &db::SpotScheduleRow> = HashMap::new();
    for row in &spots {
        // Cadencia: si tiene trackFrequency (>0) → modo CANCIONES (cada N temas). Si no
        // (el backend manda trackFrequency=null → 0 cuando el modo es "minutos") → modo
        // MINUTOS: elegible si pasaron `freq` minutos desde el último spot. Fallback
        // histórico (ninguno seteado) = cada 4 canciones.
        let freq_ok = if row.track_frequency > 0 {
            tracks_since_last_spot >= row.track_frequency as usize
        } else if row.frequency > 0 {
            match last_spot_epoch {
                Some(t) => (now_epoch - t) >= (row.frequency as i64) * 60,
                None => true,
            }
        } else {
            tracks_since_last_spot >= 4
        };
        if !freq_ok {
            continue;
        }

        // Check day of week
        let days: Vec<u32> = serde_json::from_str(&row.days_of_week).unwrap_or_default();
        if !days.is_empty() && !days.contains(&day_of_week) {
            continue;
        }

        // Check time range
        if current_time.as_str() < row.start_time.as_str() || current_time.as_str() >= row.end_time.as_str() {
            continue;
        }

        // Check date range
        if let Some(sd) = &row.start_date {
            if !sd.is_empty() && current_date.as_str() < sd.as_str() {
                continue;
            }
        }
        if let Some(ed) = &row.end_date {
            if !ed.is_empty() && current_date.as_str() > ed.as_str() {
                continue;
            }
        }

        eligible.push((row.id.clone(), row.file_path.clone()));
        by_id.insert(row.id.clone(), row);
    }

    let last = last_spot_id().lock().ok().and_then(|g| g.clone());
    let picked_schedule = pick_spot(&eligible, last.as_deref())?.0.clone();
    let row = *by_id.get(&picked_schedule)?;

    // El anti-repeat ENTRE BOXES se consume al ELEGIR, no al sonar: si se consumiera al sonar,
    // un box roto que está primero en la lista sería el elegido en todos los turnos y los
    // demás boxes de la zona no sonarían NUNCA (inanición). La cadencia por minutos sí se
    // consume al sonar — está en `start_spot_playback`, y es el bug que vinimos a arreglar.
    if let Ok(mut g) = last_spot_id().lock() {
        *g = Some(picked_schedule.clone());
    }
    db::set_last_spot_schedule_id(&picked_schedule);

    // PASO 1-4 + 7 del contrato de rotación: qué audio del box le toca a este turno.
    let cursor = db::get_spot_last_asset(&row.id);
    let pick = resolve_spot_audio(row, cursor.as_deref());

    // PASO 5: el cursor se persiste ANTES de reproducir, nunca después. Si se persistiera
    // después, un audio roto que falla siempre dejaría el cursor clavado y el box se quedaría
    // pegado en ese audio para siempre en vez de pasar al siguiente en el próximo turno.
    // Ojo: `cursor_asset_id`, NO `asset_id`. Son distintos justo en el caso de la opción A
    // (el audio que le tocaba no estaba bajado y suena el default): ahí `asset_id` es `None`
    // porque lo que suena es el default, pero el cursor TIENE que avanzar igual o el box se
    // queda pegado en el audio roto para siempre.
    if let Some(asset_id) = pick.cursor_asset_id.as_deref() {
        db::set_spot_last_asset(&row.id, asset_id);
    }

    log::info!(
        "Spot eligible: schedule={} spot={} asset={:?} file={:?}",
        pick.schedule_id, pick.spot_id, pick.asset_id, pick.file_path
    );
    Some(pick)
}

/// Epoch server-time en segundos. Lo usan los dos caminos de avance para fechar reportes y
/// consumir la cadencia con el MISMO reloj que `find_eligible_spot`.
fn server_epoch() -> i64 {
    server_now().timestamp()
}

/// Called from main.rs every second to check if track ended and advance
/// SonicBox watcher — two tiers, mirroring how votes work in real life:
///   Nivel 1 (background, ~every 11s): discover the current winning vote early
///     and PRE-DOWNLOAD its audio so it's cached and ready in time.
///   Nivel 2 (late authoritative, last ~8s of the current track): one fresh
///     fetch so a last-moment vote change still gets in. Anything that changes
///     in the final couple of seconds is intentionally ignored.
/// The winner/queue ordering lives server-side (peekNext orders by credits);
/// here we just ask "what's next?" and stage it as the forced next track.
pub async fn start_sonicbox_loop(_handle: AppHandle) {
    use std::time::Instant;
    const TICK: Duration = Duration::from_secs(3);
    const BG_INTERVAL_SECS: f32 = 11.0;
    const LATE_WINDOW_SECS: f32 = 8.0;

    let mut last_full_fetch = Instant::now() - Duration::from_secs(60);
    let mut late_fetched_for: Option<String> = None;
    // R-15: back off when offline so we don't hammer the network every 3s.
    let mut consecutive_failures: u32 = 0;

    loop {
        tokio::time::sleep(TICK).await;
        // Extra backoff after repeated failures (offline): +3s per failure up to
        // ~45s, on top of the base TICK. Resets on the first success.
        if consecutive_failures > 0 {
            let extra = (consecutive_failures.min(15) * 3) as u64;
            tokio::time::sleep(Duration::from_secs(extra)).await;
        }

        let cfg = config::AppConfig::load();
        let zone_id = match cfg.zone_id.clone() {
            Some(z) if !z.is_empty() => z,
            _ => continue,
        };

        // Snapshot player state under a short lock.
        let (cur_id, remaining, busy) = {
            let p = match audio::player().lock() {
                Ok(p) => p,
                Err(poisoned) => poisoned.into_inner(),
            };
            let cur_id = p.current_track().map(|t| t.track_id.clone()).unwrap_or_default();
            let dur = p.current_track().map(|t| t.duration).unwrap_or(0.0);
            let pos = p.get_position();
            let remaining = if dur > 0.0 { dur - pos } else { f32::INFINITY };
            // Don't stage a forced track while a spot or another voted track is
            // already mid-interrupt.
            let busy = p.playing_spot || p.playing_sonicbox || !p.is_playing();
            (cur_id, remaining, busy)
        };

        let in_late_window = remaining <= LATE_WINDOW_SECS && !busy;
        let due_background = last_full_fetch.elapsed().as_secs_f32() >= BG_INTERVAL_SECS;
        let due_late = in_late_window && late_fetched_for.as_deref() != Some(cur_id.as_str());

        if !(due_background || due_late) {
            continue;
        }
        if due_late {
            late_fetched_for = Some(cur_id.clone());
            log::info!("SonicBox: late authoritative re-check ({:.1}s left on current track)", remaining);
        }
        last_full_fetch = Instant::now();

        let json = match api::fetch_zone_now_playing(&zone_id).await {
            Ok(j) => {
                consecutive_failures = 0; // R-15: back online
                j
            }
            Err(e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                if consecutive_failures <= 3 {
                    log::warn!("SonicBox: now-playing fetch failed ({}): {}", consecutive_failures, e);
                }
                continue;
            }
        };

        // Read the staged vote. The backend puts the voted entry in
        // nextTracks[0] (with isSonicBox:true) and a summary in sonicboxNext.
        let sb_entry = json
            .get("nextTracks")
            .and_then(|n| n.get(0))
            .filter(|e| e.get("isSonicBox").and_then(|v| v.as_bool()).unwrap_or(false));

        let sb_entry = match sb_entry {
            Some(e) => e,
            None => {
                // No active winning vote — drop any previously staged track.
                let mut p = match audio::player().lock() {
                    Ok(p) => p,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if p.has_sonicbox_next() {
                    log::info!("SonicBox: no active vote, clearing staged track");
                    p.clear_sonicbox_next();
                }
                continue;
            }
        };

        let track_id = sb_entry.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let vote_id = sb_entry
            .get("voteId")
            .and_then(|v| v.as_str())
            .or_else(|| json.get("sonicboxNext").and_then(|s| s.get("voteId")).and_then(|v| v.as_str()))
            .unwrap_or_default()
            .to_string();
        if track_id.is_empty() {
            continue;
        }

        // R-12 (anti-repeat): if this track already played via SonicBox within
        // the last SONICBOX_MIN_GAP_TRACKS, do NOT re-stage it — even if the API
        // keeps returning the same vote because markPlayed hasn't landed yet.
        // This is what stops a voted track from looping back-to-back.
        if sb_recently_played(&track_id) {
            log::info!(
                "SonicBox: skipping repeat of track {} (played <{} tracks ago)",
                track_id, SONICBOX_MIN_GAP_TRACKS
            );
            let mut p = match audio::player().lock() {
                Ok(p) => p,
                Err(poisoned) => poisoned.into_inner(),
            };
            if p.has_sonicbox_next() {
                p.clear_sonicbox_next();
            }
            continue;
        }

        let title = sb_entry.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let artist = sb_entry.get("artist").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let duration = sb_entry.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
        let artwork_url = sb_entry.get("coverUrl").and_then(|v| v.as_str()).map(|s| s.to_string());
        let stream_url = sb_entry.get("streamUrl").and_then(|v| v.as_str()).unwrap_or("").to_string();

        // Ensure the audio file is cached (pre-download). Same cache layout as
        // the playlist: cache_dir/<trackId>.mp3.
        let cache_dir = config::AppConfig::cache_dir();
        let _ = std::fs::create_dir_all(&cache_dir);
        let file_path = cache_dir.join(format!("{}.mp3", track_id));
        if !file_path.exists() {
            if stream_url.is_empty() {
                log::warn!("SonicBox: voted track {} not cached and no streamUrl", track_id);
                continue;
            }
            log::info!("SonicBox: pre-downloading voted track '{}'", title);
            if let Err(e) = api::download_track(&stream_url, &file_path).await {
                log::error!("SonicBox: failed to pre-download voted track: {}", e);
                continue;
            }
        }

        let track = audio::TrackInfo {
            track_id: track_id.clone(),
            title,
            artist,
            file_path: file_path.to_string_lossy().to_string(),
            duration,
            artwork_url,
        };

        let mut p = match audio::player().lock() {
            Ok(p) => p,
            Err(poisoned) => poisoned.into_inner(),
        };
        p.set_sonicbox_next(track, vote_id.clone());
        log::info!("SonicBox: staged voted track {} (vote={}) as forced next", track_id, vote_id);
    }
}

pub fn check_track_advancement(handle: &AppHandle) {
    let mut player = match audio::player().lock() {
        Ok(p) => p,
        Err(poisoned) => {
            log::error!("Audio mutex poisoned! Recovering...");
            poisoned.into_inner()
        },
    };

    if !player.is_playing() {
        return;
    }

    // Update crossfade volumes if active
    player.update_crossfade();

    // R-13: crossfade trigger — near the end of WHATEVER is playing (grid track
    // OR SonicBox voted track), crossfade into whatever plays next, so there's
    // NEVER a hard cut: grid→grid, grid→SonicBox, and SonicBox→grid all crossfade.
    // Config (enabled/duration) comes from the API per zone/device.
    let cfg = config::AppConfig::load();
    if cfg.crossfade_enabled && !player.crossfade_active && !player.playing_spot {
        let position = player.get_position();
        let cur_track = if player.playing_sonicbox {
            player.sonicbox_current().cloned()
        } else {
            player.current_track().cloned()
        };
        if let Some(cur) = cur_track {
            let crossfade_point = cur.duration - cfg.crossfade_duration as f32;
            if crossfade_point > 0.0 && position >= crossfade_point && !player.is_finished() {
                let zone_id = cfg.zone_id.clone().unwrap_or_default();
                // `started_at` es cuando ARRANCÓ el tema, no cuando termina: acá estamos en
                // el punto de crossfade, o sea casi al final, así que le restamos lo ya
                // reproducido. Con `Utc::now()` pelado toda la línea de tiempo de
                // PlayHistory quedaba corrida ~un tema hacia adelante (mismo criterio que
                // el path SonicBox más abajo).
                let started_at =
                    (Utc::now() - chrono::Duration::seconds(position.max(0.0) as i64)).to_rfc3339();
                // Misma fórmula que usa el reporte de SonicBox: escuchado el 80% o más.
                // El desktop nunca mandaba este flag y el backend defaulteaba a false, así
                // que el 100% de los plays quedaba sin completar.
                let completed = cur.duration > 1.0 && position >= cur.duration * 0.8;

                if player.playing_sonicbox {
                    // ── C: SonicBox track ending → crossfade into the grid next,
                    //       close the vote loop, resume the grid.
                    if let Some(next) = player.peek_next().cloned() {
                        if let Err(e) = player.start_crossfade(&next) {
                            log::error!("Crossfade (sb->grid) failed: {}", e);
                        } else {
                            sb_note_played(&cur.track_id);
                            if let Some(token) = cfg.device_token.clone() {
                                let track_id = cur.track_id.clone();
                                let dur = position.max(0.0) as i64;
                                let sa = started_at.clone();
                                tauri::async_runtime::spawn(async move {
                                    let play = serde_json::json!({
                                        "trackId": track_id, "startedAt": sa, "duration": dur,
                                        "completed": completed, "skipped": !completed,
                                    });
                                    let _ = api::report_player_play(&token, vec![play]).await;
                                });
                            }
                            log::info!("Crossfade SonicBox '{}' -> grid '{}'", cur.title, next.title);
                            player.clear_playing_sonicbox();
                            player.advance();
                            player.tracks_since_last_spot += 1;
                            player.reset_position();
                            if let Some(t) = player.current_track() {
                                let _ = handle.emit("now-playing", serde_json::json!({
                                    "title": t.title, "artist": t.artist, "duration": t.duration,
                                    "position": 0.0, "artworkUrl": t.artwork_url,
                                }));
                            }
                            return;
                        }
                    }
                } else if player.has_sonicbox_next() {
                    // ── B: grid track ending + vote staged → play the voted track.
                    //       NOTE: we use play_file() (a hard sink swap), NOT
                    //       start_crossfade(). On real hardware the crossfade is
                    //       fine, but it was leaving the voted track INAUDIBLE on
                    //       some setups — the second rodio sink never took over the
                    //       output, so the grid track kept sounding while the state
                    //       said "SonicBox playing". play_file() replaces the single
                    //       sink outright (the same path R-10 re-sync uses, which is
                    //       reliably audible), so the vote the customer paid for
                    //       actually plays. Small hard cut INTO the vote is the price.
                    if let Some((sb, vote)) = player.take_sonicbox_next() {
                        if let Err(e) = player.play_file(&sb) {
                            log::error!("Play (grid->sb) failed: {}", e);
                            player.set_sonicbox_next(sb, vote); // put it back for the finish path
                        } else {
                            db::save_play_report(&cur.track_id, &zone_id, &started_at, position as f64, completed);
                            db::touch_track(&cur.track_id);
                            log::info!("Play grid '{}' -> SonicBox '{}' (vote={})", cur.title, sb.title, vote);
                            sb_note_played(&sb.track_id);
                            let sb_clone = sb.clone();
                            player.set_playing_sonicbox(sb_clone, vote);
                            player.reset_position();
                            let _ = handle.emit("now-playing", serde_json::json!({
                                "title": sb.title, "artist": sb.artist, "duration": sb.duration,
                                "position": 0.0, "artworkUrl": sb.artwork_url, "isSonicBox": true,
                            }));
                            return;
                        }
                    }
                } else {
                    // ── A: normal grid → grid.
                    // Spots fire on THIS path too. Previously the crossfade path
                    // skipped find_eligible_spot entirely, so on any zone with
                    // crossfade enabled (the retail default) announcements NEVER
                    // played — find_eligible_spot only ran in the no-crossfade
                    // finish fallback. If a spot is due, interrupt with it before
                    // crossfading; the spot-finished resume (above) then advances
                    // to the next grid track (we DON'T advance here, matching the
                    // finish path, so we don't skip a track).
                    {
                        let tz_str = cfg.timezone.as_deref().unwrap_or("America/Montevideo");
                        let tz: chrono_tz::Tz =
                            tz_str.parse().unwrap_or(chrono_tz::America::Montevideo);
                        // cur is at its crossfade point (effectively the latest
                        // finished track) → count it for spot frequency.
                        if let Some(pick) =
                            find_eligible_spot(&tz, player.tracks_since_last_spot + 1)
                        {
                            log::info!(
                                "Playing spot before next track (crossfade path, tracks since last: {})",
                                player.tracks_since_last_spot + 1
                            );
                            // El play del TEMA se reporta sólo si el anuncio efectivamente
                            // interrumpe. Si no arranca, se cae al crossfade de abajo, que
                            // reporta el tema él mismo — si se reportara acá, un anuncio
                            // fallido duplicaría la fila de PlayHistory del tema.
                            if start_spot_playback(&mut player, &pick, &zone_id, server_epoch()) {
                                db::save_play_report(&cur.track_id, &zone_id, &started_at, position as f64, completed);
                                db::touch_track(&cur.track_id);
                                let _ = handle.emit("now-playing", serde_json::json!({
                                    "title": "📢 Spot", "artist": "Anuncio",
                                    "duration": 0, "position": 0.0, "artworkUrl": null,
                                }));
                                return;
                            }
                        }
                    }
                    if let Some(next) = player.peek_next().cloned() {
                        if let Err(e) = player.start_crossfade(&next) {
                            log::error!("Crossfade failed: {}", e);
                        } else {
                            db::save_play_report(&cur.track_id, &zone_id, &started_at, position as f64, completed);
                            db::touch_track(&cur.track_id);
                            player.advance();
                            player.tracks_since_last_spot += 1;
                            player.reset_position();
                            if let Some(t) = player.current_track() {
                                let _ = handle.emit("now-playing", serde_json::json!({
                                    "title": t.title, "artist": t.artist, "duration": t.duration,
                                    "position": 0.0, "artworkUrl": t.artwork_url,
                                }));
                            }
                            return;
                        }
                    }
                }
            }
        }
    }

    if !player.is_finished() {
        if player.get_position() > 5.0 {
            player.consecutive_skips = 0;
        }
        // Log position every 30s, and every second in the last 10s of track.
        // When a SonicBox vote is on air its real duration lives in
        // sonicbox_current, NOT the grid slot current_track() returns — using the
        // grid duration produced misleading logs like "Playing 280s / 227s".
        let pos = player.get_position();
        let dur = if player.playing_sonicbox {
            player.sonicbox_current().map(|t| t.duration).unwrap_or(0.0)
        } else {
            player.current_track().map(|t| t.duration).unwrap_or(0.0)
        };
        let near_end = dur > 1.0 && pos >= dur - 10.0;
        if pos > 0.0 && ((pos as u32) % 30 == 0 || near_end) {
            log::info!("Playing {:.0}s / {:.0}s (finished={})", pos, dur, player.is_finished());
        }
        return;
    }

    let position = player.get_position();
    // Same as above: report the SonicBox vote's title/duration when it's on air.
    let (track_title, track_dur) = if player.playing_sonicbox {
        player.sonicbox_current()
            .map(|t| (t.title.clone(), t.duration))
            .unwrap_or_default()
    } else {
        player.current_track()
            .map(|t| (t.title.clone(), t.duration))
            .unwrap_or_default()
    };
    log::info!("Track '{}' finished at {:.1}s (duration={:.1}s, playing_spot={})",
        track_title, position, track_dur, player.playing_spot);

    // R-12: count every finished track so the SonicBox anti-repeat gap can be
    // measured in "tracks since". Covers grid, spot and sonicbox tracks.
    sb_bump_tracks_played();

    // If a spot just finished, resume normal playback
    if player.playing_spot {
        log::info!("Spot finished, resuming normal playback");
        // ÚNICO lugar donde termina un anuncio, sin importar por qué camino de avance
        // arrancó: acá se cierra el reporte de emisión (COMPLETED / INTERRUPTED).
        close_on_air_spot(position, None);
        player.playing_spot = false;
        player.tracks_since_last_spot = 0;
        player.advance(); // Move to NEXT track (previous track already finished before spot)
        if let Err(e) = player.play_current() {
            log::error!("Error resuming after spot: {}", e);
        }
        if let Some(track) = player.current_track() {
            let _ = handle.emit("now-playing", serde_json::json!({
                "title": track.title,
                "artist": track.artist,
                "duration": track.duration,
                "position": 0.0,
                "artworkUrl": track.artwork_url,
            }));
        }
        return;
    }

    // SonicBox: a voted track just finished. Close the vote loop by reporting
    // the completion to the REAL player endpoint (/player/play-report), then
    // resume the normal grid from the track that would have played next.
    if player.playing_sonicbox {
        let sb_pos = player.get_position();
        if let Some((sb_track, vote_id)) = player.clear_playing_sonicbox() {
            // completed = played most of it (not skipped early). Backend only
            // closes the loop when completed && !skipped.
            let completed = sb_track.duration > 1.0 && sb_pos >= sb_track.duration * 0.8;
            log::info!(
                "SonicBox track '{}' finished at {:.1}s/{:.1}s (vote={}, completed={})",
                sb_track.title, sb_pos, sb_track.duration, vote_id, completed
            );
            // R-12 (hard guarantee): remember this track played via SonicBox so
            // the vigía won't re-stage it for the next SONICBOX_MIN_GAP_TRACKS,
            // even if the play-report below never reaches the server.
            sb_note_played(&sb_track.track_id);
            let cfg_sb = config::AppConfig::load();
            if let Some(token) = cfg_sb.device_token.clone() {
                let track_id = sb_track.track_id.clone();
                let started_at =
                    (Utc::now() - chrono::Duration::seconds(sb_pos as i64)).to_rfc3339();
                let dur = sb_pos.max(0.0) as i64;
                tauri::async_runtime::spawn(async move {
                    let play = serde_json::json!({
                        "trackId": track_id,
                        "startedAt": started_at,
                        "duration": dur,
                        "completed": completed,
                        "skipped": !completed,
                    });
                    // R-12 (soft cleanup): retry so markPlayed closes the vote
                    // server-side. The local guard already prevents a loop if
                    // every attempt fails.
                    let mut sent = false;
                    for attempt in 1..=3u32 {
                        match api::report_player_play(&token, vec![play.clone()]).await {
                            Ok(_) => {
                                log::info!("SonicBox: play-report sent (completed={}, attempt={})", completed, attempt);
                                sent = true;
                                break;
                            }
                            Err(e) => {
                                log::warn!("SonicBox: play-report attempt {}/3 failed: {}", attempt, e);
                                tokio::time::sleep(Duration::from_secs(2 * attempt as u64)).await;
                            }
                        }
                    }
                    if !sent {
                        log::error!("SonicBox: play-report failed after 3 attempts — local anti-repeat guard still active");
                    }
                });
            } else {
                log::warn!("SonicBox: no device_token, cannot close vote loop");
            }
        }
        // Resume the grid: advance to the track that follows the one the vote
        // interrupted, and play it.
        player.advance();
        if let Err(e) = player.play_current() {
            log::error!("Error resuming grid after SonicBox track: {}", e);
        }
        if let Some(track) = player.current_track() {
            let _ = handle.emit("now-playing", serde_json::json!({
                "title": track.title,
                "artist": track.artist,
                "duration": track.duration,
                "position": 0.0,
                "artworkUrl": track.artwork_url,
            }));
        }
        return;
    }

    // Collect data for DB calls, then drop audio lock to avoid deadlock
    let report_data = player.current_track().map(|track| {
        let track_id = track.track_id.clone();
        let file_path = track.file_path.clone();
        let track_title = track.title.clone();
        let track_dur = track.duration;
        (track_id, file_path, track_title, track_dur)
    });

    if let Some((ref track_id, ref file_path, ref track_title, track_dur)) = report_data {
        if position < 3.0 && track_dur > 5.0 {
            log::error!("Track '{}' finished after only {:.1}s (expected {:.0}s) - file may be corrupt: {}",
                track_title, position, track_dur, file_path);
            player.consecutive_skips += 1;
        } else {
            player.consecutive_skips = 0;
        }
    }

    // Drop audio lock BEFORE DB operations to prevent deadlock
    let consecutive_skips = player.consecutive_skips;
    let playlist_len = player.playlist_len();
    drop(player);

    // Now safe to call DB without holding audio lock
    if let Some((track_id, _, _, track_dur)) = &report_data {
        if position > 3.0 {
            let zone_id = cfg.zone_id.as_deref().unwrap_or("");
            // Igual que en el path de crossfade: acá el tema YA terminó, así que
            // `Utc::now()` es el instante de fin. El startedAt real es eso menos lo
            // reproducido, si no PlayHistory queda corrido un tema entero.
            let started_at =
                (Utc::now() - chrono::Duration::seconds(position.max(0.0) as i64)).to_rfc3339();
            let completed = *track_dur > 1.0 && position >= *track_dur * 0.8;
            db::save_play_report(track_id, zone_id, &started_at, position as f64, completed);
            db::touch_track(track_id);
        }
    }

    // Re-acquire audio lock for the rest of advancement
    let mut player = match audio::player().lock() {
        Ok(p) => p,
        Err(poisoned) => poisoned.into_inner(),
    };

    if consecutive_skips >= playlist_len && playlist_len > 0 {
        // R-02: NEVER hard-stop into dead air. Every track in the current grid
        // failed (corrupt/missing cache) — recover instead: force a re-sync to
        // re-download, and meanwhile fall back to an emergency shuffle of ALL
        // cached tracks (other slots/playlists may have working files). Reset
        // the skip counter so playback keeps trying rather than freezing.
        log::error!(
            "All {} grid tracks failed — recovering (re-sync + emergency shuffle), NOT stopping",
            playlist_len
        );
        player.consecutive_skips = 0;
        drop(player);
        trigger_sync();
        enter_emergency_mode(handle);
        return;
    }

    // ── SonicBox: a winning vote takes the slot right after the current track
    // ends, ahead of any spot. We play it as an interrupt (like a spot): the
    // playlist index is NOT advanced here — it advances when the voted track
    // finishes, so the grid resumes exactly where it left off.
    if player.has_sonicbox_next() {
        if let Some((sb_track, vote_id)) = player.take_sonicbox_next() {
            match player.play_file(&sb_track) {
                Ok(_) => {
                    log::info!(
                        "SonicBox: playing voted track '{}' by '{}' (vote={})",
                        sb_track.title, sb_track.artist, vote_id
                    );
                    player.set_playing_sonicbox(sb_track.clone(), vote_id);
                    // R-12: mark as played the moment it STARTS, so a background
                    // poll mid-playback can't re-stage the same track and cause
                    // a back-to-back loop.
                    sb_note_played(&sb_track.track_id);
                    let _ = handle.emit("now-playing", serde_json::json!({
                        "title": sb_track.title,
                        "artist": sb_track.artist,
                        "duration": sb_track.duration,
                        "position": 0.0,
                        "artworkUrl": sb_track.artwork_url,
                        "isSonicBox": true,
                    }));
                    return;
                }
                Err(e) => {
                    log::error!(
                        "SonicBox: failed to play voted track '{}': {} — falling back to grid",
                        sb_track.title, e
                    );
                    // fall through to normal spot/advance below
                }
            }
        }
    }

    // Increment track counter
    player.tracks_since_last_spot += 1;

    // Check if a spot should play before next track
    let tz_str = cfg.timezone.as_deref().unwrap_or("America/Montevideo");
    let tz: chrono_tz::Tz = tz_str.parse().unwrap_or(chrono_tz::America::Montevideo);
    if let Some(pick) = find_eligible_spot(&tz, player.tracks_since_last_spot) {
        log::info!("Playing spot before next track (tracks since last: {})", player.tracks_since_last_spot);
        let zone_id = cfg.zone_id.clone().unwrap_or_default();
        // Mismo helper que el camino de crossfade: rotación, cursor, cadencia y reporte son
        // el MISMO código en los dos caminos de avance (player_two_advance_paths).
        if start_spot_playback(&mut player, &pick, &zone_id, server_epoch()) {
            let _ = handle.emit("now-playing", serde_json::json!({
                "title": "📢 Spot",
                "artist": "Anuncio",
                "duration": 0,
                "position": 0.0,
                "artworkUrl": null,
            }));
            return;
        }
        // Si no arrancó, ya quedó encolado el FAILED: se cae al avance normal y la música
        // sigue. Un anuncio que falla NUNCA puede dejar el local en silencio.
    }

    log::info!("Track finished (pos={:.1}s), advancing...", position);
    if player.advance() {
        if let Some(track) = player.current_track() {
            log::info!("Next track: '{}' by '{}' ({})", track.title, track.artist, track.file_path);
        }
        if let Err(e) = player.play_current() {
            log::error!("Error playing next track: {}", e);
            player.consecutive_skips += 1;
        }
    }

    if let Some(track) = player.current_track() {
        let _ = handle.emit("now-playing", serde_json::json!({
            "title": track.title,
            "artist": track.artist,
            "duration": track.duration,
            "position": 0.0,
            "artworkUrl": track.artwork_url,
        }));
    }
}

#[cfg(test)]
mod weighted_interleave_tests {
    use super::*;

    #[test]
    fn golden_vector_rock3_pop2() {
        // Mismo vector dorado que el backend (weighted-interleave.spec.ts).
        let rock = vec!["r0", "r1", "r2", "r3", "r4"];
        let pop = vec!["p0", "p1", "p2"];
        let seq = weighted_interleave(&[(rock, 3), (pop, 2)], 15);
        assert_eq!(
            seq,
            vec![
                "r0", "r1", "r2", "p0", "p1", "r3", "r4", "r0", "p2", "p0", "r1",
                "r2", "r3", "p1", "p2"
            ]
        );
    }

    #[test]
    fn deterministic_prefix() {
        let rock = vec!["r0", "r1", "r2", "r3", "r4"];
        let pop = vec!["p0", "p1", "p2"];
        let a = weighted_interleave(&[(rock.clone(), 3), (pop.clone(), 2)], 7);
        let b = weighted_interleave(&[(rock, 3), (pop, 2)], 40);
        assert_eq!(a, b[..7].to_vec());
    }

    #[test]
    fn single_is_cyclic() {
        let only = vec!["a", "b", "c"];
        assert_eq!(
            weighted_interleave(&[(only, 1)], 7),
            vec!["a", "b", "c", "a", "b", "c", "a"]
        );
    }
}

#[cfg(test)]
mod sonicbox_repeat_tests {
    use super::*;

    #[test]
    fn never_skips_a_track_not_seen_before() {
        assert!(!sb_should_skip_repeat(None, 100, SONICBOX_MIN_GAP_TRACKS));
    }

    #[test]
    fn skips_within_the_gap_window() {
        // played at 10, only 1 track later → must skip (would be a near-loop)
        assert!(sb_should_skip_repeat(Some(10), 11, 4));
        // played at 10, 3 tracks later → still inside gap of 4 → skip
        assert!(sb_should_skip_repeat(Some(10), 13, 4));
    }

    #[test]
    fn allows_again_once_the_gap_has_passed() {
        // exactly gap tracks later → eligible again (Pablo: "después de 3-4 vuelve")
        assert!(!sb_should_skip_repeat(Some(10), 14, 4));
        assert!(!sb_should_skip_repeat(Some(10), 20, 4));
    }

    #[test]
    fn spot_rotation_avoids_repeating_the_last() {
        let eligible = vec![
            ("spot-a".to_string(), "/a.mp3".to_string()),
            ("spot-b".to_string(), "/b.mp3".to_string()),
        ];
        // last was A → must pick B
        assert_eq!(pick_spot(&eligible, Some("spot-a")).unwrap().0, "spot-b");
        // last was B → must pick A
        assert_eq!(pick_spot(&eligible, Some("spot-b")).unwrap().0, "spot-a");
        // none played yet → first
        assert_eq!(pick_spot(&eligible, None).unwrap().0, "spot-a");
    }

    #[test]
    fn spot_plays_anyway_if_its_the_only_eligible() {
        let only = vec![("spot-a".to_string(), "/a.mp3".to_string())];
        // even though A was the last, it's the only due ad → play it (no dead air)
        assert_eq!(pick_spot(&only, Some("spot-a")).unwrap().0, "spot-a");
        // empty → nothing
        assert!(pick_spot(&[], Some("x")).is_none());
    }

    #[test]
    fn global_guard_blocks_then_releases() {
        // Self-contained against the shared global: use a unique id and relative
        // reasoning so the starting counter value doesn't matter.
        let id = "unit-test-track-xyz";
        sb_note_played(id);
        assert!(sb_recently_played(id), "just played → must be blocked");
        for _ in 0..SONICBOX_MIN_GAP_TRACKS {
            sb_bump_tracks_played();
        }
        assert!(!sb_recently_played(id), "after the gap → eligible again");
    }
}

#[cfg(test)]
mod spot_rotation_contract_tests {
    use super::*;

    /// CONTRATO DE ROTACIÓN v1 — los resultados de estos tests tienen que ser IDÉNTICOS a los
    /// de `millsonic/src/spots/spot-assets.util.spec.ts` (la implementación de referencia) y a
    /// los del player Android. Si uno de los tres se desvía, el reporte de emisión deja de ser
    /// reproducible y el cliente escucha la misma cuña siempre.

    fn asset(id: &str) -> db::SpotAssetRow {
        db::SpotAssetRow { id: id.into(), file_path: format!("/tmp/{id}.mp3"), duration: 20.0 }
    }

    fn assets(ids: &[&str]) -> Vec<db::SpotAssetRow> {
        ids.iter().map(|i| asset(i)).collect()
    }

    /// Fila de `spot_schedules` como la devuelve la DB. Por defecto: box multi-audio con
    /// rotación conocida y un audio por defecto cacheado.
    fn row(assets: Vec<db::SpotAssetRow>, rotation: Option<&str>) -> db::SpotScheduleRow {
        db::SpotScheduleRow {
            id: "ss1".into(),
            spot_id: "sp1".into(),
            days_of_week: "[]".into(),
            start_time: "00:00".into(),
            end_time: "23:59".into(),
            track_frequency: 4,
            frequency: 0,
            start_date: None,
            end_date: None,
            file_path: String::new(),
            audio_url: "https://s3/default.mp3".into(),
            assets,
            rotation: rotation.map(|r| r.to_string()),
        }
    }

    // ── Paso 2: sin assets → camino legacy ───────────────────────────────────

    #[test]
    fn paso2_sin_assets_usa_el_audio_por_defecto() {
        let f = std::env::temp_dir().join("ms_rot_default.mp3");
        std::fs::write(&f, b"x").unwrap();
        let mut r = row(vec![], None);
        r.file_path = f.to_string_lossy().to_string();

        let pick = resolve_spot_audio(&r, None);
        assert_eq!(pick.file_path.as_deref(), Some(r.file_path.as_str()));
        assert_eq!(pick.asset_id, None, "el camino legacy no reporta assetId");
        assert_eq!(pick.failure_reason, None);
        assert_eq!(pick.spot_id, "sp1");
        assert_eq!(pick.schedule_id, "ss1");
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn paso2_lista_vacia_es_lo_mismo_que_ausente() {
        // El server nunca manda `assets: []`, pero si lo hiciera no hay que romperse.
        let r = row(vec![], Some(SPOT_ROTATION_ROUND_ROBIN));
        let pick = resolve_spot_audio(&r, None);
        assert_eq!(pick.asset_id, None);
        assert_eq!(pick.file_path, None);
    }

    #[test]
    fn paso2_box_sin_audio_ninguno_reporta_audio_missing() {
        // Box TTS o box con TODOS los audios deshabilitados: el server manda audioUrl null.
        let mut r = row(vec![], None);
        r.audio_url = String::new();
        let pick = resolve_spot_audio(&r, None);
        assert_eq!(pick.file_path, None, "no suena");
        assert_eq!(pick.failure_reason, Some(FAIL_AUDIO_MISSING));
    }

    #[test]
    fn paso2_audio_por_defecto_que_no_bajo_reporta_download_failed() {
        // Hay URL pero no está en disco: la descarga falló. Es el caso "el anuncio no suena y
        // nadie se entera" que originó todo el reporte de emisión.
        let mut r = row(vec![], None);
        r.file_path = "/tmp/no-existe-ms-rot.mp3".into();
        let pick = resolve_spot_audio(&r, None);
        assert_eq!(pick.file_path, None);
        assert_eq!(pick.failure_reason, Some(FAIL_DOWNLOAD));
    }

    // ── Paso 3: un solo asset ────────────────────────────────────────────────

    #[test]
    fn paso3_un_solo_asset_suena_igual_y_no_se_saltea_el_turno() {
        let a = assets(&["a1"]);
        // Aun si el cursor ya apunta a ese mismo asset: no hay otro, no se puede dejar el
        // turno en silencio.
        assert_eq!(pick_rotation_asset(&a, None).unwrap().id, "a1");
        assert_eq!(pick_rotation_asset(&a, Some("a1")).unwrap().id, "a1");
        assert_eq!(pick_rotation_asset(&a, Some("viejo")).unwrap().id, "a1");
    }

    // ── Paso 4: (i + 1) % n ──────────────────────────────────────────────────

    #[test]
    fn paso4_cursor_ausente_arranca_en_el_primero() {
        let a = assets(&["a1", "a2", "a3"]);
        assert_eq!(pick_rotation_asset(&a, None).unwrap().id, "a1");
    }

    #[test]
    fn paso4_avanza_uno_y_da_la_vuelta() {
        let a = assets(&["a1", "a2", "a3"]);
        assert_eq!(pick_rotation_asset(&a, Some("a1")).unwrap().id, "a2");
        assert_eq!(pick_rotation_asset(&a, Some("a2")).unwrap().id, "a3");
        assert_eq!(pick_rotation_asset(&a, Some("a3")).unwrap().id, "a1");
    }

    #[test]
    fn paso4_cursor_con_id_que_ya_no_esta_en_la_lista_arranca_en_el_primero() {
        // Asset borrado o deshabilitado: el server ya no lo manda. i = None → A[0], que nunca
        // puede ser el que acaba de sonar (no está en la lista).
        let a = assets(&["a2", "a3"]);
        assert_eq!(pick_rotation_asset(&a, Some("a1-borrado")).unwrap().id, "a2");
    }

    #[test]
    fn paso4_anti_repeat_en_turnos_consecutivos() {
        let a = assets(&["a1", "a2", "a3", "a4", "a5"]);
        let mut cursor: Option<String> = None;
        let mut secuencia: Vec<String> = Vec::new();
        for _ in 0..12 {
            let elegido = pick_rotation_asset(&a, cursor.as_deref()).unwrap().id.clone();
            // ANTI-REPEAT: con n >= 2 nunca repite el inmediatamente anterior.
            assert_ne!(Some(elegido.as_str()), cursor.as_deref(), "repitió el audio anterior");
            cursor = Some(elegido.clone());
            secuencia.push(elegido);
        }
        // Round-robin puro, mismo vector que la referencia en JS.
        assert_eq!(
            secuencia,
            vec!["a1","a2","a3","a4","a5","a1","a2","a3","a4","a5","a1","a2"]
        );
    }

    #[test]
    fn paso1_el_orden_del_server_es_autoritativo_no_se_reordena() {
        // La misma lista en otro orden (el panel reordenó) da otra elección: el player NO
        // ordena nada por su cuenta, usa el índice tal como vino.
        let directo = assets(&["a1", "a2", "a3"]);
        let reordenada = assets(&["a3", "a1", "a2"]);
        assert_eq!(pick_rotation_asset(&directo, Some("a1")).unwrap().id, "a2");
        assert_eq!(pick_rotation_asset(&reordenada, Some("a1")).unwrap().id, "a2");
        // Y el cursor sigue siendo válido tras el reorden: el anti-repeat se mantiene.
        assert_eq!(pick_rotation_asset(&reordenada, Some("a3")).unwrap().id, "a1");
    }

    /// Crea archivos de verdad en disco: `resolve_spot_audio` decide con `Path::exists()`, así
    /// que un asset "bajado" tiene que existir o el test prueba otra cosa.
    fn en_disco(nombre: &str) -> String {
        let dir = std::env::temp_dir().join("ms-spot-rot-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(nombre);
        std::fs::write(&f, b"x").unwrap();
        f.to_string_lossy().into_owned()
    }

    fn asset_bajado(id: &str) -> db::SpotAssetRow {
        db::SpotAssetRow {
            id: id.into(),
            file_path: en_disco(&format!("{id}.mp3")),
            duration: 20.0,
        }
    }

    fn asset_sin_bajar(id: &str) -> db::SpotAssetRow {
        db::SpotAssetRow { id: id.into(), file_path: String::new(), duration: 20.0 }
    }

    #[test]
    fn paso1_un_asset_que_no_bajo_sigue_en_la_lista_y_no_corre_el_indice() {
        // Si se filtrara el roto, el índice se correría y este player dejaría de coincidir con
        // Android. El roto CONSERVA su turno y el cursor avanza a él igual (paso 5): si no
        // avanzara, el box se quedaría pegado en el audio roto para siempre.
        //
        // Y acá se fija la OPCIÓN A (decisión de Pablo, 2026-10-30), que es lo que hace
        // Android: lo que suena es el audio POR DEFECTO del box, la emisión NO se le atribuye
        // al audio roto, y la falla se reporta aparte con su assetId.
        let default_file = en_disco("default.mp3");
        let a = vec![asset_bajado("a1"), asset_sin_bajar("a2"), asset_bajado("a3")];
        let mut r = row(a, Some(SPOT_ROTATION_ROUND_ROBIN));
        r.file_path = default_file.clone();

        let pick = resolve_spot_audio(&r, Some("a1"));
        assert_eq!(pick.cursor_asset_id.as_deref(), Some("a2"), "el roto conserva su turno");
        assert_eq!(pick.file_path.as_deref(), Some(default_file.as_str()), "suena el default");
        assert_eq!(pick.asset_id, None, "la emisión es del default, no del audio roto");
        assert_eq!(pick.pending_failure, Some(FAIL_ASSET_NOT_CACHED), "la falla se reporta igual");
        assert_eq!(pick.failure_reason, None, "el box SUENA: no es una emisión fallida");

        // Próximo turno (el cursor ya avanzó a a2 ANTES de reproducir — paso 5): sigue a3.
        let pick = resolve_spot_audio(&r, Some("a2"));
        assert_eq!(pick.asset_id.as_deref(), Some("a3"));
        assert_eq!(pick.pending_failure, None);
    }

    #[test]
    fn opcion_a_si_el_default_tampoco_bajo_el_box_queda_mudo_y_se_reportan_las_dos_cosas() {
        // Peor caso de la opción A: no está el audio del turno NI el default. Ahí sí el box
        // queda mudo, y se reportan las dos cosas (qué audio faltó y que no sonó nada), igual
        // que Android.
        let a = vec![asset_bajado("b1"), asset_sin_bajar("b2")];
        let mut r = row(a, Some(SPOT_ROTATION_ROUND_ROBIN));
        r.file_path = String::new();

        let pick = resolve_spot_audio(&r, Some("b1"));
        assert_eq!(pick.cursor_asset_id.as_deref(), Some("b2"), "el cursor avanza igual");
        assert_eq!(pick.file_path, None, "no suena nada");
        assert_eq!(pick.asset_id, None);
        assert_eq!(pick.pending_failure, Some(FAIL_ASSET_NOT_CACHED), "el audio que faltó");
        assert_eq!(pick.failure_reason, Some(FAIL_DOWNLOAD), "y además el box quedó mudo");
    }

    #[test]
    fn opcion_a_el_throttle_de_fallos_distingue_un_asset_de_otro() {
        // Un box con varios audios sin bajar tiene que poder reportar CADA uno: si la clave
        // del throttle fuera sólo (schedule, motivo), el primero que cae en la ventana taparía
        // a los demás y soporte vería un solo audio roto de tres.
        let pick = SpotPick {
            schedule_id: "ss1".into(),
            spot_id: "sp1".into(),
            asset_id: None,
            cursor_asset_id: Some("a2".into()),
            file_path: None,
            failure_reason: None,
            pending_failure: Some(FAIL_ASSET_NOT_CACHED),
            expected_duration: 0.0,
        };
        let clave_a2 = format!("{}|{}|{}", pick.schedule_id, "a2", FAIL_ASSET_NOT_CACHED);
        let clave_a3 = format!("{}|{}|{}", pick.schedule_id, "a3", FAIL_ASSET_NOT_CACHED);
        assert_ne!(clave_a2, clave_a3);
        // Y el camino legacy (sin asset) mantiene la clave de siempre, sin asset en el medio.
        let clave_legacy = format!("{}|{}|{}", pick.schedule_id, "", FAIL_AUDIO_MISSING);
        assert!(clave_legacy.contains("ss1||audio_missing"));
    }

    // ── Paso 7: rotation desconocido ─────────────────────────────────────────

    #[test]
    fn paso7_rotation_desconocido_cae_al_audio_por_defecto() {
        let f = std::env::temp_dir().join("ms_rot_fallback.mp3");
        std::fs::write(&f, b"x").unwrap();
        let mut r = row(assets(&["a1", "a2"]), Some("WEIGHTED_V2_DEL_FUTURO"));
        r.file_path = f.to_string_lossy().to_string();

        let pick = resolve_spot_audio(&r, None);
        assert_eq!(pick.asset_id, None, "no se inventa una rotación que no se entiende");
        assert_eq!(pick.file_path.as_deref(), Some(r.file_path.as_str()));
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn paso7_assets_sin_rotation_tambien_cae_al_default() {
        let r = row(assets(&["a1", "a2"]), None);
        assert_eq!(resolve_spot_audio(&r, None).asset_id, None);
    }

    // ── Cursor que sobrevive un reinicio ─────────────────────────────────────

    #[test]
    fn el_cursor_sobrevive_un_reinicio() {
        // "Reinicio" = el proceso muere y el cursor se relee de SQLite. Se testea contra el
        // MISMO SQL que usa prod (`config_cache`), en una DB temporal.
        let dir = std::env::temp_dir().join("ms_cursor_restart_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("millsonic.db");

        let a = assets(&["a1", "a2", "a3"]);

        // Sesión 1: dos turnos.
        {
            let conn = db::test_support::open(&path);
            let mut cursor = db::test_support::get_spot_last_asset(&conn, "ss1");
            assert_eq!(cursor, None, "equipo nuevo = sin cursor");
            for esperado in ["a1", "a2"] {
                let elegido = pick_rotation_asset(&a, cursor.as_deref()).unwrap().id.clone();
                assert_eq!(elegido, esperado);
                db::test_support::set_spot_last_asset(&conn, "ss1", &elegido);
                cursor = Some(elegido);
            }
        }

        // Sesión 2: proceso nuevo, DB en disco. NO vuelve a a1.
        {
            let conn = db::test_support::open(&path);
            let cursor = db::test_support::get_spot_last_asset(&conn, "ss1");
            assert_eq!(cursor.as_deref(), Some("a2"), "el cursor se perdió en el reinicio");
            assert_eq!(pick_rotation_asset(&a, cursor.as_deref()).unwrap().id, "a3");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn la_cadencia_sobrevive_un_reinicio() {
        let dir = std::env::temp_dir().join("ms_cadencia_restart_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("millsonic.db");
        {
            let conn = db::test_support::open(&path);
            db::test_support::set_last_spot_at(&conn, 1_762_000_000);
            db::test_support::set_last_spot_schedule_id(&conn, "ss-7");
        }
        {
            let conn = db::test_support::open(&path);
            assert_eq!(db::test_support::get_last_spot_at(&conn), Some(1_762_000_000));
            assert_eq!(db::test_support::get_last_spot_schedule_id(&conn).as_deref(), Some("ss-7"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Cierre de la emisión ─────────────────────────────────────────────────

    #[test]
    fn clasificacion_del_cierre_de_emision() {
        // Sonó entero (duración conocida).
        assert_eq!(classify_spot_end(20.0, 20.0), (db::SpotOutcome::Completed, None));
        // 80% exacto sigue siendo completo (misma fórmula que los plays de música).
        assert_eq!(classify_spot_end(16.0, 20.0), (db::SpotOutcome::Completed, None));
        // mp3 truncado: el sink dijo "terminé" a los 3s de un audio de 20s.
        assert_eq!(
            classify_spot_end(3.0, 20.0),
            (db::SpotOutcome::Interrupted, Some(FAIL_SHORT))
        );
        // Duración desconocida (camino legacy): se cree al sink.
        assert_eq!(classify_spot_end(12.0, 0.0), (db::SpotOutcome::Completed, None));
        // Lo cortó el cap anti-freeze de 45s: el cliente NO escuchó el anuncio entero.
        assert_eq!(
            classify_spot_end(audio::MAX_SPOT_PLAYBACK_SECONDS, 0.0),
            (db::SpotOutcome::Interrupted, Some(FAIL_CAP))
        );
    }

    #[test]
    fn el_cap_de_45s_no_se_movio() {
        // Con ROTACIÓN suena UN audio por turno, así que el cap sigue siendo correcto. Si
        // alguien implementa tandas (N audios seguidos) este assert tiene que fallar y
        // obligar a pensarlo de nuevo — 4×20s se cortarían a los 45.
        assert_eq!(audio::MAX_SPOT_PLAYBACK_SECONDS, 45.0);
    }

    // ── Throttle de fallos ───────────────────────────────────────────────────

    #[test]
    fn el_throttle_de_fallos_deja_pasar_el_primero_y_frena_la_catarata() {
        let w = SPOT_FAILURE_THROTTLE_SECS;
        assert!(failure_throttle_allows(None, 1000, w), "el primero siempre pasa");
        assert!(!failure_throttle_allows(Some(1000), 1000 + w - 1, w), "dentro de la ventana, no");
        assert!(failure_throttle_allows(Some(1000), 1000 + w, w), "pasada la ventana, sí");
        // Reloj que se fue para atrás: mejor reportar de más que callarse para siempre.
        assert!(failure_throttle_allows(Some(1000), 500, w));
    }
}

#[cfg(test)]
mod two_advance_paths_tests {
    /// Este repo YA se comió el bug: el camino de crossfade no llamaba a `find_eligible_spot`,
    /// así que en toda zona con crossfade (el default de retail) los anuncios NO sonaban nunca.
    /// Son DOS caminos de avance entre temas (crossfade-path y finish-path) y lo que se inyecta
    /// entre temas tiene que estar en los DOS (memoria `player_two_advance_paths`).
    ///
    /// El test es estructural a propósito: `check_track_advancement` necesita un `AppHandle` de
    /// Tauri y una tarjeta de audio, así que no se puede ejecutar en un unit test. Lo que SÍ se
    /// puede garantizar mecánicamente es que no haya un segundo camino que se saltee la
    /// rotación, el cursor, la cadencia o el reporte.
    const SRC: &str = include_str!("sync.rs");

    /// Sólo el código de PRODUCCIÓN: se corta en el primer `cfg(test)` para que los literales
    /// de los propios tests no se cuenten a sí mismos.
    fn veces(aguja: &str) -> usize {
        SRC.split("#[cfg(test)]").next().unwrap_or("").matches(aguja).count()
    }

    #[test]
    fn los_dos_caminos_de_avance_disparan_el_spot_por_el_mismo_helper() {
        // 2 llamadas: una en el camino de crossfade, otra en el de finish.
        assert_eq!(
            veces("find_eligible_spot(&tz,"),
            2,
            "los anuncios tienen que evaluarse en los DOS caminos de avance"
        );
        assert_eq!(
            veces("start_spot_playback(&mut player,"),
            2,
            "los DOS caminos tienen que arrancar el spot por el mismo helper"
        );
    }

    #[test]
    fn nadie_reproduce_un_spot_por_fuera_del_helper() {
        // Un `play_spot_file` suelto se saltearía el cursor de rotación, la cadencia y el
        // reporte de emisión. Único lugar permitido: dentro de `start_spot_playback`.
        assert_eq!(
            veces("play_spot_file("),
            1,
            "hay un camino de reproducción de spots que no pasa por start_spot_playback"
        );
    }

    #[test]
    fn la_emision_se_cierra_en_un_solo_lugar() {
        // `close_on_air_spot` se llama en 2 lugares: el cierre normal (fin del spot, que es
        // común a los dos caminos) y el cierre forzado por supersede dentro del helper.
        assert_eq!(veces("close_on_air_spot("), 3, "definición + 2 llamadas");
    }
}
