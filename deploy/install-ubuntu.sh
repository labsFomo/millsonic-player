#!/usr/bin/env bash
# Millsonic Player — instalador para PC Ubuntu (retail box).
# Uso:  sudo bash install-ubuntu.sh ./millsonic-player_0.7.0_amd64.deb
#
# Qué hace:
#   1. Instala el .deb resolviendo dependencias (libwebkit2gtk-4.1, libgtk-3,
#      libappindicator3-1 — esta última vive en 'universe').
#   2. Verifica que las libs de runtime queden resueltas.
#   3. Deja una nota sobre audio (PipeWire/PulseAudio debe estar activo en la
#      sesión del usuario que corre el player).
set -euo pipefail

DEB="${1:-}"
if [[ -z "$DEB" || ! -f "$DEB" ]]; then
  echo "ERROR: pasá la ruta al .deb. Ej: sudo bash install-ubuntu.sh ./millsonic-player_0.7.0_amd64.deb" >&2
  exit 1
fi

if [[ $EUID -ne 0 ]]; then
  echo "ERROR: corré con sudo." >&2
  exit 1
fi

echo "==> Habilitando repo 'universe' (libappindicator3-1)..."
add-apt-repository -y universe 2>/dev/null || true
apt-get update -y

echo "==> Instalando $DEB (apt resuelve dependencias)..."
# 'apt install ./archivo.deb' resuelve deps automáticamente, a diferencia de dpkg -i
apt-get install -y "$DEB"

echo "==> Verificando linkeo del binario..."
BIN=/usr/bin/millsonic-player
if ldd "$BIN" | grep -i "not found"; then
  echo "ATENCIÓN: hay libs sin resolver arriba. Corré: sudo apt install -f -y" >&2
else
  echo "OK — todas las libs resuelven."
fi

echo
echo "==> Listo. Próximos pasos manuales:"
echo "   1. Abrir 'Millsonic Player' (o correr: millsonic-player)."
echo "   2. Ingresar el código de pairing de la zona."
echo "   3. Al parear, el autostart queda habilitado solo (vuelve tras reboot)."
echo
echo "   Audio: la sesión del usuario debe tener PipeWire/PulseAudio activo."
echo "   Si el player muestra el banner rojo 'No se detectó dispositivo de audio',"
echo "   verificá:  systemctl --user status pipewire  (o pulseaudio)  y  'aplay -l'."
echo
echo "   Logs del player:  ~/.config/Millsonic/millsonic.log"
