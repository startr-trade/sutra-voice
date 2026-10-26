#!/usr/bin/env python3
"""Write the IVR's stand-in prompts: one short WAV file per prompt (8 kHz, 16-bit PCM, mono).

No text-to-speech engine is assumed to be available offline, so each prompt is a distinct
pattern of tones rather than speech: 100 ms of silence (so two prompts played back to back stay
apart), then a continuous tone that steps through a few pitches, with a smooth start and end. Every prompt has its own length, so a test listening to the call can tell
them apart. The output is deterministic — running this again writes the same bytes.

    make-prompts.py [out_dir]          (default: the directory this script is in)

Replace any of them with a real recording of the same name: a WAV file, any sample rate, PCM16,
µ-law or A-law; the engine resamples it to the call's rate when it plays it.
"""
import math
import os
import struct
import sys

RATE = 8000

# name -> (duration of the tone in ms, the pitches it steps through, in Hz). The words a real recording
# would say are in the README.
PROMPTS = {
    "welcome": (1200, [440, 550, 660, 550]),
    "menu": (800, [550, 660]),
    "invalid": (500, [300]),
    "enter-pin": (700, [880, 990, 880, 990]),
    "wrong-pin": (1000, [660, 440]),
    "transferring": (600, [500, 600, 700]),
    "goodbye": (900, [660, 550, 440]),
}


def samples(duration_ms, pitches):
    n = RATE * duration_ms // 1000
    step = n / len(pitches)
    out, phase = [0] * (RATE // 10), 0.0
    for i in range(n):
        hz = pitches[min(int(i / step), len(pitches) - 1)]
        phase += 2 * math.pi * hz / RATE
        # 10 ms fade in and out, so the prompt starts and ends without a click.
        fade = min(1.0, i / 80, (n - 1 - i) / 80)
        out.append(int(9000 * fade * math.sin(phase)))
    return out


def wav(pcm):
    data = struct.pack("<%dh" % len(pcm), *pcm)
    header = struct.pack("<4sI4s4sIHHIIHH4sI", b"RIFF", 36 + len(data), b"WAVE", b"fmt ", 16,
                         1, 1, RATE, RATE * 2, 2, 16, b"data", len(data))
    return header + data


if __name__ == "__main__":
    out_dir = sys.argv[1] if len(sys.argv) > 1 else os.path.dirname(os.path.abspath(__file__))
    for name, (duration_ms, pitches) in PROMPTS.items():
        with open(os.path.join(out_dir, name + ".wav"), "wb") as f:
            f.write(wav(samples(duration_ms, pitches)))
