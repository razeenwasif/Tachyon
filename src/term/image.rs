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
    /// The line it starts on, counted from the first the screen ever had.
    pub line: u64,
    pub col: usize,
    pub cols: usize,
    pub rows: usize,
    /// Placed while a full-screen program had the alternate screen.
    pub alt: bool,
    /// kitty's image id, for `a=d` deletes.
    pub kitty_id: Option<u32>,
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
    pub cols: Option<usize>,
    pub rows: Option<usize>,
    pub stay: bool,
    pub delete: u8,
    pub compressed: bool,
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
            b"c" => k.cols = num().map(|n| n as usize).filter(|&n| n > 0),
            b"r" => k.rows = num().map(|n| n as usize).filter(|&n| n > 0),
            b"C" => k.stay = num() == Some(1),
            b"d" => k.delete = *val.first().unwrap_or(&b'a'),
            b"o" => k.compressed = val == b"z",
            _ => {}
        }
    }
    Some((k, payload))
}

/// A kitty image's pixels, once all its chunks have arrived: its format and
/// size, or why not.
pub fn kitty_image(k: &Kitty, base64: &[u8]) -> Result<(Format, u32, u32, Vec<u8>), &'static str> {
    if k.medium != b'd' {
        return Err("ENOTSUPPORTED:only direct transmission");
    }
    if k.compressed {
        return Err("ENOTSUPPORTED:compression");
    }
    let data = decode_base64(base64).ok_or("EINVAL:bad base64")?;
    match k.format {
        100 => {
            let (w, h) = encoded_size(&data).ok_or("EBADPNG:not an image")?;
            Ok((Format::Encoded, w, h, data))
        }
        24 | 32 => {
            let bpp = if k.format == 24 { 3 } else { 4 };
            if k.width == 0 || k.height == 0 || data.len() < (k.width * k.height * bpp) as usize {
                return Err("ENODATA:short pixel data");
            }
            Ok((if bpp == 3 { Format::Rgb } else { Format::Rgba }, k.width, k.height, data))
        }
        _ => Err("EINVAL:format"),
    }
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
        assert_eq!(kitty_image(&k, &p).map(|(f, w, h, _)| (f, w, h)), Ok((Format::Rgba, 2, 2)));
        let (k, p) = parse_kitty(b"Gf=32,s=4,v=4;AAAA").map(|(k, p)| (k, p.to_vec())).unwrap();
        assert!(kitty_image(&k, &p).is_err(), "too few pixels");
    }
}
