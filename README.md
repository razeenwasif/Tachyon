# Tachyon

A GPU-accelerated terminal emulator for Windows, written in Rust against
Direct3D 11, DirectWrite and ConPTY.

Ghostty has no Windows build, so this is a native one rather than a port: it
targets the Windows graphics and console stack directly instead of going through
a portability layer.

```
dist/tachyon.exe        ~740 KB, 29 dependencies, no runtime to install
```

---

## Status

Working and usable. It runs PowerShell, `cmd`, WSL and TUI applications; it
renders truecolour, all SGR styles, box drawing, CJK, combining marks and
scrollback; it can be pinned to the taskbar.

It is a v0.1 and there are real gaps — see [Limitations](#limitations). Nothing
below is claimed unless it has been run.

## Performance

Measured on a Ryzen 7 7800X3D via
`cargo test --release --test throughput -- --ignored --nocapture`:

| Workload | Native Windows | WSL2 |
|---|---:|---:|
| Plain ASCII | 482 MiB/s | 738 MiB/s |
| Build log (colour escape every 7 lines) | 272 MiB/s | 380 MiB/s |
| TUI full redraw (escape-heavy) | 207 MiB/s | 299 MiB/s |

The ground-state scanner alone runs at **~19 GiB/s against ~2.6 GiB/s for a
per-byte loop — 6.6x to 7.2x across runs**.

Where that comes from, and which ideas were rejected and why, is written up in
[docs/RESEARCH.md](docs/RESEARCH.md). The short version:

- An **AVX2 fast path in front of the VT state machine** consumes runs of
  printable ASCII 32 bytes at a time, so the parser only ever sees escapes.
- **Ring-buffer scrollback** makes a full-screen scroll `O(cols)` rather than a
  whole-grid memmove.
- **16-byte cells**, compile-time asserted, so the viewport stays in L2.
- **Flip-model swapchain with a waitable frame latency object**, and an event
  loop that only waits on it when there is something to draw — the frame pacing
  is a property of the wait, not a timer.
- **Three draw calls per frame**, no vertex buffers, with run-length coalesced
  backgrounds and nothing emitted at all for blank cells.

## Building

Requires a Rust toolchain with the MSVC target and the MSVC linker (Visual
Studio Build Tools).

```powershell
cargo build --release
```

From WSL, where `rustc` cannot link MSVC targets, use the helper — it mirrors the
source onto the Windows filesystem, builds with the Windows-side `cargo`, and
copies the binary back to `dist/`:

```bash
./build-windows.sh            # release
./build-windows.sh debug      # debug (console subsystem, prints panics)
./build-windows.sh test       # run the test suite on Windows
```

The application icon and the side-by-side manifest are generated and embedded by
`build.rs`, which writes the COFF `.res` container itself. No `rc.exe`,
`llvm-rc` or `windres` is required, which is what makes cross-building from WSL
work.

### Tests

The terminal core is platform-independent so it can be tested anywhere:

```bash
cargo test                    # 52 tests on Windows, 39 on Linux
```

### macOS (Apple silicon)

The Mac backend lives in `src/mac/`: an AppKit window with a Metal layer,
Core Text glyphs, and a `forkpty` session running your login shell. It is new
and less complete than the Windows build -- see [docs/NEXT.md](docs/NEXT.md).

```bash
cargo run                            # run from the source tree
scripts/bundle-macos.sh              # build dist/Tachyon.app
scripts/bundle-macos.sh --install    # ...and install it to /Applications
```

The window is edge-to-edge: no title bar, and the window buttons appear when
the pointer reaches the top-left corner. If you'd rather have a title bar,
View > Show Title Bar switches to the **standard layout** (`window.layout =
standard` makes it the default): a title bar on the glass with the window
buttons and the tab's name, and with two or more tabs, the glass tab capsule in
the title bar instead of along the bottom. It costs about three rows, and
double-clicking the title bar does what System Settings says. **Full screen
around the notch** (View menu, ⌥⌘F) covers the whole screen with the menu bar
and Dock hidden while Tachyon is in front. The tabs sit left of the notch, and
right of it is a status line: the command running in front and for how long,
or its folder, and the time, since the menu bar's clock is hidden. Every row
below the notch is terminal. ⌥⌘F again goes back. ⌃⌘F is still macOS's own
full screen, which keeps the window below the notch. **Liquid Glass** sits behind the text
(NSGlassEffectView on macOS 26 and later): only the default background lets it
through; text, the cursor, the selection and colours a program sets stay solid.
The glass is tinted with the theme's background (`window.glass_tint`, 0.75) in
its clear style, which keeps its refraction where drawing the background over
it would flatten it; `window.glass = regular` frosts it more. With more than
one tab, the tabs float in a glass capsule along the bottom edge. `window.backdrop = none` makes it opaque, and so does
Reduce Transparency. A **light theme** with its own ANSI colours takes over in
Light Mode and switches back as the system does (`theme.appearance` fixes one);
`theme.cursor = accent` and `theme.selection_bg = accent` use the system accent
colour. With **Increase Contrast** on (System Settings > Accessibility >
Display), both themes switch to high-contrast versions derived from them: a
black (or white) background, every ANSI colour at 7:1 contrast or better
(4.5:1 for the dim slot shells use for suggestions), a solid selection with its
own text colour, and the glass turned into a solid background. It switches back
when the setting does; `theme.contrast = high` or `normal` fixes it. Drawing is paced by a display link
that pauses when nothing changes, so an idle window uses no CPU. Box drawing
and block elements are drawn as rectangles, so borders and progress bars meet
exactly. Nerd Font icons come from any installed Nerd Font, sized so its line height
is the cell's: icons sit inside the cell, and Powerline separators fill it
exactly. The font defaults
to Cascadia Mono and falls back to SF Mono when it is not installed.

On an M2 Pro (`cargo test --release --test throughput -- --ignored
--nocapture`): plain ASCII 657–662 MiB/s, a colour-escaped build log 291–296
MiB/s, a full-screen TUI redraw 244–247 MiB/s; the NEON ground-state scanner runs
at about 14 GiB/s. Scrollback lines are packed (one byte per ASCII character
plus style runs), so 100,000 lines of build output take about 7x less memory
than full-width rows -- see [docs/NEXT.md](docs/NEXT.md) for the measurements.

Configuration is read from `~/.config/Tachyon/tachyon.conf`, in the same format
as on Windows. Logs (with `TACHYON_LOG=1`) go to `~/Library/Logs/Tachyon/`.

| | |
|---|---|
| ⌘T / ⌘N | New tab / new window |
| ⌘W / ⌘⇧W | Close the pane (its tab with the last pane, the window with the last tab) / close the window |
| ⌘Q / ⌥⌘Q | Quit, keeping the shells for next time / quit and end them |
| ⌥⌘A | Keep the Mac awake while this pane's command runs (or its next one) |
| ⌥⌘L | Latency ledger: where each keystroke's delay goes |
| ⌥⌘F | Full screen around the notch, or back |
| → or End, ⌥→ | Take the grey suggestion after the cursor, or one word of it |
| ⌘1–⌘8, ⌘9 | Show that tab, or the last one |
| ⌘D / ⌘⇧D | Split the pane: a new shell to the right / below, in the same folder |
| ⌘⌥← ⌘⌥→ ⌘⌥↑ ⌘⌥↓, or click | Focus the pane on that side, or the one clicked |
| Drag a divider | Resize the panes beside it |
| ⌘⇧↩ | Zoom the pane to fill the tab, or put it back |
| ⌘⇧[ / ⌘⇧], ⌃⇥ / ⌃⇧⇥ | Previous / next tab |
| Click / middle-click a tab | Show it / close it |
| Drag, double-click, triple-click | Select by character, word (paths count as one word), or line |
| Shift-click | Extend the selection |
| ⌘C / ⌘V / ⌘A | Copy / paste / select all |
| ⌘+ (or ⌘=) / ⌘− / ⌘0 | Bigger / smaller / configured font size |
| ⌃⌘-drag, or drag the top edge | Move the window |
| ⌘↑ / ⌘↓ | Jump to the previous / next command |
| Shift-drag in vim, tmux, htop | Select even while the app has the mouse |
| ⌘-click a path / a URL | Show the file in Quick Look / open the link |
| ⌘[ / ⌘] / esc | Rewind: the screen one frame earlier / later / live again |
| Wheel, or drag the slider, while rewound | Scrub through the frames kept |
| ⌥-click while rewound | Which output wrote that cell, and when |
| ⌘J, or ⌘-click a job id | Follow the job this tab submitted last, or that one, in its own tab |

**⌘-click** a file path to see it in Quick Look (an image, a CSV, a PDF) or a
URL to open it. Paths are taken from the tab's folder, and `file:line:col`
suffixes from compilers are dropped. **Secure Keyboard Entry** turns on by itself
while a program reads a password (sudo, ssh, getpass), so other apps can't read
those keystrokes, and turns off after; the prompt line says so while it's on.

**Rewind.** ⌘[ shows the screen as it was a moment ago, including full-screen
programs (htop, nvtop, vim, agent UIs) that scrollback never keeps; ⌘[ again
steps further back, ⌘] forward, and Esc or any key returns to live. A glass badge
says how long ago the frame is, with a slider to scrub through every frame kept;
the wheel steps through them too. **⌥-click** a cell in a rewound frame to see
which output wrote it: the bytes around it, their offset in the stream, and how
long before the frame they arrived.

Each tab keeps its last 32 MiB of output in memory, with a copy of the screen
every 2 MiB, and up to 512 MiB more on disk, LZ4-compressed in 8 MiB segments
under `~/Library/Caches/Tachyon/Rewind`. That folder is private to you (0700)
and a tab's part of it is deleted when the tab closes or Tachyon quits (or, after
a crash, at the next launch), but while a tab is open its output, passwords
you typed excepted (they aren't echoed), is on disk. On the M2 Pro a seek takes
about 4 ms in memory and 16 ms on disk, and recording costs 1 to 4% of parsing
throughput: compression runs on a thread of its own.

**Shells outlive the app.** Shells run in Tachyon's session host, a small
background process (the same binary, started as `tachyon --host`), so quitting
Tachyon, a crash, or installing an update leaves them running: a training run
or an ssh session carries on. Open Tachyon again and its windows, tabs and
splits come back where they were, each with its screen, scrollback (up to
`scrollback` lines), colours, the full-screen program it was showing, and its
command marks and exit codes. ⌥⌘Q quits and ends the shells instead; closing a
window or tab ends its shells as always. The host parses each shell's output
on an efficiency-core thread while it forwards it, answers terminal queries
while no app is attached so programs don't stall, and exits 20 seconds after
it has nothing left to run. Measured on the M2 Pro under load: keystroke echo
6–11 µs through the host against 2–4 µs straight, and 4–12% less throughput on
a 256 MB `cat`. Restored commands have no times in their receipts, and Rewind
starts over from the restored screen. Set `shell.persist = false` to have
shells end with the app.

**Keep awake.** ⌥⌘A keeps the Mac from sleeping while the pane's command
runs: the one running now, or at a prompt the next one you start. It lasts
that one command, and the receipt says `☕ awake` while it holds (a prompt says
`☕ the next command keeps the Mac awake` until then). The session host holds
it, so it keeps going after you quit Tachyon. It stops idle sleep only, like
`caffeinate -i`: the display still sleeps, and closing the lid on battery
still sleeps the Mac. `pmset -g assertions` lists it by command.

**Split panes.** ⌘D splits the pane with focus side by side and ⌘⇧D one above
the other; the new shell starts in the same folder (unless that pane is in an
ssh session). Panes land on whole cells, with one column or row for the
divider, so every pane's text lines up with the window's grid. Panes without
focus are veiled slightly. The wheel scrolls the pane under the pointer,
without taking focus. ⌘W closes the pane with focus (then the tab, then the
window), and a shell that exits closes its pane. Each pane has its own Rewind
log, receipts and notifications; the tab is named after the pane with focus.

**Tabs.** With two or more tabs, a strip along the bottom edge shows them; with
one, it goes away and the grid gets the row back. A tab is named after what's
running in it, or its folder at a prompt (zsh reports both through the
integration below), or a title a program set while it runs. When a command
finishes in a tab you aren't looking at, the tab gets a dot: green if it
succeeded, red if it failed.

Applications that turn on mouse reporting (vim, tmux, htop) get clicks, drags
and the wheel. The cursor blinks through Core Animation, so a blinking cursor
costs the app no wake-ups. To make Option act as Meta, for shell shortcuts like
⌥B and ⌥F, set `macos.option_as_meta` to `left`, `right` or `true`.

Dead keys and input methods work: ⌥E then E types é, and Japanese, Chinese and
Korean input methods compose at the cursor.

**Shell integration.** zsh, bash and fish start with Tachyon's integration
loaded from `shell/`, and none of your startup files change: zsh through a
`ZDOTDIR` that restores yours, bash through `--rcfile` (the script reads your
`.bash_profile` or `.bashrc` first, and works with macOS's bash 3.2 as well as
Homebrew's), and fish through `vendor_conf.d`. Existing `PROMPT_COMMAND`s and
DEBUG traps, like starship's, keep working. It marks every prompt and command
with OSC 133, which gives:

- a bar in the left margin per command: blue while running, green on success,
  red on failure;
- a **receipt** on the command line once it finishes or has run for two seconds:
  `✓ 4.2s · cpu 4.1s (P 100%) · 20.7 J · 425 MB` -- wall time, CPU time and
  the share on performance cores, energy, and peak memory footprint. CPU time is
  exact (from the shell's child-process accounting); P-core share, energy and
  memory are sampled every half second, so very short commands show only their
  exit status and time. Memory is the footprint Activity Monitor shows, and on
  Apple silicon it includes what the command allocates on the GPU: PyTorch's
  MPS backend, MLX, anything through Metal. Measured: two 2 GiB MPS tensors
  read 4.14 GiB, the same as macOS's `footprint` tool. With a right prompt
  (conda's `(base)` in fish, zsh's RPROMPT), the receipt sits just left of it;
- a **metric lens**: while a command runs, numbers in its output that look like
  metrics (`loss=0.41`, `acc: 87%`, `{'loss': 0.39}`, `48.2it/s`, `512 tok/s`)
  show on its prompt line, with a sparkline of the loss (or the first metric):
  `loss 0.398 ▇▆▅▄▃▂ · 48.2 it/s · 45%`. A tqdm bar's percentage also shows as
  a progress bar on the Dock icon. Every line of a training log is read; a flood
  like `cat` of a big file is only sampled, so it costs no throughput;
- **job tabs**: when a command submits a cluster job (`nexus cpu/gpu ...
  --detach` prints its `j-1a2b3c4d`; `sbatch` prints `Submitted batch job N`),
  its prompt line offers `⌘J follows j-1a2b3c4d`. ⌘J (or ⌘-click on the id)
  opens a tab that follows the job's log with `shell/tachyon-job`, named after
  its state (`nexus j-1a2b3c4d · running on forge`, then `· finished` or
  `· failed`), with a receipt and a notification when it ends. A Slurm job
  submitted over ssh is followed through the same ssh;
- **rerun diff**: run a command again (numbers in the command line ignored, so
  `train.py --lr 3e-4` then `--lr 1e-3` counts) and the lines that differ from
  the last run get an amber mark in the gutter, with `2 lines changed` (or `same
  as last run`) on the prompt line. Lines are paired by shape, numbers ignored,
  so a changed loss reads as changed; hover a mark for the old line;
- ⌘↑ / ⌘↓ to jump between commands;
- a **notification** with the receipt when a command of 10 seconds or more
  finishes while you're in another app or window (`notify.after` sets the time,
  0 turns it off). Clicking it brings the command's tab to the front. macOS asks
  once whether Tachyon may notify; outside the app bundle, the Dock icon
  bounces instead.

Set `shell.integration = false` to turn it off.

Over ssh, the shell on the other machine needs the script too. Copy it there
and load it from your startup file; ssh passes `LC_TERMINAL=Tachyon`, which is
how it knows it's in Tachyon:

```sh
scp shell/tachyon-integration.bash cluster:~/.tachyon-integration.bash
# then in ~/.bashrc on the cluster:
[[ $LC_TERMINAL == Tachyon ]] && source ~/.tachyon-integration.bash
```

For zsh, source `shell/tachyon-integration.zsh` from `~/.zshrc`; for fish, copy
`shell/xdg/fish/vendor_conf.d/tachyon-integration.fish` into
`~/.config/fish/conf.d/`. Some servers' `sshd_config` doesn't accept `LC_*`.

**Suggestions.** As you type at a prompt, Tachyon shows in grey the rest of a
command you've run before; → or End (or ⌃F) types it in for you, ⌥→ one word
of it. Suggestions come from your shell's history file (zsh, bash or fish),
read at launch, and from every command run in any Tachyon tab, recorded with
its folder: the newest match wins, one run in the same folder first. Tachyon's
record is `~/Library/Application Support/Tachyon/history`, readable only by
you. Commands typed with a leading space aren't recorded, the shells'
convention for keeping one out of history, and multi-line commands aren't
suggested. A suggestion is only drawn where the rest of the line is blank, so a
shell that suggests on its own (fish, zsh-autosuggestions) keeps its own, and it
stops short of a right prompt. `shell.suggestions = false` turns them off.

**Latency ledger.** ⌥⌘L shows, on the prompt line, where your last
keystroke's delay went. The key event is timed when the hardware delivered
it, then at the write to the shell, the first byte of its echo, that echo
parsed, the frame committed, and the frame on the display (Metal's presented
time). The line reads, for example, `⏱ 11.8 ms (median 10.9 of 40) ·
Tachyon 0.6 · shell 0.3 · frame wait 2.4 · display 8.5`. Over ssh, the
shell share includes the network. The same stages are `os_signpost`
intervals in Instruments' Points of Interest track, or
`log stream --signpost --predicate 'subsystem == "dev.tachyon.terminal"'`.
With `TACHYON_LOG=1` every keystroke's stages go to the log as well.

What the ledger found, and what changed: a keystroke took about 33 ms to
reach the screen. Up to a refresh of that was the echo waiting for the
display link's next tick, and the rest was the frame waiting for vsync before
the compositor took it. Now an idle window draws the echo as soon as it is
parsed, and in a window, frames go to the compositor without waiting for vsync
(`render.low_latency`). The compositor syncs to the display either way, so
nothing tears, and full screen keeps `render.vsync`. The display link asks
for 120 Hz on ProMotion screens, and drops to 60 Hz once output has streamed
for a quarter of a second. Measured on the M2 Pro, alternating with the
previous build under the same load, with 40 synthetic keystrokes each:
median 33.0–35.0 ms before, 23.0–25.0 ms now. A 256 MB `cat` runs at 145–165
MiB/s, against 125–140 MiB/s before, and an idle window still uses 0.0% CPU.

**Ligatures and emoji.** Core Text shapes what needs it. Emoji sequences
draw as one glyph: ZWJ families, skin tones, flags, and keycaps. Combining
marks are composed or placed by the font. With a font that has programming
ligatures (Cascadia Code, Fira Code, JetBrains Mono), runs of punctuation such
as `->`, `!=` and `|>` take them, each glyph in its own cell's colour.
`font.ligatures = false` turns them off. Cell widths stay what the shell
counts, so the cursor never drifts. A family emoji is drawn once and leaves
the extra cells blank. Shaping is cached and only runs for cells that need
it, so plain text draws exactly as before.

## Installing and pinning to the taskbar

```powershell
.\dist\tachyon.exe --install
```

This copies the binary to `%LOCALAPPDATA%\Programs\Tachyon\` and creates a Start
menu shortcut. Then open Start, find **Tachyon**, right-click, **Pin to
taskbar**.

The shortcut is not just a convenience. Windows pins an *application identity*,
not a file, so the running process calls
`SetCurrentProcessExplicitAppUserModelID("Tachyon.Terminal")` and the installer
stamps the same string into the shortcut's `System.AppUserModel.ID` property. If
those disagree you get two taskbar buttons and the pin appears to do nothing —
which is the usual reason pinning a bare `.exe` fails.

`--install` also appends the install directory to your **user** PATH, so
`tachyon` works as a command. Existing shells keep their old environment —
open a new one. The registry value's type is preserved, so entries like
`%LOCALAPPDATA%\...` elsewhere in your PATH keep expanding.

`--uninstall` removes both the shortcut and the PATH entry. Add `--quiet` to
either for no dialogs. Nothing outside `HKCU` is touched.

## Configuration

`%APPDATA%\Tachyon\tachyon.conf`, a flat `dotted.key = value` file. Copy
[`tachyon.conf.example`](tachyon.conf.example) to get started. Unknown keys and
bad values are reported on startup rather than silently ignored.

```ini
font.family    = "Cascadia Code"
font.size      = 12.0
font.antialias = subpixel       # or: grayscale
font.gamma     = 1.2
render.vsync   = true
theme.background = #0B0E14
scrollback     = 10000
```

## Keys

| | |
|---|---|
| `Ctrl+Shift+C` / `Ctrl+Shift+V` | Copy / paste |
| Right-click or middle-click | Paste |
| `Shift+PageUp` / `Shift+PageDown` | Scroll by a page |
| `Shift+Home` / `Shift+End` | Top of scrollback / back to live |
| Mouse wheel | Scroll (forwarded to the application when it asks) |
| Drag | Select; wrapped lines are copied as one logical line |
| Drag inside an app that tracks the mouse | Goes to the app -- resizes tmux and nvim splits |
| `Shift`+drag | Select even while an app is tracking the mouse |
| `Ctrl+Shift+T` / `Ctrl+Shift+W` | New tab / close tab |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |
| `Alt+1`..`Alt+9`, `Alt+0` | Jump to tab by position (`Alt+0` = last) |
| Middle-click a tab | Close it |

## What is implemented

- CSI cursor movement and editing, SGR (16 / 256 / truecolour, both `;` and `:`
  forms), erase and scroll operations, `DECSTBM` scroll regions
- All underline styles including curly, dotted, dashed and coloured; strikeout,
  overline, reverse, dim, hidden
- Alternate screen, bracketed paste, focus reporting, synchronised output
  (mode 2026), `DECSCUSR` cursor shapes, application cursor/keypad modes
- SGR and X10 mouse reporting: click (1000), button-event drag (1002) and
  any-motion (1003) tracking, so tmux and nvim splits can be dragged; alternate
  scroll
- DEC Special Graphics, wide characters, combining marks, font fallback
- Per-monitor v2 DPI, cell-snapped resizing, dark title bar
- Device reports (`DA`, `DSR`, `CPR`) and OSC window titles
- Tabs, with a strip that appears only when there is more than one session
- Reflow: wrapped lines rejoin and re-split when the window changes width
- Colour emoji (COLR/CPAL layers composited in linear light)
- Frosted glass / mica backdrops with a translucent background

## Frosted glass

```ini
window.backdrop = acrylic     # or: mica, tabbed, blur, none
window.opacity  = 0.82        # optional; a backdrop defaults to 0.82
```

The window renders through a DirectComposition swap chain with premultiplied
alpha, so the compositor's backdrop shows through wherever the terminal
background is translucent. Text stays fully opaque at any opacity — a
see-through background is pleasant, see-through letters are not.

`acrylic` and `blur` blur the windows actually behind you, live. `mica` and
`tabbed` tint from the desktop wallpaper instead: calmer, cheaper, and they do
not shimmer when something moves behind the window. Setting a backdrop switches
antialiasing to grayscale automatically, because subpixel coverage needs to know
the colour behind each glyph and over a translucent window the compositor
decides that after we are done.

## Running WSL

There are three ways, and they are not interchangeable:

**1. Just run it in the session you already have.** Type `wsl` at the prompt.
This is almost always what you want — WSL takes over the current tab, and
exiting returns you to the shell you started from.

**2. Make WSL the default shell**, so every window and every new tab starts in
it:

```ini
shell.program = "wsl.exe"
```

**3. Launch a window that starts in WSL:**

```powershell
tachyon -e wsl.exe
```

`-e` is a *launch* flag: it tells a newly starting Tachyon what to run. Typing
it at a prompt inside Tachyon does not switch the current tab — it opens a
second window. (It also only works once the installer has put Tachyon on your
PATH; see below.)

Your distro's configuration applies in full — `.bashrc`, starship, oh-my-posh
and so on — because `wsl.exe` runs your normal login shell. Tachyon sets
`TERM=xterm-256color` and `COLORTERM=truecolor` and forwards both across the
Win32/WSL boundary via `WSLENV`, so 24-bit colour is advertised correctly inside
the distro.

For prompt icons you need a Nerd Font. Tachyon looks for one automatically —
`Symbols Nerd Font`, the patched Cascadia/JetBrains/Fira/Hack families, and
`MesloLGS NF` — so installing any of them is enough; you do not have to change
`font.family`.

## Limitations

Being explicit about these rather than leaving them to be discovered:

- **No ligature shaping.** Codepoints map to glyphs 1:1, so programming
  ligatures in fonts like Fira Code do not form, and regional-indicator pairs
  render as two letters rather than a flag. This needs real shaping through
  `IDWriteTextAnalyzer` plus cluster-to-cell mapping, and it interacts with
  cursor placement and selection, so it is the one remaining gap.
- **No split panes.** Tabs yes, splits no.
- **Font fallback is a static chain**, not `IDWriteFontFallback::MapCharacters`.
  It covers Latin, CJK, symbols, Nerd Font icons and emoji on a normal Windows
  install; an unusual script may still miss.
- Selection is linear only; no block selection, and no double-click word select.
- Colour glyphs that use `paletteIndex 0xFFFF` (meaning "tint with the text
  colour") render white. Rare outside icon fonts.

What comes next is sketched in [docs/NEXT.md](docs/NEXT.md): a macOS backend
for Apple silicon, and how the shaping and split-pane gaps get closed. It is a
concept, not a commitment.

## Debugging

Two environment variables, both built for diagnosing this thing during
development and left in because they are genuinely useful:

| | |
|---|---|
| `TACHYON_LOG=1` | Lifecycle logging to `%LOCALAPPDATA%\Tachyon\tachyon.log`. Panics are always recorded there, log or no log. |
| `TACHYON_DUMP=<path>` | Write the raw PTY byte stream to a file. ConPTY re-renders its own screen buffer and re-emits VT, so what a terminal receives rarely looks like what the application wrote — this is the only reliable way to see the truth. |

The `conpty_stream` integration tests were built from captures taken this way.

## Layout

```
src/term/        terminal state — platform-independent, fully unit-tested
    parser.rs      AVX2/SWAR fast path in front of the vte state machine
    grid.rs        ring-buffer scrollback, damage tracking
    cell.rs        the 16-byte cell
src/render/      D3D11 renderer
    d3d.rs         device, flip-model swapchain, pipeline
    font.rs        DirectWrite glyph rasterization and metrics
    atlas.rs       shelf-packed glyph atlas
    shaders.hlsl   vertex-buffer-free instanced rect and glyph passes
src/pty/         ConPTY session
src/win/         window, event loop, input, installer
build.rs         procedural icon + hand-built COFF .res
```

## Licence

MIT.
