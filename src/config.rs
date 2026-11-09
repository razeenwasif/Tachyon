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
    /// macOS: Liquid Glass (NSGlassEffectView, macOS 26 and later; a blurred
    /// NSVisualEffectView before). Elsewhere it means Acrylic.
    Glass,
}

/// Which palette a window uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Contrast {
    /// Follow System Settings > Accessibility > Display > Increase Contrast.
    #[default]
    Auto,
    High,
    Normal,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Appearance {
    /// Follow the system: the light theme in Light Mode, the dark one in Dark.
    #[default]
    Auto,
    Dark,
    Light,
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
    /// Programming ligatures (`->`, `!=`, `=>`) in fonts that have them,
    /// such as Cascadia Code, Fira Code and JetBrains Mono.
    pub ligatures: bool,
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
            ligatures: true,
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
    /// macOS: Liquid Glass's `clear` style instead of `regular`: less frosted,
    /// so more of what's behind shows, and bent more at the edges.
    pub glass_clear: bool,
    /// macOS: how strongly the glass itself is tinted with the theme's
    /// background, 0..=1. Unlike `opacity`, which draws the background over
    /// the glass, the tint colours the glass and keeps its refraction.
    pub glass_tint: f32,
    /// macOS: a visible title bar (on the glass, with the window's title, and
    /// the tabs in it when there are two or more) instead of edge-to-edge.
    pub title_bar: bool,
    /// macOS: full screen means around the notch: the green button, ⌃⌘F and
    /// View > Enter Full Screen use it, and Tachyon opens that way.
    pub notch_full_screen: bool,
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
            // Glass behind the text is the Mac look; Windows keeps v0.1's
            // opaque default.
            backdrop: if cfg!(target_os = "macos") { Backdrop::Glass } else { Backdrop::None },
            // On Liquid Glass the tint goes into the glass, which keeps its
            // refraction; clear glass shows the most of it.
            glass_clear: cfg!(target_os = "macos"),
            glass_tint: if cfg!(target_os = "macos") { 0.75 } else { 0.0 },
            title_bar: false,
            notch_full_screen: false,
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
    /// macOS: in a window, hand each frame to the compositor as soon as it's
    /// drawn rather than at the next refresh. The compositor still syncs to
    /// the display, so nothing tears; in full screen, where Metal can reach
    /// the display directly, `vsync` applies.
    pub low_latency: bool,
}

impl Default for RenderConfig {
    fn default() -> Self {
        RenderConfig {
            vsync: true,
            max_fps: 0,
            sync_timeout_ms: 150,
            low_latency: true,
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
    /// macOS: take the cursor colour from the system accent colour
    /// (`theme.cursor = accent`).
    pub cursor_accent: bool,
    /// macOS: tint the selection with the system accent colour
    /// (`theme.selection_bg = accent`).
    pub selection_accent: bool,
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
            cursor_accent: false,
            selection_accent: false,
        }
    }
}

impl ThemeConfig {
    /// The light theme: near-white paper, and ANSI colours darkened until
    /// they read on it (the default ANSI set is tuned for dark backgrounds).
    /// The colours sit close to Apple's light-mode system colours.
    pub fn light() -> ThemeConfig {
        let ansi: [[u8; 3]; 16] = [
            [0x1D, 0x1F, 0x24], // black
            [0xC4, 0x30, 0x2B], // red
            [0x2D, 0x7D, 0x46], // green
            [0x9C, 0x65, 0x00], // yellow
            [0x0A, 0x60, 0xD0], // blue
            [0x89, 0x44, 0xC7], // magenta
            [0x0E, 0x7C, 0x86], // cyan
            [0x6E, 0x72, 0x7A], // white: a grey, since white on white vanishes
            [0x5A, 0x5E, 0x66], // bright black
            [0xD7, 0x3A, 0x34], // bright red
            [0x24, 0x8A, 0x3D], // bright green
            [0xB2, 0x70, 0x00], // bright yellow
            [0x00, 0x71, 0xE3], // bright blue
            [0x9B, 0x4F, 0xD9], // bright magenta
            [0x0B, 0x8A, 0x96], // bright cyan
            [0x3A, 0x3D, 0x44], // bright white
        ];
        ThemeConfig {
            background: [0xFA, 0xFA, 0xFB],
            foreground: [0x1D, 0x1F, 0x24],
            cursor: [0x0B, 0x7A, 0x5C],
            cursor_text: [0xFA, 0xFA, 0xFB],
            selection_bg: [0xC9, 0xDD, 0xF8],
            selection_fg: None,
            ansi: ansi.map(Some),
            bold_is_bright: false,
            cursor_accent: false,
            selection_accent: false,
        }
    }

    /// The high-contrast version of this theme, for Increase Contrast: a
    /// black (or, for a light theme, white) background, white (or black)
    /// text, every ANSI colour moved toward the text colour until it reaches
    /// 7:1 contrast (WCAG AAA), and a solid selection with its own text
    /// colour. The slot shells use for dim text (bright black on dark, white
    /// on light) gets 4.5:1, so it still reads as dimmer; the slot that
    /// matches the background (black on dark) is left alone.
    pub fn high_contrast(&self, dark: bool) -> ThemeConfig {
        let (bg, fg) = if dark { ([0u8; 3], [0xFF; 3]) } else { ([0xFF; 3], [0u8; 3]) };
        let palette = self.palette();
        let mut ansi = [None; 16];
        for (i, slot) in ansi.iter_mut().enumerate() {
            let target = match (dark, i) {
                (true, 0) => None,
                (true, 8) | (false, 7) => Some(4.5),
                _ => Some(7.0),
            };
            *slot = Some(match target {
                Some(t) => toward_contrast(palette.0[i], fg, bg, t),
                None => palette.0[i],
            });
        }
        let cursor = toward_contrast(self.cursor, fg, bg, 4.5);
        ThemeConfig {
            background: bg,
            foreground: fg,
            cursor,
            cursor_text: bg,
            selection_bg: if dark { [0x1F, 0x4E, 0x96] } else { [0xA8, 0xCD, 0xFF] },
            selection_fg: Some(fg),
            ansi,
            bold_is_bright: self.bold_is_bright,
            cursor_accent: self.cursor_accent,
            selection_accent: false,
        }
    }

    /// Apply one `theme.*` key (without the `theme.` or `theme.light.`
    /// prefix). Returns false for a key this doesn't know.
    fn set(&mut self, key: &str, value: &str) -> Result<bool, String> {
        match key {
            "background" => self.background = parse_hex(value)?,
            "foreground" => self.foreground = parse_hex(value)?,
            "cursor" if value.eq_ignore_ascii_case("accent") => self.cursor_accent = true,
            "cursor" => {
                self.cursor = parse_hex(value)?;
                self.cursor_accent = false;
            }
            "cursor_text" => self.cursor_text = parse_hex(value)?,
            "selection_bg" if value.eq_ignore_ascii_case("accent") => self.selection_accent = true,
            "selection_bg" => {
                self.selection_bg = parse_hex(value)?;
                self.selection_accent = false;
            }
            "selection_fg" => self.selection_fg = Some(parse_hex(value)?),
            "bold_is_bright" => self.bold_is_bright = parse_bool(value)?,
            _ => {
                // ansi0 .. ansi15
                let Some(rest) = key.strip_prefix("ansi") else {
                    return Ok(false);
                };
                let idx: usize = rest.parse().map_err(|_| format!("bad palette index in `{key}`"))?;
                if idx >= 16 {
                    return Err(format!("palette index {idx} out of range (0-15)"));
                }
                self.ansi[idx] = Some(parse_hex(value)?);
            }
        }
        Ok(true)
    }

    /// The runtime palette: the default 256 colours with this theme's ANSI
    /// overrides.
    pub fn palette(&self) -> Palette {
        let mut p = Palette::new();
        for (i, over) in self.ansi.iter().enumerate() {
            if let Some(rgb) = over {
                p.0[i] = *rgb;
            }
        }
        p
    }
}

#[derive(Clone, Debug)]
pub struct ShellConfig {
    /// Program to launch. Empty means "pick the best available".
    pub program: String,
    pub args: Vec<String>,
    /// Load Tachyon's shell integration (OSC 133 prompt and command marks)
    /// into zsh automatically.
    pub integration: bool,
    /// Run shells in the session host (macOS), so quitting, crashing or
    /// updating Tachyon leaves them running and reopening it brings them
    /// back. Off: shells end with the app.
    pub persist: bool,
    /// macOS: grey suggestions after the cursor at a prompt, from commands
    /// run before; → takes one.
    pub suggestions: bool,
}

impl Default for ShellConfig {
    fn default() -> Self {
        ShellConfig {
            program: String::new(),
            args: Vec::new(),
            integration: true,
            persist: true,
            suggestions: true,
        }
    }
}

/// Which Option keys act as Meta (sending ESC + key) on macOS. Off by default,
/// so Option types the layout's special characters as it does everywhere else.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OptionAsMeta {
    #[default]
    Off,
    Left,
    Right,
    Both,
}

#[derive(Clone, Debug)]
pub struct MacConfig {
    pub option_as_meta: OptionAsMeta,
    /// A command that ran at least this long, and finished while you were
    /// looking elsewhere, posts a notification with its receipt. 0 turns
    /// notifications off.
    pub notify_after_secs: u64,
}

impl Default for MacConfig {
    fn default() -> Self {
        MacConfig { option_as_meta: OptionAsMeta::default(), notify_after_secs: 10 }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub font: FontConfig,
    pub window: WindowConfig,
    pub render: RenderConfig,
    /// The dark theme, or the only one on Windows.
    pub theme: ThemeConfig,
    /// macOS: the theme used in Light Mode when `appearance` is Auto.
    pub theme_light: ThemeConfig,
    pub appearance: Appearance,
    /// macOS: the high-contrast themes, derived from the light and dark
    /// ones, when Increase Contrast is on.
    pub contrast: Contrast,
    pub shell: ShellConfig,
    pub macos: MacConfig,
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
            theme_light: ThemeConfig::light(),
            appearance: Appearance::default(),
            contrast: Contrast::default(),
            shell: ShellConfig::default(),
            macos: MacConfig::default(),
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
                // On Liquid Glass the glass carries the tint (glass_tint), so
                // the background drawn over it stays faint: drawing it solid
                // would flatten the glass's refraction.
                self.window.opacity = if cfg!(target_os = "macos") && self.window.backdrop == Backdrop::Glass {
                    0.05
                } else {
                    0.82
                };
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
            "font.ligatures" => self.font.ligatures = parse_bool(value)?,
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

            "macos.option_as_meta" => {
                self.macos.option_as_meta = match value {
                    "false" | "off" | "no" => OptionAsMeta::Off,
                    "true" | "on" | "yes" | "both" => OptionAsMeta::Both,
                    "left" => OptionAsMeta::Left,
                    "right" => OptionAsMeta::Right,
                    _ => return Err(format!("option_as_meta: expected false, left, right or true, got `{value}`")),
                }
            }

            "notify.after" => self.macos.notify_after_secs = parse_usize(value, 0, 86_400)? as u64,
            "window.cols" => self.window.cols = parse_usize(value, 8, 2000)?,
            "window.rows" => self.window.rows = parse_usize(value, 2, 1000)?,
            "window.padding_x" => self.window.padding_x = parse_f32(value, 0.0, 200.0)?,
            "window.padding_y" => self.window.padding_y = parse_f32(value, 0.0, 200.0)?,
            "window.opacity" => {
                self.window.opacity = parse_f32(value, 0.0, 1.0)?;
                self.window.opacity_explicit = true;
            }
            "window.fullscreen" => {
                self.window.notch_full_screen = match value {
                    "notch" | "around_notch" => true,
                    "native" | "macos" | "system" => false,
                    _ => return Err(format!("window.fullscreen: expected native or notch, got `{value}`")),
                }
            }
            "window.layout" => {
                self.window.title_bar = match value {
                    "edge" | "edge-to-edge" | "edge_to_edge" => false,
                    "standard" | "title_bar" | "titlebar" => true,
                    _ => return Err(format!("window.layout: expected edge or standard, got `{value}`")),
                }
            }
            "window.backdrop" => {
                self.window.backdrop = match value {
                    "none" | "off" => Backdrop::None,
                    "glass" | "liquid_glass" => Backdrop::Glass,
                    "acrylic" => Backdrop::Acrylic,
                    "blur" => Backdrop::Blur,
                    "mica" => Backdrop::Mica,
                    "tabbed" => Backdrop::Tabbed,
                    _ => return Err(format!("unknown backdrop `{value}`")),
                }
            }
            "window.glass" => {
                self.window.glass_clear = match value.to_ascii_lowercase().as_str() {
                    "regular" => false,
                    "clear" => true,
                    _ => return Err(format!("window.glass: expected regular or clear, got `{value}`")),
                }
            }
            "window.glass_tint" => self.window.glass_tint = parse_f32(value, 0.0, 1.0)?,
            "window.blur" => {
                self.window.backdrop = if parse_bool(value)? {
                    Backdrop::Acrylic
                } else {
                    Backdrop::None
                }
            }

            "render.vsync" => self.render.vsync = parse_bool(value)?,
            "render.low_latency" => self.render.low_latency = parse_bool(value)?,
            "render.max_fps" => self.render.max_fps = parse_usize(value, 0, 1000)? as u32,
            "render.sync_timeout_ms" => {
                self.render.sync_timeout_ms = parse_usize(value, 0, 5000)? as u64
            }

            "theme.appearance" => {
                self.appearance = match value.to_ascii_lowercase().as_str() {
                    "auto" | "system" => Appearance::Auto,
                    "dark" => Appearance::Dark,
                    "light" => Appearance::Light,
                    _ => return Err(format!("theme.appearance: expected auto, dark or light, got `{value}`")),
                }
            }

            "theme.contrast" => {
                self.contrast = match value.to_ascii_lowercase().as_str() {
                    "auto" | "system" => Contrast::Auto,
                    "high" | "increased" => Contrast::High,
                    "normal" => Contrast::Normal,
                    _ => return Err(format!("theme.contrast: expected auto, high or normal, got `{value}`")),
                }
            }

            "shell.program" => self.shell.program = value.to_string(),
            "shell.integration" => self.shell.integration = parse_bool(value)?,
            "shell.persist" => self.shell.persist = parse_bool(value)?,
            "shell.suggestions" => self.shell.suggestions = parse_bool(value)?,
            "shell.args" => {
                self.shell.args = value
                    .split_whitespace()
                    .map(str::to_string)
                    .collect()
            }

            "scrollback" => self.scrollback = parse_usize(value, 0, 5_000_000)?,

            _ => {
                // theme.light.* sets the light theme; other theme.* the dark.
                let known = if let Some(rest) = key.strip_prefix("theme.light.") {
                    self.theme_light.set(rest, value)?
                } else if let Some(rest) = key.strip_prefix("theme.") {
                    // The accent colour isn't light or dark, so `accent` for
                    // the cursor or selection applies to both themes.
                    if value.eq_ignore_ascii_case("accent") {
                        self.theme_light.set(rest, value)?;
                    }
                    self.theme.set(rest, value)?
                } else {
                    false
                };
                if !known {
                    return Err(format!("unknown key `{key}`"));
                }
            }
        }
        Ok(())
    }

    /// Build the runtime palette, applying any `theme.ansiN` overrides.
    pub fn palette(&self) -> Palette {
        self.theme.palette()
    }
}

/// WCAG relative luminance of an sRGB colour.
pub fn luminance(c: [u8; 3]) -> f64 {
    let lin = |v: u8| {
        let c = v as f64 / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}

/// WCAG contrast ratio between two colours, 1 to 21.
pub fn contrast_ratio(a: [u8; 3], b: [u8; 3]) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// `c` mixed toward `toward` just enough to reach `target` contrast against
/// `bg`, keeping as much of its colour as it can.
fn toward_contrast(c: [u8; 3], toward: [u8; 3], bg: [u8; 3], target: f64) -> [u8; 3] {
    let mix = |t: f64| -> [u8; 3] { std::array::from_fn(|i| (c[i] as f64 + (toward[i] as f64 - c[i] as f64) * t).round() as u8) };
    if contrast_ratio(c, bg) >= target {
        return c;
    }
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..20 {
        let mid = (lo + hi) / 2.0;
        if contrast_ratio(mix(mid), bg) >= target { hi = mid } else { lo = mid }
    }
    mix(hi)
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
    fn light_theme_keys_and_accent() {
        let c = Config::parse(
            "theme.appearance = auto\ntheme.light.background = #FFFFFF\ntheme.background = #000000\ntheme.cursor = accent\n",
        );
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        assert_eq!(c.appearance, Appearance::Auto);
        assert_eq!(c.theme_light.background, [0xFF; 3]);
        assert_eq!(c.theme.background, [0; 3]);
        // accent goes to both themes; other keys only to their own.
        assert!(c.theme.cursor_accent && c.theme_light.cursor_accent);
        assert!(!c.theme.selection_accent);
        assert_eq!(c.theme_light.ansi[3], Some([0x9C, 0x65, 0x00]), "the light theme's own yellow");
    }

    #[test]
    fn glass_is_the_mac_default() {
        let c = Config::parse("");
        if cfg!(target_os = "macos") {
            assert_eq!(c.window.backdrop, Backdrop::Glass);
            assert!(c.window.opacity < 1.0);
            assert!(c.window.glass_clear && c.window.glass_tint > 0.5, "tinted clear glass");
        }
        let regular = Config::parse("window.glass = regular\nwindow.glass_tint = 0.4\n");
        assert!(!regular.window.glass_clear);
        assert_eq!(regular.window.glass_tint, 0.4);
        let opaque = Config::parse("window.backdrop = none\n");
        assert_eq!(opaque.window.backdrop, Backdrop::None);
        assert_eq!(opaque.window.opacity, 1.0);
    }

    #[test]
    fn option_as_meta_accepts_sides() {
        assert_eq!(Config::parse("").macos.option_as_meta, OptionAsMeta::Off);
        assert_eq!(Config::parse("macos.option_as_meta = left").macos.option_as_meta, OptionAsMeta::Left);
        assert_eq!(Config::parse("macos.option_as_meta = true").macos.option_as_meta, OptionAsMeta::Both);
        let bad = Config::parse("macos.option_as_meta = sideways");
        assert_eq!(bad.macos.option_as_meta, OptionAsMeta::Off);
        assert_eq!(bad.warnings.len(), 1);
    }

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

#[cfg(test)]
mod contrast_tests {
    use super::*;

    #[test]
    fn high_contrast_themes_reach_aaa() {
        for (theme, dark) in [(ThemeConfig::default(), true), (ThemeConfig::light(), false)] {
            let hc = theme.high_contrast(dark);
            assert_eq!(contrast_ratio(hc.foreground, hc.background), 21.0);
            for (i, c) in hc.ansi.iter().enumerate() {
                let r = contrast_ratio(c.unwrap(), hc.background);
                let want = match (dark, i) {
                    (true, 0) => 1.0,
                    (true, 8) | (false, 7) => 4.5,
                    _ => 7.0,
                };
                assert!(r >= want - 0.01, "{} slot {i}: {r:.2}:1", if dark { "dark" } else { "light" });
            }
            let sel = contrast_ratio(hc.selection_fg.unwrap(), hc.selection_bg);
            assert!(sel >= 7.0, "selection text {sel:.2}:1");
            // Colours keep their hue: red stays reddest in its own slot.
            let red = hc.ansi[1].unwrap();
            assert!(red[0] > red[1] && red[0] > red[2], "{red:?}");
        }
    }

    #[test]
    fn the_layout_is_a_setting() {
        let mut c = Config::default();
        assert!(!c.window.title_bar, "edge-to-edge by default");
        c.set("window.layout", "standard").unwrap();
        assert!(c.window.title_bar);
        c.set("window.layout", "edge").unwrap();
        assert!(!c.window.title_bar);
        assert!(c.set("window.layout", "floating").is_err());
        assert!(!c.window.notch_full_screen, "macOS's own full screen by default");
        c.set("window.fullscreen", "notch").unwrap();
        assert!(c.window.notch_full_screen);
        assert!(c.set("window.fullscreen", "kiosk").is_err());
    }

    #[test]
    fn contrast_is_a_setting() {
        let mut c = Config::default();
        c.set("theme.contrast", "high").unwrap();
        assert_eq!(c.contrast, Contrast::High);
        assert!(c.set("theme.contrast", "loud").is_err());
    }
}
