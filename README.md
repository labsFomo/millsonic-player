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

## Build
```bash
npm run tauri build
```

## CI/CD
Push a tag `v*` para trigger build automático en GitHub Actions para Windows, Mac y Linux.
