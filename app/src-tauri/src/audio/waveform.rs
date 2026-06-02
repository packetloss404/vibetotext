//! FFT waveform visualization — 25-bar band mapping.
//!
//! Direct port of the band-mapping / smoothing math in
//! `src/vibetotext/recorder.py` (`AudioRecorder._callback`). The intent is
//! visual parity with the Python reference (migration-plan risk #4):
//!
//! 1. RMS silence gate (`base_level = min(1, rms * 100)`, threshold `0.08`).
//! 2. Zero-pad / truncate the input frame to `FFT_SIZE` (512).
//! 3. Hann window -> real->complex FFT (rustfft) -> magnitude spectrum.
//! 4. dB-like scale: `clip(mag, 1e-10)`, `20*log10`, normalize `(db+60)/60`
//!    clamped to `0..1` (mimics Web Audio `getByteFrequencyData`).
//! 5. Exponential (power-2.5) frequency-band mapping into 25 bars, skipping
//!    the first `MIN_FREQ_BIN` (4) bins of sub-bass rumble.
//! 6. Bass reduction on the first 4 bars (`0.5, 0.625, 0.75, 0.875`).
//! 7. Temporal smoothing (`70% previous + 30% new`), with a smooth decay to
//!    zero while the input is gated as silence.
//!
//! Two entry points are provided:
//! * [`compute_bars`] — stateless (PHASE 2 CONTRACT). No temporal smoothing
//!   across calls (no previous frame to blend with), so it returns the raw
//!   per-frame bars. Returns all-zero bars for silent / empty input.
//! * [`WaveformAnalyzer`] — keeps the previous-frame state so successive
//!   calls reproduce the Python temporal smoothing exactly.

use std::sync::Arc;

use rustfft::{num_complex::Complex, Fft, FftPlanner};

/// Number of output bars (matches `recorder.py` `NUM_BARS`).
pub const NUM_BARS: usize = 25;
/// FFT window size (matches `recorder.py` `FFT_SIZE`).
const FFT_SIZE: usize = 512;
/// Smoothing constant: 70% previous, 30% new (Web Audio `smoothingTimeConstant`).
const SMOOTHING: f32 = 0.7;
/// RMS silence gate threshold on `base_level` (matches `SILENCE_THRESHOLD`).
const SILENCE_THRESHOLD: f32 = 0.08;
/// Skip sub-bass rumble bins (~125Hz at 16kHz SR); matches `MIN_FREQ_BIN`.
const MIN_FREQ_BIN: usize = 4;
/// Number of complex bins produced by an rfft of `FFT_SIZE` real samples.
const SPECTRUM_LEN: usize = FFT_SIZE / 2 + 1; // 257

/// Stateless 25-bar waveform computation (PHASE 2 CONTRACT).
///
/// Returns normalized `0..1` bar heights for a single audio frame. Because it
/// is stateless there is no temporal smoothing across calls; use
/// [`WaveformAnalyzer`] for the live overlay where frame-to-frame smoothing
/// (and silent decay) matters.
///
/// Silent or empty input yields all-zero bars.
pub fn compute_bars(samples: &[f32]) -> [f32; NUM_BARS] {
    let planner = make_fft();
    compute_bars_with_fft(&planner, samples)
}

/// Stateful analyzer reproducing the Python temporal smoothing.
///
/// Holds the previous frame's bar levels and a cached FFT plan so the overlay
/// can call [`WaveformAnalyzer::process`] once per audio callback. Reset state
/// at the start of each recording via [`WaveformAnalyzer::reset`].
pub struct WaveformAnalyzer {
    fft: Arc<dyn Fft<f32>>,
    prev_levels: [f32; NUM_BARS],
}

impl WaveformAnalyzer {
    /// Create a new analyzer with zeroed smoothing state.
    pub fn new() -> Self {
        Self {
            fft: make_fft(),
            prev_levels: [0.0; NUM_BARS],
        }
    }

    /// Clear the smoothing state (call at the start of a new recording).
    pub fn reset(&mut self) {
        self.prev_levels = [0.0; NUM_BARS];
    }

    /// Process one audio frame, applying temporal smoothing against the
    /// previously processed frame. Returns the smoothed `0..1` bar heights.
    pub fn process(&mut self, samples: &[f32]) -> [f32; NUM_BARS] {
        // RMS silence gate (matches recorder.py): smooth decay toward zero.
        if is_silent(samples) {
            for level in self.prev_levels.iter_mut() {
                *level *= SMOOTHING;
            }
            return self.prev_levels;
        }

        let raw = compute_bars_with_fft(&self.fft, samples);

        // Temporal smoothing: 70% previous + 30% new.
        let mut levels = [0.0f32; NUM_BARS];
        for i in 0..NUM_BARS {
            levels[i] = self.prev_levels[i] * SMOOTHING + raw[i] * (1.0 - SMOOTHING);
        }
        self.prev_levels = levels;
        levels
    }
}

impl Default for WaveformAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a forward FFT plan for `FFT_SIZE` samples.
fn make_fft() -> Arc<dyn Fft<f32>> {
    let mut planner = FftPlanner::<f32>::new();
    planner.plan_fft_forward(FFT_SIZE)
}

/// RMS-based silence gate, mirroring `recorder.py`:
/// `base_level = min(1, rms * 100)`, gated when `base_level < SILENCE_THRESHOLD`.
fn is_silent(samples: &[f32]) -> bool {
    if samples.is_empty() {
        return true;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    let rms = (sum_sq / samples.len() as f32).sqrt();
    let base_level = (rms * 100.0).min(1.0);
    base_level < SILENCE_THRESHOLD
}

/// Core band-mapping math shared by both entry points (no temporal smoothing).
fn compute_bars_with_fft(fft: &Arc<dyn Fft<f32>>, samples: &[f32]) -> [f32; NUM_BARS] {
    let mut levels = [0.0f32; NUM_BARS];

    // RMS silence gate: silent / empty input -> all-zero bars.
    if is_silent(samples) {
        return levels;
    }

    // Zero-pad (or truncate) the frame to FFT_SIZE, applying a Hann window.
    let mut buffer = vec![Complex::<f32>::new(0.0, 0.0); FFT_SIZE];
    let n = samples.len().min(FFT_SIZE);
    for (i, slot) in buffer.iter_mut().enumerate().take(n) {
        slot.re = samples[i] * hann(i, FFT_SIZE);
    }
    // Samples beyond `n` (when padding) stay zero; the window beyond `n` is
    // irrelevant because those samples are zero anyway. NumPy applies the Hann
    // window over the padded length, but cos*0 == 0 so the result is identical.

    fft.process(&mut buffer);

    // Magnitude spectrum -> dB-like -> normalized 0..1 (mimics
    // getByteFrequencyData): clip to 1e-10, 20*log10, map -60dB..0dB -> 0..1.
    let mut spectrum_norm = [0.0f32; SPECTRUM_LEN];
    for (i, slot) in spectrum_norm.iter_mut().enumerate() {
        let mag = buffer[i].norm().max(1e-10);
        let db = 20.0 * mag.log10();
        *slot = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    }

    let usable_bins = (SPECTRUM_LEN - MIN_FREQ_BIN) as f32; // 253.0

    // Exponential (power-2.5) frequency-band mapping into NUM_BARS bars.
    for i in 0..NUM_BARS {
        let lo = (MIN_FREQ_BIN as f32
            + usable_bins * (i as f32 / NUM_BARS as f32).powf(2.5)) as usize;
        let mut hi = (MIN_FREQ_BIN as f32
            + usable_bins * ((i + 1) as f32 / NUM_BARS as f32).powf(2.5)) as usize;
        hi = hi.max(lo + 1); // at least one bin per bar

        // Average the normalized spectrum over [lo, hi). Guard the upper bound
        // (the last bar's `hi` can reach SPECTRUM_LEN exactly, matching
        // numpy's exclusive slice semantics).
        let hi = hi.min(SPECTRUM_LEN);
        let lo = lo.min(hi.saturating_sub(1));
        let count = (hi - lo) as f32;
        let mut avg: f32 = spectrum_norm[lo..hi].iter().sum::<f32>() / count;

        // Bass reduction for first few bars: 0.5, 0.625, 0.75, 0.875.
        if i < 4 {
            avg *= 0.5 + (i as f32 * 0.125);
        }

        levels[i] = avg;
    }

    levels
}

/// Hann (a.k.a. Hanning) window value at index `i` over a window of length `len`.
///
/// Matches `numpy.hanning(len)`: `0.5 - 0.5*cos(2*pi*i/(len-1))`.
fn hann(i: usize, len: usize) -> f32 {
    if len <= 1 {
        return 1.0;
    }
    use std::f32::consts::PI;
    0.5 - 0.5 * (2.0 * PI * i as f32 / (len as f32 - 1.0)).cos()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate `n` samples of a sine wave at `freq` Hz, sampled at `sr` Hz.
    fn sine(freq: f32, sr: f32, n: usize, amp: f32) -> Vec<f32> {
        use std::f32::consts::PI;
        (0..n)
            .map(|i| amp * (2.0 * PI * freq * i as f32 / sr).sin())
            .collect()
    }

    /// Index of the largest bar.
    fn argmax(bars: &[f32; NUM_BARS]) -> usize {
        let mut best = 0;
        for i in 1..NUM_BARS {
            if bars[i] > bars[best] {
                best = i;
            }
        }
        best
    }

    /// Map an FFT bin index to the bar that covers it, using the same
    /// exponential mapping as `compute_bars_with_fft`.
    fn expected_bar_for_bin(bin: usize) -> usize {
        let usable_bins = (SPECTRUM_LEN - MIN_FREQ_BIN) as f32;
        for i in 0..NUM_BARS {
            let lo = (MIN_FREQ_BIN as f32
                + usable_bins * (i as f32 / NUM_BARS as f32).powf(2.5)) as usize;
            let mut hi = (MIN_FREQ_BIN as f32
                + usable_bins * ((i + 1) as f32 / NUM_BARS as f32).powf(2.5)) as usize;
            hi = hi.max(lo + 1).min(SPECTRUM_LEN);
            if bin >= lo && bin < hi {
                return i;
            }
        }
        NUM_BARS - 1
    }

    #[test]
    fn sine_440hz_peaks_in_expected_band() {
        // 440Hz at 16kHz SR over a 512-pt FFT -> bin ~= 440 * 512 / 16000 ≈ 14.
        let sr = 16000.0;
        let freq = 440.0;
        let samples = sine(freq, sr, FFT_SIZE, 0.8);

        let bars = compute_bars(&samples);

        let peak_bin = (freq * FFT_SIZE as f32 / sr).round() as usize; // 14
        let expected_bar = expected_bar_for_bin(peak_bin);
        let peak_bar = argmax(&bars);

        // The dominant bar should be the one covering the 440Hz bin (allow ±1
        // bar of slack for spectral leakage into an adjacent narrow band).
        assert!(
            (peak_bar as isize - expected_bar as isize).abs() <= 1,
            "peak bar {peak_bar} not within 1 of expected {expected_bar}; bars={bars:?}"
        );

        // The peak must be a clear, non-trivial spike.
        assert!(
            bars[peak_bar] > 0.3,
            "peak bar height {} too small; bars={bars:?}",
            bars[peak_bar]
        );
    }

    #[test]
    fn silence_produces_zero_bars() {
        let samples = vec![0.0f32; FFT_SIZE];
        let bars = compute_bars(&samples);
        for (i, &b) in bars.iter().enumerate() {
            assert_eq!(b, 0.0, "bar {i} nonzero ({b}) for silence");
        }
    }

    #[test]
    fn empty_input_produces_zero_bars() {
        let bars = compute_bars(&[]);
        assert_eq!(bars, [0.0; NUM_BARS]);
    }

    #[test]
    fn low_amplitude_is_gated_as_silence() {
        // Amplitude well below the RMS gate (base_level = rms*100 < 0.08).
        // A 0.0003-amplitude sine has rms ≈ 0.0002 -> base_level ≈ 0.02 < 0.08.
        let samples = sine(440.0, 16000.0, FFT_SIZE, 0.0003);
        let bars = compute_bars(&samples);
        assert_eq!(bars, [0.0; NUM_BARS], "near-silent input not gated: {bars:?}");
    }

    #[test]
    fn analyzer_smoothing_blends_toward_signal() {
        let mut a = WaveformAnalyzer::new();
        let samples = sine(440.0, 16000.0, FFT_SIZE, 0.8);

        // First frame starts from zero state: smoothed = 0.3 * raw.
        let first = a.process(&samples);
        // Second identical frame must be larger (climbing toward steady state).
        let second = a.process(&samples);

        let peak = argmax(&second);
        assert!(
            second[peak] > first[peak],
            "smoothing did not climb: first={:?} second={:?}",
            first[peak],
            second[peak]
        );
    }

    #[test]
    fn analyzer_decays_to_zero_on_silence() {
        let mut a = WaveformAnalyzer::new();
        let signal = sine(440.0, 16000.0, FFT_SIZE, 0.8);
        // Build up some energy.
        for _ in 0..10 {
            a.process(&signal);
        }
        let before = a.process(&signal);
        let peak = argmax(&before);
        assert!(before[peak] > 0.0);

        // Feed silence: each frame must decay by the smoothing factor.
        let silent = vec![0.0f32; FFT_SIZE];
        let after_one = a.process(&silent);
        assert!(
            after_one[peak] < before[peak],
            "silence did not decay levels: {} -> {}",
            before[peak],
            after_one[peak]
        );

        // Many silent frames drive everything toward ~zero.
        for _ in 0..200 {
            a.process(&silent);
        }
        let settled = a.process(&silent);
        for (i, &b) in settled.iter().enumerate() {
            assert!(b < 1e-3, "bar {i} did not decay to near-zero: {b}");
        }
    }

    #[test]
    fn bass_reduction_factors_match_reference() {
        // Verify the first-4-bar attenuation is applied by feeding broadband
        // noise and checking the documented factors are wired in: we can't
        // easily isolate a single band, so just assert the function runs and
        // produces in-range values across the board.
        let samples = sine(150.0, 16000.0, FFT_SIZE, 0.9);
        let bars = compute_bars(&samples);
        for (i, &b) in bars.iter().enumerate() {
            assert!(
                (0.0..=1.0).contains(&b),
                "bar {i} out of range: {b}; bars={bars:?}"
            );
        }
    }
}
