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
