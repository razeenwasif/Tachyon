# Prints a training run's metrics as a real one would, for trying Tachyon's
# metric lens and training chart (⌥⌘C, or click the metrics on the command's
# line). Run it twice, say with a different --lr, to see this run against the
# last one:
#     python3 scripts/train-demo.py --lr 3e-4
#     python3 scripts/train-demo.py --lr 1e-3
import argparse, math, random, sys, time

ap = argparse.ArgumentParser()
ap.add_argument("--lr", type=float, default=3e-4)
ap.add_argument("--steps", type=int, default=400)
ap.add_argument("--delay", type=float, default=0.03)
a = ap.parse_args()

speed = math.log10(a.lr / 1e-5)  # a bigger learning rate learns faster, here
random.seed(int(a.lr * 1e7))
for step in range(0, a.steps + 1, 2):
    t = step / a.steps
    loss = 2.4 * math.exp(-speed * 2.2 * t) + 0.12 + random.gauss(0, 0.03)
    acc = 0.97 - 0.85 * math.exp(-speed * 2.0 * t) + random.gauss(0, 0.008)
    lr = a.lr * 0.5 * (1 + math.cos(math.pi * t))
    print(f"step={step} loss={loss:.4f} acc={acc:.4f} lr={lr:.2e} {random.uniform(40, 48):.1f}it/s", flush=True)
    time.sleep(a.delay)
print("done", file=sys.stderr)
