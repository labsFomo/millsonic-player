# 🎵 Millsonic Player — Santo Grial

> **Documento vivo.** Preguntas y soluciones que SIEMPRE tenemos que tener mapeadas para un sistema de música retail de nivel regional (McDonald's, Pizza Hut, Gucci, LV, cervecerías con mucho SonicBox).
>
> Auditoría línea por línea del código del Desktop Player (Tauri/Rust) — `millsonic-player/src-tauri/src/`. Última auditoría: **2026-05-30** sobre la versión **0.7.0**.

---

## 0. Principios rectores (en orden de prioridad)

1. **La música del local NUNCA para.** Dead-air = falla grave. Todo lo demás es secundario.
2. **Si hay un error, debe ser IMPERCEPTIBLE.** El operario y el cliente no se tienen que enterar.
3. **NUNCA repetir canciones seguidas ni en períodos cortos.** La gente se enoja muchísimo. Lo mismo con anuncios.
4. **SonicBox es secundario a la grilla.** Si SonicBox falla, la grilla sigue como si nada.
5. **Consistencia de marca:** sucursales de la misma zona deberían sonar parecido a la misma hora.

> Regla de oro de diseño: **cualquier fallo (red, disco, CPU, API, SonicBox) debe degradar hacia "seguir tocando lo cacheado", nunca hacia silencio o glitch.**

---

## 1. Arquitectura en 30 segundos

- **Tauri 2 (Rust + WebView).** Audio por `rodio` (ALSA/Pulse/PipeWire).
- **Caché local** de tracks/spots en `~/.config/Millsonic/cache/` + SQLite `millsonic.db`.
- **Tasks tokio** (todas spawneadas en `main.rs:setup`):
  - `start_sync_loop` — cada **300s**: baja grilla + tracks, calcula posición.
  - loop de avance (`check_track_advancement`) — cada **1s**: decide qué suena.
  - `start_sonicbox_loop` — cada **3s**: vigía de votos (2 niveles).
  - `start_http_polling_loop` / `start_telemetry_loop` — telemetría + comandos (backoff).
  - watchdog — cada **30s**: si la posición no avanza >60s, fuerza el salto.
  - `start_update_loop` — cada **1h**: chequea updates.
- **WebSocket está deshabilitado** (`ws.rs:94` early return) — todo es HTTP polling.

---

## 2. Las 11 preguntas de Pablo (respondidas con código)

### Q1 — ¿Qué pasa si no hay internet? (música normal y SonicBox)

**Música normal: SIGUE SONANDO.** Flujo de degradación (`sync.rs:164-278`):
- Si **ya está sonando** algo → solo marca `ConnectionStatus::Offline` y sigue (`sync.rs:167-174`). Cero corte.
- Si no, intenta cargar la **grilla cacheada** de la hora actual desde SQLite (`load_cached_tracks_into_player`, `sync.rs:211-241`).
- Si no hay grilla para esa hora → **Emergency mode**: shuffle de TODO lo cacheado (`enter_emergency_mode`, `sync.rs:243-278`).

**SonicBox sin internet: se apaga limpio.** El vigía hace `fetch_zone_now_playing` con timeout 5s; si falla → `continue` (`sync.rs:734-740`). No bloquea, no glitchea, la grilla sigue. Los votos simplemente no entran hasta que vuelva la red.

⚠️ **GAP (P0): sin caché = silencio total.** Si el device se pareó pero **nunca completó un sync** y se queda sin internet, no hay nada cacheado → Emergency mode encuentra la lista vacía y **retorna sin tocar nada** (`sync.rs:263-265`). Ver §4 / RIESGO-01.

### Q2 — ¿Qué pasa si el reproductor se tranca?

**Hay watchdog y funciona** (`main.rs:428-486`): cada 30s compara la posición; si no avanzó en >60s (2 chequeos) y se supone que está tocando → `stop()` + `advance()` + `play_current()`. Se recupera solo saltando a la siguiente.
- Además, el loop de avance está envuelto en `catch_unwind` (`main.rs:431`), así que un panic en la lógica de audio **no mata la app**.
- El Mutex del audio se recupera de *poisoning* con `into_inner()` (`sync.rs:817-821`).

⚠️ **GAP (P1):** 60s es mucho para retail — son ~1 canción de dead-air/loop antes de recuperar. Y el watchdog **siempre salta a la siguiente** (no reintenta la misma); aceptable, pero conviene bajarlo a ~20-30s. Ver RIESGO-07.

### Q3 — ¿Y si no hay disco / la descarga llena el disco?

- Hay limpieza LRU reactiva (`db.rs:cleanup_cache`): si quedan <500MB libres, borra hasta 20 tracks por los menos usados (`last_played`).
- Si `download_track` falla por disco lleno (`api.rs:std::fs::write`) → error propagado → se **saltea** ese track y sigue (`sync.rs:486-495`). La música cacheada sigue.

⚠️ **GAPs (P0/P1):**
- **No hay tope absoluto de caché** → el disco se puede llenar al 100% antes de que el umbral de 500MB ayude (RIESGO-03).
- **Límite de 20 tracks por limpieza** (~80MB) puede no alcanzar si bajás 100MB de una (RIESGO-03).
- **Archivos parciales/corruptos no se auto-borran**: un write a mitad deja basura <1000 bytes que ensucia la caché (RIESGO-04).

### Q4 — ¿Y si CPU/RAM van al 100% y después bajan?

- No hay tareas que se acumulen sin control (los `tokio::spawn` son fijos, no dentro de loops). El polling es liviano.
- Bajo CPU saturado, `rodio` tiene buffer (~100ms): glitches <100ms se enmascaran; **>500ms se escuchan** pero se recupera (no queda trancado). El watchdog cubre el caso extremo.

⚠️ **GAP (P2):** no hay prioridad de proceso/hilo de audio ni `nice`/realtime scheduling. En una PC modesta saturada, puede haber micro-cortes audibles. Mitigación: pinear prioridad del proceso + hardware mínimo garantizado. Ver RIESGO-08.

### Q5 — ¿Y si hay un update mientras suena música?

- El chequeo es cada 1h, async, **no bloquea audio** (`updater.rs:5-19`).
- El update **NO se instala solo**: notifica al frontend → modal "Después / Actualizar" → el operario decide (`main.js:247-251`, `updater.rs:21-42`). Se puede posponer indefinido.
- Si el operario acepta → descarga + `app.restart()` (`updater.rs:93`).

⚠️ **GAPs (P1):** el `app.restart()` es **hard, sin graceful shutdown**: no hace `player.stop()` ni flush de reports → **corta la música a mitad de canción** y puede perder reports pendientes. Además, si la descarga del update corta a la mitad, no hay retry hasta +1h. Ver RIESGO-09.

### Q6 — ¿Y si una canción/anuncio queda en loop? (NO repetir — crítico)

**Estado actual:**
- La grilla usa **shuffle determinístico con seed** (`zoneId-fecha-horaSlot`, `sync.rs:51-61`): reproduce TODA la playlist antes de repetir → dentro de una pasada **no repite**.
- ⚠️ **NO existe mecanismo "no repetir las últimas N canciones"** entre pasadas ni protección para playlists chicas. Si la playlist tiene pocas canciones, o el índice se resetea, se repiten seguido.
- ⚠️ **Los spots NO tienen dedup** (`find_eligible_spot`, `sync.rs:629-676`): devuelve el primer spot elegible — el **mismo spot puede sonar repetido** si es el único elegible en su ventana. No hay `last_spot_id`.
- ⚠️ **SonicBox puede repetir** si el `play-report` no llega (ver Q8 / RIESGO-12) — es el riesgo de repetición más serio.
- ⚠️ El contador de spots (`tracks_since_last_spot`) **no se persiste**: tras reinicio vuelve a 0 → spots suenan más seguido de lo configurado.

Ver RIESGO-05, 06, 12 — esta es el área que más atención necesita.

### Q7 — ¿Y si nos quedamos sin grilla/canciones y estamos offline?

- Cae a Emergency mode (shuffle de todo lo cacheado, `sync.rs:243`).
- Si la caché tiene algo → sigue sonando. Si está vacía → **silencio** (RIESGO-01).
- Reportes y nuevas descargas se reintentan cuando vuelve la red (sync cada 300s + report flusher).

### Q8 — ¿Qué reportamos a la API, cuándo, y en qué parte de la grilla estamos?

- Al terminar cada track (si `position > 3s`) se guarda un `pending_report` en SQLite con `{trackId, zoneId, startedAt, durationSecs}` (`sync.rs:save_play_report`).
- Flush en batch a `/devices/:id/play-report-batch` cada **300s** (`sync.rs:125-136`); si falla, queda en DB y reintenta.
- SonicBox reporta aparte a `/player/play-report` con `completed/skipped` (cierra el loop del voto → `markPlayed`).

⚠️ **GAPs (P2):** el reporte **NO incluye en qué índice/offset de la timeline estaba** — el backend no sabe la posición en la grilla, solo el track. Y `completed/skipped` solo se mandan en el path SonicBox, no en el batch normal.

### Q9 — Al reiniciar, ¿arranca siempre con la misma canción o se guía por la API?

**Se guía por la API/tiempo — retoma la timeline, no arranca de cero.** Al sincronizar, el player calcula `elapsed = ahora - inicioSlot`, hace `looped = elapsed % duraciónTotal` y busca el track+segundo que corresponde (`sync.rs:516-549`). Setea `current_index` ahí. El backend hace el mismo cálculo (`zones.service.ts:getNowPlaying`).

⚠️ **GAPs (P1):**
- Entre el boot y el primer sync (hasta ~300s) **`current_index` arranca en 0** (`audio.rs:71`) → toca la primera canción "equivocada" hasta sincronizar.
- La posición **no se persiste localmente**: depende 100% del reloj del sistema. Reloj desfasado (DST, sin NTP) → arranca en la canción/segundo equivocado.
- El **seek dentro de la canción no se aplica** al arrancar el track (arranca el track desde 0, no desde el segundo calculado) — entre sucursales esto desincroniza. Ver RIESGO-10.

### Q10 — Dos sucursales de la misma radio/zona, ¿suenan lo mismo a la misma hora?

**En teoría sí**, porque el shuffle es determinístico con el mismo seed (`zoneId-fecha-horaSlot`) y ambos calculan el mismo `start_index` por tiempo. **En la práctica derivan** por:
- ⚠️ Cada player calcula la posición **una sola vez** (al sync) y después corre con su propio reloj — **sin re-sync durante la reproducción** → deriva acumulativa.
- ⚠️ El **seek no se aplica** (arrancan el track desde 0) → ya arrancan desfasados hasta varios segundos.
- ⚠️ Si los relojes no están en NTP, o las sucursales están en husos distintos, el seed/fecha puede diferir.
- ⚠️ La duración real del MP3 puede diferir ±0.5s de la declarada → deriva.

Conclusión honesta: **suenan la misma canción (aprox.), pero NO sincronizadas al segundo.** Para sincronía real haría falta re-sync periódico contra `syncTimestamp` del server. Ver RIESGO-10, 11.

### Q11 — ¿Cómo y cuándo suenan los anuncios sin internet?

- Los spots se **pre-descargan** en el último sync online a `cache/spots/` (`sync.rs:591-627`).
- Offline, `find_eligible_spot` solo considera spots **con archivo local existente** (`sync.rs:640`) → los cacheados siguen sonando por su ventana de día/hora/frecuencia.
- Spots no descargados se ignoran en silencio (sin glitch).

⚠️ **GAPs:** ver dedup de spots (Q6) y limpieza incremental de spots viejos (RIESGO-03).

---

## 3. Preguntas adicionales que DEBEMOS tener mapeadas

Generadas a partir del ejercicio — las que importan para retail de alto nivel:

**Continuidad de audio**
- ¿Qué pasa si el archivo de la canción actual se borra (limpieza LRU) mientras se está reproduciendo? → race posible (RIESGO-04).
- ¿Qué pasa si TODAS las canciones de la playlist están corruptas? → hoy **para del todo** y muestra "Error de reproducción" (`sync.rs:1006-1017`) en vez de caer a emergency shuffle. **(P0, RIESGO-02)**
- ¿Crossfade puede dejar silencio o doble audio si falla a mitad? → posible si `update_crossfade` no corre (RIESGO-13).
- ¿Qué pasa si el dispositivo de audio (ALSA/Pulse) desaparece en runtime (no al boot)? → hoy solo se detecta al boot (P0-2). En runtime no hay re-detección. **(P1)**

**No-repetición**
- ¿Hay "no repetir artista seguido", no solo canción? → no existe.
- ¿Un spot puede sonar 2 veces en pocos minutos al cruzar ventanas? → sí (RIESGO-06).
- ¿Al reiniciar, el shuffle del día es el mismo? → sí (mismo seed) — bueno para determinismo, pero si reinicia seguido toca el mismo arranque.

**Datos / estado**
- ¿La SQLite puede corromperse y matar la app? → **sí, `db.rs:12` panica** (RIESGO-14, P0).
- ¿`pending_reports` o la tabla `tracks` crecen sin límite? → bloat lento, no crítico (P2).
- ¿Qué pasa si `config.json` se corrompe? → cae a default → **pierde el pairing** (P1).
- ¿`hardware_id` persiste entre reinstalaciones? → hoy el campo existe pero **no se usa/persiste bien** → re-pairing podría duplicar device (P1, era P0-4 del plan).

**Red / API**
- ¿El vigía SonicBox spamea fetches en offline? → sí, sin backoff (~1 cada 3s) (RIESGO-15, P2).
- ¿Un comando (forcesync, etc.) puede ejecutarse dos veces por timeout? → posible doble-ACK (P2).
- ¿Cuándo vuelve internet, en cuánto se recupera? → hasta 300s (próximo sync) salvo forcesync.

**Reloj / sincronía**
- ¿El player valida que su reloj esté sincronizado (NTP)? → no. Reloj malo = grilla mal posicionada (RIESGO-11, P1).
- ¿Cambio de slot (mañana→tarde) corta la canción? → no, espera a que termine; pero puede saltar 1 track por el recálculo de índice (RIESGO-16, P2).

**Update / lifecycle**
- ¿Un update puede dejar la app sin arrancar (instalación corrupta)? → el plugin valida firma; sin retry automático (P1).
- ¿Tras update/reboot arranca solo? → sí, autostart se habilita al parear (P0-7 ✓).

**Seguridad / operación**
- ¿Qué pasa si alguien despareja por error? → requiere PIN (`unpair_device`).
- ¿El device_token (JWT) vence? → 365d. ¿Hay refresh? → **no** — a los 365 días deja de poder reportar/cerrar SonicBox. **(P1 a futuro)**

---

## 3.5. Estado de hardening — 0.7.0 (2026-05-30)

Fixes ya aplicados y **testeados** (unit tests + boot headless):

| ID | Qué se hizo | Test |
|----|-------------|------|
| ✅ **R-01** | Emergency vacío ya no se resigna: fuerza `trigger_sync()` para recuperar apenas llega contenido (`sync.rs:enter_emergency_mode`) | boot |
| ✅ **R-02** | Todas las pistas fallan → **ya no STOP**: reset skips + re-sync + emergency shuffle (`sync.rs:check_track_advancement`) | boot |
| ✅ **R-03** | Tope de caché real: mide el disco del caché (no el `sum` de todos), libera en batches hasta `CACHE_MIN_FREE_MB` (800MB) (`db.rs:cleanup_cache`) | — |
| ✅ **R-06** | Spots ya no se repiten back-to-back: rotación con `last_spot_id` (`sync.rs:find_eligible_spot`/`pick_spot`) | 2 unit |
| ✅ **R-07** | Watchdog recupera en ~30s (antes 60s) (`main.rs:429`) | boot |
| ✅ **R-12** | **SonicBox no repite**: guard local por ventana de N=4 canciones (salta aunque la API repita el id) + play-report con retry 3x (`sync.rs`) | 4 unit |
| ✅ **R-14** | SQLite corrupta ya no panica: borra+recrea (último recurso, in-memory) (`db.rs:open_or_recreate`) | 1 unit |
| ✅ **R-04** | Descarga atómica (`.part`→rename) + rechazo de payloads <2KB + barrido de parciales al boot (`api.rs:download_track`, `sync.rs:sweep_partial_downloads`) | boot |
| ✅ **R-09** | Update graceful: flush de reports + stop antes del restart (`updater.rs`, `sync.rs:flush_reports_before_exit`) | — |
| ✅ **R-17** | `config.json` atómico + backup `.bak`; recupera el pairing si el principal se corrompe (`config.rs`) | 1 unit |
| 🟡 **R-13** | Parcial: crossfade ya no dispara durante SonicBox (`sync.rs:893`) | — |

**Suite:** 11 unit tests verdes + boot headless 0.7.0 sin panic. **Resueltos: 10/18** (todos los P0 + R-04/06/07/09/17). **Pendientes: 7** → R-05 (mitigado por shuffle), R-08, R-15, R-16 (P2), y **R-10/R-11/R-18 que tocan backend** (sincronía multi-sucursal + refresh token).

---

## 4. Tabla maestra de riesgos (priorizada)

> Severidad por impacto en los principios: **P0 = puede causar silencio/repetición/crash** · **P1 = perceptible o desincroniza** · **P2 = bloat/eficiencia/edge raro**.

| ID | Sev | Riesgo | Dónde | Síntoma | Solución propuesta |
|----|-----|--------|-------|---------|--------------------|
| **R-01** | 🔴P0 | Pareado pero sin sync previo + offline = **silencio total** | `sync.rs:263-265` | Local mudo | Pre-cachear N tracks "de emergencia" en el pairing; nunca dejar emergency vacío |
| **R-02** | 🔴P0 | Si TODAS las pistas fallan → **STOP total** ("Error de reproducción") | `sync.rs:1006-1017` | Local mudo | En vez de `stop()`, caer a emergency shuffle / re-descargar / reintentar |
| **R-03** | 🔴P0 | Sin tope absoluto de caché → **disco al 100%** → descargas fallan | `db.rs:cleanup_cache` | Grilla incompleta → emergency | Tope duro (~80% disco / 8GB); subir LIMIT de limpieza; bajar umbral |
| **R-04** | 🟠P1 | Archivo parcial/corrupto no se borra; race con limpieza durante playback | `api.rs` write, `db.rs` LRU | Skip silencioso / posible corte | Validar bytes escritos == descargados; borrar <1KB al boot; no borrar el track en reproducción |
| **R-05** | 🟠P1 | **No hay anti-repetición** "últimas N" ni guard de playlist chica | `audio.rs:advance` | Repite canciones | `VecDeque` de últimas N track_ids; al avanzar, saltar las recientes |
| **R-06** | 🟠P1 | **Spots sin dedup** → mismo anuncio repetido | `sync.rs:find_eligible_spot` | Anuncio repetido (enoja) | `last_spot_id` + "no repetir spot X en Y minutos"; rotar elegibles |
| **R-07** | 🟠P1 | Watchdog tarda 60s en recuperar; no distingue pausa real | `main.rs:428-486` | Hasta 60s de loop/dead-air | Bajar a 20-30s; usar timestamp para distinguir pausa |
| **R-08** | 🟡P2 | CPU 100% → glitch audible >500ms | `rodio` | Micro-corte | Prioridad de proceso/hilo audio; hardware mínimo |
| **R-09** | 🟠P1 | `app.restart()` en update **corta música** + pierde reports | `updater.rs:93` | Corte abrupto | Graceful: esperar fin de canción / `stop()` + flush antes de restart; ventana horaria de update |
| **R-10** | 🟠P1 | Sucursales derivan: sin re-sync, **seek no se aplica** | `sync.rs:516-549` | No sincronizadas | Aplicar seek al arrancar el track; re-sync posición cada 1-2 min contra server |
| **R-11** | 🟠P1 | Reloj desfasado (sin NTP) → grilla mal posicionada | `sync.rs:521` | Canción equivocada | Validar/forzar NTP en pairing; usar `syncTimestamp` del server como fuente |
| **R-12** | 🔴P0 | **SonicBox se repite** si `play-report` falla (voto sigue ACTIVE) | `sync.rs:915-947` | Tema votado en loop hasta próximo sync | Encolar el play-report en DB con retry (no fire-and-forget); set local de votos ya tocados para ignorar re-stage |
| **R-13** | 🟡P2 | Crossfade + SonicBox simultáneos → se pierde el track B | `sync.rs:830-869` | Salto de track | No permitir SonicBox interrupt con `crossfade_active` |
| **R-14** | 🔴P0 | **SQLite corrupta → panic** (`db.rs:12 .expect`) | `db.rs:12` | App muere | Abrir con manejo de error; si corrupta, borrar+recrear (la DB es caché, se rearma del sync) |
| **R-15** | 🟡P2 | Vigía SonicBox spamea fetches sin backoff en offline | `sync.rs:687` | CPU/red desperdiciada | Backoff exponencial; consultar `ConnectionStatus` antes de pollear |
| **R-16** | 🟡P2 | Cambio de slot puede saltar 1 track por recálculo de índice | `sync.rs:440-549` | Salto puntual | Preservar posición relativa al cambiar de slot |
| **R-17** | 🟠P1 | `config.json` corrupto → pierde pairing | `config.rs:load` | Re-pairing manual | Backup `config.json.bak`; restaurar si el principal falla |
| **R-18** | 🟠P1 | `device_token` (JWT) vence a 365d, sin refresh | pairing | Deja de reportar/cerrar SonicBox | Endpoint de refresh de token antes del vencimiento |

---

## 5. Lo que YA está bien (no romper)

- ✅ Música sigue si ya está sonando y se cae la red (offline mode no interrumpe).
- ✅ Todos los `fetch` tienen timeout (5s API, 120s descarga) — nada cuelga indefinido.
- ✅ Watchdog real que recupera de cuelgues.
- ✅ `catch_unwind` en los loops de audio → un panic no mata la app.
- ✅ Recuperación de Mutex poisoning.
- ✅ SonicBox degrada limpio: si falla el fetch/descarga, la grilla sigue imperceptiblemente.
- ✅ `is_finished()` respeta la duración propia del tema SonicBox (no loopea por eso).
- ✅ Shuffle determinístico (base correcta para sincronía entre sucursales).
- ✅ Spots y grilla se cachean para offline.
- ✅ Autostart tras pairing (vuelve solo tras reboot).
- ✅ Logger no-panic (P0-3) + aviso de audio ausente (P0-2).

---

## 6. Roadmap de hardening sugerido

**Tanda P0 (antes de escalar a clientes grandes):**
- R-01 + R-02: garantizar que **nunca haya silencio** (emergency siempre con contenido, nunca `stop()` total).
- R-03 + R-14: disco y DB a prueba de balas (tope de caché, recuperación de SQLite).
- R-12: SonicBox idempotente — **nunca repetir un voto** (encolar report con retry + set de tocados).

**Tanda P1 (calidad percibida):**
- R-05 + R-06: anti-repetición de canciones y spots (lo que más enoja a la gente).
- R-07: watchdog más rápido.
- R-09: update sin cortar música.
- R-10 + R-11: sincronía entre sucursales (seek + re-sync + NTP).
- R-04, R-17, R-18: robustez de archivos/config/token.

**Tanda P2 (eficiencia / edge):**
- R-08, R-13, R-15, R-16: prioridad de audio, crossfade+SonicBox, backoff, cambio de slot.

---

## 7. Cómo validar cada uno (banco de pruebas)

Casos que el banco de QA del player debería cubrir siempre:
1. Parear → cortar internet ANTES del primer sync → ¿suena algo? (R-01)
2. Corromper 1 .mp3 cacheado → ¿salta sin glitch? Corromper todos → ¿cae a emergency, no a STOP? (R-02)
3. Llenar el disco al 95% → descargar grilla → ¿se mantiene tocando? (R-03)
4. Playlist de 3 canciones por 1h → ¿repite seguido? medir gaps entre repeticiones (R-05)
5. Spot único en ventana → ¿suena repetido? (R-06)
6. Votar SonicBox + bloquear `/player/play-report` → ¿el voto se repite? (R-12)
7. Borrar/corromper `millsonic.db` → ¿la app arranca o panica? (R-14)
8. Dos players misma zona/hora → medir desfase en segundos (R-10)
9. Reloj +1h → ¿arranca en la canción correcta? (R-11)
10. Disparar update mientras suena → ¿corta a mitad? (R-09)

---

*Mantener este doc actualizado en cada cambio del player. Cada bug nuevo de producción → entra acá como R-xx con su solución.*
