#!/usr/bin/env python3
"""Original soundtrack for a GymClaw film, synthesized from scratch (stdlib only).

The page exports its cue list (render.mjs --cues), so music and foley stay in sync with the script.
Usage: python3 sound.py cues.json OUT.wav
"""
import json
import math
import random
import struct
import sys
import wave

RATE = 48000
TAU = math.tau
rng = random.Random(7)


class Bus:
    def __init__(self, seconds):
        self.n = int(RATE * seconds)
        self.l = [0.0] * self.n
        self.r = [0.0] * self.n

    def add(self, start, dur, fn, gain=1.0, pan=0.0):
        """Mix fn(local_t) into the stereo bus. pan -1 (left) .. 1 (right)."""
        gl, gr = gain * math.cos((pan + 1) * math.pi / 4), gain * math.sin((pan + 1) * math.pi / 4)
        for i in range(max(0, int(start * RATE)), min(self.n, int((start + dur) * RATE))):
            v = fn(i / RATE - start)
            self.l[i] += v * gl
            self.r[i] += v * gr

    def write(self, path, echo=.18):
        d = int(.375 * RATE)
        for i in range(d, self.n):
            self.l[i] += echo * self.r[i - d]
            self.r[i] += echo * self.l[i - d]
        peak = max(max(abs(math.tanh(x)) for x in self.l), max(abs(math.tanh(x)) for x in self.r))
        gain = .89 / peak
        with wave.open(path, 'wb') as w:
            w.setnchannels(2); w.setsampwidth(2); w.setframerate(RATE)
            w.writeframes(b''.join(struct.pack('<hh', int(math.tanh(a) * gain * 32767), int(math.tanh(b) * gain * 32767))
                                   for a, b in zip(self.l, self.r)))


def note(n):
    return 440 * 2 ** ((n - 69) / 12)


# ---- instruments: each returns fn(local_t) ----
def noise(lp=1.0):
    """Stateful one-pole filtered noise; lower lp is darker."""
    state = [0.0]
    def f():
        state[0] += lp * (rng.uniform(-1, 1) - state[0])
        return state[0]
    return f


def kick(t):
    return math.sin(TAU * (45 * t + 9 * (1 - math.exp(-28 * t)))) * math.exp(-7 * t)


def hat():
    n, prev = noise(1.0), [0.0]
    def f(t):
        x = n(); y = x - prev[0]; prev[0] = x
        return y * math.exp(-60 * t)
    return f


def clap():
    n = noise(.6)
    return lambda t: n() * (sum(math.exp(-180 * (t - d)) for d in (0, .011, .022) if t >= d) + .5 * math.exp(-18 * t)) * .6


def pluck(freq, decay=9.0, bright=3):
    return lambda t: sum(math.sin(TAU * freq * k * t) / k ** 1.4 for k in range(1, bright + 1)) * math.exp(-decay * t) * min(1, t * 400)


def keys(freq, decay=2.5):
    """Soft electric-piano-ish tone for the calmer story score."""
    return lambda t: (math.sin(TAU * freq * t + .8 * math.sin(TAU * freq * 2 * t) * math.exp(-6 * t))) * math.exp(-decay * t) * min(1, t * 300)


def pad(freqs, dur):
    def f(t):
        env = min(1, t / .25) * min(1, (dur - t) / .3)
        return env * sum(math.sin(TAU * fr * t + .3 * math.sin(TAU * .5 * t)) + .35 * math.sin(TAU * fr * 2.003 * t) for fr in freqs) / len(freqs)
    return f


def whoosh(dur, up=True, lp=.35):
    n = noise(lp)
    def f(t):
        p = t / dur
        return n() * (p ** 2 if up else (1 - p) ** 2) * math.sin(math.pi * min(1, p * 1.05))
    return f


def boom(t):
    return math.sin(TAU * (38 * t + 30 * (1 - math.exp(-12 * t)) / 12)) * math.exp(-2.6 * t)


def blip(f0, f1, dur=.09):
    return lambda t: math.sin(TAU * (f0 + (f1 - f0) * min(1, t / dur)) * t) * math.exp(-28 * t) * min(1, t * 900)


def bell(freq):
    return lambda t: (math.sin(TAU * freq * t) + .4 * math.sin(TAU * freq * 2.76 * t) * math.exp(-8 * t)) * math.exp(-4.5 * t) * min(1, t * 600)


def click():
    n = noise(1.0)
    return lambda t: n() * math.exp(-400 * t) + .5 * math.sin(TAU * 2400 * t) * math.exp(-300 * t)


def burst(decay, lp=.5):
    n = noise(lp)
    return lambda t: n() * math.exp(-decay * t)


# ---- score sections ----
def hook(bus, c):
    claw, flood = c['claw'], c['flood']
    for k in range(int(claw / .2)):  # clock ticks
        bus.add(k * .2, .05, lambda t: math.sin(TAU * 1700 * t) * math.exp(-120 * t), .16, -.3 + .1 * (k % 6))
    m = noise(.04)
    bus.add(0, flood + .04, lambda t: m() * min(1, t / .5) * 2.4, .5)
    bus.add(0, flood, lambda t: math.sin(TAU * (55 + 30 * t / flood * 1.4) * t) * min(1, t / .4), .18)
    for k in range(3):
        bus.add(claw - .02 + k * .05, .22, whoosh(.22, up=False, lp=.9), .36, -.5 + .5 * k)
    bus.add(claw - .35, .85, whoosh(.85), .32)
    impact(bus, flood, (57, 64, 69, 72, 76))


def impact(bus, at, chord, ring=1.6):
    bus.add(at, 2.0, boom, .9)
    bus.add(at, 1.2, burst(5), .35)
    for n in chord:
        fr = note(n)
        bus.add(at, ring, lambda t, fr=fr: math.sin(TAU * fr * t) * math.exp(-2.2 * t), .07)


GROOVE = [(57, (57, 60, 64)), (53, (53, 57, 60)), (48, (55, 60, 64)), (55, (55, 59, 62))]
LOFI = [(53, (57, 60, 64, 65)), (52, (55, 59, 62, 64)), (50, (53, 57, 60, 62)), (48, (52, 55, 59, 60))]


def music(bus, c, chapters):
    beat = 60 / c['bpm']; bar = beat * 4
    start = math.ceil(c['from'] / beat) * beat
    lofi = c.get('style') == 'lofi'
    chords = LOFI if lofi else GROOVE
    t0, i = start, 0
    while t0 < c['to'] - .01:
        dur = min(bar, c['to'] - t0)
        root, triad = chords[i % len(chords)]
        # Every other chapter drops the clap and arp for a breather.
        calm = sum(1 for a in chapters if a <= t0) % 2 == 1 and not lofi
        bus.add(t0, dur, pad([note(n) for n in triad], dur), .06 if lofi else .075)
        for e in range(8 if not lofi else 4):
            step = bar / (8 if not lofi else 4)
            if t0 + e * step < c['to']:
                bus.add(t0 + e * step, step, pluck(note(root - 12), 8 if lofi else 11, 3), .24 if e % 2 == 0 else .16)
        if lofi:
            for k, n in enumerate(triad):  # strummed keys, twice a bar
                for h in (0, bar / 2 + beat / 2):
                    if t0 + h + k * .03 < c['to']:
                        bus.add(t0 + h + k * .03, 1.6, keys(note(n + 12)), .045, -.3 + .2 * k)
        elif not calm:
            for s in range(16):
                if t0 + s * bar / 16 < c['to']:
                    n = triad[s % 3] + 12 * (1 + (s // 3) % 2)
                    bus.add(t0 + s * bar / 16, .2, pluck(note(n), 16, 2), .045, .4 * math.sin(s))
        for b in range(4):
            bt = t0 + b * beat
            if bt >= c['to']:
                break
            if lofi:
                if b in (0, 2) or (b == 3 and i % 2):
                    bus.add(bt + (beat / 2 if b == 3 else 0), .45, kick, .45)
                if b in (1, 3):
                    bus.add(bt + .012, .3, clap(), .2, .1)
                bus.add(bt + beat * .58, .08, hat(), .08, .35)  # swung offbeat hat
            else:
                bus.add(bt, .45, kick, .62)
                if b in (1, 3) and not calm:
                    bus.add(bt, .3, clap(), .32, .1)
                bus.add(bt + beat / 2, .08, hat(), .11, .35)
        t0 += bar; i += 1


def outro(bus, at, duration):
    bus.add(at - .4, .4, whoosh(.4), .3)
    ring = max(.6, duration - at)
    bus.add(at, ring, boom, .85)
    bus.add(at, 1.0, burst(4), .3)
    for n in (48, 55, 60, 64, 67, 72):
        fr = note(n)
        bus.add(at, ring, lambda t, fr=fr: (math.sin(TAU * fr * t) + .3 * math.sin(TAU * 2 * fr * t)) * math.exp(-1.6 * t) * min(1, (ring - t) / .4), .07)
    for k, n in enumerate((72, 76, 79, 84)):
        bus.add(at + .6 + k * .06, .7, pluck(note(n), 5, 3), .07, -.3 + .2 * k)


def foley(bus, c):
    k, t = c['kind'], c['t']
    if k == 'msg':
        bus.add(t, .2, blip(1046, 1568), .2, -.35)
    elif k == 'send':
        bus.add(t, .18, blip(880, 1320, .06), .2, .3); bus.add(t - .05, .25, whoosh(.25), .14)
    elif k == 'chip':
        bus.add(t, .15, blip(660, 880, .05), .08)
    elif k == 'key':
        bus.add(t, .04, click(), .13, -.3)
    elif k == 'tap':
        bus.add(t, .05, click(), .3, -.3)
    elif k == 'notif':
        bus.add(t, 1.2, bell(1318.5), .16, -.2); bus.add(t + .1, 1.2, bell(1760), .13, -.2)
    elif k == 'unlock':
        bus.add(t, .45, whoosh(.45), .22, .2)
    elif k == 'stamp':
        bus.add(t, .5, boom, .42); bus.add(t, .2, burst(25, .3), .3)
    elif k in ('swoosh', 'chapter'):
        bus.add(t - (.3 if k == 'chapter' else 0), .4, whoosh(.4), .16 if k == 'chapter' else .2, .3)
    elif k == 'drop':
        bus.add(t, .35, lambda x: math.sin(TAU * (90 - 40 * x) * x) * math.exp(-10 * x), .35); bus.add(t, .12, burst(40, .4), .15)
    elif k == 'pick':
        bus.add(t, .5, pluck(note((72, 76, 79)[c['n'] % 3]), 7, 3), .12, .3)
    elif k == 'climb':
        for j, n in enumerate((69, 72, 76, 79, 81)):
            bus.add(t + j * c['step'], .4, pluck(note(n), 7, 3), .12, .4)
    elif k == 'ding':
        bus.add(t, 1.2, bell(note(88)), .1, .4)
    elif k == 'scan':
        bus.add(t, c['dur'], lambda x: math.sin(TAU * (600 + 900 * x / c['dur']) * x) * .5 * math.sin(math.pi * x / c['dur']), .05, .2)
    elif k == 'buzz':  # phone vibrating on a surface
        bus.add(t, .5, lambda x: math.sin(TAU * 150 * x) * (1 if (x * 8) % 1 < .6 else .1) * math.exp(-1.5 * x), .22)
    elif k == 'ambience':  # filtered room tone for a scene
        n = noise(c.get('lp', .05)); dur = c['dur']
        bus.add(t, dur, lambda x: n() * min(1, x / .4, (dur - x) / .4) * 2, c.get('gain', .25))


def main(cue_path, out):
    spec = json.load(open(cue_path))
    cues, duration = spec['cues'], spec['duration']
    bus = Bus(duration)
    chapters = [c['t'] for c in cues if c['kind'] == 'chapter']
    for c in cues:
        if c['kind'] == 'hook':
            hook(bus, c)
        elif c['kind'] == 'music':
            music(bus, c, chapters)
        elif c['kind'] == 'outro':
            outro(bus, c['t'], duration)
        elif c['kind'] == 'impact':
            impact(bus, c['t'], (53, 60, 65, 69, 72))
        else:
            foley(bus, c)
    bus.write(out)


if __name__ == '__main__':
    main(sys.argv[1], sys.argv[2])
