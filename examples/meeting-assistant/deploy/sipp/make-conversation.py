#!/usr/bin/env python3
"""Write the stand-in conference's conversation: G.711 µ-law RTP in a pcap, for SIPp's
play_pcap_audio. Two voice-like signals take turns — speaker A lower (a 140 Hz buzz with its
harmonics), speaker B higher (230 Hz) — each stretch with a syllable-rate swell, so a speech
detector sees four stretches of speech separated by pauses:

    A 2.0 s, pause 0.9 s, B 1.8 s, pause 0.9 s, A 2.4 s, pause 0.9 s, B 1.6 s, then 1.0 s quiet

The output is deterministic — running this again writes the same bytes.

    make-conversation.py <out.pcap>
"""
import math
import struct
import sys

RATE = 8000
TURNS = [("A", 2000), ("B", 1800), ("A", 2400), ("B", 1600)]
PAUSE_MS, TAIL_MS = 900, 1000
VOICES = {"A": 140.0, "B": 230.0}


def ulaw(sample):
    bias, clip = 0x84, 32635
    sign = 0x80 if sample < 0 else 0
    sample = min(abs(sample), clip) + bias
    exponent, mask = 7, 0x4000
    while exponent > 0 and not sample & mask:
        exponent -= 1
        mask >>= 1
    mantissa = (sample >> (exponent + 3)) & 0x0F
    return ~(sign | (exponent << 4) | mantissa) & 0xFF


def stretch(voice, ms):
    n = RATE * ms // 1000
    f0 = VOICES[voice]
    out = []
    for i in range(n):
        t = i / RATE
        # A smooth start and end, and a 4 Hz swell that never drops below half loudness.
        edge = min(1.0, i / 400, (n - 1 - i) / 400)
        swell = 0.75 + 0.25 * math.sin(2 * math.pi * 4 * t)
        tone = (0.6 * math.sin(2 * math.pi * f0 * t) + 0.3 * math.sin(2 * math.pi * 2 * f0 * t)
                + 0.1 * math.sin(2 * math.pi * 3 * f0 * t))
        out.append(int(10000 * edge * swell * tone))
    return out


def conversation():
    pcm = []
    for k, (voice, ms) in enumerate(TURNS):
        if k:
            pcm += [0] * (RATE * PAUSE_MS // 1000)
        pcm += stretch(voice, ms)
    return pcm + [0] * (RATE * TAIL_MS // 1000)


def write_pcap(path, pcm):
    with open(path, "wb") as f:
        f.write(struct.pack("<IHHiIII", 0xA1B2C3D4, 2, 4, 0, 0, 65535, 1))
        seq, ts, t = 1, 0, 0.0
        for k in range(0, len(pcm) - 159, 160):
            payload = bytes(ulaw(x) for x in pcm[k:k + 160])
            rtp = struct.pack("!BBHII", 0x80, 0, seq, ts, 0x5E1A0002) + payload
            udp = struct.pack("!HHHH", 6000, 6002, 8 + len(rtp), 0) + rtp
            ip = struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(udp), seq, 0, 64, 17, 0,
                             bytes([192, 0, 2, 1]), bytes([192, 0, 2, 2])) + udp
            frame = b"\x00" * 12 + b"\x08\x00" + ip
            sec = int(t)
            f.write(struct.pack("<IIII", sec, int((t - sec) * 1e6), len(frame), len(frame)) + frame)
            seq, ts, t = seq + 1, ts + 160, t + 0.02


if __name__ == "__main__":
    write_pcap(sys.argv[1], conversation())
