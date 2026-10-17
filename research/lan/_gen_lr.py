import wave, math, struct, sys
sr = 44100
def tone(freq, dur, amp=0.6):
    return [amp*math.sin(2*math.pi*freq*i/sr) for i in range(int(sr*dur))]
def sil(dur):
    return [0.0]*int(sr*dur)
segs = []
for v in tone(440, 4.0): segs.append((v, 0.0))
for _ in sil(1.5): segs.append((0.0, 0.0))
for v in tone(880, 4.0): segs.append((0.0, v))
for _ in sil(1.5): segs.append((0.0, 0.0))
data = bytearray()
for l, r in segs:
    data += struct.pack('<hh', int(l*32767), int(r*32767))
with wave.open(sys.argv[1], 'wb') as w:
    w.setnchannels(2); w.setsampwidth(2); w.setframerate(sr)
    w.writeframes(bytes(data))
print('wrote', sys.argv[1], round(len(segs)/sr, 2), 's  (0-4s LEFT 440Hz only, 5.5-9.5s RIGHT 880Hz only)')
