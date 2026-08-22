//! Configuration.
//!
//! The format is a flat `dotted.key = value` file. It is deliberately not TOML:
//! a terminal has no nested data, and hand-rolling this keeps the dependency
//! graph (and therefore cold-start time) minimal. Unknown keys are collected
//! and reported rather than silently ignored, because a typo'd setting that
//! quietly does nothing is worse than a warning.

use std::fs;
use std::path::{Path, PathBuf};

use crate::term::color::Palette;

/// The DWM system backdrop drawn behind a translucent window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backdrop {
    /// Opaque; no compositor involvement.
    None,
    /// Acrylic -- a strong blur that samples whatever is behind the window.
    /// This is the "frosted glass" look.
    Acrylic,
    /// Mica -- tints from the desktop wallpaper rather than live content.
    /// Cheaper, calmer, and it does not shimmer when windows move behind it.
    Mica,
    /// Mica's tabbed variant, slightly more opaque.
    Tabbed,
    /// A plain gaussian blur of what is behind, without acrylic's noise and
    /// tint. Cheaper, and it holds up better on older machines.
    Blur,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Antialias {
    /// ClearType-style RGB coverage. Sharpest on a normal desktop LCD.
    Subpixel,
    /// Single-channel coverage. Correct on rotated or non-RGB-stripe panels,
    /// and what you want if the window is translucent.
    Grayscale,
}

#[derive(Clone, Debug)]
pub struct FontConfig {
    pub family: String,
    pub size: f32,
    pub antialias: Antialias,
    /// Blending gamma. 1.0 is physically linear; slightly above renders
    /// stems fuller, which most people prefer for terminal text.
    pub gamma: f32,
    /// Stem-darkening applied to coverage before blending, 0.0..1.0.
    pub contrast: f32,
    /// Multiplier on the font's natural line height.
    pub line_height: f32,
    /// Multiplier on the advance width used for the cell.
    pub cell_width: f32,
    pub use_bold_font: bool,
    pub use_italic_font: bool,
}

impl Default for FontConfig {
    fn default() -> Self {
        FontConfig {
            family: "Cascadia Mono".into(),
            size: 12.0,
            antialias: Antialias::Subpixel,
            gamma: 1.2,
            contrast: 0.15,
            line_height: 1.0,
            cell_width: 1.0,
            use_bold_font: true,
            use_italic_font: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct WindowConfig {
    pub cols: usize,
    pub rows: usize,
    /// Padding in logical pixels around the grid.
    pub padding_x: f32,
    pub padding_y: f32,
    /// How opaque the terminal background is, 0.1..=1.0. Only meaningful with a
    /// backdrop, since there is otherwise nothing behind us to show through.
    pub opacity: f32,
    pub backdrop: Backdrop,
    /// Whether the user set `window.opacity` explicitly. Enabling a backdrop
    /// picks a sensible translucency by default, but must not override a value
    /// the user asked for.
    pub opacity_explicit: bool,
}

impl Default for WindowConfig {
    fn default() -> Self {
        WindowConfig {
            cols: 120,
            rows: 32,
            padding_x: 8.0,
            padding_y: 6.0,
            opacity: 1.0,
            backdrop: Backdrop::None,
            opacity_explicit: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RenderConfig {
    /// Synchronise presentation to the display refresh. Turning this off
    /// permits tearing in exchange for the lowest possible latency.
    pub vsync: bool,
    /// Upper bound on presents per second. Zero means "display refresh".
    pub max_fps: u32,
    /// How long to hold a synchronised-output (mode 2026) batch before giving
    /// up and drawing anyway, in milliseconds. Guards against an application
    /// that begins a batch and never ends it.
    pub sync_timeout_ms: u64,
}

impl Default for RenderConfig {
    fn default() -> Self {
        RenderConfig {
            vsync: true,
            max_fps: 0,
            sync_timeout_ms: 150,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ThemeConfig {
    pub background: [u8; 3],
    pub foreground: [u8; 3],
    pub cursor: [u8; 3],
    pub cursor_text: [u8; 3],
    pub selection_bg: [u8; 3],
    pub selection_fg: Option<[u8; 3]>,
    /// Overrides for palette slots 0-15.
    pub ansi: [Option<[u8; 3]>; 16],
    /// Render SGR-bold text with the bright palette variant.
    pub bold_is_bright: bool,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        ThemeConfig {
            background: [0x0B, 0x0E, 0x14],
            foreground: [0xC4, 0xCB, 0xD8],
            cursor: [0x7D, 0xF9, 0xC4],
            cursor_text: [0x0B, 0x0E, 0x14],
            selection_bg: [0x2A, 0x3A, 0x52],
            selection_fg: None,
            ansi: [None; 16],
            bold_is_bright: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ShellConfig {
    /// Program to launch. Empty means "pick the best available".
    pub program: String,
    pub args: Vec<String>,
}

impl Default for ShellConfig {
    fn default() -> Self {
        ShellConfig {
            program: String::new(),
            args: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub font: FontConfig,
    pub window: WindowConfig,
    pub render: RenderConfig,
    pub theme: ThemeConfig,
    pub shell: ShellConfig,
    pub scrollback: usize,
    /// Diagnostics produced while loading, surfaced by the caller.
    pub warnings: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            font: FontConfig::default(),
            window: WindowConfig::default(),
            render: RenderConfig::default(),
            theme: ThemeConfig::default(),
            shell: ShellConfig::default(),
            scrollback: 10_000,
            warnings: Vec::new(),
        }
    }
}

impl Config {
    /// `%APPDATA%\Tachyon\tachyon.conf`, or `$XDG_CONFIG_HOME` equivalent when
    /// running the test suite on a non-Windows host.
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("Tachyon").join("tachyon.conf"))
    }

    pub fn load_default() -> Config {
        match Config::default_path() {
            Some(p) if p.exists() => Config::load(&p),
            _ => Config::default(),
        }
    }

    pub fn load(path: &Path) -> Config {
        match fs::read_to_string(path) {
            Ok(text) => Config::parse(&text),
            Err(e) => {
                let mut c = Config::default();
                c.warnings.push(format!("could not read {}: {e}", path.display()));
                c
            }
        }
    }

    pub fn parse(text: &str) -> Config {
        let mut c = Config::default();

        for (lineno, raw) in text.lines().enumerate() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                c.warnings
                    .push(format!("line {}: expected `key = value`", lineno + 1));
                continue;
            };
            let key = key.trim();
            let value = unquote(value.trim());

            if let Err(msg) = c.set(key, value) {
                c.warnings.push(format!("line {}: {msg}", lineno + 1));
            }
        }
        c.finish();
        c
    }

    /// Resolve settings that depend on each other.
    fn finish(&mut self) {
        if self.window.backdrop != Backdrop::None {
            // A fully opaque background hides the backdrop completely, which
            // looks like the feature is broken. Pick a translucency that shows
            // it while keeping text readable -- unless the user chose one.
            if !self.window.opacity_explicit {
                self.window.opacity = 0.82;
            }
            // Subpixel antialiasing needs to know the colour behind each glyph.
            // Over a translucent window that colour is decided by the
            // compositor after we are done, so the per-channel coverage would be
            // blended against the wrong thing and fringe badly. Grayscale
            // coverage composites correctly at any opacity.
            if self.window.opacity < 1.0 {
                self.font.antialias = Antialias::Grayscale;
            }
        }
    }

    /// Does the window need an alpha channel and a composition swap chain?
    pub fn translucent(&self) -> bool {
        self.window.backdrop != Backdrop::None || self.window.opacity < 1.0
    }

    fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "font.family" => self.font.family = value.to_string(),
            "font.size" => self.font.size = parse_f32(value, 4.0, 200.0)?,
            "font.antialias" => {
                self.font.antialias = match value {
                    "subpixel" | "cleartype" | "rgb" => Antialias::Subpixel,
                    "grayscale" | "greyscale" | "gray" => Antialias::Grayscale,
                    _ => return Err(format!("unknown antialias mode `{value}`")),
                }
            }
            "font.gamma" => self.font.gamma = parse_f32(value, 0.5, 3.0)?,
            "font.contrast" => self.font.contrast = parse_f32(value, 0.0, 1.0)?,
            "font.line_height" => self.font.line_height = parse_f32(value, 0.5, 3.0)?,
            "font.cell_width" => self.font.cell_width = parse_f32(value, 0.5, 3.0)?,
            "font.bold" => self.font.use_bold_font = parse_bool(value)?,
            "font.italic" => self.font.use_italic_font = parse_bool(value)?,

            "window.cols" => self.window.cols = parse_usize(value, 8, 2000)?,
            "window.rows" => self.window.rows = parse_usize(value, 2, 1000)?,
            "window.padding_x" => self.window.padding_x = parse_f32(value, 0.0, 200.0)?,
            "window.padding_y" => self.window.padding_y = parse_f32(value, 0.0, 200.0)?,
            "window.opacity" => {
                self.window.opacity = parse_f32(value, 0.1, 1.0)?;
                self.window.opacity_explicit = true;
            }
            "window.backdrop" => {
                self.window.backdrop = match value {
                    "none" | "off" => Backdrop::None,
                    "acrylic" | "glass" => Backdrop::Acrylic,
                    "blur" => Backdrop::Blur,
                    "mica" => Backdrop::Mica,
                    "tabbed" => Backdrop::Tabbed,
                    _ => return Err(format!("unknown backdrop `{value}`")),
                }
            }
            "window.blur" => {
                self.window.backdrop = if parse_bool(value)? {
                    Backdrop::Acrylic
                } else {
                    Backdrop::None
                }
            }

            "render.vsync" => self.render.vsync = parse_bool(value)?,
            "render.max_fps" => self.render.max_fps = parse_usize(value, 0, 1000)? as u32,
            "render.sync_timeout_ms" => {
                self.render.sync_timeout_ms = parse_usize(value, 0, 5000)? as u64
            }

            "theme.background" => self.theme.background = parse_hex(value)?,
            "theme.foreground" => self.theme.foreground = parse_hex(value)?,
            "theme.cursor" => self.theme.cursor = parse_hex(value)?,
            "theme.cursor_text" => self.theme.cursor_text = parse_hex(value)?,
            "theme.selection_bg" => self.theme.selection_bg = parse_hex(value)?,
            "theme.selection_fg" => self.theme.selection_fg = Some(parse_hex(value)?),
            "theme.bold_is_bright" => self.theme.bold_is_bright = parse_bool(value)?,

            "shell.program" => self.shell.program = value.to_string(),
            "shell.args" => {
                self.shell.args = value
                    .split_whitespace()
                    .map(str::to_string)
                    .collect()
            }

            "scrollback" => self.scrollback = parse_usize(value, 0, 5_000_000)?,

            _ => {
                // theme.ansi0 .. theme.ansi15
                if let Some(rest) = key.strip_prefix("theme.ansi") {
                    let idx: usize = rest
                        .parse()
                        .map_err(|_| format!("bad palette index in `{key}`"))?;
                    if idx >= 16 {
                        return Err(format!("palette index {idx} out of range (0-15)"));
                    }
                    self.theme.ansi[idx] = Some(parse_hex(value)?);
                } else {
                    return Err(format!("unknown key `{key}`"));
                }
            }
        }
        Ok(())
    }

    /// Build the runtime palette, applying any `theme.ansiN` overrides.
    pub fn palette(&self) -> Palette {
        let mut p = Palette::new();
        for (i, over) in self.theme.ansi.iter().enumerate() {
            if let Some(rgb) = over {
                p.0[i] = *rgb;
            }
        }
        p
    }
}

fn strip_comment(line: &str) -> &str {
    // `#` is overloaded: it opens a comment, but it also introduces a colour
    // literal. Disambiguate positionally -- a comment `#` starts a token, so it
    // sits at the beginning of the line or follows whitespace. `#101418` after
    // an `=` is a value. Quoted `#` is always literal.
    let bytes = line.as_bytes();
    let mut in_quotes = false;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' => in_quotes = !in_quotes,
            b'#' if !in_quotes => {
                let starts_token = i == 0 || bytes[i - 1].is_ascii_whitespace();
                // `x = #fff` has whitespace before the `#` too, so also require
                // that we are not immediately after the `=`.
                let after_assign = line[..i]
                    .trim_end()
                    .ends_with('=');
                if starts_token && !after_assign {
                    return &line[..i];
                }
            }
            _ => {}
        }
    }
    line
}

fn unquote(v: &str) -> &str {
    let b = v.as_bytes();
    if b.len() >= 2 && b[0] == b'"' && b[b.len() - 1] == b'"' {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

fn parse_f32(v: &str, lo: f32, hi: f32) -> Result<f32, String> {
    let n: f32 = v.parse().map_err(|_| format!("`{v}` is not a number"))?;
    if !(lo..=hi).contains(&n) {
        return Err(format!("{n} is outside {lo}..={hi}"));
    }
    Ok(n)
}

fn parse_usize(v: &str, lo: usize, hi: usize) -> Result<usize, String> {
    let n: usize = v.parse().map_err(|_| format!("`{v}` is not an integer"))?;
    if !(lo..=hi).contains(&n) {
        return Err(format!("{n} is outside {lo}..={hi}"));
    }
    Ok(n)
}

fn parse_bool(v: &str) -> Result<bool, String> {
    match v {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => Err(format!("`{v}` is not a boolean")),
    }
}

fn parse_hex(v: &str) -> Result<[u8; 3], String> {
    let s = v.strip_prefix('#').unwrap_or(v);
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("`{v}` is not a #rrggbb colour"));
    }
    let n = u32::from_str_radix(s, 16).map_err(|e| e.to_string())?;
    Ok([(n >> 16) as u8, (n >> 8) as u8, n as u8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_representative_file() {
        let c = Config::parse(
            r##"
            # Tachyon config
            font.family = "Cascadia Code"   # inline comment
            font.size = 13.5
            font.antialias = grayscale
            window.cols = 100
            window.opacity = 0.95
            render.vsync = off
            theme.background = #101418
            theme.ansi4 = "#4488FF"
            scrollback = 50000
            "##,
        );
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        assert_eq!(c.font.family, "Cascadia Code");
        assert_eq!(c.font.size, 13.5);
        assert_eq!(c.font.antialias, Antialias::Grayscale);
        assert_eq!(c.window.cols, 100);
        assert_eq!(c.window.opacity, 0.95);
        assert!(!c.render.vsync);
        assert_eq!(c.theme.background, [0x10, 0x14, 0x18]);
        assert_eq!(c.palette().0[4], [0x44, 0x88, 0xFF]);
        assert_eq!(c.scrollback, 50_000);
    }

    #[test]
    fn reports_bad_keys_and_values_without_dying() {
        let c = Config::parse(
            "font.size = huge\nnope.what = 1\nfont.size = 900\ntheme.ansi99 = #000000\n",
        );
        assert_eq!(c.warnings.len(), 4, "{:?}", c.warnings);
        // Defaults survive.
        assert_eq!(c.font.size, FontConfig::default().size);
    }

    #[test]
    fn hash_inside_quotes_is_not_a_comment() {
        let c = Config::parse("theme.foreground = \"#ABCDEF\"\n");
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        assert_eq!(c.theme.foreground, [0xAB, 0xCD, 0xEF]);
    }
}
