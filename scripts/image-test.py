# Shows images with both protocols. Run it in Tachyon: python3 scripts/image-test.py
# (q=2 on the kitty commands: nothing here reads the terminal's replies.)

import base64, io, sys, time
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

out = sys.stdout.buffer
def w(b): out.write(b); out.flush()

# A training-style chart, as iTerm2's imgcat sends it.
x = np.arange(200)
fig, ax = plt.subplots(figsize=(6, 2.6), dpi=110)
ax.plot(x, 2.3 * np.exp(-x / 50) + 0.05 * np.random.rand(200), label="train loss")
ax.plot(x, 2.4 * np.exp(-x / 60) + 0.1, label="val loss")
ax.legend(); ax.set_xlabel("step"); fig.tight_layout()
buf = io.BytesIO(); fig.savefig(buf, format="png"); png = buf.getvalue()
w(b"iTerm2 protocol (OSC 1337), 60 columns wide:\r\n")
w(b"\x1b]1337;File=name=bG9zcy5wbmc=;size=%d;inline=1;width=60:" % len(png) + base64.b64encode(png) + b"\x07\r\n")
w(b"text after the chart\r\n")

# kitty protocol: raw RGBA, sent in 4096-byte chunks, with an id.
W, H = 240, 120
yy, xx = np.mgrid[0:H, 0:W]
rgba = np.dstack([(xx * 255 // W), (yy * 255 // H), np.full_like(xx, 180), np.full_like(xx, 255)]).astype(np.uint8).tobytes()
data = base64.b64encode(rgba)
w(b"kitty protocol (APC G), raw pixels in chunks:\r\n")
chunks = [data[i:i + 4096] for i in range(0, len(data), 4096)]
for i, c in enumerate(chunks):
    more = 1 if i < len(chunks) - 1 else 0
    keys = b"a=T,f=32,s=%d,v=%d,i=9,q=2,m=%d" % (W, H, more) if i == 0 else b"m=%d" % more
    w(b"\x1b_G" + keys + b";" + c + b"\x1b\\")
w(b"\r\nkitty PNG next to text: ")
w(b"\x1b_Ga=T,f=100,c=12,q=2;" + base64.b64encode(png) + b"\x1b\\")
w(b" <- 12 columns\r\n")
for i in range(3):
    w(b"line %d\r\n" % i)

