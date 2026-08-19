// VibeToText dashboard renderer (Tauri port of history-app/renderer.js).
//
// PHASE 1 CONTRACT: every former Node/Electron coupling point goes through the
// global `vtt` shim from api.js (a thin wrapper over Tauri invoke/listen).
//   - better-sqlite3 loadHistory()      -> await vtt.getEntries()
//   - fs config read                    -> vtt.loadConfig()
//   - navigator.mediaDevices mic list   -> await vtt.listAudioDevices()
//   - config custom_dictionary CRUD     -> vtt.getDictionary/addWord/removeWord
//   - chokidar 'history-updated' (ipc)  -> vtt.onHistoryUpdated(() => render())
// The restart-engine / restart.sh path is gone: this is a single process and
// settings hot-apply, so there is nothing to restart.
//
// There is intentionally NO require(), better-sqlite3, fs, os, child_process,
// or ipcRenderer in this file.

// Stopwords to filter from common words
const STOPWORDS = new Set([
  'a', 'an', 'the', 'and', 'or', 'but', 'in', 'on', 'at', 'to', 'for',
  'of', 'with', 'by', 'from', 'as', 'is', 'was', 'are', 'were', 'been',
  'be', 'have', 'has', 'had', 'do', 'does', 'did', 'will', 'would',
  'could', 'should', 'may', 'might', 'must', 'shall', 'can', 'need',
  'i', 'you', 'he', 'she', 'it', 'we', 'they', 'me', 'him', 'her',
  'my', 'your', 'his', 'its', 'our', 'their', 'this', 'that', 'these',
  'what', 'which', 'who', 'where', 'when', 'why', 'how', 'all', 'each',
  'some', 'no', 'not', 'only', 'so', 'than', 'too', 'very', 'just',
  'also', 'now', 'here', 'there', 'then', 'if', 'because', 'about',
  'any', 'up', 'down', 'out', 'off', 'over', 'going', 'gonna', 'like',
  'okay', 'ok', 'yeah', 'yes', 'um', 'uh', 'ah', 'oh', 'well', 'right',
  'actually', 'basically', 'really', 'thing', 'things', 'something',
  'know', 'think', 'want', 'get', 'got', 'make', 'way', 'see', 'go',
]);

// Track last data hash to avoid unnecessary re-renders
let lastDataHash = '';

// Current filter mode
let currentMode = 'all';

// Cache of the most recently fetched entries. The data flow is now async
// (every read crosses the IPC boundary), so we keep a single cached snapshot
// that render() refreshes and that the synchronous-feeling helpers
// (updateHeaderStats, getCommonWords) read from. It is refreshed by
// loadHistory() and on every 'history-updated' event.
let cachedEntries = [];
let pipelineBannerTimer = null;

const PIPELINE_MESSAGES = {
  preparing_model: 'Preparing the local Whisper model… first launch may take a few minutes.',
  ready: 'VibeToText is ready.',
  model_unavailable: 'The Whisper model could not be prepared. Check your network and the VibeToText log; the next recording will retry.',
  not_ready: 'The local Whisper model is still preparing. Try again when VibeToText is ready.',
  busy: 'VibeToText is still processing the previous recording.',
  transcribing: 'Transcribing locally…',
  searching_context: 'Searching the configured codebase for context…',
  cleaning_up: 'Cleaning up the transcription with Gemini…',
  generating_plan: 'Generating an implementation plan with Gemini…',
  searching_files: 'Finding relevant files with Greppy…',
  saving: 'Saving transcription history…',
  pasting: 'Pasting at the cursor…',
  done: 'Transcription pasted.',
  failed: 'Transcription failed. Check the VibeToText log for details.',
  permission_needed: 'VibeToText needs an operating-system permission before hotkeys can work.',
};

function showPipelineStatus(phase, mode) {
  const banner = document.getElementById('pipeline-banner');
  const message = document.getElementById('pipeline-message');
  if (!banner || !message) return;

  if (pipelineBannerTimer) {
    clearTimeout(pipelineBannerTimer);
    pipelineBannerTimer = null;
  }

  const text = PIPELINE_MESSAGES[phase] || phase.replaceAll('_', ' ');
  const modeSuffix = mode && ['transcribing', 'cleaning_up', 'generating_plan'].includes(phase)
    ? ` (${mode})`
    : '';
  message.textContent = text + modeSuffix;
  banner.classList.toggle('error', ['failed', 'not_ready', 'busy', 'permission_needed', 'model_unavailable'].includes(phase));
  banner.classList.toggle('success', ['ready', 'done'].includes(phase));
  banner.hidden = false;

  if (['ready', 'done'].includes(phase)) {
    pipelineBannerTimer = setTimeout(() => {
      banner.hidden = true;
    }, 2500);
  }
}

// Load history entries via the Rust backend (was: better-sqlite3 + JSON fallback).
// Returns the entries array and updates the cache.
async function loadHistory() {
  try {
    const entries = await vtt.getEntries();
    cachedEntries = Array.isArray(entries) ? entries : [];
  } catch (err) {
    console.error('Error loading history:', err);
    cachedEntries = [];
  }
  return cachedEntries;
}

function formatTime(isoString) {
  const date = new Date(isoString);
  const now = new Date();
  const diffMs = now - date;
  const diffMins = Math.floor(diffMs / 60000);
  const diffHours = Math.floor(diffMs / 3600000);
  const diffDays = Math.floor(diffMs / 86400000);

  if (diffMins < 1) return 'Just now';
  if (diffMins < 60) return `${diffMins}m ago`;
  if (diffHours < 24) return `${diffHours}h ago`;
  if (diffDays < 7) return `${diffDays}d ago`;

  return date.toLocaleDateString('en-US', {
    month: 'short',
    day: 'numeric',
    hour: 'numeric',
    minute: '2-digit',
  });
}

function getCommonWords(entries) {
  const wordCounts = {};

  entries.forEach(entry => {
    const words = entry.text.toLowerCase()
      .replace(/[.,!?;:'"()\[\]{}]/g, '')
      .split(/\s+/)
      .filter(w => w.length > 2 && !STOPWORDS.has(w));

    words.forEach(word => {
      wordCounts[word] = (wordCounts[word] || 0) + 1;
    });
  });

  return Object.entries(wordCounts)
    .sort((a, b) => b[1] - a[1])
    .slice(0, 10);
}

function escapeHtml(text) {
  const div = document.createElement('div');
  div.textContent = text;
  return div.innerHTML;
}

// Update header stats for a given mode. Operates on the cached entries
// snapshot so it does not trigger another async fetch; callers that need fresh
// data call loadHistory() first (render() already does).
function updateHeaderStats(mode) {
  const allEntries = cachedEntries;

  // Settings tabs (analytics, microphone, dictionary) show "all" stats
  const settingsTabs = ['analytics', 'microphone', 'dictionary', 'settings'];
  const effectiveMode = settingsTabs.includes(mode) ? 'all' : mode;
  const entries = effectiveMode === 'all'
    ? allEntries
    : allEntries.filter(e => (e.mode || 'transcribe') === effectiveMode);

  // Calculate stats for filtered entries
  const totalSessions = entries.length;
  const totalWords = entries.reduce((sum, e) => sum + (e.word_count || e.text.split(/\s+/).length), 0);

  // Calculate average WPM from entries that have it
  const wpmEntries = entries.filter(e => e.wpm).map(e => e.wpm);
  const avgWpm = wpmEntries.length > 0 ? Math.round(wpmEntries.reduce((a, b) => a + b, 0) / wpmEntries.length) : 0;

  // Calculate time saved - only from entries that have duration data
  const entriesWithDuration = entries.filter(e => e.duration_seconds);
  const totalDuration = entriesWithDuration.reduce((sum, e) => sum + e.duration_seconds, 0);
  const wordsWithDuration = entriesWithDuration.reduce((sum, e) => sum + (e.word_count || e.text.split(/\s+/).length), 0);
  const typingWpm = 40;
  const timeToTypeMinutes = wordsWithDuration / typingWpm;
  const timeDictatingMinutes = totalDuration / 60;
  const timeSavedMinutes = Math.max(0, timeToTypeMinutes - timeDictatingMinutes);

  // Update stats (only text content, no DOM rebuild)
  document.getElementById('total-sessions').textContent = totalSessions.toLocaleString();
  document.getElementById('total-words').textContent = totalWords.toLocaleString();
  document.getElementById('avg-wpm').textContent = avgWpm > 0 ? avgWpm : '--';
  document.getElementById('time-saved').textContent = timeSavedMinutes.toFixed(1);
}

async function render(forceRender = false) {
  // Skip render when in special tabs - they handle their own views
  const settingsTabs = ['analytics', 'microphone', 'dictionary', 'settings'];
  if (settingsTabs.includes(currentMode)) {
    return;
  }

  const allEntries = await loadHistory();

  // Create a hash to check if data changed
  const dataHash = JSON.stringify(allEntries) + currentMode;
  if (!forceRender && dataHash === lastDataHash) {
    // Only update timestamps if data hasn't changed
    updateTimestamps();
    return;
  }
  lastDataHash = dataHash;

  // Sort by timestamp, newest first
  allEntries.sort((a, b) => new Date(b.timestamp) - new Date(a.timestamp));

  // Filter entries based on current mode
  const entries = currentMode === 'all'
    ? allEntries
    : allEntries.filter(e => (e.mode || 'transcribe') === currentMode);

  // Update header stats
  updateHeaderStats(currentMode);

  // Show/hide empty state
  const emptyState = document.getElementById('empty-state');
  const entriesContainer = document.getElementById('entries');
  const commonWordsSection = document.getElementById('common-words-section');

  if (entries.length === 0) {
    emptyState.style.display = 'flex';
    entriesContainer.style.display = 'none';
    commonWordsSection.style.display = 'none';
    return;
  }

  emptyState.style.display = 'none';
  entriesContainer.style.display = 'block';
  commonWordsSection.style.display = 'block';

  // Render common words
  const commonWords = getCommonWords(entries);
  const commonWordsContainer = document.getElementById('common-words');
  commonWordsContainer.innerHTML = commonWords
    .map(([word, count]) => `<span class="word-chip">${escapeHtml(word)}<span class="count">${count}</span></span>`)
    .join('');

  // Render entries
  entriesContainer.innerHTML = entries.slice(0, 100).map((entry, index) => {
    const wordCount = entry.word_count || entry.text.split(/\s+/).length;
    const mode = entry.mode || 'transcribe';
    const timeStr = formatTime(entry.timestamp);
    const wpm = entry.wpm;
    const duration = entry.duration_seconds;

    // Format duration as seconds
    const durationStr = duration ? `${duration.toFixed(1)}s` : '';
    const wpmStr = wpm ? `${wpm} WPM` : '';
    const statsStr = [durationStr, wpmStr, `${wordCount} words`].filter(Boolean).join(' · ');

    return `
      <div class="entry" data-timestamp="${escapeHtml(entry.timestamp)}">
        <div class="entry-header">
          <span class="entry-time">${timeStr}</span>
          <span class="entry-mode ${escapeHtml(mode)}">${escapeHtml(mode)}</span>
        </div>
        <div class="entry-text">${escapeHtml(entry.text)}</div>
        <div class="entry-stats">${statsStr}</div>
      </div>
    `;
  }).join('');
}

function updateTimestamps() {
  // Only update the time strings without rebuilding DOM
  document.querySelectorAll('.entry').forEach(entry => {
    const timestamp = entry.dataset.timestamp;
    if (timestamp) {
      const timeEl = entry.querySelector('.entry-time');
      if (timeEl) {
        timeEl.textContent = formatTime(timestamp);
      }
    }
  });
}

// Config functions (was: fs read/write of ~/.vibetotext/config.json).
async function loadConfig() {
  try {
    const config = await vtt.loadConfig();
    return config || {};
  } catch (err) {
    console.error('Error loading config:', err);
    return {};
  }
}

// Microphone panel functions (was: navigator.mediaDevices.enumerateDevices()).
// listAudioDevices() is backed by cpal in Rust; it is expected to return an
// empty list until Phase 2 wires up the audio backend, so empty is handled
// gracefully here.
async function loadAudioDevices() {
  const micSelect = document.getElementById('mic-select');
  const micStatus = document.getElementById('mic-status');

  try {
    const audioInputs = await vtt.listAudioDevices() || [];

    const config = await loadConfig();
    const savedDeviceId = config.audio_device_id;
    const savedDeviceName = config.audio_device_name;
    const savedDeviceIndex = config.audio_device_index;

    micSelect.innerHTML = '';

    if (audioInputs.length === 0) {
      // Empty until Phase 2 (cpal not wired yet) — degrade gracefully.
      micSelect.innerHTML = '<option value="">No microphones found</option>';
      if (savedDeviceName) {
        micStatus.textContent = `Currently using: ${savedDeviceName}`;
      }
      return;
    }

    audioInputs.forEach((device, index) => {
      // Device shape from cpal: { id, name, index } (tolerate web-style { deviceId, label }).
      const deviceId = device.id != null ? device.id : device.deviceId;
      const deviceName = device.name || device.label;
      const deviceIndex = device.index != null ? device.index : index;

      const option = document.createElement('option');
      // Native cpal devices do not expose a stable web-style device id. Use the
      // enumeration index as the selectable value so index 0 is still truthy
      // ("0") and the change handler can persist the selection.
      option.value = deviceId != null ? String(deviceId) : String(deviceIndex);
      option.textContent = deviceName || `Microphone ${index + 1}`;
      option.dataset.index = deviceIndex;
      option.dataset.deviceId = deviceId != null ? String(deviceId) : '';

      // Try to match by device index first, then ID, then name.
      if (savedDeviceIndex != null && deviceIndex === savedDeviceIndex) {
        option.selected = true;
      } else if (savedDeviceId != null && deviceId === savedDeviceId) {
        option.selected = true;
      } else if (savedDeviceName && deviceName === savedDeviceName) {
        option.selected = true;
      }

      micSelect.appendChild(option);
    });

    // Show current selection status
    if (savedDeviceName) {
      micStatus.textContent = `Currently using: ${savedDeviceName}`;
    }

  } catch (err) {
    console.error('Error loading audio devices:', err);
    micSelect.innerHTML = '<option value="">Error loading devices</option>';
  }
}

async function handleMicChange(event) {
  const select = event.target;
  const selectedOption = select.options[select.selectedIndex];
  const micStatus = document.getElementById('mic-status');

  if (selectedOption && selectedOption.value) {
    const deviceIndex = parseInt(selectedOption.dataset.index, 10);
    const deviceId = selectedOption.dataset.deviceId || null;
    try {
      await vtt.setAudioDevice(deviceIndex, selectedOption.textContent, deviceId);
      // Single process now: settings hot-apply, no restart required.
      micStatus.textContent = `Saved: ${selectedOption.textContent}`;
    } catch (err) {
      micStatus.textContent = `Could not save microphone: ${err}`;
      await loadAudioDevices();
    }
  }
}

// Dictionary panel functions (was: config.custom_dictionary read/write via fs).
async function loadDictionary() {
  try {
    const words = await vtt.getDictionary();
    return Array.isArray(words) ? words : [];
  } catch (err) {
    console.error('Error loading dictionary:', err);
    return [];
  }
}

async function renderDictionaryWords() {
  const wordsContainer = document.getElementById('dict-words');
  const words = await loadDictionary();

  if (words.length === 0) {
    wordsContainer.innerHTML = '<span class="dict-empty">No custom words added yet</span>';
    return;
  }

  wordsContainer.innerHTML = words.map(word => `
    <span class="dict-word">
      ${escapeHtml(word)}
      <button class="dict-word-remove" data-word="${escapeHtml(word)}" aria-label="Remove &quot;${escapeHtml(word)}&quot;">&times;</button>
    </span>
  `).join('');

  // Add remove handlers
  wordsContainer.querySelectorAll('.dict-word-remove').forEach(btn => {
    btn.addEventListener('click', async () => {
      const wordToRemove = btn.dataset.word;
      try {
        await vtt.removeWord(wordToRemove);
      } catch (err) {
        console.error('Error removing word:', err);
        showDictStatus(String(err));
        return;
      }
      await renderDictionaryWords();
      showDictStatus(`Removed "${wordToRemove}"`);
    });
  });
}

async function addDictionaryWord() {
  const input = document.getElementById('dict-input');
  const word = input.value.trim();

  if (!word) return;

  const words = await loadDictionary();
  if (words.includes(word)) {
    showDictStatus(`"${word}" is already in your dictionary`);
    return;
  }

  try {
    await vtt.addWord(word);
  } catch (err) {
    console.error('Error adding word:', err);
    showDictStatus(String(err));
    return;
  }
  await renderDictionaryWords();
  input.value = '';
  showDictStatus(`Added "${word}"`);
}

function showDictStatus(message) {
  const status = document.getElementById('dict-status');
  status.textContent = message;
  setTimeout(() => {
    status.textContent = '';
  }, 3000);
}

// Settings panel functions
async function renderSettings() {
  const config = await loadConfig();
  const orbPos = config.orb_position || 'bottom-center';
  const model = config.whisper_model === 'large' ? 'large-v3' : (config.whisper_model || 'small');

  // Set active preset
  document.querySelectorAll('.orb-preset').forEach(btn => {
    btn.classList.toggle('active', btn.dataset.preset === orbPos);
  });

  // If custom position, clear active presets and fill inputs
  if (typeof orbPos === 'object' && orbPos.x !== undefined) {
    document.querySelectorAll('.orb-preset').forEach(btn => btn.classList.remove('active'));
    document.getElementById('orb-x').value = orbPos.x;
    document.getElementById('orb-y').value = orbPos.y;
  } else {
    document.getElementById('orb-x').value = '';
    document.getElementById('orb-y').value = '';
  }

  // Set model dropdown
  document.getElementById('model-select').value = model;
  document.getElementById('codebase-path').value = config.codebase_path || '';
  await refreshGeminiStatus();
  await refreshCodebasePathStatus();
}

async function refreshCodebasePathStatus() {
  const statusEl = document.getElementById('codebase-status');
  try {
    const status = await vtt.getCodebasePathStatus();
    if (!status.path) {
      statusEl.textContent = '';
      statusEl.className = 'settings-status codebase-status';
      return;
    }
    if (status.exists && status.readable) {
      statusEl.textContent = 'Path is valid';
      statusEl.className = 'settings-status codebase-status codebase-status-ok';
    } else if (!status.exists) {
      statusEl.textContent = 'Path no longer exists on disk; Greppy will fall back to raw text';
      statusEl.className = 'settings-status codebase-status codebase-status-warn';
    } else {
      statusEl.textContent = 'Path is not readable; check filesystem permissions';
      statusEl.className = 'settings-status codebase-status codebase-status-warn';
    }
  } catch (err) {
    statusEl.textContent = String(err);
    statusEl.className = 'settings-status codebase-status codebase-status-warn';
  }
}

async function saveOrbPreset(preset) {
  try {
    await vtt.setOrbPosition(preset);
  } catch (err) {
    showSettingsStatus('orb-status', String(err));
    return;
  }

  // Update UI
  document.querySelectorAll('.orb-preset').forEach(btn => {
    btn.classList.toggle('active', btn.dataset.preset === preset);
  });
  document.getElementById('orb-x').value = '';
  document.getElementById('orb-y').value = '';
  // Settings hot-apply now (single process) — no restart prompt.
  showSettingsStatus('orb-status', `Orb position set to ${preset.replace('-', ' ')}`);
}

async function saveOrbCustom() {
  const x = parseInt(document.getElementById('orb-x').value, 10);
  const y = parseInt(document.getElementById('orb-y').value, 10);

  if (isNaN(x) || isNaN(y)) {
    showSettingsStatus('orb-status', 'Please enter valid X and Y coordinates');
    return;
  }

  try {
    await vtt.setOrbPosition({ x, y });
  } catch (err) {
    showSettingsStatus('orb-status', String(err));
    return;
  }

  document.querySelectorAll('.orb-preset').forEach(btn => btn.classList.remove('active'));
  showSettingsStatus('orb-status', `Orb position set to (${x}, ${y})`);
}

async function saveWhisperModel(model) {
  try {
    await vtt.setWhisperModel(model);
  } catch (err) {
    showSettingsStatus('model-status', String(err));
    return;
  }
  // Model hot-reloads in the single process — no restart needed.
  showSettingsStatus('model-status', `Model set to ${model}`);
}

async function saveCodebasePath() {
  const input = document.getElementById('codebase-path');
  try {
    const config = await vtt.setCodebasePath(input.value.trim() || null);
    input.value = config.codebase_path || '';
    showSettingsStatus('codebase-status', config.codebase_path ? 'Codebase path saved' : 'Codebase context disabled');
    // Re-poll status so the badge reflects the new path's existence immediately
    // (a saved-but-missing path is the most useful case to flag).
    await refreshCodebasePathStatus();
  } catch (err) {
    showSettingsStatus('codebase-status', String(err));
  }
}

async function refreshGeminiStatus() {
  const status = document.getElementById('gemini-status');
  try {
    const source = await vtt.getGeminiKeyStatus();
    status.textContent = source ? `Configured via ${source}` : 'Not configured — Cleanup and Plan fall back to raw transcription';
  } catch (err) {
    status.textContent = `Could not read status: ${err}`;
  }
}

async function saveGeminiKey() {
  const input = document.getElementById('gemini-key');
  if (!input.value.trim()) {
    showSettingsStatus('gemini-status', 'Paste a key before saving');
    return;
  }
  try {
    await vtt.setGeminiApiKey(input.value.trim());
    input.value = '';
    await refreshGeminiStatus();
  } catch (err) {
    showSettingsStatus('gemini-status', String(err));
  }
}

async function clearGeminiKey() {
  try {
    await vtt.setGeminiApiKey(null);
    document.getElementById('gemini-key').value = '';
    await refreshGeminiStatus();
  } catch (err) {
    showSettingsStatus('gemini-status', String(err));
  }
}

async function clearHistory() {
  if (!window.confirm('Delete all transcription history? This cannot be undone.')) return;
  try {
    await vtt.clearHistory();
    cachedEntries = [];
    lastDataHash = '';
    updateHeaderStats('all');
    showSettingsStatus('history-status', 'Transcription history cleared');
  } catch (err) {
    showSettingsStatus('history-status', String(err));
  }
}

function showSettingsStatus(elementId, message) {
  const status = document.getElementById(elementId);
  status.textContent = message;
  setTimeout(() => {
    status.textContent = '';
  }, 4000);
}

// Main tab activation: render the panel for the newly-selected mode. The
// ARIA-side state (active class, aria-selected, roving tabindex) is managed
// by `setupTablist`; this callback only handles the panel-rendering side.
async function activateMainTab(tab) {
  currentMode = tab.dataset.mode;

  const analyticsPanel = document.getElementById('analytics-panel');
  const microphonePanel = document.getElementById('microphone-panel');
  const dictionaryPanel = document.getElementById('dictionary-panel');
  const entriesPanel = document.getElementById('panel-entries');
  const settingsPanel = document.getElementById('settings-panel');

  // Hide all panels first; show the one(s) for the selected mode. Use the
  // `hidden` attribute so screen readers + the tab roving focus honor the
  // ARIA tabpanel contract.
  analyticsPanel.hidden = true;
  microphonePanel.hidden = true;
  dictionaryPanel.hidden = true;
  settingsPanel.hidden = true;

  if (currentMode === 'analytics') {
    analyticsPanel.hidden = false;
    const entries = await loadHistory();
    updateHeaderStats(currentMode);
    if (typeof renderAnalytics === 'function') {
      renderAnalytics(entries);
    }
  } else if (currentMode === 'microphone') {
    microphonePanel.hidden = false;
    await loadHistory();
    updateHeaderStats(currentMode);
    await loadAudioDevices();
  } else if (currentMode === 'dictionary') {
    dictionaryPanel.hidden = false;
    await loadHistory();
    updateHeaderStats(currentMode);
    await renderDictionaryWords();
  } else if (currentMode === 'settings') {
    settingsPanel.hidden = false;
    await loadHistory();
    updateHeaderStats(currentMode);
    await renderSettings();
  } else {
    // all / transcribe / greppy / cleanup / plan → shared entries panel
    entriesPanel.hidden = false;
    await render(true);
  }
}

// Wire the main tablist to the panel-renderer. setupTablist handles
// click + roving tabindex + ArrowLeft/Right/Home/End + automatic aria-selected.
setupTablist({
  tablist: document.getElementById('tabs'),
  onActivate: (tab) => { activateMainTab(tab); },
});

// Activity (hourly / yearly) sub-tablist. Same ARIA pattern, but inside the
// analytics card; on activation we just toggle the inner panels without
// touching the entries list.
setupTablist({
  tablist: document.querySelector('.activity-tabs'),
  onActivate: (tab) => {
    const view = tab.dataset.view;
    document.getElementById('activity-heatmap').hidden = view !== 'hourly';
    document.getElementById('activity-yearly').hidden = view !== 'yearly';
  },
});

// Set up mic dropdown change handler
document.getElementById('mic-select').addEventListener('change', handleMicChange);

// Set up dictionary handlers
document.getElementById('dict-add-btn').addEventListener('click', addDictionaryWord);
document.getElementById('dict-input').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') {
    addDictionaryWord();
  }
});

// Set up settings handlers
document.querySelectorAll('.orb-preset').forEach(btn => {
  btn.addEventListener('click', () => saveOrbPreset(btn.dataset.preset));
});
document.getElementById('orb-custom-save').addEventListener('click', saveOrbCustom);
document.getElementById('model-select').addEventListener('change', (e) => {
  saveWhisperModel(e.target.value);
});
document.getElementById('codebase-save').addEventListener('click', saveCodebasePath);
document.getElementById('gemini-save').addEventListener('click', saveGeminiKey);
document.getElementById('gemini-clear').addEventListener('click', clearGeminiKey);
document.getElementById('history-clear').addEventListener('click', clearHistory);

// Initial render
render();
if (typeof vtt.getPipelineStatus === 'function') {
  vtt.getPipelineStatus()
    .then(phase => showPipelineStatus(phase || 'preparing_model'))
    .catch(err => console.error('Error loading pipeline status:', err));
}

// Refresh when the backend writes a new entry. This replaces both the chokidar
// 'history-updated' ipcRenderer listener AND the 5-second setInterval poll from
// the Electron version — the Rust side now emits an event after each write.
if (typeof vtt.onHistoryUpdated === 'function') {
  vtt.onHistoryUpdated(() => {
    // For special tabs, refresh the cache + that tab's view; otherwise re-render
    // the entries list. render() short-circuits on special tabs.
    if (currentMode === 'analytics') {
      loadHistory().then(entries => {
        updateHeaderStats(currentMode);
        if (typeof renderAnalytics === 'function') renderAnalytics(entries);
      });
    } else if (['microphone', 'dictionary', 'settings'].includes(currentMode)) {
      loadHistory().then(() => updateHeaderStats(currentMode));
    } else {
      render();
    }
  });
}

if (typeof vtt.onPipelineStatus === 'function') {
  vtt.onPipelineStatus(payload => {
    if (payload && payload.phase) {
      showPipelineStatus(payload.phase, payload.mode);
    }
  });
}

if (typeof vtt.onRecordingState === 'function') {
  vtt.onRecordingState(payload => {
    if (payload && payload.recording) {
      showPipelineStatus('recording', payload.mode);
      const message = document.getElementById('pipeline-message');
      if (message) message.textContent = `Recording (${payload.mode || 'transcribe'})… release the hotkey to finish.`;
    }
  });
}

if (typeof vtt.onPermissionNeeded === 'function') {
  vtt.onPermissionNeeded(() => showPipelineStatus('permission_needed'));
}
