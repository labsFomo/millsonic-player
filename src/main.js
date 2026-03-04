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
  const code = document.getElementById('pairing-code').value.trim().toUpperCase();
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
      document.getElementById('player-screen').classList.remove('hidden');
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

    // Update artwork if available
    if (data.artworkUrl) {
      document.getElementById('artwork').src = data.artworkUrl;
    }

    // Ensure play icon shows pause when playing
    if (!isPlaying) {
      isPlaying = true;
      document.getElementById('play-icon').textContent = '⏸';
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
      document.getElementById('player-screen').classList.remove('hidden');

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

    await setupListeners();
  } catch (e) {
    console.error('Init error:', e);
  }
}

document.getElementById('pairing-code')?.addEventListener('keydown', (e) => {
  if (e.key === 'Enter') pairDevice();
});

// Wait for Tauri IPC bridge to be ready
function waitForTauri(retries = 50) {
  if (window.__TAURI__) {
    init();
  } else if (retries > 0) {
    setTimeout(() => waitForTauri(retries - 1), 100);
  } else {
    document.getElementById('pair-error').textContent = 'Tauri API no disponible (timeout)';
  }
}
waitForTauri();
