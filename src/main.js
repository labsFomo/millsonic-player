const { invoke } = window.__TAURI__.core;

let isPaired = false;
let isPlaying = false;

async function pairDevice() {
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
      errorEl.textContent = '';
    } else {
      errorEl.textContent = result.message || 'Código inválido o expirado';
    }
  } catch (e) {
    errorEl.textContent = e.toString();
  }
}

async function togglePlay() {
  try {
    const state = await invoke('toggle_playback');
    isPlaying = state === 'playing';
    document.getElementById('play-icon').textContent = isPlaying ? '⏸' : '▶';
  } catch (e) {
    console.error('Toggle error:', e);
  }
}

async function setVolume(val) {
  document.getElementById('volume-label').textContent = val + '%';
  try {
    await invoke('set_volume', { volume: parseInt(val) });
  } catch (e) {
    console.error('Volume error:', e);
  }
}

async function init() {
  try {
    const status = await invoke('get_status');
    console.log('Player status:', status);
  } catch (e) {
    console.error('Init error:', e);
  }
}

document.getElementById('pairing-code')?.addEventListener('keydown', (e) => {
  if (e.key === 'Enter') pairDevice();
});

init();
