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

**Versión actual del player:** 0.7.0
**Última actualización:** 2026-05-30

---

## 🆕 Hallazgos de la prueba en hardware real (PC Ubuntu, 2026-05-30)

Probamos el `.deb` 0.7.0 en una PC Xubuntu real (Ubuntu 24.04, GPU Intel). Funcionó:
instalación, pairing, **audio sonando (confirmado por Pablo)**, UI, y **SonicBox E2E
completo** (voto → entra al terminar la canción → toca entero → `play-report` cierra el
loop → grilla retoma sin loopear). En esa prueba salieron estos upgrades:

### U1 — Descarga progresiva (no bajar toda la playlist antes de tocar)  ·  🔴 P1
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

### U2 — Render de la WebView/UI en GPU Intel (blanco transitorio + "Conectando" lento)  ·  🔴 P1
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

### U6 — Cambios de canciones en una playlist existente no se aplican hasta reiniciar  ·  🔴 P1
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

### U5 — Arrancar en la canción y SEGUNDO correctos (usar el now-playing del server, como el web)  ·  🔴 P1
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

## Próxima tanda sugerida
1. **U4 (instancia única)** — chico y de alto impacto; evita audio superpuesto y doble-launch.
2. **U5 (arrancar en canción + segundo correctos)** — alinea con el web player; arregla el
   "siempre arranca la misma canción desde 0".
3. **U6 (cambios de canciones en playlist se apliquen sin reiniciar)** — propaga edits de
   contenido en ~60s, no al próximo slot/reinicio.
4. **U1 (descarga progresiva)** — el que más mueve la aguja para "música ASAP".
5. **U2 (render WebView)** — para que la pantalla nunca se vea rota en GPUs Intel.
6. **R-10/R-11/R-18** (backend) — sincronía multi-sucursal + token, cuando ataquemos backend.
7. P2 client-side (R-05/08/13/15/16) cuando haya espacio.
