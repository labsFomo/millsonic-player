# Millsonic Player

Reproductor offline de música para locales comerciales. Se conecta al servidor Millsonic, descarga la grilla y música, y reproduce sin interrupciones.

## Stack
- **Tauri 2** — Rust backend + WebView nativo
- **rodio** — Audio engine nativo Rust
- **rusqlite** — SQLite para cache offline
- **sysinfo** — Telemetría del sistema

## Desarrollo
```bash
npm install
npm run tauri dev
```

## Build (local, SOLO para desarrollo / prueba descartable)
```bash
npm run tauri build
```
> ⚠️ Un build local **no está firmado** y **no se distribuye**. Para sacar una versión a los
> devices reales NO alcanza con buildear local y copiar el binario — hay que hacer el **release
> firmado por CI**. Ver **[`RELEASE.md`](./RELEASE.md)**.

## Release / CI/CD — ver [`RELEASE.md`](./RELEASE.md)
Proceso único válido: commit a `main` → bump versión (`src-tauri/Cargo.toml` + `tauri.conf.json`)
→ `git tag vX.Y.Z && git push origin vX.Y.Z` → CI (`.github/workflows/build.yml`) buildea y
**firma con minisign** (Win/Linux/macOS) → GitHub Release con AppImage + `.sig` → actualizar
`latest.json` en EC2 → los devices se **auto-actualizan** (verifican firma contra `updater.pubkey`).
Detalle completo y checklist en **[`RELEASE.md`](./RELEASE.md)**.
