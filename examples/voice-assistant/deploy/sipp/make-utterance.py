#!/usr/bin/env python3
"""Write a G.711 µ-law RTP stream as a pcap for SIPp's play_pcap_audio: 400 ms of silence, a
1.5 s voice-like burst (two tones under a smooth envelope), then 1.5 s of silence — so a turn
detector sees speech start and, after its silence window, the end of the turn.

    make-utterance.py <out.pcap> [lead_ms speech_ms tail_ms]
"""
import math
import struct
import sys


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


def samples(lead_ms, speech_ms, tail_ms):
    out = [0] * (8 * lead_ms)
    n = 8 * speech_ms
    for i in range(n):
        envelope = math.sin(math.pi * i / n)
        out.append(int(9000 * envelope * (0.6 * math.sin(2 * math.pi * 300 * i / 8000)
                                          + 0.4 * math.sin(2 * math.pi * 700 * i / 8000))))
    return out + [0] * (8 * tail_ms)


def write_pcap(path, pcm):
    with open(path, "wb") as f:
        f.write(struct.pack("<IHHiIII", 0xA1B2C3D4, 2, 4, 0, 0, 65535, 1))
        seq, ts, t = 1, 0, 0.0
        for k in range(0, len(pcm) - 159, 160):
            payload = bytes(ulaw(x) for x in pcm[k:k + 160])
            rtp = struct.pack("!BBHII", 0x80, 0, seq, ts, 0x5E1A0001) + payload
            udp = struct.pack("!HHHH", 6000, 6002, 8 + len(rtp), 0) + rtp
            ip = struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(udp), seq, 0, 64, 17, 0,
                             bytes([192, 0, 2, 1]), bytes([192, 0, 2, 2])) + udp
            frame = b"\x00" * 12 + b"\x08\x00" + ip
            sec = int(t)
            f.write(struct.pack("<IIII", sec, int((t - sec) * 1e6), len(frame), len(frame)) + frame)
            seq, ts, t = seq + 1, ts + 160, t + 0.02


if __name__ == "__main__":
    lengths = [int(v) for v in sys.argv[2:5]] or [400, 1500, 1500]
    write_pcap(sys.argv[1], samples(*lengths))
