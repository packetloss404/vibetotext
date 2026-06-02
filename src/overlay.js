// Floating waveform overlay renderer (Phase 3).
//
// Ports the visual of the Python `ui_tkinter.py` waveform: 25 vertical bars,
// center-anchored, animating to per-band audio levels. Styling is intentionally
// minimal — pink bars on a transparent background ("pink bars like the
// reference").
//
// Data flow (plan §5 event contract): the Rust pipeline emits
//   - 'waveform-levels'  payload: number[25], each 0..1 (bar heights)
//   - 'recording-state'  payload: { recording: bool, mode: string }
// This script subscribes via window.__TAURI__.event.listen and animates the
// canvas. When recording stops we fade the bars back to the idle flat line.

const NUM_BARS = 25;

// Pink, matching the reference accent. A slightly lighter tip gives the bars a
// bit of dimension without extra chrome.
const BAR_COLOR = "#ff4f9a";
const IDLE_COLOR = "#3f3f46"; // onyx gray flat line when not recording (ports ui_tkinter idle)

const canvas = document.getElementById("waveform");
const ctx = canvas.getContext("2d");

// Target levels (driven by events) and the currently-rendered levels. We lerp
// rendered -> target every frame for smooth motion, matching the Python overlay
// feel without needing the backend to send interpolated frames.
let targetLevels = new Array(NUM_BARS).fill(0);
let renderLevels = new Array(NUM_BARS).fill(0);
let recording = false;

// Handle HiDPI + the transparent window's CSS size. Re-measured on resize.
let dpr = 1;
function resize() {
  dpr = window.devicePixelRatio || 1;
  const w = window.innerWidth;
  const h = window.innerHeight;
  canvas.width = Math.max(1, Math.round(w * dpr));
  canvas.height = Math.max(1, Math.round(h * dpr));
  canvas.style.width = w + "px";
  canvas.style.height = h + "px";
}
window.addEventListener("resize", resize);
resize();

function clampLevel(v) {
  if (typeof v !== "number" || Number.isNaN(v)) return 0;
  if (v < 0) return 0;
  if (v > 1) return 1;
  return v;
}

function draw() {
  const w = canvas.width;
  const h = canvas.height;

  // Transparent clear: never paint a background, so only the bars show through
  // the click-through window.
  ctx.clearRect(0, 0, w, h);

  const centerY = h / 2;

  // Layout: ~5% padding each side, small inter-bar spacing — ports the geometry
  // from ui_tkinter.draw_waveform.
  const padding = w * 0.05;
  const usableWidth = w - padding * 2;
  const barSpacing = (usableWidth * 0.02);
  const totalSpacing = barSpacing * (NUM_BARS - 1);
  const barWidth = (usableWidth - totalSpacing) / NUM_BARS;
  const radius = Math.min(barWidth / 2, 4 * dpr);

  const minHeight = Math.max(2 * dpr, h * 0.1);

  for (let i = 0; i < NUM_BARS; i++) {
    // Smoothly approach the target level for this bar.
    renderLevels[i] += (targetLevels[i] - renderLevels[i]) * 0.35;
    const level = renderLevels[i];

    const x = padding + i * (barWidth + barSpacing);

    let barHeight;
    let color;
    if (recording) {
      color = BAR_COLOR;
      barHeight = Math.max(minHeight, level * h * 0.7);
      barHeight = Math.min(barHeight, h * 0.85);
    } else {
      color = IDLE_COLOR;
      barHeight = minHeight; // flat line when idle
    }

    const y1 = centerY - barHeight / 2;

    ctx.fillStyle = color;
    drawRoundedBar(x, y1, barWidth, barHeight, radius);
  }

  requestAnimationFrame(draw);
}

// Rounded-rectangle bar. Falls back to a plain rect where roundRect is missing.
function drawRoundedBar(x, y, w, h, r) {
  if (typeof ctx.roundRect === "function") {
    ctx.beginPath();
    ctx.roundRect(x, y, w, h, r);
    ctx.fill();
  } else {
    ctx.fillRect(x, y, w, h);
  }
}

requestAnimationFrame(draw);

// ── Event wiring ────────────────────────────────────────────────────────────
// __TAURI__ is injected by the Tauri webview. Guard so the file can also be
// opened in a plain browser during UI tweaking without throwing.
const tauri = window.__TAURI__;
if (tauri && tauri.event && typeof tauri.event.listen === "function") {
  const { listen } = tauri.event;

  listen("waveform-levels", (event) => {
    const payload = event.payload;
    if (!Array.isArray(payload)) return;
    for (let i = 0; i < NUM_BARS; i++) {
      targetLevels[i] = clampLevel(payload[i]);
    }
  });

  listen("recording-state", (event) => {
    const payload = event.payload || {};
    recording = !!payload.recording;
    if (!recording) {
      // Stopped: drop the targets so the bars settle back to the idle line.
      targetLevels = new Array(NUM_BARS).fill(0);
    }
  });
}
