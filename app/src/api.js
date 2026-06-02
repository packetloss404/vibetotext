// api.js — thin shim exposing a global `vtt` object that wraps the Tauri
// command/event surface. Phase 1 of the VibeToText Tauri migration.
//
// `withGlobalTauri` is enabled in tauri.conf.json, so the Tauri JS API is
// available at `window.__TAURI__`:
//   - commands: window.__TAURI__.core.invoke(cmd, args)
//   - events:   window.__TAURI__.event.listen(eventName, handler)
//
// This file maps each frontend coupling point (§5 IPC contract) onto a Rust
// command, and the `history-updated` event onto a subscription helper. Every
// method returns a Promise.
//
// Guard: when opened in a plain browser (no Tauri runtime) the shim degrades
// gracefully — methods reject with a clear error and `onHistoryUpdated` is a
// no-op — so the page loads without throwing.
(function (global) {
  'use strict';

  var TAURI = global.__TAURI__;
  var hasTauri = !!(TAURI && TAURI.core && typeof TAURI.core.invoke === 'function');

  // Resolve invoke/listen once. When Tauri is absent we substitute safe stubs
  // so the rest of the app can load and feature-detect via `vtt.available`.
  var invoke = hasTauri
    ? TAURI.core.invoke.bind(TAURI.core)
    : function (cmd) {
        return Promise.reject(
          new Error('Tauri runtime unavailable (command "' + cmd + '" not invoked)')
        );
      };

  var listen =
    hasTauri && TAURI.event && typeof TAURI.event.listen === 'function'
      ? TAURI.event.listen.bind(TAURI.event)
      : null;

  // --- Commands (frontend -> Rust via invoke) -----------------------------
  // Argument keys are snake_case to match the Rust `#[tauri::command]`
  // signatures (Tauri converts camelCase JS args to snake_case, but we pass
  // explicit snake_case to be unambiguous).

  function getEntries(mode, limit) {
    return invoke('get_entries', { mode: mode, limit: limit });
  }

  function getStatistics(mode) {
    return invoke('get_statistics', { mode: mode });
  }

  function clearHistory() {
    return invoke('clear_history');
  }

  function loadConfig() {
    return invoke('load_config');
  }

  function saveConfig(cfg) {
    // save_config takes a partial config and preserves unknown keys server-side.
    return invoke('save_config', { config: cfg });
  }

  function getDictionary() {
    return invoke('get_dictionary');
  }

  function addWord(word) {
    return invoke('add_word', { word: word });
  }

  function removeWord(word) {
    return invoke('remove_word', { word: word });
  }

  function setAudioDevice(index, name) {
    return invoke('set_audio_device', { index: index, name: name });
  }

  function setWhisperModel(model) {
    return invoke('set_whisper_model', { model: model });
  }

  function setOrbPosition(position) {
    return invoke('set_orb_position', { position: position });
  }

  function listAudioDevices() {
    return invoke('list_audio_devices');
  }

  // --- Events (Rust -> frontend via emit) ---------------------------------

  // Subscribe to the 'history-updated' event (replaces chokidar watch + 5s poll).
  // Returns a Promise that resolves to an unlisten function. When Tauri is
  // absent it resolves to a no-op unlisten so callers can `await` unconditionally.
  function onHistoryUpdated(cb) {
    if (!listen) {
      return Promise.resolve(function () {});
    }
    return listen('history-updated', function (event) {
      cb(event && event.payload);
    });
  }

  global.vtt = {
    available: hasTauri,
    getEntries: getEntries,
    getStatistics: getStatistics,
    clearHistory: clearHistory,
    loadConfig: loadConfig,
    saveConfig: saveConfig,
    getDictionary: getDictionary,
    addWord: addWord,
    removeWord: removeWord,
    setAudioDevice: setAudioDevice,
    setWhisperModel: setWhisperModel,
    setOrbPosition: setOrbPosition,
    listAudioDevices: listAudioDevices,
    onHistoryUpdated: onHistoryUpdated
  };

  if (!hasTauri && global.console && global.console.warn) {
    global.console.warn(
      '[vtt] Tauri runtime not detected — running in degraded browser mode; ' +
        'IPC commands will reject.'
    );
  }
})(typeof window !== 'undefined' ? window : this);
