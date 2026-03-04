let isPaired = false;
let isPlaying = false;

function getInvoke() {
  return window.__TAURI__?.core?.invoke || window.__TAURI__?.tauri?.invoke;
}
function getListen() {
  return window.__TAURI__?.event?.listen;
}

// --- Pairing ---
async function pairDevice() {
  const invoke = getInvoke();
  if (!invoke) { document.getElementById('pair-error').textContent = 'Tauri API no disponible'; return; }
  const code = document.getElementById('pairing-code').value.trim().toUpperCase().replace(/\s/g, '');
  const errorEl = document.getElementById('pair-error');

  if (code.length !== 6) {
    errorEl.textContent = 'El código debe tener 6 caracteres';
    return;
  }

  try {
    const result = await invoke('pair_device', { code });
    if (result.deviceToken) {
      isPaired = true;
      document.getElementById('pairing-screen').classList.add('hidden');
      document.getElementById('loading-overlay').classList.remove('hidden');
      if (result.zone && result.zone.name) {
        document.getElementById('zone-name').textContent = result.zone.name;
      }
      errorEl.textContent = '';
    } else {
      errorEl.textContent = result.message || 'Código inválido o expirado';
    }
  } catch (e) {
    errorEl.textContent = e.toString();
  }
}

// --- Unpair ---
function unpairDevice() {
  // Show password modal
  let modal = document.getElementById('unpair-modal');
  if (modal) modal.remove();
  modal = document.createElement('div');
  modal.id = 'unpair-modal';
  modal.style.cssText = 'position:fixed;top:0;left:0;right:0;bottom:0;background:rgba(0,0,0,0.8);z-index:1000;display:flex;align-items:center;justify-content:center;';
  modal.innerHTML = `
    <div style="background:#111827;border-radius:16px;padding:24px;width:280px;text-align:center;">
      <p style="font-size:14px;font-weight:600;margin-bottom:4px;">Desvincular dispositivo</p>
      <p style="font-size:12px;color:#94A3B8;margin-bottom:16px;">Ingresá el instrumento de seguridad</p>
      <input type="text" id="unpair-pass" placeholder="Ej: PIANO" autocomplete="off" style="width:100%;padding:10px;text-align:center;font-size:16px;background:#0A1628;border:1px solid #2A2F3E;border-radius:10px;color:white;outline:none;text-transform:uppercase;">
      <p id="unpair-error" style="color:#EF4444;font-size:12px;min-height:18px;margin-top:6px;"></p>
      <div style="display:flex;gap:8px;margin-top:12px;">
        <button onclick="document.getElementById('unpair-modal').remove()" style="flex:1;padding:10px;background:#1E293B;border:none;border-radius:10px;color:#94A3B8;cursor:pointer;font-size:13px;">Cancelar</button>
        <button onclick="confirmUnpair()" style="flex:1;padding:10px;background:#EF4444;border:none;border-radius:10px;color:white;cursor:pointer;font-size:13px;font-weight:600;">Desvincular</button>
      </div>
    </div>`;
  document.body.appendChild(modal);
  setTimeout(() => document.getElementById('unpair-pass').focus(), 100);
  document.getElementById('unpair-pass').addEventListener('keydown', (e) => { if (e.key === 'Enter') confirmUnpair(); });
}

async function confirmUnpair() {
  const pass = document.getElementById('unpair-pass').value.trim();
  if (!pass) return;
  const invoke = getInvoke();
  if (!invoke) return;
  try {
    await invoke('unpair_device', { pin: pass });
    document.getElementById('unpair-modal')?.remove();
    isPaired = false;
    isPlaying = false;
    document.getElementById('player-screen').classList.add('hidden');
    document.getElementById('loading-overlay').classList.add('hidden');
    document.getElementById('pairing-screen').classList.remove('hidden');
    document.getElementById('pairing-code').value = '';
    document.getElementById('play-icon').textContent = '▶';
    document.getElementById('track-title').textContent = '—';
    document.getElementById('track-artist').textContent = '—';
  } catch (e) {
    if (String(e).includes('PIN_MISMATCH')) {
      document.getElementById('unpair-error').textContent = 'Instrumento incorrecto';
      document.getElementById('unpair-pass').value = '';
      return;
    }
    console.error('Unpair error:', e);
  }
}

// --- Playback Controls ---
async function togglePlay() {
  const invoke = getInvoke();
  if (!invoke) return;
  try {
    const state = await invoke('toggle_playback');
    isPlaying = state === 'playing';
    document.getElementById('play-icon').textContent = isPlaying ? '⏸' : '▶';
  } catch (e) {
    console.error('Toggle error:', e);
  }
}

async function setVolume(val) {
  const invoke = getInvoke();
  document.getElementById('volume-label').textContent = val + '%';
  if (!invoke) return;
  try {
    await invoke('set_volume', { volume: parseInt(val) });
  } catch (e) {
    console.error('Volume error:', e);
  }
}

// --- Time Formatting ---
function formatTime(seconds) {
  if (!seconds || seconds < 0) return '0:00';
  const m = Math.floor(seconds / 60);
  const s = Math.floor(seconds % 60);
  return `${m}:${s.toString().padStart(2, '0')}`;
}

// --- Event Listeners ---
async function setupListeners() {
  const listen = getListen();
  if (!listen) { console.warn('Tauri event API not available'); return; }
  await listen('now-playing', (event) => {
    const data = event.payload;
    if (!data) return;

    document.getElementById('track-title').textContent = data.title || '—';
    document.getElementById('track-artist').textContent = data.artist || '—';
    document.getElementById('time-current').textContent = formatTime(data.position);
    document.getElementById('time-total').textContent = formatTime(data.duration);

    // Update progress bar
    if (data.duration > 0) {
      const pct = Math.min((data.position / data.duration) * 100, 100);
      document.getElementById('progress').style.width = pct + '%';
    }

    // Update artwork if available, fallback to default
    const artworkEl = document.getElementById('artwork');
    if (data.artworkUrl) {
      artworkEl.src = data.artworkUrl;
      artworkEl.onerror = () => { artworkEl.src = 'assets/default-artwork.svg'; };
    } else {
      artworkEl.src = 'assets/default-artwork.svg';
    }

    // Ensure play icon shows pause when playing
    if (!isPlaying) {
      isPlaying = true;
      document.getElementById('play-icon').textContent = '⏸';
    }
  });

  await listen('sync-progress', (event) => {
    const data = event.payload;
    if (!data) return;
    const overlay = document.getElementById('loading-overlay');
    const loadingText = document.getElementById('loading-text');
    const loadingBar = document.getElementById('loading-bar');
    const loadingDetail = document.getElementById('loading-detail');

    if (data.phase === 'downloading') {
      overlay.classList.remove('hidden');
      if (data.current === 0) {
        loadingText.textContent = 'Sincronizando tu música...';
      } else {
        loadingText.textContent = `Descargando canciones (${data.current}/${data.total})...`;
        loadingDetail.textContent = data.trackName || '';
      }
      loadingBar.style.width = data.percent + '%';
    } else if (data.phase === 'ready') {
      loadingText.textContent = '¡Listo! 🎵';
      loadingBar.style.width = '100%';
      setTimeout(() => {
        overlay.classList.add('hidden');
        document.getElementById('player-screen').classList.remove('hidden');
      }, 600);
    }
  });

  await listen('connection-status', (event) => {
    const data = event.payload;
    if (!data) return;
    const dot = document.getElementById('status-dot');
    const text = document.getElementById('status-text');
    if (data.status === 'online') {
      dot.className = 'dot green';
      text.textContent = 'En línea';
    } else if (data.status === 'offline') {
      dot.className = 'dot yellow';
      text.textContent = 'Sin conexión (caché)';
    } else if (data.status === 'emergency') {
      dot.className = 'dot red';
      text.textContent = 'Modo emergencia';
    }
  });

  await listen('status-change', (event) => {
    const data = event.payload;
    if (data && data.playing !== undefined) {
      isPlaying = data.playing;
      document.getElementById('play-icon').textContent = isPlaying ? '⏸' : '▶';
    }
  });
}

// --- Init ---
async function init() {
  const invoke = getInvoke();
  if (!invoke) { console.error('Tauri API not loaded yet'); return; }
  try {
    const status = await invoke('get_status');
    console.log('Player status:', status);

    if (status.paired) {
      isPaired = true;
      document.getElementById('pairing-screen').classList.add('hidden');
      // If already playing, show player; otherwise show loading overlay
      if (status.playing) {
        document.getElementById('loading-overlay').classList.add('hidden');
        document.getElementById('player-screen').classList.remove('hidden');
      } else {
        document.getElementById('loading-overlay').classList.remove('hidden');
        document.getElementById('player-screen').classList.add('hidden');
      }

      if (status.zoneName) {
        document.getElementById('zone-name').textContent = status.zoneName;
      }
      if (status.volume) {
        document.getElementById('volume').value = status.volume;
        document.getElementById('volume-label').textContent = status.volume + '%';
      }
      if (status.playing) {
        isPlaying = true;
        document.getElementById('play-icon').textContent = '⏸';
      }
      if (status.track) {
        document.getElementById('track-title').textContent = status.track;
      }
      if (status.artist) {
        document.getElementById('track-artist').textContent = status.artist;
      }
    }

    // Set initial connection status
    if (status.connectionStatus) {
      const dot = document.getElementById('status-dot');
      const text = document.getElementById('status-text');
      if (status.connectionStatus === 'online') {
        dot.className = 'dot green'; text.textContent = 'En línea';
      } else if (status.connectionStatus === 'offline') {
        dot.className = 'dot yellow'; text.textContent = 'Sin conexión (caché)';
      } else if (status.connectionStatus === 'emergency') {
        dot.className = 'dot red'; text.textContent = 'Modo emergencia';
      }
    }

    if (!status.paired) {
      // Not paired: hide loading overlay, show pairing screen
      document.getElementById('loading-overlay').classList.add('hidden');
    }

    await setupListeners();
  } catch (e) {
    console.error('Init error:', e);
  }
}

// --- Log Viewer ---
async function showLogs() {
  const invoke = getInvoke();
  if (!invoke) return;
  try {
    const logs = await invoke('get_logs');
    let modal = document.getElementById('log-modal');
    if (!modal) {
      modal = document.createElement('div');
      modal.id = 'log-modal';
      modal.style.cssText = 'position:fixed;top:0;left:0;right:0;bottom:0;background:rgba(0,0,0,0.85);z-index:1000;display:flex;flex-direction:column;padding:16px;';
      modal.innerHTML = '<div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:8px;"><span style="color:#fff;font-weight:bold;">📋 Logs</span><button onclick="document.getElementById(\'log-modal\').remove()" style="background:none;border:none;color:#fff;font-size:20px;cursor:pointer;">✕</button></div><pre id="log-content" style="flex:1;overflow:auto;background:#111;color:#0f0;padding:12px;border-radius:8px;font-size:11px;white-space:pre-wrap;word-break:break-all;margin:0;"></pre>';
      document.body.appendChild(modal);
    }
    document.getElementById('log-content').textContent = logs;
  } catch (e) {
    console.error('Logs error:', e);
  }
}

document.getElementById('pairing-code')?.addEventListener('keydown', (e) => {
  if (e.key === 'Enter') pairDevice();
});

// Auto-strip spaces and uppercase on pairing code input/paste
document.getElementById('pairing-code')?.addEventListener('input', (e) => {
  const el = e.target;
  const pos = el.selectionStart;
  const cleaned = el.value.replace(/\s/g, '').toUpperCase();
  if (el.value !== cleaned) {
    el.value = cleaned;
    el.selectionStart = el.selectionEnd = Math.min(pos, cleaned.length);
  }
});

// Wait for Tauri IPC bridge to be ready
function waitForTauri(retries = 50) {
  if (window.__TAURI__) {
    // Set version from Tauri
    try {
      const version = window.__TAURI__?.app?.getVersion;
      if (version) version().then(v => { const el = document.getElementById('app-version'); if (el) el.textContent = 'v' + v; });
    } catch(e) {}
    init();
  } else if (retries > 0) {
    setTimeout(() => waitForTauri(retries - 1), 100);
  } else {
    document.getElementById('pair-error').textContent = 'Tauri API no disponible (timeout)';
  }
}
waitForTauri();
