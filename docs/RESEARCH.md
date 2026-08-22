# Design notes: where the speed actually comes from

This is the reasoning behind Tachyon's architecture, and an honest account of
which parts are genuinely novel, which are established practice, and which
ideas from the literature were considered and rejected.

A caveat up front, because the brief asked for "state-of-the-art advancements":
terminal emulation is not an area with an active academic literature. Searching
current work surfaces engineering writeups, shipped implementations and vendor
documentation rather than papers. What follows is drawn from those, plus general
systems technique (SIMD scanning, cache-conscious layout, latency-oriented
presentation) applied to this problem. I have flagged the one place where I
think the combination is actually new, and I have not dressed up the rest as
research.

---

## 1. The real bottleneck is per-byte dispatch, not drawing

The instinct is that a terminal is slow because it draws too much. For a
GPU-accelerated terminal that has not been true for years. Profiling any
`cat`-a-large-file workload puts the time in the parser: a conventional VT
implementation is a state machine that takes a transition and an indirect
`print(char)` call *per byte*.

Alacritty's [`vte`](https://docs.rs/vte) — the most careful open-source
implementation — already uses `memchr` to find the next `ESC`, but it then walks
the run it found and calls `Perform::print` once per character. Microsoft's
Atlas Engine and Ghostty are in a similar position; Mitchell Hashimoto's recent
work on Ghostty focused on shortening the *renderer's lock hold* precisely
because the parse-and-apply side is the hot path
([Ghostty architecture notes](https://mitchellh.com/writing/ghostty-and-useful-zig-patterns),
[Windows Terminal Atlas Engine](https://deepwiki.com/microsoft/terminal/3.2-atlas-engine)).

### What Tachyon does

`src/term/parser.rs` puts a vectorised scanner *in front* of the state machine.
While the machine is provably in its Ground state, we scan with AVX2 for the
first byte outside `0x20..=0x7E`, 32 bytes per instruction, and hand the entire
run to the grid as a bulk cell fill. `vte` only ever sees escape sequences,
UTF-8 and state-changing control bytes.

The subtle part is *proving* we are in Ground, since `vte` does not expose its
state. The argument is spelled out in the module docs and rests on three facts:

- C0 bytes below `0x20` other than `ESC` execute without changing state, so we
  can handle them inline and stay on the fast path.
- `ESC` and any byte `>= 0x7F` may begin a multi-byte construct, so we hand off.
- The machine has returned to Ground exactly when it dispatches a *terminating*
  action — `print`, `csi_dispatch`, `esc_dispatch`, `osc_dispatch`, `unhook`.
  Notably **not** `execute`: a C0 byte inside a CSI is executed without leaving
  the sequence. Getting that wrong is a silent corruption bug.

There is a second precondition that is easy to miss and that I did in fact get
wrong on the first pass: the fast path is only valid when the character set
mapped into GL passes ASCII through unchanged. With DEC Special Graphics
selected (`ESC ( 0`), `q` means `─`. The unit test
`dec_graphics_maps_line_drawing` caught it; `charset_is_ascii()` guards it now.

### Measured

`cargo test --release --test throughput -- --ignored --nocapture`, on a
Ryzen 7 7800X3D:

| Workload | Native Windows | WSL2 |
|---|---:|---:|
| Plain ASCII | 482 MiB/s | 738 MiB/s |
| Build log (SGR every 7 lines) | 272 MiB/s | 380 MiB/s |
| TUI full redraw (escape-heavy) | 207 MiB/s | 299 MiB/s |

The scanner in isolation: **~19 GiB/s vectorised vs ~2.6 GiB/s per-byte, a 6.6-7.2x
speedup** (9.0x under WSL). The end-to-end numbers are lower than the raw scan
rate because they include writing cells and scrolling — which is the point of
measuring them that way.

**Is this novel?** The pieces are not: SIMD prefilters are standard practice
(simdjson popularised the shape). Applying one to VT ground-state scanning, with
an explicit soundness argument for when the fast path may be taken, is the one
thing here I have not seen written down elsewhere. I would call it a good
engineering transfer rather than a research result.

---

## 2. Scrolling should not be a memmove

The most frequent structural operation in a terminal is "scroll up one line",
once per output line. Implemented naively that memmoves the whole grid: at
200x50 with 16-byte cells, 160 KB per newline. `cat` becomes memory-bound.

`src/term/grid.rs` stores rows in a **ring buffer**. A full-screen scroll
advances an index and clears one row — `O(cols)` instead of `O(rows * cols)` —
and evicted lines land in scrollback for free, because scrollback is just the
part of the ring behind the viewport.

Scroll regions (`DECSTBM`) cannot use the index trick. Those rotate *row
handles*, so a sub-region scroll still moves 32-byte structs rather than cell
arrays.

This is established practice (Alacritty and kitty both do versions of it), not
novel. It is here because it is correct.

---

## 3. Cache-conscious cell layout

`Cell` is exactly 16 bytes, asserted at compile time:

```rust
const _: () = assert!(core::mem::size_of::<Cell>() == 16);
```

Four cells per cache line. A full 200x50 viewport is 160 KB — L2-resident on any
machine we care about, so the renderer's per-frame walk never leaves cache.

Holding that budget forced two decisions. Underline colours live in a side table
referenced by a `u16` index, because styled underline colours are rare and would
otherwise cost 4 bytes on every cell. Grapheme clusters that do not fit in a
single scalar are stored in an arena, with the high bit of `ch` marking an index
rather than a codepoint — free, since Unicode scalars never exceed `U+10FFFF`.

---

## 4. Presentation: latency is a scheduling problem

Frame *rate* is irrelevant for a terminal; what matters is the delay between a
keystroke and the photons changing. Microsoft's guidance and Raph Levien's
[swapchain and frame pacing writeup](https://raphlinus.github.io/ui/graphics/gpu/2021/10/22/swapchain-frame-pacing.html)
both point the same way, and `src/render/d3d.rs` follows it:

- **Flip model** (`DXGI_SWAP_EFFECT_FLIP_DISCARD`). The buffer goes straight to
  the compositor instead of being copied into a DWM-owned surface — one full
  frame of latency removed
  ([DirectX blog](https://devblogs.microsoft.com/directx/dxgi-flip-model/)).
- **Waitable frame latency object**, maximum latency 1. Our frames are cheap
  enough that we do not need CPU/GPU overlap to hit refresh rate, so we spend
  the slack on latency instead.
- **`ALLOW_TEARING`** where the adapter supports it, for users who want to go
  below one refresh interval.
- An **sRGB render target view over a UNORM flip-model swapchain**, so blending
  happens in linear light with no shader-side conversion. Flip model rejects
  `_SRGB` swapchain formats; attaching an `_SRGB` view to a `UNORM` chain is the
  documented way around that.

The scheduling detail that matters most is in `src/win/mod.rs`: the event loop
parks in `MsgWaitForMultipleObjectsEx` on the message queue, a "screen changed"
event signalled by the reader thread, **and the frame latency waitable — but
only when there is something to draw**. Including it unconditionally would spin,
because it stays signalled while the chain can accept frames. With that
condition, "render at most once per refresh, as late as possible, and not at all
when idle" falls out of the wait itself rather than being enforced by a timer.

---

## 5. Drawing: the win is in what is *not* drawn

Three draw calls per frame, no vertex buffers, no input layout. Each pass is
`DrawInstanced(4, N)` over a triangle strip; the vertex shader synthesises corner
positions from `SV_VertexID` and pulls instance data from a `StructuredBuffer`.

Two decisions do most of the work:

- **Background run-length coalescing.** A screen is mostly one background
  colour. Rather than one quad per cell, each row is scanned for runs of equal
  background; a shell prompt collapses to roughly one rectangle per row.
- **Blank cells produce nothing.** The glyph list only receives cells with ink,
  so whitespace costs zero vertices and zero fragments.

Glyphs come from a shelf-packed atlas, so cost scales with *distinct* glyphs on
screen (typically under 100) rather than cell count.

### Text quality

Rasterization goes through `IDWriteGlyphRunAnalysis` with
`CLEARTYPE_3x1` coverage, always. The subpixel path consumes the three channels
as per-channel blend weights via **dual-source blending**
(`SRC1_COLOR` / `INV_SRC1_COLOR`) — fixed-function blending cannot otherwise
express a different alpha per channel. The grayscale path averages the three,
which is a better coverage estimate than asking DirectWrite for one channel.
Both blend in linear light with a configurable gamma and stem-darkening term,
which is what kitty's "correct sRGB linear gamma blending" is getting at.

### Considered and rejected: GPU curve rasterization

The [Slug algorithm](https://hackaday.com/2026/03/20/slug-algorithm-for-on-gpu-rendering-of-fonts-with-bezier-curves-now-in-public-domain/)
(JCGT 2017) entered the public domain in March 2026, and evaluating it was
tempting. It computes per-fragment coverage from Bézier curves with no atlas at
all, and it is genuinely the state of the art for *resolution-independent* text.

It is the wrong tool here. A terminal draws the same few dozen glyphs at one
fixed size thousands of times per frame. An atlas amortises rasterization to
approximately zero; Slug pays curve math per fragment, every frame, forever. It
wins when glyphs are large, arbitrary-scale, or animated — none of which
describes a terminal. Mesh-shader vector rendering
([AMD GPUOpen](https://gpuopen.com/learn/mesh_shaders/mesh_shaders-font_and_vector_art_rendering_with_mesh_shaders/))
was rejected for the same reason, plus a hard hardware floor.

This is worth stating plainly: adopting the newest technique would have made
this renderer slower. The atlas is not a compromise, it is the right answer for
the workload.

---

## 5a. Frosted glass: two different things called "acrylic"

Worth writing down, because the documentation does not make the distinction and
the wrong choice silently does nothing.

Windows exposes two mechanisms:

- **`DWMWA_SYSTEMBACKDROP_TYPE`** (Windows 11 22H2+) is documented, stable, and
  what WinUI uses. `DWMSBT_MAINWINDOW` is Mica. These sample the **desktop
  wallpaper**, not the windows behind yours. On a dark wallpaper the result is
  nearly a flat tint -- I measured the terminal background move from
  `(11,14,20)` opaque to `(15,18,23)` with the backdrop enabled, and putting a
  saturated window behind changed nothing.

- **`SetWindowCompositionAttribute`** with `ACCENT_ENABLE_ACRYLICBLURBEHIND`
  blurs what is **actually behind the window**, live. That is what people mean
  by frosted glass. It is undocumented -- exported from user32 but absent from
  the headers, so it must be resolved with `GetProcAddress` -- and has worked
  since Windows 10 1803.

Tachyon uses the second for `acrylic`/`blur` and the first for `mica`/`tabbed`,
falling back to the documented path if the entry point is missing. With a
magenta window placed exactly behind the terminal, the background measured
`(50,12,58)` -- the blur is genuinely sampling live content.

Two supporting pieces are needed either way: a **composition swap chain**
(`CreateSwapChainForComposition` with `DXGI_ALPHA_MODE_PREMULTIPLIED`, presented
through a DirectComposition visual) so our frame has a meaningful alpha channel,
and `WS_EX_NOREDIRECTIONBITMAP` so GDI does not allocate a redirection surface
we never use.

One consequence is worth stating: **subpixel antialiasing is incompatible with a
translucent window.** ClearType computes per-channel coverage against an assumed
background; over translucency the compositor picks that background after we are
done, so the fringes are blended against the wrong colour. Enabling a backdrop
switches to grayscale coverage automatically.

---

## 6. ConPTY

Windows' pseudoconsole is the only way to get a VT stream from console programs
that still use the legacy console API. It has a documented history of throughput
problems, and overlapped I/O support
([issue #262](https://github.com/microsoft/terminal/issues/262)) landed only
recently — after the Windows 11 24H2 feature cutoff.

ConPTY also re-renders its own screen buffer and re-emits VT, so the byte stream
a terminal receives bears little resemblance to what the application wrote. It
sets attributes before moving the cursor, terminates lines with explicit erases,
and splits styling across separate SGR sequences. That is not a detail you can
guess at -- the `TACHYON_DUMP` capture facility exists precisely so the real
stream can be read, and the `conpty_stream` regression tests are built from
captures taken that way.

Tachyon uses a dedicated thread with a blocking `ReadFile` and 128 KiB reads.
Overlapped I/O would let one thread service several sessions, which we do not
need; a blocked thread costs a stack and no CPU. The code that is not written
cannot deadlock.

The threading split is what matters: the reader thread owns the parser and
applies output; the main thread pumps messages and renders. A process saturating
the reader can never make a keystroke wait.

---

## 7. Why D3D11 and not D3D12 or wgpu

A terminal frame is three draw calls and a few hundred kilobytes of instance
data. There is no CPU submission cost worth removing, so D3D12's explicit model
buys nothing and costs a large amount of synchronisation code that can be wrong.
`wgpu` would add a shader translation layer and a dependency tree for
portability we do not want — this targets Windows deliberately.

Total dependency count is 29 crates, and the release binary is ~740 KB.

---

## Sources

- [For best performance, use DXGI flip model](https://devblogs.microsoft.com/directx/dxgi-flip-model/) — Microsoft DirectX blog
- [Swapchains and frame pacing](https://raphlinus.github.io/ui/graphics/gpu/2021/10/22/swapchain-frame-pacing.html) — Raph Levien
- [Atlas Engine](https://deepwiki.com/microsoft/terminal/3.2-atlas-engine) and [VT sequence processing](https://deepwiki.com/microsoft/terminal/2.3-vt-sequence-processing) — microsoft/terminal
- [ConPTY should support overlapped I/O](https://github.com/microsoft/terminal/issues/262) — microsoft/terminal
- [Introducing Ghostty and some useful Zig patterns](https://mitchellh.com/writing/ghostty-and-useful-zig-patterns) — Mitchell Hashimoto
- [Slug algorithm now in the public domain](https://hackaday.com/2026/03/20/slug-algorithm-for-on-gpu-rendering-of-fonts-with-bezier-curves-now-in-public-domain/) — Hackaday
- [Font and vector art rendering with mesh shaders](https://gpuopen.com/learn/mesh_shaders/mesh_shaders-font_and_vector_art_rendering_with_mesh_shaders/) — AMD GPUOpen
- [Subpixel text rendering with dual source blending](https://arkanis.de/weblog/2023-08-14-simple-good-quality-subpixel-text-rendering-in-opengl-with-stb-truetype-and-dual-source-blending/) — Arkanis
- [The Raster Tragedy in Skia](https://skia.org/docs/dev/design/raster_tragedy/) — Skia docs
- [A parser for DEC's ANSI-compatible video terminals](https://vt100.net/emu/dec_ansi_parser) — Paul Williams
- [Pin your app to the taskbar](https://learn.microsoft.com/en-us/windows/apps/develop/windows-integration/pin-to-taskbar) and [Application User Model IDs](https://github.com/MicrosoftDocs/win32/blob/docs/desktop-src/shell/appids.md) — Microsoft Learn
