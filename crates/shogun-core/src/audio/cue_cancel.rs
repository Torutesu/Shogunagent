//! Remove SHOGUN's own dictation start cue from the captured microphone stream (#951). Pure.
//!
//! The start cue plays only once the microphone is delivering frames, so the user can speak
//! on the cue without losing words. On built-in speakers the cue reaches the microphone. Dropping
//! the cue window would also drop any words spoken over it, so instead the known cue waveform is
//! located in the stream and subtracted, leaving the user's voice. Headphones and external
//! speakers far from the microphone produce no detectable echo; the window then passes unchanged.

use super::{resample, SAMPLE_RATE};

/// Output plus input latency allowed after playback is acknowledged, before the cue is heard.
const MAX_LAG: usize = SAMPLE_RATE as usize / 2;
/// Shortest captured stretch of cue worth matching (40 ms, the first pulse). Shorter slivers of
/// ordinary audio correlate with it by chance.
const MIN_OVERLAP: usize = SAMPLE_RATE as usize / 25;
/// Normalised correlation below which the cue is treated as absent from the microphone.
const MIN_CORRELATION: f32 = 0.15;
/// Echo-path filter models speaker/room colouring: taps before the detected lag, and in total.
const PRE_TAPS: usize = 16;
const TAPS: usize = 64;

/// Decode a canonical PCM16 WAV into the 16 kHz mono reference the capture path produces.
pub fn reference_from_wav(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.get(0..4)? != b"RIFF" || bytes.get(8..12)? != b"WAVE" {
        return None;
    }
    let le16 = |at: usize| Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?));
    let le32 = |at: usize| Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    let (mut at, mut format) = (12, None);
    while at + 8 <= bytes.len() {
        let id = bytes.get(at..at + 4)?;
        let len = le32(at + 4)? as usize;
        let body = at + 8;
        if id == b"fmt " {
            // (PCM tag, channels, rate, bits per sample)
            format = Some((le16(body)?, le16(body + 2)?, le32(body + 4)?, le16(body + 14)?));
        } else if id == b"data" {
            let (1, channels, rate, 16) = format? else {
                return None;
            };
            let samples: Vec<f32> = bytes
                .get(body..body + len)?
                .as_chunks::<2>().0.iter()
                .map(|s| i16::from_le_bytes([s[0], s[1]]) as f32 / 32_768.0)
                .collect();
            return Some(resample::to_16k_mono(
                &resample::to_mono(&samples, channels),
                rate,
            ));
        }
        at = body + len + (len & 1);
    }
    None
}

/// Holds the stream from the cue request until the cue's echo window has been captured, then
/// releases it with the echo removed. Samples before the request pass straight through.
pub struct CueCanceller<'a> {
    reference: &'a [f32],
    /// Stream position (16 kHz samples) at which the cue was requested.
    start: usize,
    /// Samples to hold from `start`: until the cue could have finished sounding.
    window: usize,
    seen: usize,
    held: Vec<f32>,
    done: bool,
}

impl<'a> CueCanceller<'a> {
    /// `requested_at` and `played_by` are stream positions when the cue was asked for and when
    /// playback was acknowledged; a slow output device stretches the window between them.
    pub fn new(reference: &'a [f32], requested_at: usize, played_by: usize) -> Self {
        Self {
            done: reference.is_empty(),
            reference,
            start: requested_at,
            window: played_by.saturating_sub(requested_at) + MAX_LAG + reference.len(),
            seen: 0,
            held: Vec::new(),
        }
    }

    /// Feed captured samples in order. Returns the samples now ready for transcription.
    pub fn push(&mut self, samples: &[f32]) -> Vec<f32> {
        if self.done {
            return samples.to_vec();
        }
        let before = self.start.saturating_sub(self.seen).min(samples.len());
        self.seen += samples.len();
        let mut ready = samples[..before].to_vec();
        self.held.extend_from_slice(&samples[before..]);
        if self.held.len() >= self.window {
            ready.append(&mut self.finish());
        }
        ready
    }

    /// Release everything held, e.g. when the user lets go before the echo window filled. A cue
    /// cut short by the release is still cancelled over the part that was captured.
    pub fn finish(&mut self) -> Vec<f32> {
        if !self.done {
            self.done = true;
            cancel(&mut self.held, self.reference);
        }
        std::mem::take(&mut self.held)
    }
}

fn cancel(signal: &mut [f32], reference: &[f32]) {
    let Some(lag) = locate(signal, reference) else {
        return;
    };
    // ponytail: one least-squares FIR fit over the cue window. 64 taps (4 ms) model the speaker
    // and the near path; a long room tail stays as low residual. Raise TAPS if device QA hears it.
    // Double-talk ceiling: voiced speech near the cue's ~200 Hz pitch can pass `locate` without
    // any echo; the fit then trims its in-band projection (~-13 dB in tests) for that window.
    // Platform voice processing (VoiceProcessingIO) is the upgrade if either shows on devices.
    let offset = lag as isize - PRE_TAPS as isize;
    let tap = |n: usize, k: usize| {
        let i = n as isize - offset - k as isize;
        usize::try_from(i)
            .ok()
            .and_then(|i| reference.get(i))
            .map_or(0.0, |v| f64::from(*v))
    };
    let from = offset.max(0) as usize;
    let to = (lag + reference.len() + TAPS).min(signal.len());
    // Normal equations. The user's voice is uncorrelated with the cue, so the fit leaves it in.
    let mut normal = [[0.0f64; TAPS]; TAPS];
    let mut target = [0.0f64; TAPS];
    for (n, sample) in signal.iter().enumerate().take(to).skip(from) {
        let x: [f64; TAPS] = std::array::from_fn(|k| tap(n, k));
        for (row, xi) in normal.iter_mut().zip(&x) {
            for (cell, xj) in row.iter_mut().zip(&x) {
                *cell += xi * xj;
            }
        }
        for (t, xi) in target.iter_mut().zip(&x) {
            *t += xi * f64::from(*sample);
        }
    }
    // The cue is narrowband; a small ridge keeps the out-of-band taps from blowing up.
    let ridge = (0..TAPS).map(|i| normal[i][i]).sum::<f64>() / TAPS as f64 * 1e-2 + 1e-12;
    for (i, row) in normal.iter_mut().enumerate() {
        row[i] += ridge;
    }
    let Some(weights) = solve(normal, target) else {
        return;
    };
    for (n, sample) in signal.iter_mut().enumerate().take(to).skip(from) {
        let estimate: f64 = weights.iter().enumerate().map(|(k, w)| w * tap(n, k)).sum();
        *sample -= estimate as f32;
    }
}

/// Gaussian elimination with partial pivoting.
fn solve(mut a: [[f64; TAPS]; TAPS], mut b: [f64; TAPS]) -> Option<[f64; TAPS]> {
    for col in 0..TAPS {
        let pivot = (col..TAPS).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
        if a[pivot][col].abs() < 1e-15 {
            return None;
        }
        a.swap(col, pivot);
        b.swap(col, pivot);
        for row in col + 1..TAPS {
            let factor = a[row][col] / a[col][col];
            let pivot_row = a[col];
            for (cell, p) in a[row].iter_mut().zip(&pivot_row).skip(col) {
                *cell -= factor * p;
            }
            b[row] -= factor * b[col];
        }
    }
    let mut x = [0.0f64; TAPS];
    for row in (0..TAPS).rev() {
        let tail: f64 = (row + 1..TAPS).map(|k| a[row][k] * x[k]).sum();
        x[row] = (b[row] - tail) / a[row][row];
    }
    Some(x)
}

/// The lag at which `reference` best matches `signal`, if the match is strong enough to be the
/// cue rather than coincidence. Either polarity counts, and a cue cut off by the end of `signal`
/// is matched over its captured part (normalised by the whole cue, so a sliver cannot match).
fn locate(signal: &[f32], reference: &[f32]) -> Option<usize> {
    let ref_energy: f64 = reference.iter().map(|v| f64::from(v * v)).sum();
    if signal.len() < MIN_OVERLAP || ref_energy == 0.0 {
        return None;
    }
    // f64 prefix sums of energy: window energy at any lag without drift.
    let mut prefix = Vec::with_capacity(signal.len() + 1);
    prefix.push(0.0f64);
    for v in signal {
        prefix.push(prefix[prefix.len() - 1] + f64::from(v * v));
    }
    let mut best = (0.0f64, 0usize);
    for lag in 0..=signal.len() - MIN_OVERLAP {
        let end = (lag + reference.len()).min(signal.len());
        let energy = prefix[end] - prefix[lag];
        if energy > 1e-9 {
            let dot: f32 = signal[lag..end]
                .iter()
                .zip(reference)
                .map(|(s, r)| s * r)
                .sum();
            let correlation = f64::from(dot).abs() / (energy * ref_energy).sqrt();
            if correlation > best.0 {
                best = (correlation, lag);
            }
        }
    }
    (best.0 >= f64::from(MIN_CORRELATION)).then_some(best.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The start cue the desktop app ships (copied under fixtures/ for this test), decoded as the app decodes it.
    fn cue() -> Vec<f32> {
        reference_from_wav(include_bytes!(
            "fixtures/voice-start.wav"
        ))
        .expect("shipped cue decodes")
    }

    /// Deterministic broadband stand-in for speech.
    fn voice(len: usize) -> Vec<f32> {
        let mut state = 0x2545_f491_u32;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state as f32 / u32::MAX as f32 - 0.5) * 0.2
            })
            .collect()
    }

    fn energy(samples: &[f32]) -> f32 {
        samples.iter().map(|v| v * v).sum()
    }

    fn run(canceller: &mut CueCanceller, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::new();
        for chunk in input.chunks(160) {
            out.extend(canceller.push(chunk));
        }
        out.extend(canceller.finish());
        out
    }

    /// The cue coloured by a three-tap speaker path, ~8 dB under a voice over the cue window,
    /// heard `delay` samples after the request. Returns suppression of the error against the
    /// clean voice, in dB, after cancelling the first `len` samples.
    fn suppression_db(polarity: f32, delay: usize, played_by: usize, len: usize) -> f32 {
        let reference = cue();
        let start = 1_600;
        let voice = voice(32_000);
        let mut mic = voice.clone();
        for (i, r) in reference.iter().enumerate() {
            for (k, h) in [0.3f32, -0.12, 0.05].iter().enumerate() {
                mic[start + delay + i + k] += polarity * h * r;
            }
        }
        let (mic, voice) = (&mic[..len], &voice[..len]);
        let echo: Vec<f32> = mic.iter().zip(voice).map(|(m, v)| m - v).collect();

        let out = run(&mut CueCanceller::new(&reference, start, played_by), mic);

        assert_eq!(out.len(), len);
        assert_eq!(out[..start], mic[..start]);
        let residual: Vec<f32> = out.iter().zip(voice).map(|(o, v)| o - v).collect();
        10.0 * (energy(&echo) / energy(&residual)).log10()
    }

    #[test]
    fn removes_a_delayed_coloured_cue_and_keeps_the_voice_over_it() {
        let db = suppression_db(1.0, 2_400, 1_600, 32_000);
        assert!(db >= 15.0, "cue suppressed by {db} dB");
    }

    #[test]
    fn removes_an_inverted_cue() {
        let db = suppression_db(-1.0, 2_400, 1_600, 32_000);
        assert!(db >= 15.0, "cue suppressed by {db} dB");
    }

    #[test]
    fn waits_for_a_cue_that_a_slow_output_device_played_late() {
        // Playback acknowledged 1.2 s after the request, heard 1.4 s after it.
        let db = suppression_db(1.0, 22_400, 20_800, 32_000);
        assert!(db >= 15.0, "cue suppressed by {db} dB");
    }

    #[test]
    fn release_during_the_cue_still_cancels_what_was_captured() {
        // Released halfway through the cue's echo.
        let db = suppression_db(1.0, 2_400, 1_600, 1_600 + 2_400 + 1_760);
        assert!(db >= 10.0, "cue suppressed by {db} dB");
    }

    #[test]
    fn passes_audio_unchanged_when_the_cue_never_reached_the_microphone() {
        let input = voice(32_000);
        let out = run(&mut CueCanceller::new(&cue(), 1_600, 1_600), &input);
        let changed: Vec<f32> = out.iter().zip(&input).map(|(o, i)| o - i).collect();
        let changed_db = 10.0 * (energy(&changed) / energy(&input[1_600..13_120])).log10();
        eprintln!("CHANGED {changed_db}");
        assert_eq!(out, input);
    }

    #[test]
    fn bounds_the_change_to_voiced_speech_in_the_cue_pitch_range() {
        // f0 sweeping 180–220 Hz with five harmonics, syllable-rate (4 Hz) envelope: the worst
        // case for chance correlation with the ~200 Hz cue. It can be mistaken for the cue (the
        // double-talk ceiling noted in `cancel`); the fit then removes only its in-band
        // projection, which must stay well under the voice.
        let input: Vec<f32> = (0..32_000)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                let f0 = 200.0 + 20.0 * (2.0 * std::f32::consts::PI * 0.7 * t).sin();
                let envelope = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * 4.0 * t).cos();
                let tone: f32 = (1..=5)
                    .map(|h| (2.0 * std::f32::consts::PI * f0 * h as f32 * t).sin() / h as f32)
                    .sum();
                0.05 * envelope * tone
            })
            .collect();
        let out = run(&mut CueCanceller::new(&cue(), 1_600, 1_600), &input);
        let changed: Vec<f32> = out.iter().zip(&input).map(|(o, i)| o - i).collect();
        // Relative to the voice over the held window (cue + 0.5 s lag).
        let changed_db = 10.0 * (energy(&changed) / energy(&input[1_600..13_120])).log10();
        assert!(changed_db <= -10.0, "voice changed by {changed_db} dB");
    }

    #[test]
    fn early_release_flushes_every_held_sample() {
        let input = voice(4_000);
        let reference = cue();
        let mut canceller = CueCanceller::new(&reference, 1_000, 1_000);
        let mut out = canceller.push(&input);
        assert_eq!(out.len(), 1_000);
        out.extend(canceller.finish());
        assert_eq!(out, input);
    }

    #[test]
    fn decodes_pcm16_wav_to_16k_mono() {
        let frames: [i16; 6] = [16_384, 16_384, -16_384, -16_384, 0, 0];
        let data: Vec<u8> = frames.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        for v in [1u16, 2] {
            wav.extend_from_slice(&v.to_le_bytes());
        }
        wav.extend_from_slice(&16_000u32.to_le_bytes());
        wav.extend_from_slice(&64_000u32.to_le_bytes());
        for v in [4u16, 16] {
            wav.extend_from_slice(&v.to_le_bytes());
        }
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);

        assert_eq!(reference_from_wav(&wav), Some(vec![0.5, -0.5, 0.0]));
        assert_eq!(reference_from_wav(b"not a wav"), None);
    }
}
