# Shows images with each protocol Tachyon reads. Run it in Tachyon:
#     python3 scripts/image-test.py
# Inside tmux, the kitty and iTerm2 images are wrapped for tmux's passthrough
# (`set -g allow-passthrough on` in tmux.conf), and sixel goes through tmux
# itself (tmux 3.4 or later, built with sixel, as Homebrew's is).
# The kitty commands carry q=2: nothing here reads the terminal's replies.

import base64, io, os, sys, time
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

out = sys.stdout.buffer
TMUX = bool(os.environ.get("TMUX"))


def w(b):
    out.write(b)
    out.flush()


def passthrough(seq):
    """An escape sequence for the terminal outside tmux, when inside it."""
    if not TMUX:
        return seq
    return b"\x1bPtmux;" + seq.replace(b"\x1b", b"\x1b\x1b") + b"\x1b\\"


def make_room(rows):
    """Inside tmux, which doesn't know a passed-through image takes space,
    move down past it so the text after it doesn't land on it."""
    if TMUX:
        w(b"\r\n" * rows)


def sixel(rgba, width, height):
    """RGBA pixels as sixel, in a 6x6x6 colour cube."""
    px = np.frombuffer(rgba, np.uint8).reshape(height, width, 4)[:, :, :3].astype(np.int32)
    idx = (px[:, :, 0] * 5 // 255) * 36 + (px[:, :, 1] * 5 // 255) * 6 + (px[:, :, 2] * 5 // 255)
    s = [b'\x1bPq"1;1;%d;%d' % (width, height)]
    for i in range(216):
        r, g, b = i // 36, i // 6 % 6, i % 6
        s.append(b"#%d;2;%d;%d;%d" % (i, r * 20, g * 20, b * 20))
    for band in range(0, height, 6):
        rows = idx[band:band + 6]
        for c in np.unique(rows):
            bits = np.zeros(width, np.int32)
            for k in range(rows.shape[0]):
                bits |= (rows[k] == c).astype(np.int32) << k
            s.append(b"#%d" % c + bytes((bits + 63).astype(np.uint8)) + b"$")
        s.append(b"-")
    return b"".join(s) + b"\x1b\\"


# A training-style chart.
x = np.arange(200)
fig, ax = plt.subplots(figsize=(6, 2.6), dpi=110)
ax.plot(x, 2.3 * np.exp(-x / 50) + 0.05 * np.random.rand(200), label="train loss")
ax.plot(x, 2.4 * np.exp(-x / 60) + 0.1, label="val loss")
ax.legend()
ax.set_xlabel("step")
fig.tight_layout()
buf = io.BytesIO()
fig.savefig(buf, format="png")
png = buf.getvalue()
fig.canvas.draw()
cw, ch = fig.canvas.get_width_height()
chart_rgba = bytes(fig.canvas.buffer_rgba())

w(b"iTerm2 protocol (OSC 1337), 60 columns wide:\r\n")
w(passthrough(b"\x1b]1337;File=name=bG9zcy5wbmc=;size=%d;inline=1;width=60:" % len(png) + base64.b64encode(png) + b"\x07"))
make_room(13)
w(b"\r\ntext after the chart\r\n")

# kitty: raw RGBA, sent in 4096-byte chunks, with an id.
W, H = 240, 120
yy, xx = np.mgrid[0:H, 0:W]
rgba = np.dstack([(xx * 255 // W), (yy * 255 // H), np.full_like(xx, 180), np.full_like(xx, 255)]).astype(np.uint8).tobytes()
data = base64.b64encode(rgba)
w(b"kitty protocol (APC G), raw pixels in chunks:\r\n")
chunks = [data[i:i + 4096] for i in range(0, len(data), 4096)]
for i, c in enumerate(chunks):
    more = 1 if i < len(chunks) - 1 else 0
    keys = b"a=T,f=32,s=%d,v=%d,i=9,q=2,m=%d" % (W, H, more) if i == 0 else b"m=%d" % more
    w(passthrough(b"\x1b_G" + keys + b";" + c + b"\x1b\\"))
make_room(4)
w(b"\r\nkitty PNG next to text: ")
w(passthrough(b"\x1b_Ga=T,f=100,c=12,q=2;" + base64.b64encode(png) + b"\x1b\\"))
if TMUX:
    w(b"\x1b[12C")  # past it, as the terminal outside moved
w(b" <- 12 columns\r\n")
make_room(2)

w(b"sixel (through tmux itself, when in it):\r\n")
w(sixel(chart_rgba, cw, ch))
w(b"\r\n")
for i in range(3):
    w(b"line %d\r\n" % i)
