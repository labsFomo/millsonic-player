# Millsonic Player — Upgrades / Backlog

> **Documento vivo.** Acá anotamos TODO lo que hay que mejorar/arreglar en el player,
> con detalle, para no olvidarnos. Cada ítem: qué es, por qué importa, comportamiento
> actual, solución propuesta, dónde tocar en el código, prioridad y estado.
>
> Complementa a `PLAYER_SANTO_GRIAL.md` (auditoría + mapa de riesgos). Acá va lo
> **accionable pendiente**. Cada vez que aparezca algo nuevo, se agrega abajo.

**Leyenda de estado:** 🔴 pendiente · 🟡 en progreso · ✅ **LISTO** (implementado, compila/tests) · ✅✅ **PROBADO** (validado en la PC real)
**Prioridad:** P0 (crítico) · P1 (calidad percibida) · P2 (eficiencia/edge)

> **Seguimiento:** cada update se marca **✅ LISTO** al implementarlo, y **✅✅ PROBADO**
> recién cuando se verifica en hardware real. Así sabemos siempre qué está hecho vs validado.

**Versión actual del player:** 0.8.1
**Última actualización:** 2026-05-30

---

## 🆕 Hallazgos de la prueba en hardware real (PC Ubuntu, 2026-05-30)

Probamos el `.deb` 0.7.0 en una PC Xubuntu real (Ubuntu 24.04, GPU Intel). Funcionó:
instalación, pairing, **audio sonando (confirmado por Pablo)**, UI, y **SonicBox E2E
completo** (voto → entra al terminar la canción → toca entero → `play-report` cierra el
loop → grilla retoma sin loopear). En esa prueba salieron estos upgrades:

### U1 — Descarga progresiva (no bajar toda la playlist antes de tocar)  ·  ✅✅ PROBADO · P1
**Implementado en 0.8.1** (`sync.rs:do_sync` — baja solo el track de arranque, reproduce,
y baja el resto en background en orden adelante del playhead; fallback al primer cacheado si
el de arranque no está, nunca silencio). **Probado en la PC real (2026-05-30):** arrancó a
sonar de inmediato + `Background download of remaining tracks complete`. ✓

**Qué pasa hoy:** al sincronizar, el player **descarga las 30 canciones completas de la
playlist ANTES de reproducir la primera**. Muestra una pantalla "Descargando canciones
(N/30)…" y recién cuando termina TODO, arranca el audio.
- Con 30 tracks son ~90 s de espera. Con una playlist de **100+ tracks serían varios
  minutos de silencio** en cada apertura/sync.
- Peor para resiliencia: si la red es lenta o se corta a mitad del lote, **no suena nada
  hasta que el lote entero termine** → viola el principio "música lo antes posible".

**Por qué importa:** un local abriendo no puede esperar minutos en silencio; y una red
inestable no debería impedir que suene lo que ya está cacheado.

**Solución propuesta (lo que pidió Pablo):**
1. Calcular la posición de la grilla (ya se hace) y **descargar PRIMERO la canción que
   debe sonar AHORA** → arrancar a reproducir en segundos.
2. Lanzar un **task en background** que descarga el resto, en orden "adelante del
   playhead" (la próxima, la siguiente, etc.), sin bloquear la reproducción.
3. Spots y SonicBox **ya** son background (SonicBox pre-descarga vía el vigía; los spots
   se cachean aparte) — no hay que tocarlos.
4. Si la canción del playhead aún no bajó al momento de tocar, caer a la primera que SÍ
   esté cacheada / emergency, nunca silencio.

**Dónde tocar:** `src-tauri/src/sync.rs` → `do_sync()`, el loop de descarga de tracks
(la parte que hace `api::download_track` para cada track antes de `set_playlist` +
`play_current`). Hay que reordenar: bajar la del índice de arranque → play → spawnear la
descarga del resto.

**Cómo verificar:** con una playlist grande, el log debe mostrar `play_file:` **antes** de
que terminen todas las descargas; medir el tiempo desde "Sync" hasta el primer `Sink
created` (debe ser segundos, no el total del lote).

---

### U2 — Render de la WebView/UI en GPU Intel (blanco transitorio + "Conectando" lento)  ·  ✅✅ PROBADO · P1
**Implementado en 0.8.1** (`main.rs` setea `WEBKIT_DISABLE_DMABUF_RENDERER=1` en Linux antes
de crear la WebView). **Probado en la PC real (Intel GPU, 2026-05-30):** la UI renderiza
limpia (carátula + título + controles), sin pantalla en blanco. ✓ (El delay de "Conectando…"
queda como refinamiento menor de la transición de UI, ver U7/`main.js`.)

**Qué pasa hoy:** en la PC de prueba (GPU Intel), al lanzar el player aparecen warnings de
GPU en stderr:
```
libEGL warning: egl: failed to create dri2 screen
MESA: error: ZINK: failed to choose pdev
```
Síntomas observados:
- La pantalla de **pairing renderiza bien**.
- Durante la **descarga pesada inicial**, la ventana quedó unos segundos **en blanco**
  (transitorio).
- Tras conectar, la UI mostró **"Conectando…" por ~30 s** antes de pasar a la vista de
  reproducción (carátula/título). Después renderizó **perfecto** (carátula + título +
  artista + controles).

**Por qué importa:** en un retail, el operario mira la pantalla. Un blanco prolongado o un
"Conectando…" pegado da sensación de "está roto" aunque el audio suene. En GPUs Intel/viejas
el problema puede ser peor o permanente.

**Solución propuesta (a investigar/validar):**
1. Probar lanzar con variables de entorno de WebKitGTK que evitan el path de GPU roto:
   `WEBKIT_DISABLE_DMABUF_RENDERER=1` y/o `WEBKIT_DISABLE_COMPOSITING_MODE=1`
   (y como último recurso `LIBGL_ALWAYS_SOFTWARE=1`). **Nota:** en la prueba intenté
   lanzarlo con esas vars por SSH pero tuve problemas de *detach* del proceso (no del
   render) — **queda pendiente probarlas en serio** (idealmente desde la propia sesión de
   escritorio o baked en el wrapper de lanzamiento).
2. Si una env var lo estabiliza, **hornearla en el `.desktop`/wrapper de arranque** del
   `.deb` para que aplique siempre, sin depender del operario.
3. Investigar el **delay de ~30 s en "Conectando…"**: ver por qué el front tarda en pasar
   a la vista de reproducción aunque el audio ya suene. Posible: el front espera un evento
   de `connection-status`/WS que tarda; convendría que muestre el now-playing apenas hay
   `now-playing` emitido (el backend Rust lo emite cada 1 s mientras toca).

**Dónde tocar:** empaquetado/launch (`src-tauri/tauri.conf.json`, el `.desktop` generado,
o un wrapper) para las env vars; `src/main.js` para la lógica de transición de la UI.

**Cómo verificar:** relanzar con las env vars y confirmar (screenshot) que la UI nunca
queda en blanco y pasa a now-playing en pocos segundos. Probar en la GPU Intel real.

---

### U7 — Manejo y presentación de errores (nunca mostrar un error crudo en blanco)  ·  ✅ LISTO · P1
**Implementado en 0.8.1** (`main.js`: error boundary global — `window.error` →
`showFatalError()` con pantalla branded, código `E-UI-002`, botón Reintentar + auto-reload a
los 15s; aclara "la música sigue sonando"; `unhandledrejection` solo loguea para no tapar la
UI por hiccups transitorios). Deployado y la UI se ve sana (sin error crudo). **Falta PROBAR:**
forzar un error real para ver la pantalla branded en vivo. **Pendiente (sub-ítem):** detección
Rust-side de fallo de carga de la WebView (pantalla en blanco) con auto-restart — más robusto
que solo el boundary JS.

**Qué pasa hoy:** ante una falla, la WebView puede mostrar un **error crudo de browser sobre
fondo blanco**, ej:
```
Could not connect to localhost: Connection refused
```
(se vio en la PC real). Es **horrible para retail** — parece roto y no le dice nada útil al
operario. No hay ningún manejo de error con diseño propio.

> **Causa del caso puntual visto:** un binario construido con `cargo build` directo (sin
> embeber el frontend de producción) apuntó al `devUrl` (`localhost:1420`). Eso fue un error
> de build/deploy nuestro (se arregla construyendo con `tauri build`/`npm run build`, ver nota
> de build abajo). **Pero independientemente de la causa, el player nunca debería exponer un
> error así.**

**Comportamiento esperado (lo que pidió Pablo):** ningún error crudo. Ante una falla, el
player debe seguir **nuestro diseño** y hacer una de estas, según gravedad:
- **Seguir / reintentar** en silencio si es transitorio (ej: sin red → modo offline, ya
  existe a nivel audio; la UI debe mostrar un estado branded "sin conexión", no un error).
- **Auto-reiniciar la app** si la UI/WebView quedó en mal estado (pantalla en blanco, assets
  no cargaron).
- **Pantalla de error branded** con **código de error** + "Contactá a soporte" (+ reintentar
  / reiniciar), con el logo de Millsonic — nunca el error de browser pelado.

**Solución propuesta:**
1. **Rust:** detectar fallo de carga del WebView / pantalla en blanco (evento de load del
   webview, o un watchdog que verifica que el front respondió un "ready"). Si falla →
   reintentar cargar / `app.restart()` / mostrar un diálogo nativo branded con código.
2. **Frontend (`main.js`):** error boundary global (capturar errores JS + `unhandledrejection`)
   que renderiza una **vista de error con diseño** (logo + mensaje claro + `ERROR CODE` +
   acción), en vez de dejar el error crudo.
3. **Esquema de códigos de error** (ej: `E-NET-001` conexión, `E-UI-002` carga de UI,
   `E-AUD-003` audio) para que soporte sepa qué pasó de un vistazo.
4. Toda pantalla de "estado feo" (sin programación, sin audio, error de reproducción) pasa a
   tener el mismo lenguaje visual branded.

**Dónde tocar:** `src/main.js` (error boundary + componente de error branded),
`src/index.html`/`styles.css` (estilo), `src-tauri/src/main.rs` (detección de WebView caído +
auto-restart + diálogo nativo de fallback).

**Cómo verificar:** forzar fallas (sin red al boot, frontend que no carga, audio ausente) y
confirmar que **siempre** se ve una pantalla branded con mensaje claro + código, nunca un
error crudo ni un blanco.

---

### 🛠️ Nota de build/deploy (para no repetir el error de hoy)
**SIEMPRE construir el player con `npm run build` (= `tauri build`), NUNCA con `cargo build`
directo para distribuir.** `cargo build` no embebe el frontend de producción y el binario
apunta al `devUrl` (`localhost:1420`) → pantalla "Could not connect to localhost". El `.deb`
correcto sale de `npm run build -- --bundles deb`. (`cargo build`/`cargo test` solo sirven
para validar compilación y correr tests de Rust, no para deployar.)

---

### U6 — Cambios de canciones en una playlist existente no se aplican hasta reiniciar  ·  ✅✅ PROBADO · P1
**Implementado** (fingerprint de la lista de tracks en `sync.rs`; el early-return de "misma
playlist" ahora requiere mismo id **y** mismo contenido). **Probado en la PC real
(2026-05-30):** se quitó 1 track de la playlist activa (30→29) + force-sync → el player logueó
`Same playlist id but TRACK LIST CHANGED — reloading playlist (U6)` y recargó. ✓

**Qué pasa hoy:** cuando el player sincroniza y la **playlist activa tiene el mismo
`playlist_id`** que la que ya está cargada, ejecuta el path de "misma playlist"
(`sync.rs:557-560`): solo llama `refresh_track_cache()` (descarga archivos faltantes) y
**retorna early**. **NUNCA recarga la lista de tracks en memoria** (`set_playlist`).
Resultado: si editás las canciones de una playlist (agregar / quitar / reordenar) **sin
cambiar el playlist_id**, el player **no toma los cambios** hasta que: cambie el slot, cambie
el playlist_id, o se reinicie el player.

**Por qué importa:** un operador/marca que actualiza las canciones de su playlist espera que
suene la lista nueva en minutos, no recién al próximo cambio de slot o reinicio.

**Detección:** confirmado en código — `refresh_track_cache()` (`sync.rs:684`) solo baja MP3s,
no actualiza `player.playlist` ni la grilla en DB. La propagación (force-sync ~60s) llega,
pero el player la descarta por el early-return de "same playlist".

**Solución propuesta:** en el path de "misma playlist", detectar si la **lista de tracks
cambió** (comparar set de `trackId` / un hash de la lista vs la cargada). Si cambió:
- Re-descargar lo que falte (ya se hace) **y reconstruir `player.playlist`** con la lista
  nueva, recalculando la posición (ver U5) **sin cortar la canción actual** — aplicar al
  terminar el track en curso para que el cambio sea imperceptible.
- Si no cambió, mantener el comportamiento actual (solo cache).

**Dónde tocar:** `src-tauri/src/sync.rs` → bloque `same_playlist` (~551-561) y
`refresh_track_cache` (~684); `audio.rs` para un update de playlist que preserve la
reproducción en curso.

**Cómo verificar:** con el player sonando, editar las canciones de la playlist activa en
admin → en ~60s el player debe reflejar la lista nueva (al terminar la canción actual),
sin reiniciar.

> Relacionado: el cambio de **playlist distinta** sí se aplica, pero **corta la canción
> actual** (arranca el track nuevo de una). Mejora menor: esperar el fin del track en curso
> también en ese caso para que ningún cambio de grilla se escuche como un corte abrupto.

---

### U5 — Arrancar en la canción y SEGUNDO correctos (usar el now-playing del server, como el web)  ·  ✅✅ PROBADO · P1
**Implementado en 0.8.1** (`sync.rs:do_sync` usa `currentTrack.id` + `seekPosition` del
now-playing del server; fallback time-based local offline. Nuevo `audio::play_file_at(seek)`
que saltea al segundo correcto). **Probado en la PC real (2026-05-30):** `Start from server
now-playing: track 17 of 30, seek 181s` → reprodujo desde el segundo 181, alineado con el web
player. Se acabó el "siempre la misma canción desde 0". ✓

**Qué pasa hoy:** al sincronizar (online), el player calcula **localmente** qué track toca
por horario (`elapsed % duración_total`) y reproduce ese track **desde el segundo 0**. Dos
problemas:
1. **No aplica el seek dentro de la canción.** Elige el track correcto pero arranca desde 0,
   no desde el segundo que corresponde. → Reiniciar dentro de una misma canción la repite
   **desde el principio** cada vez (esto fue lo que notó Pablo).
2. **Recalcula local en vez de usar la fuente autoritativa del server.** El **web player**
   usa `GET /zones/:id/now-playing`, que ya devuelve la respuesta exacta. Confirmado, el
   endpoint trae:
   ```json
   currentTrack: { "id": "...", "title": "...", "seekPosition": 240 }
   syncTimestamp, totalTimelineDuration
   ```
   El cálculo local del Desktop **ignora `seekPosition`** y además **no contempla los spots**
   del timeline (el server interleava spots con `startAt`; el player solo suma tracks) →
   **deriva** respecto del web player y de otras sucursales.

**Por qué importa:** consistencia con el web player y entre sucursales (misma marca, misma
hora → misma canción Y mismo segundo). Y que reiniciar no replantee la canción desde 0.

**Solución propuesta (online):**
1. Al arrancar/sincronizar, llamar a `GET /zones/:id/now-playing` (ya tenemos
   `api::fetch_zone_now_playing`, lo agregué para SonicBox) y leer `currentTrack.id` +
   `seekPosition`.
2. Posicionar la playlist en ese track **y aplicar el seek** a `seekPosition` segundos al
   arrancar el audio (rodio: `Sink::try_seek(Duration)` o `Source::skip_duration` antes de
   `append`; para MP3 el seek puede ser aproximado — validar).
3. (Idealmente, más adelante) **re-sincronizar periódicamente** contra `syncTimestamp` para
   no derivar durante la reproducción larga — esto se cruza con **R-10**.
4. **Offline:** mantener el cálculo local actual como fallback (tema aparte, OK).

**Dónde tocar:** `src-tauri/src/sync.rs` → `do_sync()` (el bloque `start_index` en
~628-661): en vez de (o además de) calcular el índice local, consumir `currentTrack` +
`seekPosition` del now-playing cuando hay red. `src-tauri/src/audio.rs` → soporte de seek al
iniciar un track (nuevo método tipo `play_file_at(track, seek_secs)`).

**Cómo verificar:** reiniciar el player varias veces dentro de una misma canción → debe
retomar en el **segundo correcto** (no desde 0), alineado con lo que muestra el web player
para esa zona en ese momento.

> Relacionado con **R-10/R-11** (sincronía multi-sucursal + reloj/NTP). U5 es el primer paso
> concreto y client-side de eso.

---

### U4 — Instancia única (no permitir 2 players abiertos en la misma sesión)  ·  ✅✅ PROBADO · P1
**Implementado en 0.8.0-dev** (`tauri-plugin-single-instance` en `main.rs`, registrado
primero). **Probado en la PC real (2026-05-30):** lanzar el player 2 veces → la 2da rebota
(`[WARN] Second instance launch ignored — focusing existing window`), queda 1 sola
instancia, sin audio superpuesto. ✓

**Qué pasa hoy:** el player **no controla instancia única**. Se puede lanzar el ejecutable
varias veces y quedan **N players corriendo a la vez en la misma sesión**. En la prueba real
quedaron **5 instancias simultáneas** escribiendo al mismo log y, potencialmente,
**superponiendo audio** (cacofonía) y duplicando CPU/RAM/descargas.

**Por qué importa:** en un local, un operario que hace doble-click en el icono, o un
autostart que se combina con un lanzamiento manual, no debe terminar con 2+ players sonando
encima. Es un problema grande para retail.

**Comportamiento esperado (lo que pidió Pablo):** si el player **ya está abierto** en la
sesión, un segundo lanzamiento **no debe arrancar otro** — debe **enfocar/levantar la
ventana existente** y/o avisar "Millsonic Player ya está abierto", y salir.

**Solución propuesta:** integrar **`tauri-plugin-single-instance`** (plugin oficial de
Tauri). Al detectar una instancia previa, el segundo proceso le pasa el foco a la ventana
existente y se cierra solo. Opcional: mostrar un toast/diálogo "ya está abierto".
- Como refuerzo, considerar un **lockfile** (ej. `~/.config/Millsonic/player.lock` con el
  PID) por si el plugin no cubre algún edge.

**Dónde tocar:** `src-tauri/Cargo.toml` (dependencia del plugin) + `src-tauri/src/main.rs`
(registrar el plugin con el callback que enfoca la ventana) + permisos/capabilities de Tauri
si aplica.

**Cómo verificar:** con el player abierto, lanzar `millsonic-player` de nuevo → no debe
aparecer un segundo proceso; la ventana existente se levanta y/o avisa. `pgrep` debe seguir
mostrando **1** sola instancia.

> **Nota:** este gap fue justamente lo que me complicó la prueba por SSH (quedaron 5
> instancias por mis re-lanzamientos). Con single-instance, eso es imposible.

---

### U3 — (nota, no bug) Automatización del input de pairing por GUI  ·  ℹ️ informativo
El campo de código de pairing es un input de **6 cajas** dentro de la WebView. Tipearlo por
coordenadas con `xdotool` es frágil (el click no siempre cae en el input). **No es un
problema del producto** — para QA automatizada conviene **parear headless** (generar código
+ `POST /devices/pair` + escribir `config.json`), que es lo que usamos y es confiable. Se
deja anotado para el harness de QA, no para el player.

---

## 🔧 Hardening pendiente (del Santo Grial, todavía sin hacer)

Estos ya están mapeados en `PLAYER_SANTO_GRIAL.md`. Se listan acá para tener UN solo backlog.

### Client-side
- **R-05 — Anti-repetición de grilla "últimas N"**  ·  🔴 P2 (bajo riesgo real)
  El shuffle determinístico ya evita repetir dentro de una pasada; el riesgo solo aparece
  con playlists muy chicas. Cuidado: un "skip de últimas N" client-side **rompería el
  determinismo** que sincroniza sucursales (ver R-10). Probablemente se resuelve mejor por
  config (no asignar playlists diminutas) + un guard de "no tocar el MISMO track dos veces
  seguidas" (sync-safe porque nunca se daría en escenarios sincronizados).
- **R-08 — Prioridad del proceso/hilo de audio**  ·  🔴 P2
  Bajo CPU 100% puede haber micro-cortes >500 ms. Mitigar con prioridad de proceso (`nice`/
  scheduling) y hardware mínimo garantizado.
- **R-13 — Crossfade + SonicBox**  ·  🟡 P2 (parcial)
  Ya se evita disparar crossfade durante SonicBox (`sync.rs`). Falta revisar el caso
  crossfade activo + interrupción de spot/voto para no perder un track. Verificar.
- **R-15 — Backoff del vigía SonicBox en offline**  ·  🔴 P2
  El vigía pollea cada ~3 s aunque no haya red. Agregar backoff exponencial / consultar
  `ConnectionStatus` antes de pollear, para no gastar CPU/red.
- **R-16 — Salto de track al cambiar de slot de grilla**  ·  🔴 P2
  Al cambiar de slot (ej: Mañana→Tarde) el recálculo de índice puede saltar 1 track.
  Preservar la posición relativa al cambiar de slot.

### Requieren cambios de backend
- **R-10 — Sincronía multi-sucursal**  ·  🔴 P1
  Dos sucursales de la misma zona suenan ~la misma canción pero **no sincronizadas al
  segundo**: el player no aplica el *seek* (arranca el track desde 0) y no se re-sincroniza
  durante la reproducción. Solución: aplicar el seek calculado al arrancar el track + re-
  sync periódico contra el `syncTimestamp` del server. (Toca backend para exponer el
  timestamp de forma consistente.)
- **R-11 — Reloj sin NTP**  ·  🔴 P1
  La posición de la grilla depende 100% del reloj del sistema. Reloj desfasado (sin NTP,
  DST) → arranca en la canción/segundo equivocado. Validar/forzar NTP en el pairing o usar
  el `syncTimestamp` del server como fuente de verdad.
- **R-18 — Refresh del token de device**  ·  🔴 P1
  El `deviceToken` (JWT) vence a 365 días sin mecanismo de refresh → a los 365 días el
  device deja de poder reportar/cerrar SonicBox. Agregar endpoint + lógica de refresh antes
  del vencimiento.

---

## ✅ Ya resuelto en 0.7.0 (referencia)

Cerrados y testeados (detalle + file:line en `PLAYER_SANTO_GRIAL.md`):
R-01, R-02 (nunca silencio) · R-03, R-14 (disco/DB a prueba de balas) ·
R-04 (descargas atómicas) · R-06 (anti-repeat de spots) · R-07 (watchdog ~30s) ·
R-09 (update graceful) · R-12 (**SonicBox anti-loop** — validado en HW real) ·
R-17 (config con backup). Más: SonicBox Desktop CAMBIO 1-4, P0-2/3/7.

---

## Estado al 2026-05-30 (0.8.2, probado en la PC real)
**Hechos y probados:** U1, U2, U4, U5, U6 ✅✅ · U7 ✅ LISTO · **R-10, R-11 ✅✅ PROBADO**
(player 0.8.2) · **R-18 ✅✅ endpoint probado** (backend deployado; lógica player wired).
Todos los **P0** + U1-U7 + R-10/11/18 cerrados.
- **R-11:** offset de reloj del server (`syncTimestamp`) usado en seed del shuffle + spots.
- **R-10:** re-sync cada 180s, re-alinea solo si quedó en canción equivocada (rate-limit 5min).
  Probado en la PC: 0 re-aligns espurios, playback fluido.
- **R-18:** `POST /devices/:id/refresh-token` (backend, deployado) + player refresca con <30
  días. Endpoint probado por curl (token nuevo 365d que autentica).
  ⚠️ Backend R-18 **deployado a prod pero sin commitear** (junto al resto del working tree).

## Próxima tanda sugerida (lo que queda)
1. **U7 sub-ítem** — forzar un error para validar la pantalla branded en vivo + detección
   Rust-side de WebView en blanco con auto-restart.
2. **R-10 refinamiento** — adoptar el timeline del server (spots por offset) para sync al
   segundo perfecto entre sucursales (hoy alinea a nivel canción, no segundo, durante la
   reproducción larga).
3. **P2 client-side** — R-05 (no-repeat grilla), R-08 (prioridad audio), R-13 (crossfade+SB),
   R-15 (backoff vigía offline), R-16 (salto al cambiar slot).
4. **Refinamiento U2** — el delay de "Conectando…": mostrar now-playing apenas hay evento,
   no esperar el de conexión.
