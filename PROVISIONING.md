# Provisión de un box de local (PC de reproducción) — checklist de confiabilidad

Una PC de local **se apaga de noche y debe volver a sonar sola a la mañana, y recuperarse sola si el player crashea**. Para eso el box necesita TRES mecanismos (los 3 verificados en la PC QA):

## 1. Auto-login del SO (lo configura la imagen del box, NO el player)
LightDM con auto-login del usuario, sin password:
```
# /etc/lightdm/lightdm.conf.d/50-autologin.conf
[Seat:*]
autologin-user=<usuario>
autologin-user-timeout=0
```
Sin esto, al bootear queda en la pantalla de login y el autostart NUNCA dispara. **Crítico.**

## 2. Autostart del player en el login (lo hace el player solo)
Al parear, el player llama `tauri-plugin-autostart` → crea `~/.config/autostart/millsonic-player.desktop` apuntando a la ruta estable del AppImage (`~/MillsonicPlayer.AppImage`). Sobrevive auto-updates (el updater reemplaza el archivo in-place). **Ya funciona, verificado:** boot → auto-login → autostart → player abre → (paired) resume sin OK humano.

## 3. Supervisor de crash-restart (lo agrega la provisión)
El autostart cubre el **boot**, NO el **crash**. Si el player muere (crash nativo/segfault), nada lo relanza hasta el próximo reboot → queda mudo horas. Opciones (en orden de preferencia):

**A) systemd --user (recomendado):** `~/.config/systemd/user/millsonic-player.service`
```ini
[Unit]
Description=Millsonic Player (auto-restart)
[Service]
Environment=DISPLAY=:0
Environment=XAUTHORITY=/home/<usuario>/.Xauthority
Environment=WEBKIT_DISABLE_DMABUF_RENDERER=1
Environment=RUST_BACKTRACE=full
ExecStart=/home/<usuario>/MillsonicPlayer.AppImage
Restart=always
RestartSec=5
[Install]
WantedBy=default.target
```
`systemctl --user enable --now millsonic-player.service`. (Si se usa esto, deshabilitar el autostart .desktop para no doble-lanzar; el single-instance del player igual evita el dup.)

**B) cron watchdog (lo que corre HOY en la QA):** un script cada minuto que relanza si no hay proceso. Simple y suficiente (`~/watchdog.sh` + `* * * * * ~/watchdog.sh`). Loguea cada relanzamiento con timestamp.

## Diagnóstico de crashes
- Coredumps: `/proc/sys/kernel/core_pattern=/tmp/core.%e.%p` (gdb para backtrace).
- `RUST_BACKTRACE=full` + stderr a archivo (captura panics de Rust).
- El player detecta shutdown sucio (un `running.marker`) y reporta `lastCrashAt`/`lastCrashClass` en la telemetría (visible en Health del admin) al próximo arranque — **caza incluso crashes nativos**, no solo panics.
