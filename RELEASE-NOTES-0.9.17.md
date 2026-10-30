# 0.9.17 — Reporte de emisión de anuncios + boxes multi-audio

> Notas de release listas para usar como mensaje de commit. **Sin taggear**: el release lo
> hace Pablo (ver `RELEASE.md`, el proceso no cambia).
> Fases 4 y 5 de `millsonic/ANUNCIOS-REPORTE-Y-BOXES.md`. El backend ya está deployado y vivo.

```
feat(spots): reporte de emisión de anuncios + rotación multi-audio del box (0.9.17)

El desktop era el agujero del pedido comercial: no existía NADA del lado del reporte
(la Fase 4 del handoff decía "acá no existe nada") y un box con varios audios hubiera
pisado el mismo archivo de cache. Son ~90 de los ~120 equipos de la flota.

TAREA A — reportar las emisiones

1. `pending_reports` no tenía tipo de media. Columnas nuevas (media_type, spot_id,
   schedule_id, asset_id, outcome, failure_reason) con migración tolerante — el MISMO
   patrón `ADD COLUMN` + error ignorado que 0.9.16 usó para `completed`, nunca un
   rebuild de tabla. Una fila ya encolada sobrevive y se sigue mandando como play de
   música. `track_id NOT NULL` y `duration_secs NOT NULL` no se relajan (SQLite exige
   reescribir la tabla entera para eso): las filas de spot guardan '' y -1 como "no
   aplica" y el serializador OMITE esos campos del JSON.

2. `find_eligible_spot` devolvía sólo el path, así que no se sabía QUÉ spot sonó. Ahora
   devuelve el `SpotPick` completo (spotId, scheduleId, assetId, duración esperada).

3. Los fallos eran `log::error!` y nada más. Ahora se encolan como `outcome:'FAILED'`
   con un motivo útil (`audio_missing` / `download_failed` / `audio_device_error`), que
   es justo el caso "el anuncio no suena y nadie se entera". Clave: `load_spot_schedules`
   ya NO descarta el box cuyo audio no está en disco — ese `continue` silencioso era el
   agujero. Throttle de 30 min por (box, motivo) para que un box roto no encole un FAILED
   cada 3 minutos para siempre.

4. La cadencia vivía en memoria y se perdía en cada reinicio: un equipo que se apaga a la
   noche volvía con el baseline en cero y, con cadencia de 60 min, podía no emitir nunca.
   `spot_last_played_at` y `spot_last_schedule_id` van a `config_cache` (tabla que ya
   existe en los equipos instalados: cero migración). Timestamp futuro = reloj roto → se
   rebasea a ahora en vez de quedar trabado.

5. BUG DE CADENCIA: el turno se consumía al ELEGIR el spot, antes de que sonara, así que
   un box roto le quemaba el turno a los demás. La cadencia por minutos ahora se consume
   DESPUÉS del `play_spot_file` exitoso. El anti-repeat ENTRE boxes sigue consumiéndose al
   elegir, a propósito: si no, un box roto primero en la lista sería el elegido en todos
   los turnos y los demás no sonarían nunca (inanición).

TAREA B — rotación ROUND_ROBIN del box

Contrato v1 implementado al pie de la letra (docblock de `src/spots/spot-assets.util.ts`
del backend; Android implementa EL MISMO). Pasos 1-7: lista del server tal como vino (no
se reordena, no se baraja, no se filtra ni los audios que no bajaron), `(i+1) % n` con
anti-repeat garantizado para n>=2, cursor `lastAssetId` persistido por `SpotSchedule.id`
y ANTES de reproducir, `rotation` desconocido → audio por defecto sin inventar.
El cache de audios pasa a ser por asset (`cache/spots/assets/{assetId}.mp3`): la ruta
vieja era una sola por box y con varios audios se pisaba. Se purgan los assets que
salieron de la grilla.

**El cap anti-freeze de 45s por spot NO se tocó** (audio.rs:8): con rotación suena UN
audio por turno, así que sigue siendo correcto. Hay un test que lo congela en 45.0 para
que una implementación de tanda tenga que fallar y pensarlo de nuevo.

De paso (necesario, no oportunista)

`play_spot_file` prendía `playing_spot` arriba de todo y tres caminos de error salían sin
apagarlo (stat, open y decode). Con el flag colgado el crossfade queda desactivado y el
siguiente fin de tema se interpreta como "terminó un spot" → avanza de más. Antes era casi
inalcanzable; ahora que los boxes sin archivo SÍ se evalúan (para poder reportar el fallo)
el camino se usa de verdad. Wrapper que garantiza la invariante.

En el camino de crossfade, el play del TEMA se reportaba ANTES de intentar el spot y, si el
spot fallaba, el fall-through al crossfade lo reportaba OTRA VEZ: fila duplicada en
PlayHistory. Ahora se reporta una sola vez.

Lo que NO se tocó

El camino de música es intocable: una fila de track serializa byte-idéntico a 0.9.16 (hay
un test que compara el JSON exacto y cuenta los campos). Un batch sin filas de spot viaja
igual que hoy. El cap de 45s, el crossfade, el seeded shuffle, el weighted interleave, el
camino SonicBox y el updater quedaron como estaban.

50 tests, 0 fallos (baseline 16/0 — 34 nuevos: contrato de rotación completo, serializado
del reporte con retrocompat, migración de la tabla vieja y guarda estructural de los dos
caminos de avance).

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
```
