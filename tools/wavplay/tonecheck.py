#!/usr/bin/env python3
"""Sample-exact check of a captured `wavplay --tone` against what wavplay
generates: 440 Hz, amplitude 0.35, 20 ms fade in/out, 3 s, 48 kHz S16LE.

Usage:  tonecheck.py capture.wav [--hz 440]

Capture the guest's audio to a file rather than to the speakers, e.g. with
QEMU:

    -audiodev wav,id=snd0,path=out.wav,out.frequency=48000,out.channels=2,out.format=s16
    -device intel-hda,id=snd -device hda-output,bus=snd.0,cad=0,audiodev=snd0

then play the tone in the guest and run this on out.wav. The capture is
walked in 64-frame windows against the expected waveform; after a mismatch
the expected phase is searched within +-4096 frames, so a repeated or
dropped block registers as a "slip" with its size. A clean run reads

    checked N frames, 0 slips, peak 11468

and says the driver delivered every sample; dropouts that are heard but
not seen here happen after the capture point (the host's audio backend).
"""
import math
import struct
import sys
import wave

RATE = 48000
AMP = 0.35
SECS = 3.0
FADE = 0.02
WINDOW = 64
TOL = 40
ONSET = 200
SEARCH = 4096


def expected(freq):
    frames = int(RATE * SECS)
    env_len = RATE * FADE
    out = []
    for i in range(frames):
        env = min(i / env_len, (frames - i) / env_len, 1.0)
        out.append(int(math.sin(2 * math.pi * freq * i / RATE) * AMP * env * 32767))
    return out


def main():
    args = sys.argv[1:]
    freq = 440.0
    if "--hz" in args:
        k = args.index("--hz")
        freq = float(args[k + 1])
        del args[k : k + 2]
    if len(args) != 1:
        print(__doc__)
        return 2
    exp = expected(freq)
    frames = len(exp)
    w = wave.open(args[0])
    if w.getframerate() != RATE or w.getsampwidth() != 2:
        print(f"expected {RATE} Hz 16-bit, got {w.getframerate()} Hz {8 * w.getsampwidth()}-bit")
        return 2
    n = w.getnframes()
    ch = w.getnchannels()
    raw = w.readframes(n)
    left = struct.unpack("<%dh" % (ch * n), raw)[0::ch]

    start = next((i for i, x in enumerate(left) if abs(x) > ONSET), None)
    if start is None:
        print("silence: no tone found")
        return 1
    peak = max(abs(x) for x in left)
    j = next(i for i, x in enumerate(exp) if abs(x) > ONSET)
    i = start
    checked = slips = 0
    while i + WINDOW <= n and j + WINDOW <= frames:
        cap = left[i : i + WINDOW]
        if max(abs(a - b) for a, b in zip(cap, exp[j : j + WINDOW])) <= TOL:
            i += WINDOW
            j += WINDOW
            checked += WINDOW
            continue
        best = None
        for d in range(-SEARCH, SEARCH + 1):
            jj = j + d
            if jj < 0 or jj + WINDOW > frames:
                continue
            if max(abs(a - b) for a, b in zip(cap, exp[jj : jj + WINDOW])) <= TOL:
                best = d
                break
        if best is None:
            print(f"unrecoverable mismatch at capture frame {i} (expected index {j})")
            break
        slips += 1
        print(f"slip at capture frame {i}: expected index {j} -> {j + best} ({best:+d} frames, {best * 1000 / RATE:+.1f} ms)")
        j += best
    print(f"checked {checked} frames, {slips} slips, peak {peak}")
    return 0 if slips == 0 and checked > 0 else 1


if __name__ == "__main__":
    sys.exit(main())
