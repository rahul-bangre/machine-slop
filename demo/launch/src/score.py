import array
import math
import random
import sys
import wave

rate = 44100
seconds = 64
samples = array.array("f", [0]) * (rate * seconds)
rng = random.Random(75)


def tone(start, duration, frequency, gain, kind="note"):
    first = int(start * rate)
    count = min(int(duration * rate), len(samples) - first)
    phase = 0
    for i in range(count):
        t = i / rate
        progress = t / duration
        attack = min(1, t * 80)
        if kind == "kick":
            phase += 2 * math.pi * (45 + 110 * math.exp(-t * 30)) / rate
            value = math.sin(phase) * math.exp(-t * 14)
        elif kind == "hat":
            value = rng.uniform(-1, 1) * math.exp(-t * 75)
        elif kind == "sweep":
            phase += 2 * math.pi * (80 + 1400 * progress ** 3) / rate
            value = math.sin(phase) * math.sin(math.pi * progress) ** 2
        elif kind == "pad":
            value = (math.sin(t * frequency * math.tau) + 0.2 * math.sin(t * frequency * 2.004 * math.tau)) * math.sin(math.pi * progress) ** 2
        else:
            value = (math.sin(t * frequency * math.tau) + 0.18 * math.sin(t * frequency * 2 * math.tau)) * attack * math.exp(-t * 5)
        samples[first + i] += value * gain


for start, notes in [(0, [110, 164.81, 220]), (8, [130.81, 196, 261.63]), (16, [146.83, 220, 293.66]), (24, [110, 164.81, 220]), (32, [130.81, 196, 261.63]), (40, [146.83, 220, 293.66]), (48, [164.81, 246.94, 329.63]), (56, [110, 164.81, 220])]:
    for frequency in notes:
        tone(start, 8, frequency, 0.035, "pad")

for start, end in [(0, 6), (8, 28), (32, 38), (42, 60)]:
    beat = start
    step = 0
    while beat < end:
        tone(beat, 0.32, 0, 0.23 if start == 8 else 0.16, "kick")
        tone(beat + 0.25, 0.075, 0, 0.036, "hat")
        tone(beat, 0.55, [440, 659.25, 880, 587.33, 523.25, 783.99, 659.25, 987.77][step % 8], 0.04)
        beat += 0.5
        step += 1

for beat in [6, 8, 11.5, 23.5, 28, 32, 38, 42, 50, 55, 60]:
    for interval in [1, 1.5, 2]:
        tone(beat, 1.5, 220 * interval, 0.09)
tone(6, 2, 0, 0.08, "sweep")

peak = max(abs(value) for value in samples)
output = array.array("h")
for i, value in enumerate(samples):
    fade = min(1, i / (rate * 0.1), (len(samples) - i) / (rate * 1.4))
    output.append(round(value / max(1, peak) * 26000 * max(0, fade)))
with wave.open(sys.argv[1], "wb") as file:
    file.setnchannels(1)
    file.setsampwidth(2)
    file.setframerate(rate)
    file.writeframes(output.tobytes())
