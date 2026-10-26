#!/usr/bin/env python3
"""Write a caller's side of a call as a pcap for SIPp's play_pcap_audio: G.711 µ-law silence,
with key presses as RFC 2833 telephone events (payload type 101) — what a phone sends when its
keys are pressed. Prints the stream's length in milliseconds.

    make-dtmf.py <out.pcap> <step>...

A step is a pause (`2500ms`, silence) or keys (`2`, `4321#`): each key is a 100 ms event
followed by 300 ms of silence. Example: `3000ms 2 4000ms 4321#` waits 3 s, presses 2, waits 4 s,
then enters 4, 3, 2, 1 and #.
"""
import struct
import sys

EVENTS = {**{str(d): d for d in range(10)}, "*": 10, "#": 11}
SSRC = 0x1D7F0001
SILENCE = bytes([0xFF]) * 160  # µ-law digital silence, 20 ms at 8 kHz


class Stream:
    def __init__(self, path):
        self.f = open(path, "wb")
        self.f.write(struct.pack("<IHHiIII", 0xA1B2C3D4, 2, 4, 0, 0, 65535, 1))
        self.seq, self.ts, self.t = 1, 0, 0.0

    def packet(self, payload_type, payload, marker=False, timestamp=None):
        first = 0x80 | payload_type if marker else payload_type
        rtp = struct.pack("!BBHII", 0x80, first, self.seq, self.ts if timestamp is None else timestamp,
                          SSRC) + payload
        udp = struct.pack("!HHHH", 6000, 6002, 8 + len(rtp), 0) + rtp
        ip = struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(udp), self.seq & 0xFFFF, 0, 64, 17, 0,
                         bytes([192, 0, 2, 1]), bytes([192, 0, 2, 2])) + udp
        frame = b"\x00" * 12 + b"\x08\x00" + ip
        sec = int(self.t)
        self.f.write(struct.pack("<IIII", sec, int(round((self.t - sec) * 1e6)), len(frame),
                                 len(frame)) + frame)
        self.seq = (self.seq + 1) & 0xFFFF
        self.t += 0.02

    def silence(self, ms):
        for _ in range(max(1, ms // 20)):
            self.packet(0, SILENCE)
            self.ts += 160

    def key(self, key):
        event, start = EVENTS[key], self.ts
        # 100 ms of tone: five updates with a growing duration, then the end of the event three
        # times (the end is repeated because RTP is unreliable).
        for n in range(1, 6):
            self.packet(101, struct.pack("!BBH", event, 10, 160 * n), marker=(n == 1), timestamp=start)
        for _ in range(3):
            self.packet(101, struct.pack("!BBH", event, 0x80 | 10, 800), timestamp=start)
        self.ts += 160 * 8
        self.silence(300)


if __name__ == "__main__":
    stream = Stream(sys.argv[1])
    stream.silence(200)
    for step in sys.argv[2:]:
        if step.endswith("ms"):
            stream.silence(int(step[:-2]))
        else:
            for key in step:
                stream.key(key)
    stream.silence(200)
    print(int(stream.t * 1000))
