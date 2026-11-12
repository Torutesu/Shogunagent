#!/usr/bin/env python3
"""Synthesize the music bed for one video: python3 audio.py <seconds> <out.wav> [cue,cue,...]

A slow four-chord pad, a quiet plucked arpeggio and a soft chime at each scene
change (the cue times). Generated, so there is nothing to license. Deterministic:
the same arguments give the same bytes.
"""
import sys
import wave

import numpy as np

SR = 48000
CHORDS = [  # D major, voiced wide: Dadd9, Bm7, Gmaj7, A6
    [146.83, 220.00, 369.99, 329.63],
    [123.47, 185.00, 293.66, 440.00],
    [98.00, 146.83, 246.94, 369.99],
    [110.00, 164.81, 277.18, 369.99],
]
BAR = 7.5  # seconds per chord


def envelope(n, attack, release):
    env = np.ones(n)
    a, r = int(attack * SR), int(release * SR)
    env[:a] = np.sin(np.linspace(0, np.pi / 2, a)) ** 2
    env[n - r:] *= np.cos(np.linspace(0, np.pi / 2, r)) ** 2
    return env


def pad_voice(freq, n, detune):
    t = np.arange(n) / SR
    f = freq * (1 + detune)
    tone = np.sin(2 * np.pi * f * t) + 0.32 * np.sin(2 * np.pi * 2 * f * t + 0.6) + 0.1 * np.sin(2 * np.pi * 3 * f * t + 1.1)
    return tone * (1 + 0.12 * np.sin(2 * np.pi * 0.19 * t + freq))


def pluck(freq, n):
    t = np.arange(n) / SR
    return (np.sin(2 * np.pi * freq * t) + 0.25 * np.sin(2 * np.pi * 2 * freq * t)) * np.exp(-t * 5.5) * np.minimum(1, t * 400)


def render(seconds, cues):
    total = int(seconds * SR)
    mix = np.zeros((total, 2))

    def add(signal, start, gain, pan):
        start = int(start * SR)
        if start >= total:
            return
        end = min(total, start + len(signal))
        chunk = signal[: end - start] * gain
        mix[start:end, 0] += chunk * np.cos(pan * np.pi / 2)
        mix[start:end, 1] += chunk * np.sin(pan * np.pi / 2)

    bars = int(np.ceil(seconds / BAR)) + 1
    overlap = 2.6
    for bar in range(bars):
        chord = CHORDS[bar % len(CHORDS)]
        n = int((BAR + overlap) * SR)
        env = envelope(n, 2.4, overlap)
        for i, freq in enumerate(chord):
            weight = [0.9, 0.7, 0.5, 0.42][i]
            add(pad_voice(freq, n, -0.0016) * env, bar * BAR, 0.05 * weight, 0.32)
            add(pad_voice(freq, n, 0.0016) * env, bar * BAR, 0.05 * weight, 0.68)
        # Arpeggio: eight steps a bar, up an octave, skipping the last step.
        step = BAR / 8
        order = [0, 2, 1, 3, 2, 1, 3]
        for k, idx in enumerate(order):
            when = bar * BAR + k * step
            if when < 6 or when > seconds - 5:  # stay out of the intro and the tail
                continue
            add(pluck(chord[idx] * 2, int(1.4 * SR)), when, 0.028, 0.35 + 0.3 * ((k * 7) % 5) / 4)

    for cue in cues:
        if cue < 0.5 or cue > seconds - 2:
            continue
        n = int(2.2 * SR)
        t = np.arange(n) / SR
        chime = (np.sin(2 * np.pi * 1174.66 * t) + 0.5 * np.sin(2 * np.pi * 1760 * t)) * np.exp(-t * 3.2) * np.minimum(1, t * 200)
        add(chime, cue, 0.022, 0.5)

    fade = np.ones(total)
    fi, fo = int(1.2 * SR), int(min(3.0, seconds / 3) * SR)
    fade[:fi] = np.linspace(0, 1, fi) ** 2
    fade[total - fo:] = np.linspace(1, 0, fo) ** 2
    mix *= fade[:, None]
    mix *= 10 ** (-9 / 20) / np.max(np.abs(mix))  # peak at -9 dBFS
    return mix


def main():
    seconds = float(sys.argv[1])
    cues = [float(x) for x in sys.argv[3].split(',')] if len(sys.argv) > 3 and sys.argv[3] else []
    pcm = (render(seconds, cues) * 32767).astype('<i2')
    with wave.open(sys.argv[2], 'wb') as out:
        out.setnchannels(2)
        out.setsampwidth(2)
        out.setframerate(SR)
        out.writeframes(pcm.tobytes())


if __name__ == '__main__':
    main()
