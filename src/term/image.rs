//! Inline images: the iTerm2 protocol (OSC 1337 `File=`, what `imgcat` and
//! many plotting backends send) and the kitty graphics protocol (APC `G`,
//! what `matplotlib-backend-kitty`, `timg` and `chafa` use).
//!
//! The terminal keeps an image's bytes as they came (PNG, JPEG, GIF, or raw
//! RGB and RGBA from kitty) and where it sits: from a line and column, so
//! many cells wide and tall. Lines are counted from the first one the screen
//! ever showed, so an image scrolls with the text around it, and goes when its
//! lines leave the scrollback. Decoding and drawing are the platform's.
//!
//! Sizes are in device pixels, as kitty clients expect: `CSI 16 t` reports a
//! cell's.

use std::sync::Arc;

use base64::Engine;

/// How an image's bytes are stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// PNG, JPEG, GIF or anything else the platform decodes.
    Encoded,
    /// Raw pixels from kitty: 3 or 4 bytes each.
    Rgb,
    Rgba,
}

#[derive(Debug)]
pub struct Image {
    /// Unique for this terminal, so the renderer can cache its texture.
    pub id: u64,
    pub format: Format,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// An image on the grid.
#[derive(Clone, Debug)]
pub struct Placement {
    pub image: Arc<Image>,
    /// The line it starts on, counted from the first the screen ever had
    /// (negative once a region scroll has pushed it partly off the top).
    pub line: i64,
    pub col: usize,
    pub cols: usize,
    pub rows: usize,
    /// Placed while a full-screen program had the alternate screen.
    pub alt: bool,
    /// kitty's image id, for `a=d` deletes.
    pub kitty_id: Option<u32>,
    /// It takes the place of the cells it covers, as iTerm2's and sixel
    /// images do: text written or erased over any of them removes it. A
    /// kitty image is a layer of its own, under the text.
    pub in_cells: bool,
}

/// The width and height of a PNG, GIF or JPEG, from its header.
pub fn encoded_size(data: &[u8]) -> Option<(u32, u32)> {
    let be32 = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    if data.len() >= 24 && data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some((be32(&data[16..20]), be32(&data[20..24])));
    }
    if data.len() >= 10 && (data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")) {
        return Some((u16::from_le_bytes([data[6], data[7]]) as u32, u16::from_le_bytes([data[8], data[9]]) as u32));
    }
    if data.len() >= 26 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        match &data[12..16] {
            b"VP8X" if data.len() >= 30 => {
                let w = u32::from_le_bytes([data[24], data[25], data[26], 0]) + 1;
                let h = u32::from_le_bytes([data[27], data[28], data[29], 0]) + 1;
                return Some((w, h));
            }
            b"VP8L" if data[20] == 0x2F => {
                let bits = u32::from_le_bytes([data[21], data[22], data[23], data[24]]);
                return Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1));
            }
            b"VP8 " if data.len() >= 30 && data[23] == 0x9D && data[24] == 0x01 && data[25] == 0x2A => {
                let w = (u16::from_le_bytes([data[26], data[27]]) & 0x3FFF) as u32;
                let h = (u16::from_le_bytes([data[28], data[29]]) & 0x3FFF) as u32;
                if w > 0 && h > 0 {
                    return Some((w, h));
                }
            }
            _ => {}
        }
    }
    if data.starts_with(&[0xFF, 0xD8]) {
        // JPEG: walk the segments to a start-of-frame marker.
        let mut i = 2;
        while i + 9 < data.len() {
            if data[i] != 0xFF {
                return None;
            }
            let marker = data[i + 1];
            let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
            let sof = matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF);
            if sof {
                let h = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
                let w = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
                return Some((w, h));
            }
            i += 2 + len;
        }
    }
    None
}

/// How many cells `px` pixels take, at `cell` pixels a cell, rounding up.
fn cells(px: u32, cell: u32) -> usize {
    (px as usize).div_ceil(cell.max(1) as usize).max(1)
}

/// An image's size in cells: as asked for, or its own size, never wider than
/// `max_cols` (scaled down, keeping its shape).
pub fn fit(width: u32, height: u32, cell: (u32, u32), want: (Option<usize>, Option<usize>), max_cols: usize) -> (usize, usize) {
    let natural = (cells(width, cell.0), cells(height, cell.1));
    let aspect = |c: usize| ((c as f64 * cell.0 as f64 * height as f64) / (width.max(1) as f64 * cell.1 as f64)).ceil().max(1.0) as usize;
    let (mut c, mut r) = match want {
        (Some(c), Some(r)) => (c, r),
        (Some(c), None) => (c, aspect(c)),
        (None, Some(r)) => (((r as f64 * cell.1 as f64 * width as f64) / (height.max(1) as f64 * cell.0 as f64)).ceil().max(1.0) as usize, r),
        (None, None) => natural,
    };
    if c > max_cols {
        r = (r * max_cols).div_ceil(c).max(1);
        c = max_cols;
    }
    (c.max(1), r.max(1))
}

fn decode_base64(s: &[u8]) -> Option<Vec<u8>> {
    let clean: Vec<u8> = s.iter().copied().filter(|b| !b.is_ascii_whitespace()).collect();
    base64::engine::general_purpose::STANDARD.decode(&clean).ok().or_else(|| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&clean).ok())
}

// ---------------------------------------------------------------------------
// iTerm2: OSC 1337 ; File=[key=value;...] : base64
// ---------------------------------------------------------------------------

/// An iTerm2 image to show: its bytes, size, and the cells asked for.
pub struct Iterm {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Columns and rows asked for, if any.
    pub want: (Option<usize>, Option<usize>),
}

/// The image in an OSC 1337 `File=` sequence, given everything after
/// `1337;`. `None` for a file not meant to be shown inline, or one that
/// isn't an image.
pub fn parse_iterm(rest: &[u8], cell: (u32, u32), (cols, rows): (usize, usize)) -> Option<Iterm> {
    let rest = rest.strip_prefix(b"File=")?;
    let colon = rest.iter().position(|&b| b == b':')?;
    let (args, payload) = (&rest[..colon], &rest[colon + 1..]);
    let mut inline = false;
    let (mut w_arg, mut h_arg) = (None, None);
    for kv in args.split(|&b| b == b';') {
        let kv = String::from_utf8_lossy(kv);
        match kv.split_once('=') {
            Some(("inline", v)) => inline = v == "1",
            Some(("width", v)) => w_arg = Some(v.to_string()),
            Some(("height", v)) => h_arg = Some(v.to_string()),
            _ => {}
        }
    }
    if !inline {
        return None;
    }
    let data = decode_base64(payload)?;
    let (width, height) = encoded_size(&data)?;
    // "N" cells, "Npx", "N%" of the terminal, or "auto".
    let dim = |arg: &Option<String>, cell: u32, total: usize| -> Option<usize> {
        let a = arg.as_deref()?;
        if let Some(px) = a.strip_suffix("px") {
            return px.parse::<u32>().ok().map(|p| cells(p, cell));
        }
        if let Some(pc) = a.strip_suffix('%') {
            return pc.parse::<f64>().ok().map(|p| ((total as f64 * p / 100.0).round() as usize).max(1));
        }
        a.parse::<usize>().ok()
    };
    let want = (dim(&w_arg, cell.0, cols), dim(&h_arg, cell.1, rows));
    Some(Iterm { data, width, height, want })
}

// ---------------------------------------------------------------------------
// kitty: APC G key=value,... ; base64 ST
// ---------------------------------------------------------------------------

/// The keys of one kitty graphics command.
#[derive(Clone, Debug, Default)]
pub struct Kitty {
    pub action: u8,
    pub format: u32,
    pub medium: u8,
    pub more: bool,
    pub id: Option<u32>,
    pub quiet: u32,
    pub width: u32,
    pub height: u32,
    pub size: Option<usize>,
    pub offset: u64,
    pub cols: Option<usize>,
    pub rows: Option<usize>,
    pub stay: bool,
    pub delete: u8,
    pub compressed: bool,
    /// Tachyon-internal snapshot restore placement hints (`Term::to_vt()`).
    pub restore_line: Option<i64>,
    pub restore_col: Option<usize>,
    pub restore_in_cells: Option<bool>,
}

/// A kitty graphics command's keys and its (still base64) payload.
pub fn parse_kitty(apc: &[u8]) -> Option<(Kitty, &[u8])> {
    let body = apc.strip_prefix(b"G")?;
    let (keys, payload) = match body.iter().position(|&b| b == b';') {
        Some(i) => (&body[..i], &body[i + 1..]),
        None => (body, &body[body.len()..]),
    };
    let mut k = Kitty { action: b't', format: 32, medium: b'd', delete: b'a', ..Kitty::default() };
    for kv in keys.split(|&b| b == b',') {
        let Some(eq) = kv.iter().position(|&b| b == b'=') else { continue };
        let (key, val) = (&kv[..eq], &kv[eq + 1..]);
        let num = || std::str::from_utf8(val).ok().and_then(|s| s.parse::<u32>().ok());
        match key {
            b"a" => k.action = *val.first()?,
            b"f" => k.format = num()?,
            b"t" => k.medium = *val.first()?,
            b"m" => k.more = num() == Some(1),
            b"i" => k.id = num(),
            b"q" => k.quiet = num().unwrap_or(0),
            b"s" => k.width = num().unwrap_or(0),
            b"v" => k.height = num().unwrap_or(0),
            b"S" => k.size = std::str::from_utf8(val).ok().and_then(|s| s.parse::<usize>().ok()),
            b"O" => k.offset = std::str::from_utf8(val).ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0),
            b"c" => k.cols = num().map(|n| n as usize).filter(|&n| n > 0),
            b"r" => k.rows = num().map(|n| n as usize).filter(|&n| n > 0),
            b"C" => k.stay = num() == Some(1),
            b"d" => k.delete = *val.first().unwrap_or(&b'a'),
            b"o" => k.compressed = val == b"z",
            b"ty" => k.restore_line = std::str::from_utf8(val).ok().and_then(|s| s.parse::<i64>().ok()),
            b"tx" => k.restore_col = std::str::from_utf8(val).ok().and_then(|s| s.parse::<usize>().ok()),
            b"tc" => {
                k.restore_in_cells = match val {
                    b"1" => Some(true),
                    b"0" => Some(false),
                    _ => None,
                };
            }
            _ => {}
        }
    }
    Some((k, payload))
}

const MAX_KITTY_DATA: usize = 64 << 20;

/// Compress bytes into zlib format for `o=z` kitty graphics transfers.
pub fn compress_zlib(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
}

fn is_safe_temp_image_path(path: &std::path::Path) -> bool {
    let has_marker = path
        .to_string_lossy()
        .contains("tty-graphics-protocol");
    if !has_marker {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    let Ok(canon_parent) = std::fs::canonicalize(parent) else {
        return false;
    };
    let mut roots: Vec<std::path::PathBuf> = vec![std::env::temp_dir()];
    for extra in ["/tmp", "/var/tmp", "/dev/shm"] {
        let p = std::path::PathBuf::from(extra);
        if p.exists() {
            roots.push(p);
        }
    }
    roots
        .into_iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .any(|r| canon_parent.starts_with(r))
}

/// A kitty image's pixels, once all its chunks have arrived: its format and
/// size, or why not. Supports `t=d` (direct), `t=f` (file), `t=t` (temp file
/// unlinked when `unlink_temp` is true), and `o=z` zlib decompression.
pub fn kitty_image(
    k: &Kitty,
    base64: &[u8],
    unlink_temp: bool,
) -> Result<(Format, u32, u32, Vec<u8>), &'static str> {
    let mut data = match k.medium {
        b'd' => decode_base64(base64).ok_or("EINVAL:bad base64")?,
        b'f' | b't' => {
            use std::io::{Read, Seek, SeekFrom};
            let path_bytes = decode_base64(base64).ok_or("EINVAL:bad base64")?;
            let path_str = std::str::from_utf8(&path_bytes).map_err(|_| "EINVAL:bad path")?;
            let path = std::path::Path::new(path_str);
            let meta = std::fs::metadata(path).map_err(|_| "ENOENT:file not found")?;
            if !meta.is_file() {
                return Err("EINVAL:not a regular file");
            }
            if k.size.is_none() && meta.len() > MAX_KITTY_DATA as u64 {
                return Err("EFBIG:file too large");
            }
            let max_read = k.size.unwrap_or(MAX_KITTY_DATA).min(MAX_KITTY_DATA);
            let mut file = std::fs::File::open(path).map_err(|_| "ENOENT:file open failed")?;
            if k.offset > 0 {
                file.seek(SeekFrom::Start(k.offset))
                    .map_err(|_| "EINVAL:bad offset")?;
            }
            let mut buf = Vec::new();
            file.take(max_read as u64)
                .read_to_end(&mut buf)
                .map_err(|_| "EIO:file read failed")?;
            if k.medium == b't' && unlink_temp && is_safe_temp_image_path(path) {
                let _ = std::fs::remove_file(path);
            }
            buf
        }
        _ => return Err("ENOTSUPPORTED:transmission medium"),
    };

    if k.compressed {
        data = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&data, MAX_KITTY_DATA)
            .map_err(|_| "EINVAL:bad zlib")?;
    }

    match k.format {
        100 => {
            let (w, h) = encoded_size(&data).ok_or("EBADPNG:not an image")?;
            Ok((Format::Encoded, w, h, data))
        }
        24 | 32 => {
            let bpp = if k.format == 24 { 3usize } else { 4usize };
            let need = (k.width as usize)
                .checked_mul(k.height as usize)
                .and_then(|px| px.checked_mul(bpp));
            if k.width == 0 || k.height == 0 || need.is_none_or(|n| data.len() < n) {
                return Err("ENODATA:short pixel data");
            }
            Ok((if bpp == 3 { Format::Rgb } else { Format::Rgba }, k.width, k.height, data))
        }
        _ => Err("EINVAL:format"),
    }
}

/// Extract per-frame delays in milliseconds from an animated GIF (`GIF87a`/`GIF89a`).
/// Returns `Some(delays)` when more than one image frame is present, else `None`.
pub fn gif_frame_delays_ms(data: &[u8]) -> Option<Vec<u32>> {
    if data.len() < 13 || (!data.starts_with(b"GIF87a") && !data.starts_with(b"GIF89a")) {
        return None;
    }
    let packed = data[10];
    let mut i = 13usize;
    if packed & 0x80 != 0 {
        i += 3 * (1usize << ((packed & 0x07) + 1));
    }
    if i > data.len() {
        return None;
    }

    fn skip_sub_blocks(data: &[u8], mut pos: usize) -> Option<usize> {
        while pos < data.len() {
            let sz = data[pos] as usize;
            pos += 1;
            if sz == 0 {
                return Some(pos);
            }
            pos = pos.checked_add(sz)?;
            if pos > data.len() {
                return None;
            }
        }
        None
    }

    let mut pending_delay_ms: Option<u32> = None;
    let mut delays = Vec::new();
    while i < data.len() {
        match data[i] {
            0x3B => break,
            0x21 => {
                if i + 2 > data.len() {
                    break;
                }
                let label = data[i + 1];
                i += 2;
                if label == 0xF9 && i + 5 <= data.len() && data[i] == 4 {
                    let delay_cs = u16::from_le_bytes([data[i + 2], data[i + 3]]) as u32;
                    let ms = delay_cs * 10;
                    pending_delay_ms = Some(if ms < 20 { 100 } else { ms });
                }
                let Some(next) = skip_sub_blocks(data, i) else {
                    break;
                };
                i = next;
            }
            0x2C => {
                if i + 10 > data.len() {
                    break;
                }
                delays.push(pending_delay_ms.take().unwrap_or(100));
                let lct = data[i + 9];
                i += 10;
                if lct & 0x80 != 0 {
                    i += 3 * (1usize << ((lct & 0x07) + 1));
                }
                if i >= data.len() {
                    break;
                }
                i += 1; // LZW minimum code size
                let Some(next) = skip_sub_blocks(data, i) else {
                    break;
                };
                i = next;
            }
            _ => break,
        }
    }
    (delays.len() > 1).then_some(delays)
}

// ---------------------------------------------------------------------------
// Sixel: DCS P1 ; P2 ; P3 q <data> ST
// ---------------------------------------------------------------------------

/// The VT340's sixteen colours, in percent, the registers' defaults.
const VT340: [[u8; 3]; 16] = [
    [0, 0, 0], [20, 20, 80], [80, 13, 13], [20, 80, 20], [80, 20, 80], [20, 80, 80], [80, 80, 20], [53, 53, 53],
    [26, 26, 26], [33, 33, 60], [60, 26, 26], [33, 60, 33], [60, 33, 60], [33, 60, 60], [60, 60, 33], [80, 80, 80],
];

/// The largest side of a sixel image kept.
const SIXEL_MAX: usize = 8192;

/// A sixel image's width, height and pixels (straight RGBA; pixels no sixel
/// set are transparent).
pub fn decode_sixel(_params: &[u16], data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let pct = |v: u16| ((v.min(100) as u32 * 255 + 50) / 100) as u8;
    let mut palette: Vec<[u8; 4]> = (0..1024)
        .map(|i| VT340.get(i).map_or([0, 0, 0, 255], |c| [pct(c[0] as u16), pct(c[1] as u16), pct(c[2] as u16), 255]))
        .collect();
    let mut color = 0usize;
    // Rows of pixels, each as long as drawn so far.
    let mut rows: Vec<Vec<[u8; 4]>> = Vec::new();
    let (mut x, mut y) = (0usize, 0usize);
    let mut raster: Option<(usize, usize)> = None;
    let mut width = 0usize;

    let mut i = 0;
    // Numbers separated by `;` from `i`.
    let numbers = |i: &mut usize| -> Vec<u16> {
        let mut out = vec![0u16];
        while *i < data.len() {
            match data[*i] {
                d @ b'0'..=b'9' => {
                    let last = out.last_mut().expect("one");
                    *last = last.saturating_mul(10).saturating_add((d - b'0') as u16);
                }
                b';' => out.push(0),
                _ => break,
            }
            *i += 1;
        }
        out
    };
    while i < data.len() {
        let b = data[i];
        i += 1;
        let mut count = 1;
        let sixel = match b {
            b'"' => {
                let n = numbers(&mut i);
                if let (Some(&w), Some(&h)) = (n.get(2), n.get(3)) {
                    if w > 0 && h > 0 {
                        raster = Some((w as usize, h as usize));
                    }
                }
                continue;
            }
            b'#' => {
                let n = numbers(&mut i);
                color = n[0] as usize % palette.len();
                if n.len() >= 5 {
                    palette[color] = match n[1] {
                        1 => hls(n[2], n[3], n[4]),
                        _ => [pct(n[2]), pct(n[3]), pct(n[4]), 255],
                    };
                }
                continue;
            }
            b'!' => {
                count = numbers(&mut i)[0].max(1) as usize;
                let Some(&c) = data.get(i) else { break };
                i += 1;
                c
            }
            b'$' => {
                x = 0;
                continue;
            }
            b'-' => {
                x = 0;
                y += 6;
                continue;
            }
            c => c,
        };
        if !(0x3F..=0x7E).contains(&sixel) {
            continue;
        }
        let bits = sixel - 0x3F;
        let end = (x + count).min(SIXEL_MAX);
        if bits != 0 && y + 6 <= SIXEL_MAX {
            if rows.len() < y + 6 {
                rows.resize_with(y + 6, Vec::new);
            }
            let px = palette[color];
            for bit in 0..6 {
                if bits & (1 << bit) != 0 {
                    let row = &mut rows[y + bit];
                    if row.len() < end {
                        row.resize(end, [0; 4]);
                    }
                    row[x..end].fill(px);
                }
            }
        }
        x = end;
        width = width.max(x);
    }
    let (w, h) = match raster {
        Some((rw, rh)) => (rw.min(SIXEL_MAX), rh.min(SIXEL_MAX)),
        None => (width, rows.len()),
    };
    if w == 0 || h == 0 {
        return None;
    }
    let mut out = vec![0u8; w * h * 4];
    for (r, row) in rows.iter().take(h).enumerate() {
        for (c, px) in row.iter().take(w).enumerate() {
            out[(r * w + c) * 4..][..4].copy_from_slice(px);
        }
    }
    Some((w as u32, h as u32, out))
}

/// A sixel HLS colour (hue 0 is blue, as on the VT340) as RGBA.
fn hls(h: u16, l: u16, s: u16) -> [u8; 4] {
    let h = ((h as f32 + 240.0) % 360.0) / 360.0;
    let (l, s) = (l.min(100) as f32 / 100.0, s.min(100) as f32 / 100.0);
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let channel = |t: f32| {
        let t = t.rem_euclid(1.0);
        let v = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        (v * 255.0).round() as u8
    };
    [channel(h + 1.0 / 3.0), channel(h), channel(h - 1.0 / 3.0), 255]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2x3 PNG header (enough for the size).
    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0]);
        v
    }

    #[test]
    fn sizes_from_headers() {
        assert_eq!(encoded_size(&png(640, 480)), Some((640, 480)));
        assert_eq!(encoded_size(b"GIF89a\x20\x00\x10\x00rest"), Some((32, 16)));
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 0, 0, 0xFF, 0xC0, 0, 11, 8, 0, 50, 0, 100, 3, 0, 0];
        assert_eq!(encoded_size(&jpeg), Some((100, 50)));
        assert_eq!(encoded_size(b"plain text"), None);
    }

    #[test]
    fn fitting_into_cells() {
        // 140x56 px at 14x28 px cells: 10 by 2.
        assert_eq!(fit(140, 56, (14, 28), (None, None), 80), (10, 2));
        // Asked for 20 columns: rows follow the shape.
        assert_eq!(fit(140, 56, (14, 28), (Some(20), None), 80), (20, 4));
        // Too wide for the terminal: scaled down to fit.
        assert_eq!(fit(1400, 560, (14, 28), (None, None), 50), (50, 10));
    }

    #[test]
    fn iterm_sequences() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(png(280, 56));
        let seq = format!("File=name=eC5wbmc=;size=33;inline=1;width=10:{b64}");
        let i = parse_iterm(seq.as_bytes(), (14, 28), (80, 24)).unwrap();
        assert_eq!((i.width, i.height, i.want), (280, 56, (Some(10), None)));
        let download = format!("File=name=eC5wbmc=:{b64}");
        assert!(parse_iterm(download.as_bytes(), (14, 28), (80, 24)).is_none(), "not inline: a download");
        let px = format!("File=inline=1;width=140px;height=50%:{b64}");
        assert_eq!(parse_iterm(px.as_bytes(), (14, 28), (80, 24)).unwrap().want, (Some(10), Some(12)));
    }

    #[test]
    fn kitty_commands() {
        let (k, payload) = parse_kitty(b"Ga=T,f=100,i=7,m=1,q=1;AAAA").unwrap();
        assert_eq!((k.action, k.format, k.id, k.more, k.quiet), (b'T', 100, Some(7), true, 1));
        assert_eq!(payload, b"AAAA");
        let (k, _) = parse_kitty(b"Ga=d,d=I,i=3").unwrap();
        assert_eq!((k.action, k.delete, k.id), (b'd', b'I', Some(3)));
        let raw = base64::engine::general_purpose::STANDARD.encode([255u8; 2 * 2 * 4]);
        let (k, p) = parse_kitty(format!("Gf=32,s=2,v=2;{raw}").as_bytes()).map(|(k, p)| (k, p.to_vec())).unwrap();
        assert_eq!(kitty_image(&k, &p, true).map(|(f, w, h, _)| (f, w, h)), Ok((Format::Rgba, 2, 2)));
        let (k, p) = parse_kitty(b"Gf=32,s=4,v=4;AAAA").map(|(k, p)| (k, p.to_vec())).unwrap();
        assert!(kitty_image(&k, &p, true).is_err(), "too few pixels");
    }

    #[test]
    fn kitty_compressed_zlib_and_file_transfers_decode_safely() {
        let raw = vec![10u8, 20, 30, 255, 40, 50, 60, 255];
        let z = compress_zlib(&raw);
        let b64_z = base64::engine::general_purpose::STANDARD.encode(&z);
        let (k, p) = parse_kitty(format!("Ga=T,f=32,s=2,v=1,o=z;{b64_z}").as_bytes())
            .map(|(k, p)| (k, p.to_vec()))
            .unwrap();
        let (fmt, w, h, decoded) = kitty_image(&k, &p, true).unwrap();
        assert_eq!((fmt, w, h, decoded), (Format::Rgba, 2, 1, raw.clone()));

        // Regular temp-file transfer (`t=t`) unlinks only matching tty-graphics-protocol paths
        // and only when `unlink_temp == true`.
        let dir = std::env::temp_dir().join(format!("tachyon-img-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let safe_tmp = dir.join("tty-graphics-protocol-frame.rgba");
        std::fs::write(&safe_tmp, &raw).unwrap();
        let b64_path = base64::engine::general_purpose::STANDARD.encode(safe_tmp.to_string_lossy().as_bytes());

        // Attached pass (`unlink_temp = false`) keeps the file intact for the peer parser.
        let (kt, pt) = parse_kitty(format!("Ga=T,t=t,f=32,s=2,v=1;{b64_path}").as_bytes())
            .map(|(k, p)| (k, p.to_vec()))
            .unwrap();
        assert_eq!(kitty_image(&kt, &pt, false).unwrap().3, raw);
        assert!(safe_tmp.exists(), "attached reader must not unlink before peer sees t=t");
        assert_eq!(kitty_image(&kt, &pt, true).unwrap().3, raw);
        assert!(!safe_tmp.exists(), "temp tty-graphics-protocol file should be unlinked");

        // Regular file without `tty-graphics-protocol` (`t=t` safety guard + `t=f` preservation).
        let keep_file = dir.join("keep-plot.rgba");
        std::fs::write(&keep_file, &raw).unwrap();
        let b64_keep = base64::engine::general_purpose::STANDARD.encode(keep_file.to_string_lossy().as_bytes());
        let (kf, pf) = parse_kitty(format!("Ga=T,t=f,f=32,s=2,v=1;{b64_keep}").as_bytes())
            .map(|(k, p)| (k, p.to_vec()))
            .unwrap();
        assert_eq!(kitty_image(&kf, &pf, true).unwrap().3, raw);
        assert!(keep_file.exists(), "t=f must never delete user files");

        let (kt_unsafe, pt_unsafe) = parse_kitty(format!("Ga=T,t=t,f=32,s=2,v=1;{b64_keep}").as_bytes())
            .map(|(k, p)| (k, p.to_vec()))
            .unwrap();
        assert_eq!(kitty_image(&kt_unsafe, &pt_unsafe, true).unwrap().3, raw);
        assert!(keep_file.exists(), "t=t without tty-graphics-protocol must not unlink");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gif_frame_delays_parser_reads_and_clamps_gce_delays() {
        // Minimal 1x1 2-frame GIF89a with GCE delays: 6cs (60ms) and 1cs (clamped -> 100ms).
        let mut gif = Vec::new();
        gif.extend_from_slice(b"GIF89a");
        gif.extend_from_slice(&[1, 0, 1, 0, 0x80, 0, 0]); // 1x1, 2-color GCT
        gif.extend_from_slice(&[255, 0, 0, 0, 0, 255]); // red, blue
        // Frame 1: delay 6cs = 60ms
        gif.extend_from_slice(&[0x21, 0xF9, 0x04, 0x04, 6, 0, 0, 0]);
        gif.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x44, 0x01, 0]);
        // Frame 2: delay 1cs -> clamped to 100ms
        gif.extend_from_slice(&[0x21, 0xF9, 0x04, 0x04, 1, 0, 0, 0]);
        gif.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x4C, 0x01, 0]);
        gif.push(0x3B);

        assert_eq!(encoded_size(&gif), Some((1, 1)));
        assert_eq!(gif_frame_delays_ms(&gif), Some(vec![60, 100]));

        // Single-frame GIF returns None (static image).
        let single = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\xff\x00\x00\x00\x00\xff\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02\x44\x01\x00\x3b";
        assert_eq!(gif_frame_delays_ms(single), None);
    }

    #[test]
    fn sixel_images() {
        // Raster 4x7; colour 1 red, a 4-wide band of full sixels, then one
        // more row (bit 0) of colour 2 green on the next band, 1 pixel wide.
        let data = b"\"1;1;4;7#1;2;100;0;0#1!4~-#2;2;0;100;0#2@";
        let (w, h, px) = decode_sixel(&[0, 1, 0], data).unwrap();
        assert_eq!((w, h), (4, 7));
        let at = |x: usize, y: usize| &px[(y * 4 + x) * 4..][..4];
        assert_eq!(at(3, 5), &[255, 0, 0, 255]);
        assert_eq!(at(0, 6), &[0, 255, 0, 255]);
        assert_eq!(at(1, 6), &[0, 0, 0, 0], "never set: transparent");
        // No raster attributes: the size is what was drawn.
        let (w, h, _) = decode_sixel(&[], b"#0~~$-~").unwrap();
        assert_eq!((w, h), (2, 12));
        // HLS: hue 120 is red on the VT340's wheel.
        assert_eq!(hls(120, 50, 100), [255, 0, 0, 255]);
        assert!(decode_sixel(&[], b"").is_none());
    }
}
