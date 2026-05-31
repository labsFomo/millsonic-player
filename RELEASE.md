# Millsonic Player — Proceso de Release (ÚNICO VÁLIDO)

> ⚠️ **REGLA DE ORO: todo build de producción se hace en CI (GitHub Actions), firmado, y se
> distribuye por auto-update. NUNCA se buildea local y se copia el binario a un device.**
>
> Un `cargo build` / `tauri build` local produce un binario **sin firmar** que:
> - no está versionado en git (no hay tag, no hay release),
> - **no se puede instalar por auto-update** (el updater de Tauri exige firma minisign válida
>   contra la `pubkey` embebida en `tauri.conf.json`; un binario local no tiene `.sig`),
> - rompe la trazabilidad (nadie sabe qué código corre en el device).
>
> El build local **sólo** sirve como prueba funcional descartable (correr el binario en una PC
> de QA por SSH). Si hiciste eso, **no terminaste**: hay que hacer el release firmado de verdad
> y dejar que el device se auto-actualice a la versión firmada.

---

## TL;DR — sacar una versión nueva del player desktop

```bash
# 1. Cambios de código, commiteados a main
git add -A && git commit -m "feat(player): ..."

# 2. Bump de versión en LOS DOS archivos (deben coincidir)
#    - src-tauri/Cargo.toml      -> version = "X.Y.Z"   (esto es lo que el player reporta como appVersion via CARGO_PKG_VERSION)
#    - src-tauri/tauri.conf.json -> "version": "X.Y.Z"

git commit -am "chore: bump vX.Y.Z"
git push origin main

# 3. Tag vX.Y.Z y push del tag -> dispara el workflow build.yml
git tag vX.Y.Z
git push origin vX.Y.Z

# 4. Esperar a que CI termine (build + sign + GitHub Release). ~10-20 min.
#    Verás un Release nuevo en github.com/labsFomo/millsonic-player/releases/tag/vX.Y.Z
#    con: Millsonic.Player_X.Y.Z_amd64.AppImage  +  ...AppImage.sig

# 5. Actualizar latest.json en EC2 (PASO MANUAL — ver abajo). CI lo intenta por scp
#    pero sólo funciona si el secret EC2_SSH_KEY está seteado (hoy NO lo está).

# 6. Los devices se auto-actualizan solos (chequean cada hora; bajan el AppImage firmado,
#    verifican la firma, hacen restart graceful R-09).
```

---

## Las piezas

### Versión — dos archivos que deben coincidir
- `src-tauri/Cargo.toml` → `version = "X.Y.Z"`. **Esto es lo que el player reporta como `appVersion`**
  en telemetría (via `env!("CARGO_PKG_VERSION")`). Si esto no coincide con lo que muestra
  `Settings > About`, el device no está corriendo lo que creés.
- `src-tauri/tauri.conf.json` → `"version": "X.Y.Z"`. Lo que el updater compara.

### CI — `.github/workflows/build.yml`
- **Trigger:** push de un tag `v*` (o `workflow_dispatch` manual).
- **Matrix:** Windows (msvc), Linux (ubuntu-22.04), macOS (universal).
- **macOS falla soft** (`continue-on-error`) — su firma/notarización es inestable y **no debe
  bloquear** el release de Linux/Windows (los retail boxes son Linux).
- **Firma (minisign):** el build usa los secrets `TAURI_SIGNING_PRIVATE_KEY` +
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. La **clave pública** está embebida en
  `tauri.conf.json` → `updater.pubkey`. El updater del device verifica el AppImage contra esa
  pubkey: si la firma no valida, **no instala**. Por eso un binario local sin `.sig` jamás se
  auto-instala.
- **Artefactos Linux:** `*.deb` (instalación fresca) + `*.AppImage` + `*.AppImage.sig` (auto-update).
- **Job `release`** (corre con `if: always() && tag v*` para que el fallo soft de macOS no lo
  saltee):
  1. Crea el GitHub Release con todos los artefactos.
  2. Genera `latest.json` (lee las `.sig` de cada plataforma).
  3. Intenta `scp latest.json` a `ubuntu@44.213.88.75:/home/ubuntu/millsonic/update-releases/latest.json`
     **sólo si existe el secret `EC2_SSH_KEY`**. Si no existe (estado actual), imprime el
     `latest.json` y dice "update manually".

### El endpoint de update (backend)
- `GET https://apifo.millsonic.com/api/v1/devices/update/:target/:arch/:currentVersion`
  - Handler: `src/devices/devices.controller.ts` → `src/devices/update-manifest.ts`.
  - Lee `/app/update-releases/latest.json` dentro del container, que es un **bind-mount** de
    `/home/ubuntu/millsonic/update-releases/latest.json` en el host EC2 (modo rw). O sea:
    editar ese archivo en el host = cambiar lo que sirve el API, sin rebuild del backend.
  - Compara semver: devuelve update **sólo si `currentVersion < latest.json.version`**
    (`hasReachedVersion`). Misma versión o mayor → 204/no-update.
  - Android usa el mismo endpoint con `target=android` → lee `update-releases/android/latest.json`
    (canales, rollout %, sha256).

### El updater del device (`src-tauri/src/updater.rs`)
- Chequea 30s después de bootear, y luego cada **3600s** (1h).
- Si hay versión nueva: emite `update-available` a la UI. Al confirmar (o auto), baja el AppImage
  firmado desde la URL del GitHub Release, **verifica la firma minisign**, y antes de reiniciar
  hace flush de play-reports + corta audio limpio (**R-09 graceful**), luego `app.restart()`.
- **Sólo funciona con el AppImage** (el updater se auto-reemplaza el ejecutable corriendo). Un
  `.deb` instalado en `/usr/bin` necesitaría root para reemplazarse → los retail boxes deben
  correr el **AppImage** para tener auto-update.

### Paso manual: actualizar `latest.json` en EC2
Mientras `EC2_SSH_KEY` no esté en los secrets del repo, después de que CI publique el Release:

```bash
# Opción A: bajar el latest.json que CI ya generó (está en los logs del job 'release') y scpearlo
scp latest.json ubuntu@44.213.88.75:/home/ubuntu/millsonic/update-releases/latest.json

# Opción B: armarlo a mano. La 'signature' es el CONTENIDO del archivo .AppImage.sig del Release.
#   url -> https://github.com/labsFomo/millsonic-player/releases/download/vX.Y.Z/Millsonic.Player_X.Y.Z_amd64.AppImage
# Verificar que quedó:
curl -s "https://apifo.millsonic.com/api/v1/devices/update/linux/x86_64/0.0.1" | head
```
No requiere restart del backend (es un archivo bind-montado).

---

## Checklist de "release bien hecho"
- [ ] Cambios commiteados a `main`.
- [ ] `Cargo.toml` y `tauri.conf.json` con la **misma** versión nueva.
- [ ] Tag `vX.Y.Z` pusheado.
- [ ] CI verde (al menos el job Linux + el job `release`).
- [ ] GitHub Release con `*_amd64.AppImage` **y** `*_amd64.AppImage.sig`.
- [ ] `latest.json` en EC2 apuntando a la versión nueva (verificado con curl al endpoint).
- [ ] Un device viejo se auto-actualizó (verificar `appVersion` en `/devices` del admin).

## Anti-checklist (lo que NO es un release)
- ❌ `tauri build` local + `scp` del binario a un device.
- ❌ Bumpear versión sin tag (CI no dispara).
- ❌ Publicar el Release pero olvidarse del `latest.json` en EC2 (nadie se entera de la versión).
- ❌ Subir sólo el `.deb` (no se auto-actualiza; el updater necesita el AppImage + `.sig`).
