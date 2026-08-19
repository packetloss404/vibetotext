# VibeToText Analytics Feature - Implementation Plan

## Overview

Add a detailed Analytics tab to the history-app with D3.js visualizations showing usage patterns, productivity metrics, and trends over time.

---

## Color Scheme (from datafeeds Onyx theme + existing modes)

```css
/* Backgrounds (Onyx dark theme) */
--bg-void: #000000;
--bg-sidebar: #09090b;
--bg-input: #121212;
--bg-card: #18181b;
--bg-hover: #27272a;

/* Text */
--text-primary: #e4e4e7;
--text-secondary: #a1a1aa;
--text-muted: #71717a;

/* Chart Accent */
--chart-accent: #fbbf24;  /* Amber/gold from Onyx theme */

/* Mode Colors (existing) */
--green: #34d399;    /* transcribe */
--purple: #a78bfa;   /* greppy */
--orange: #fb923c;   /* cleanup */
--blue: #60a5fa;     /* plan */
--accent: #6366f1;   /* general accent/analytics */
```

---

## Tab Placement

Add "Analytics" tab at the end:

```
[ All ] [ Transcribe ] [ Greppy ] [ Cleanup ] [ Plan ] [ Analytics ]
```

---

## Graphs to Implement

### 1. Activity Heatmap - "When You Talk Most"
**Type:** D3 calendar heatmap (GitHub contribution style)
**Data:** Entries grouped by hour-of-day (0-23) × day-of-week (Mon-Sun)
**Color:** Intensity gradient from `--bg-card` → `--chart-accent`
**Shows:** Peak productivity hours

### 2. Words Over Time
**Type:** D3 area chart with smooth curve
**Data:** Daily word count totals
**Color:** Fill gradient `--accent` → transparent
**Shows:** Usage trends, cumulative productivity

### 3. Minutes Saved (Cumulative)
**Type:** D3 line chart
**Data:** Running total of `(words/40wpm) - duration_seconds`
**Color:** `--green` (matches "time saved" stat styling)
**Shows:** Total productivity gain over time

### 4. WPM Trends
**Type:** D3 line chart with dots
**Data:** Average WPM per day/week
**Color:** `--chart-accent`
**Shows:** Speaking speed trends, improvement over time

### 5. Usage by Mode (Breakdown)
**Type:** D3 donut chart
**Data:** Entry count or word count per mode
**Colors:** Mode colors (green/purple/orange/blue)
**Shows:** Feature usage distribution

---

## File Structure

```
history-app/
├── index.html          # Add Analytics tab button + analytics container
├── styles.css          # Add analytics panel + chart styles
├── renderer.js         # Add tab switching logic for analytics
├── analytics.js        # NEW: D3 chart rendering logic
└── lib/
    └── d3.min.js       # NEW: D3.js library (v7)
```

---

## Implementation Steps

### Phase 1: Setup
1. Download D3.js v7 to `history-app/lib/d3.min.js`
2. Add `<script>` tag to index.html
3. Add Analytics tab button to `.tabs` div (after Greppy)
4. Create `#analytics-panel` container div (hidden by default)

### Phase 2: Tab Infrastructure
5. Add CSS for analytics panel layout
6. Update renderer.js tab click handler to show/hide analytics panel
7. Hide entries list + common words when Analytics tab active

### Phase 3: Data Processing (analytics.js)
8. Create `processDataForCharts(entries)` function:
   - Group entries by hour/day for heatmap
   - Aggregate daily word counts
   - Calculate cumulative time saved
   - Compute rolling WPM averages
   - Count entries by mode

### Phase 4: Chart Components
9. **Activity Heatmap** - `renderActivityHeatmap(container, data)`
   - 7 rows (days) × 24 cols (hours)
   - Tooltip on hover showing count

10. **Words Over Time** - `renderWordsChart(container, data)`
    - X-axis: dates, Y-axis: word count
    - Smooth area fill

11. **Minutes Saved** - `renderTimeSavedChart(container, data)`
    - Cumulative line chart
    - Highlight current total

12. **WPM Trends** - `renderWpmChart(container, data)`
    - Line with data points
    - Show trend direction

13. **Mode Breakdown** - `renderModeDonut(container, data)`
    - Interactive donut segments
    - Legend with counts

### Phase 5: Polish
14. Add responsive sizing (charts resize with window)
15. Add loading states
16. Add empty state for insufficient data
17. Animate chart transitions

---

## HTML Changes (index.html)

```html
<!-- Add to .tabs div, after Plan button (at the end) -->
<button class="tab" data-mode="analytics">Analytics</button>

<!-- Add after .entries div -->
<div class="analytics-panel" id="analytics-panel" style="display: none;">
  <div class="chart-grid">
    <div class="chart-card full-width">
      <h3>Activity by Hour</h3>
      <div id="activity-heatmap"></div>
    </div>
    <div class="chart-card">
      <h3>Words Over Time</h3>
      <div id="words-chart"></div>
    </div>
    <div class="chart-card">
      <h3>Time Saved</h3>
      <div id="time-saved-chart"></div>
    </div>
    <div class="chart-card">
      <h3>Speaking Speed</h3>
      <div id="wpm-chart"></div>
    </div>
    <div class="chart-card">
      <h3>Usage by Mode</h3>
      <div id="mode-donut"></div>
    </div>
  </div>
</div>

<!-- Add before renderer.js -->
<script src="lib/d3.min.js"></script>
<script src="analytics.js"></script>
```

---

## CSS Changes (styles.css)

```css
/* Analytics Tab Styling */
.tab[data-mode="analytics"].active {
  background: rgba(251, 191, 36, 0.15);  /* amber soft */
  color: #fbbf24;  /* amber */
}

/* Analytics Panel Layout */
.analytics-panel {
  flex: 1;
  overflow-y: auto;
  padding: 20px;
}

.chart-grid {
  display: grid;
  grid-template-columns: repeat(2, 1fr);
  gap: 16px;
}

.chart-card {
  background: var(--bg-secondary);
  border: 1px solid var(--border);
  border-radius: 12px;
  padding: 16px;
}

.chart-card.full-width {
  grid-column: span 2;
}

.chart-card h3 {
  font-size: 11px;
  font-weight: 600;
  color: var(--text-muted);
  text-transform: uppercase;
  letter-spacing: 0.5px;
  margin-bottom: 12px;
}

/* D3 Chart Styles */
.activity-heatmap rect {
  rx: 2;
}

.chart-line {
  fill: none;
  stroke-width: 2;
}

.chart-area {
  opacity: 0.3;
}

.chart-dot {
  r: 4;
}

.chart-axis text {
  fill: var(--text-muted);
  font-size: 10px;
}

.chart-axis line,
.chart-axis path {
  stroke: var(--border);
}

.chart-tooltip {
  position: absolute;
  background: var(--bg-tertiary);
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: 8px 12px;
  font-size: 12px;
  pointer-events: none;
  z-index: 100;
}
```

---

## Data Model (already exists)

Each entry in `~/.vibetotext/history.json`:
```json
{
  "text": "transcribed text",
  "mode": "transcribe|greppy|cleanup|plan",
  "timestamp": "2025-01-14T10:30:00.000Z",
  "word_count": 42,
  "duration_seconds": 8.5,
  "wpm": 296
}
```

**No schema changes needed** - all required data already captured.

---

## Dependencies

- **D3.js v7** - Data visualization library
  - Download: https://d3js.org/d3.v7.min.js
  - Size: ~280KB minified

---

## Estimated Complexity

| Component | Complexity |
|-----------|------------|
| Tab infrastructure | Low |
| Activity heatmap | Medium |
| Line/area charts | Medium |
| Donut chart | Low |
| Data processing | Low |
| Responsive/polish | Medium |

---

## Future Enhancements (out of scope)

- Date range picker for filtering
- Export charts as PNG
- Compare periods (this week vs last week)
- Goal setting (words per day target)
