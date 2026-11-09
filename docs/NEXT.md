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

That first run led to two hypotheses: aarch64 needed a NEON scanner, and the
cost was "16 bytes per character written". The first held. Profiling showed the
second was wrong in an instructive way.

### Update, 9 November 2026: NEON scanner and packed scrollback (done)

- **NEON scanner.** Two 16-byte vectors per step, `vminvq` for the all-printable
  case, narrow-by-4 to locate the stop byte. 13.8–14.2 GiB/s, up from 7.0–8.3
  with SWAR. Short of the 20 GiB/s target: terminal output stops at every line
  ending, so much of each call is finding the stop, not scanning.
- **Where the time actually went.** `Grid::scroll_up` took about 60% of samples
  on plain output, and throughput fell with scrollback size alone: 681 MiB/s
  with none, 481 with 10,000 lines, 452 with 50,000. The ring held every
  scrollback line at full width (10,000 x 200 x 16 B = 32 MB, beyond the
  caches), and each scroll recycled a row that had long left the cache. The
  cost was the *footprint* of 16-byte cells in history, not writing them.
- **Packed history.** The screen keeps full rows; a line leaving the top is
  packed into one allocation: one byte per character when the line is ASCII,
  plus 14-byte style runs, trailing blanks dropped. The common case (one style,
  all ASCII) is checked and narrowed with NEON `vld4q_u32`, which splits four
  cells into character and style lanes in one load. Once scrollback is full the
  evicted line's buffer is reused, so packing allocates nothing. Clearing a
  recycled row now touches only its content, with a constant zero fill that
  compiles to `memset`.

Measured on the M2 Pro, `cargo test --release --test throughput`, five runs:

| | Before | After | Change |
|---|---:|---:|---:|
| Scanner | 7.0–8.3 GiB/s | 13.8–14.2 GiB/s | +80% |
| Plain ASCII, end to end | 429–455 MiB/s | 657–662 MiB/s | +49% |
| Build log (SGR every 7 lines) | 210–230 MiB/s | 291–296 MiB/s | +33% |
| TUI full redraw | 206–222 MiB/s | 244–247 MiB/s | +14% |

Memory, 100,000 lines of build-log output in scrollback (peak RSS of the test
process, about 25 MiB of which is its input buffer): **420 MiB before, 78 MiB
after**, roughly 7x less for the scrollback itself. That is about 550 bytes per
packed line in practice once allocator overhead and buffer capacity are counted,
not the ~94 bytes estimated in §4.

Still short of the 1 GiB/s target. For build logs, scrolling has dropped out
of the profile and parsing and writing cells dominate; for long plain lines,
packing a full 200-character line is now about half the cost.

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

*Built (9 November 2026)*, with snapshots every 2 MiB in memory and every
8 MiB on disk rather than the 8 MiB planned above; see step 5 of the plan.

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

*Built (9 November 2026):* `src/mac/ledger.rs`. Stamps on
`CLOCK_UPTIME_RAW` (the clock of `NSEvent.timestamp` and
`CACurrentMediaTime`): the key event, the PTY write, the first byte read
back, its parse, the command-buffer commit, and the drawable's
`presentedTime` through `addPresentedHandler`. Each stage is an
`os_signpost` interval (`dev.tachyon.terminal`, Points of Interest), emitted
through `_os_signpost_emit_with_name_impl`. ⌥⌘L shows the last keystroke and
the median of up to 200. First reading on the M2 Pro, under heavy load, with
synthetic keys: about 35 ms median, mostly display (commit to presented,
about 27 ms) and frame wait (about 8 ms); shell echo 0.1 ms; Tachyon 4.6 ms,
mostly event delivery of the synthetic key. So the next latency work is the
present path (drawable count, display sync), not parsing.
*Present path done (9 November 2026):* an idle window draws as soon as output
is parsed instead of at the link's next tick; `render.low_latency` (default
on) turns display sync off in a window, where the compositor syncs anyway, and
back on in full screen. The link asks for 120 Hz, falling to 60 Hz after 30
frames of continuous output. Measured alternating with the previous build:
median keystroke to screen 33.0–35.0 → 23.0–25.0 ms. Frame wait went from
8–12 to 1–2 ms, display from 19–24 to 17–18 ms, about two 120 Hz refreshes,
which is the compositor. In-app `cat` of 256 MB: 125–140 → 145–165 MiB/s.

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

### Icon

A block cursor on a near-black squircle, with a motion trail fading off to the
left and a faint blue glow behind it. No `>_` chevron, no text, one accent.

- `assets/icon/tachyon.svg`: the 1024 pt master on Apple's 824 pt icon grid
  (superellipse, exponent 5).
- `assets/icon/tachyon-small.svg`: for 16–32 pt. The trail and glow are
  dropped and the cursor grows.
- `assets/icon/tachyon-mark.svg`: the foreground layer for Icon Composer. On
  macOS 26 and later, shipping the squircle and the mark as separate layers
  lets the system generate the Clear and Tinted appearances. The icon is dark
  by design, so Default and Dark are the same.
- `build.rs` draws the same mark for the Windows `.ico`. Its trail and glow
  weights are scaled because it blends in linear light, where the SVG's sRGB
  opacities would come out about three times brighter.

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
   *Liquid Glass and appearance done (9 November 2026):* an NSGlassEffectView
   behind a non-opaque Metal layer (NSVisualEffectView before macOS 26), with
   only the default background translucent; Reduce Transparency makes it
   opaque. A light theme with its own ANSI set follows Light and Dark Mode
   through viewDidChangeEffectiveAppearance; `accent` for the cursor and
   selection. Then a clearer default (0.25 over regular glass, corner radius
   16 so the rim follows the window) and the tab strip as a floating glass
   capsule drawn by AppKit, since Liquid Glass shows its refraction at the
   edges of shapes floating over content. Then the tint moved into the glass
   (NSGlassEffectView.tintColor, 0.75 of the theme background, clear style)
   with the Metal background at 0.05: darker and still refracting. Still to do: the Increase Contrast
   palette.
   *Tabs done (9 November 2026):* a strip drawn in cells along the bottom
   edge, shown only with two or more tabs. Tabs are named from OSC 7 (the
   folder), the command line the zsh integration reports with OSC 133;C, or a
   title the running program set; a background tab whose command finishes gets
   a green or red dot. ⌘T, ⌘W, ⌘1–9, ⌘⇧[ ], ⌃⇥, click and middle-click.
5. **Rewind log and snapshots**, with cell provenance. New `src/rewind/`.
   *First version done (9 November 2026):* `src/term/rewind.rs` keeps each
   tab's reads with their times and resizes in memory, up to 32 MiB, with a
   screen snapshot (screen and alternate screen, no scrollback) every 2 MiB
   taken while the parser is in Ground. A frame is a burst of reads less than
   40 ms apart; ⌘[ / ⌘] replay from the nearest snapshot, Esc returns to live,
   and a glass badge says when. Recording measured 375 against 379 MiB/s
   parsing alone.
   *Finished (9 November 2026):* past 32 MiB, output moves to disk in 8 MiB
   LZ4 segments (one snapshot each, up to 512 MiB a tab) under
   `~/Library/Caches/Tachyon/Rewind/<pid>-<tab>`, written by a background
   thread and deleted on tab close, quit, or the next launch after a crash. The
   badge has a slider over every frame kept, and the wheel steps frames.
   ⌥-click shows which bytes last wrote a cell (or scrolled it there), their
   stream offset and how long before the frame. Measured: seek 3.9 ms in
   memory, 15.8 ms on disk; recording 1 to 4% of parsing throughput.
6. **Shell integration and receipts**: OSC 133 for zsh first, then bash and
   fish. `proc_pid_rusage` receipts, rerun diff, notifications, ⌘↑/⌘↓.
   *Done for zsh (9 November 2026):* marks stored on rows so they survive
   packing and reflow; receipts with exact CPU time (shell child accounting)
   and sampled P-core share, energy and peak footprint; gutter bars; ⌘↑/⌘↓;
   Dock bounce for long commands. Still to do: rerun diff.
   *bash and fish done (9 November 2026):* bash through `--rcfile` (3.2 and
   5.x, chaining existing DEBUG traps and PROMPT_COMMAND), fish through
   `vendor_conf.d`; `LC_TERMINAL=Tachyon` turns the scripts on over ssh.
   *Notifications done (9 November 2026):* a command of `notify.after` seconds
   (10) or more that finishes while you're elsewhere posts a notification with
   its receipt; clicking it shows the tab. ⌘-click Quick Look and URLs, and
   Secure Keyboard Entry at password prompts, landed with it.
7. **Metric lens and job tabs**: Nexus first, then Slurm. New `src/lens/`,
   `src/jobs/`.
   *Metric lens done (9 November 2026):* `src/term/lens.rs` reads the output
   stream as lines (escapes dropped, `\r` ends a line, so tqdm redraws count),
   keeps up to 8 named series per command, and the prompt line shows the latest
   values with a sparkline of the loss; a tqdm percentage drives a Dock progress
   bar. Lines are all read below 256 KB per 100 ms and sampled above, which
   measured the same as no lens on a 160 MB `cat`.
   *Job tabs done (9 November 2026):* `src/term/jobs.rs` spots a Nexus id in
   the output of a `nexus cpu/gpu` submission (any part of a chained line) and
   Slurm's `Submitted batch job N`, remembering the ssh it came through. The
   prompt line offers ⌘J; the tab runs `shell/tachyon-job`, which streams the
   log (`nexus logs`, or `tail -f` of the StdOut from `scontrol`), names the tab
   from `nexus jobs` or `squeue` through OSC 2, and exits with the job's code
   (from `sacct` for Slurm). zsh runs the command at its first prompt through a
   line-init hook added from precmd; other shells get it typed ahead.
   *Rerun diff done (9 November 2026):* `src/term/rerun.rs`. When a command
   finishes, its output lines (wrapped rows joined, up to 2000) are compared
   with the last run of the same command line with numbers ignored, by a
   Myers diff (the `similar` crate) on line shapes; equal shapes with different
   text are changed, unmatched lines new. Marks go on the rows (CHANGED, NEW)
   so they travel through scrollback and reflow; the prompt line counts them
   and hovering shows the old line as a tooltip.
8. **Session host**: `tachyon-host` as a login agent with keep-awake
   assertions. After Rewind, because it owns the logs.
   *First version done (9 November 2026):* not a login agent but the app's
   own binary started as `tachyon --host <socket>` in its own session
   (`setsid`), on demand, exiting 20 s after it has no shells and no app.
   `src/mac/host.rs`: a Unix socket in `$TMPDIR` named by the app binary and
   protocol version, checked with `getpeereid`; requests are 0x1F-separated
   fields in length-prefixed frames. Each shell's PTY master and a socketpair
   carrying its output go to the app with `SCM_RIGHTS`, so the app writes keys
   and resizes to the PTY directly and reads output as before. The host reads
   each PTY, forwards at once, and parses on a utility-QoS thread into its own
   `Term`; a hand-over stops forwarding, parses what's queued, and starts the
   new stream with `Term::to_vt()` (`src/term/restore.rs`: scrollback, both
   screens, SGR, wraps, modes, cursor, title, OSC 7, OSC 133 marks with exit
   codes), which the app parses as the past (`Term::restoring`: no replies,
   events or times). The app keeps the layout (window frames, tabs, split
   trees, focus, zoom) at the host and reopens it at launch; ⌘Q keeps shells,
   ⌥⌘Q ends them, closing a window or tab ends its own. Measured under load:
   echo 6–11 µs median through the host (2–4 µs straight), throughput 4–12%
   lower on a 256 MB `cat`, with 1 MiB socket buffers. Still to do:
   Rewind history across a restart, notifications while the app is closed.
   *Keep-awake done (9 November 2026):* ⌥⌘A arms the focused pane: the
   running command, or the next to start (`Term::set_keep_awake`, cleared by
   that command's OSC 133 D). The host follows its own terminal and holds one
   `PreventUserIdleSystemSleep` assertion (`src/mac/power.rs`, IOKit) while any
   shell's armed command runs, named after the commands for `pmset -g
   assertions`, so it holds with the app closed; shells run in the app hold it
   there. The receipt reads `☕ awake` while held.
9. **Split panes** on cell boundaries. Shaping already arrived with Core Text
   in step 1.
   *Ligatures and emoji sequences done (9 November 2026):* `FontSet::shape`
   lays text out with a CTLine and maps glyphs back to characters, cached by
   text and style. The renderer shapes only spans that need it: emoji
   sequences the grid keeps in separate cells (ZWJ, skin-tone modifiers,
   regional-indicator pairs), multi-character clusters, and with a ligature
   font, runs of two or more punctuation cells in one weight. Each glyph is
   coloured by its own cell. Ligatures switch on only when shaping a probe
   string changes a glyph (JetBrains Mono yes, Cascadia Mono no).
   *Done (9 November 2026), ahead of the session host:* `src/mac/panes.rs` is
   a tab's layout tree (splits with a ratio, on whole cells, one cell for each
   divider, panes kept at least two cells). Each pane is a session of its own
   (shell, reader, Rewind log, selection, receipts); the window swaps per-pane
   state on focus as it does per tab. One Metal frame draws every visible pane
   at its cell offset, then hairline dividers and a 30% veil over panes
   without focus. ⌘D / ⌘⇧D split (the new shell starts in the pane's folder
   from OSC 7, not over ssh), ⌘⌥ arrows move focus to the nearest pane on that
   side, a click focuses, dividers drag (with resize pointers), ⌘⇧↩ zooms, ⌘W
   and `exit` close a pane. Idle stays at 0.0% CPU with no wake-ups.
