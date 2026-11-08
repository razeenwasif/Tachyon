# Tachyon Next: concept for Apple silicon

A design concept for taking Tachyon to macOS on Apple silicon, as the primary
platform, with the Windows backend kept and sharing `src/term`. Nothing here is
built yet. It is written in the same spirit as [RESEARCH.md](RESEARCH.md):
every figure says where it comes from.

- **Measured**: on an M2 Pro MacBook Pro (6 performance + 4 efficiency cores,
  16 GB, macOS 27.0.1), 8 November 2026, with
  `cargo test --release --test throughput -- --ignored --nocapture`. One run.
- **v0.1**: the Windows build, per the README and RESEARCH.md (Ryzen 7 7800X3D).
- **Computed**: arithmetic on measured figures or constants.
- **Proposed**: design, not built.
- **Target**: a budget the Mac version has to hit.

The goal: draw only what changed, do no work at all when idle, keep a
replayable log of everything the terminal showed, and look and behave like a
Mac app. It is aimed at ML, AI, systems and HPC work driven from a MacBook,
with heavy jobs running elsewhere.

An interactive version, with a working window mockup, is at
<https://claude.ai/artifact/JyJLNGM4CJPxZWJXjW3mo2>.

---

## 0. What the M2 Pro measurement says

The portable core (`src/term`) builds and runs on macOS unchanged. The
throughput test gives:

| | M2 Pro (Measured) | Ryzen 7 7800X3D, native (v0.1) |
|---|---:|---:|
| Scanner, fast path | 8.28 GiB/s | ~19 GiB/s |
| Scanner, per byte | 6.03 GiB/s | ~2.6 GiB/s |
| Speedup | **1.37×** | 6.6–7.2× |
| Plain ASCII, end to end | 447.7 MiB/s | 482 MiB/s |
| Build log (SGR every 7 lines) | 223.2 MiB/s | 272 MiB/s |
| TUI full redraw | 222.8 MiB/s | 207 MiB/s |

Two conclusions:

1. **There is no NEON path.** `scan_printable_ascii` checks for AVX2 on x86 and
   otherwise falls back to `scan_swar`, so aarch64 gets the 8-bytes-at-a-time
   SWAR loop. On Apple silicon the per-byte loop is already fast, so SWAR buys
   only 1.37×.
2. **Scanning isn't the bottleneck here; applying is.** The scanner is 19×
   faster than end-to-end throughput. Every input byte becomes a 16-byte cell,
   so 448 MiB/s in is about 7.0 GiB/s of cell writes (Computed). A NEON scanner
   is still worth having, but the next version's speed comes from writing fewer
   bytes per character (see compact rows, §3).

---

## 1. Features

Each one builds on something v0.1 already has (a parser fast enough to replay
gigabytes, a per-row damage map) plus what macOS exposes. "Closest today" is as
far as we know, as of November 2026, from the public docs of Terminal.app,
iTerm2, Ghostty, WezTerm, kitty, Alacritty and Warp.

On the Mac, Ghostty is the closest relative: native AppKit and Metal, and
already Apple-styled. Being native isn't Tachyon's argument there. These
features and measured efficiency are.

### Rewind (and cell provenance)

Scrub the screen back to any moment, including the alternate screen, where
htop, asitop, vim and agent UIs draw and which scrollback never keeps.

- Each tab tees its raw PTY bytes into a compressed ring on disk. This is the
  stream `TACHYON_DUMP` already captures.
- Every 8 MiB of stream, store a viewport snapshot: 200×50 cells × 16 B =
  160 KB.
- To seek, load the nearest snapshot and replay at most 8 MiB. At the slowest
  rate measured on the M2 Pro (222.8 MiB/s), that's **≤ 36 ms** (Computed).
- **Cell provenance**: Option-click a cell in a rewound frame to see the escape
  sequence and byte offset that last wrote it.
- Keys: ⌘[ steps back, ⌘] returns to live.

Closest today: asciinema, outside the terminal. Ghostty's inspector shows live
cell state.

### Run receipts, with energy

Every command gets a receipt on its prompt line: exit code, wall time, CPU time
and its P-core share, energy, and peak memory footprint. On unified memory, the
footprint is the number that decides whether the next batch fits.

- Command boundaries come from OSC 133 marks.
- Figures come from `proc_pid_rusage` with `RUSAGE_INFO_V6` (P-core time,
  energy in nJ, lifetime peak footprint), summed over the command's process
  tree.
- Children are sampled every second and at exit through kqueue `NOTE_EXIT`.
  Very short-lived children can be missed, and the receipt marks the figure as
  sampled.
- Check that MPS/MLX allocations show up in the footprint before relying on
  it.
- A long command that finishes in the background posts a notification with
  its receipt.

Closest today: exit status and sometimes duration. Energy and P-core share per
command are new.

### Metric lens

Numbers in output that look like metrics (`loss=`, `acc`, `it/s`, `tok/s`)
become a sparkline on the prompt line. It's drawn in cells, so it copies as
text. A tqdm bar drives a progress bar on the Dock icon (`NSDockTile`). Only
dirty rows are scanned.

Closest today: TensorBoard or W&B, outside the terminal. Some terminals honour
OSC 9;4, which tqdm doesn't send.

### Rerun diff

Run a command again and lines that differ from the last run get a gutter mark.
Hover a mark to see the old line. Output is matched by structure first, then by
Myers diff on lines.

Closest today: a diff tool, by hand.

### Runs that outlive the window

`tachyon-host`, a login agent, owns the PTYs and Rewind logs; the app is a
client. Quitting, crashing or updating the app leaves runs going. Opt in per
command and the host holds an `IOPMAssertion` while it runs, so a local run
doesn't stall when the display sleeps. The assertion is released when the
receipt closes. The host sends damage-tracked rows to the app, not pixels.

Closest today: tmux or WezTerm's mux server, plus `caffeinate -i` by hand.

### Job tabs

The MacBook submits; forge and atlas run. When output contains a job ID (a
detached Nexus job, or Slurm's `Submitted batch job N`), offer to open it as a
tab. The tab follows the log, its title shows queued, running or done, and its
receipt lists node, wall time and exit state. Each source is a small adapter.
Nexus ships first, then Slurm.

Closest today: `nexus logs` or `squeue` plus `tail -f`, by hand.

### Latency ledger

Time every keystroke through its whole path and split the delay between shell,
ssh and terminal.

- Timestamps are `mach_absolute_time` readings at the key event, the PTY
  write, the first echoed byte, the apply and the command-buffer commit.
- When the frame reached the screen comes from the drawable's
  `presentedTime`.
- Each stage is also an `os_signpost` interval, so Instruments shows
  Tachyon's own frames.

Closest today: Typometer, or a high-speed camera.

---

## 2. The look: native where macOS is native, cells everywhere else

- **Apple's chrome, untouched.** A real `NSWindow` with native tabs, full
  screen, Stage Manager, the standard menu bar and ⌘ shortcuts (⌘T, ⌘W, ⌘K,
  ⌘F, ⌘D for split). The title bar and tabs use Liquid Glass through
  `NSGlassEffectView` on macOS 26+, and `NSVisualEffectView` before that.
- **Follows the system.** Paired light and dark palettes switch with the
  appearance. The cursor and selection use the system accent colour. Reduce
  Transparency makes the background opaque; Increase Contrast switches to a
  high-contrast palette.
- **The grid is the layout, inside.** Split dividers, the status line,
  receipts and the rewind track are drawn in cells. Splits land on cell
  boundaries. Command blocks are joined by a one-cell gutter mark, not cards.
- **SF Mono, rendered the Mac way.** `NSFont.monospacedSystemFont` by default,
  rasterised by Core Text at backing scale with grayscale antialiasing. Core
  Text shaping closes the ligature gap for fonts that have ligatures, and
  `CTFontCreateForString` replaces v0.1's static fallback chain.
- **Glass behind, opaque letters**, kept from v0.1.

### Window layouts (`window.chrome`)

- **Edge-to-edge** (`edge`). No title bar and no tab bar; the grid starts at
  the top edge.
  - The window buttons fade in when the pointer reaches the top-left corner,
    and the cells beneath them stay usable.
  - Tabs move into the existing status line, so they take no extra rows. The
    rewind track shares that row and appears on hover or ⌘[.
  - Drag the status line to move the window, double-click it to zoom, or ⌃⌘-drag
    anywhere.
  - Trade-off: tabs drawn in cells lose native tab behaviour (drag-out into a
    window, Merge All Windows), which has to be rebuilt.
  - Closest today: Ghostty's hidden title bar, iTerm2's minimal theme with
    bottom tabs, kitty's `hide_window_decorations`.
- **Standard** (`standard`, the default). A unified Liquid Glass title bar with
  native tabs. It costs roughly 50–60 pt, about 3 to 4 rows of 13 pt SF Mono at
  a 1.2 line height (15.6 pt a row) (Computed, approximate).
- **Full screen around the notch** (`window.fullscreen = notch`). Tabs to the
  left of the notch, status to the right (`NSScreen.auxiliaryTopLeftArea` /
  `auxiliaryTopRightArea`), and every row below it is output. Native full
  screen keeps windows below the notch, so this needs a borderless window
  covering the screen's full frame and gives up some of how Spaces treats
  full-screen apps. Prototype it before promising it.

Mac details: Quick Look on ⌘-click of a path, Secure Keyboard Entry that turns
on by itself while the PTY has echo off, ⌘↑/⌘↓ to jump between commands, and
independent left and right Option keys. These are expected on a Mac rather than
new.

---

## 3. The pipeline on Apple silicon

| Stage | v0.1 / Measured | Proposed |
|---|---|---|
| **Read** (new `pty/unix.rs`) | ConPTY reader thread, blocking `ReadFile`, 128 KiB reads | `forkpty`, same reader-thread design, tee into the Rewind log. Reader at `USER_INITIATED` QoS. Hidden or occluded window drops it to `UTILITY` so it can move to efficiency cores |
| **Scan** (`term/parser.rs`) | Measured: SWAR only, 8.28 vs 6.03 GiB/s, 1.37× | NEON scanner, two 16-byte vectors per step, narrow-by-4 in place of `movemask`. Keep valid multi-byte UTF-8 (tqdm `█▊`, box drawing) on the fast path |
| **Apply** (`term/grid.rs`) | Measured: 448 MiB/s plain, 223 MiB/s logs and TUI; ~7.0 GiB/s of cell writes (Computed) | **Compact rows**: bytes plus style runs until an escape edits mid-row, then expand to 16-B cells. About a sixteenth of the writes for plain output, and the same format serves as compressed scrollback. Hypothesis: measure it |
| **Build** (`render/mod.rs`) | Only dirty rows become instances; uploads into D3D11 dynamic buffers | Triple-buffered shared-storage `MTLBuffer` ring in unified memory: written directly, no upload, semaphore-guarded |
| **Draw** (new `render/metal.rs`) | Three instanced draws, DirectWrite atlas, ClearType dual-source blend | Same three passes in MSL. Core Text atlas at 2× with grayscale coverage; no dual-source blend needed. Colour emoji from Core Text |
| **Present** (`CAMetalLayer`) | Flip model, waitable latency object | `CADisplayLink` with `preferredFrameRateRange` from the two clocks: 120 Hz input, 30 fps output, paused when idle. Two drawables. Dirty rows render into a persistent texture, scrolling is a blit, then copy to the drawable. Compare against full redraws with Xcode GPU counters first; on a tile-based GPU a full redraw may be cheap enough |
| **Compose** (Core Animation) | DirectComposition; cursor drawn into the frame | Cursor as its own `CALayer` with a repeating `CABasicAnimation` on opacity, run by the window server. Non-opaque Metal layer over the glass material |

---

## 4. Efficiency

- **Zero-wake idle.** `BLINK_INTERVAL` is 530 ms, so v0.1 redraws about 1.9
  times a second while idle and focused (Computed). Target: 0, and Activity
  Monitor's Energy Impact reading 0.0.
- **Two clocks on ProMotion.** Keystroke frames ask for 120 Hz, output floods
  ask for 30. That's 75% fewer frames during a flood (Computed).
- **Scrollback in unified memory** (200 columns; compact estimate assumes
  70-character lines with two style runs, ~94 B a row):

  | Lines | 16 B cells | Share of 16 GB | Compact (est.) |
  |---:|---:|---:|---:|
  | 10,000 | 30.5 MiB | 0.2% | 0.9 MiB |
  | 100,000 | 305 MiB | 1.9% | 9.0 MiB |
  | 1,000,000 | 2.98 GiB | 18.6% | 89.6 MiB |

- **Big logs.** 2 GiB at the 223 MiB/s measured here is about 9.2 s to stream
  once. Seeks afterwards stay under 36 ms (Computed).

---

## 5. Budgets

| Budget | Today | Target |
|---|---|---|
| Keystroke to glass, terminal's share | Not yet measured | ≤ 1 ms + next refresh (8.3 ms at 120 Hz) |
| Energy Impact, idle and focused | ~1.9 redraws/s in v0.1 (Computed) | 0.0, display link paused |
| Window hidden or occluded | Not yet measured | 0 fps, reader on efficiency cores |
| Scanner on M2 Pro | 8.28 GiB/s (Measured) | ≥ 20 GiB/s with NEON |
| Plain ASCII end to end, M2 Pro | 448 MiB/s (Measured) | ≥ 1 GiB/s, with Rewind logging on |
| Log and TUI output, M2 Pro | 223 MiB/s (Measured) | ≥ 400 MiB/s |
| Scrollback, 100k lines × 200 cols | 305 MiB (Computed) | ≤ 16 MiB |
| Rewind seek | None | ≤ 40 ms |
| Launch to first prompt | Not yet measured | ≤ 150 ms, warm |
| App bundle | ~740 KB Windows binary (v0.1) | ≤ 2 MB, arm64 app plus host |

---

## 6. Build order

1. **macOS backend**: AppKit window via the `objc2` crates, `CAMetalLayer`,
   Core Text atlas and shaping, `forkpty`, the three passes ported from HLSL
   to MSL. The existing `#[cfg(windows)]` gates in `lib.rs` already separate
   the portable core. New `src/mac/`, `src/render/metal.rs`,
   `src/pty/unix.rs`.
2. **NEON scanner and compact rows**: the bottleneck measured on the M2 Pro.
   Re-run the throughput test before and after. `term/parser.rs`,
   `term/grid.rs`.
3. **Display-link pacing, two clocks, Core Animation cursor**: idle goes to
   zero.
4. **Apple look**: Liquid Glass, appearance-following palettes, accent cursor,
   native tabs, menus and shortcuts, accessibility settings. The edge-to-edge
   layout with cell tabs in the status line and hover-reveal window buttons.
   The notch full-screen mode as a prototype.
5. **Rewind log and snapshots**, with cell provenance. New `src/rewind/`.
6. **Shell integration and receipts**: OSC 133 for zsh first, then bash and
   fish. `proc_pid_rusage` receipts, rerun diff, notifications, ⌘↑/⌘↓.
7. **Metric lens and job tabs**: Nexus first, then Slurm. New `src/lens/`,
   `src/jobs/`.
8. **Session host**: `tachyon-host` as a login agent with keep-awake
   assertions. After Rewind, because it owns the logs.
9. **Split panes** on cell boundaries. Shaping already arrived with Core Text
   in step 1.
