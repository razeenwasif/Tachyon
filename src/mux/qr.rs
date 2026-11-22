//! Zero-dependency ISO/IEC 18004 Byte-mode QR Code encoder (Versions 1..=12, ECC Level L).

/// `(total_cw, ec_per_block, g1_blocks, g1_data_cw, g2_blocks, g2_data_cw)` for Versions 1..=12, Level L.
const VERSION_PARAMS_L: [(usize, usize, usize, usize, usize, usize); 12] = [
    (26, 7, 1, 19, 0, 0),    // V1 (21x21)
    (44, 10, 1, 34, 0, 0),   // V2 (25x25)
    (70, 15, 1, 55, 0, 0),   // V3 (29x29)
    (100, 20, 1, 80, 0, 0),  // V4 (33x33)
    (134, 26, 1, 108, 0, 0), // V5 (37x37)
    (172, 18, 2, 68, 0, 0),  // V6 (41x41)
    (196, 20, 2, 78, 0, 0),  // V7 (45x45)
    (242, 24, 2, 97, 0, 0),  // V8 (49x49)
    (292, 30, 2, 116, 0, 0), // V9 (53x53)
    (346, 18, 2, 68, 2, 69), // V10 (57x57)
    (404, 20, 4, 81, 0, 0),  // V11 (61x61)
    (466, 24, 2, 92, 2, 93), // V12 (65x65)
];

const ALIGN_CENTERS: [&[usize]; 12] = [
    &[],
    &[6, 18],
    &[6, 22],
    &[6, 26],
    &[6, 30],
    &[6, 34],
    &[6, 22, 38],
    &[6, 24, 42],
    &[6, 26, 46],
    &[6, 28, 50],
    &[6, 30, 54],
    &[6, 32, 58],
];

/// Encoded QR code matrix (dark = `true`, light = `false`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QrCode {
    version: usize,
    size: usize,
    modules: Vec<bool>,
}

impl QrCode {
    pub fn size(&self) -> usize {
        self.size
    }

    pub fn version(&self) -> usize {
        self.version
    }

    pub fn get(&self, x: usize, y: usize) -> bool {
        if x < self.size && y < self.size {
            self.modules[y * self.size + x]
        } else {
            false
        }
    }

    /// Encodes `data` in Byte mode at ECC Level L (supports up to 367 bytes / V12).
    pub fn encode(data: &[u8]) -> Result<Self, String> {
        let len = data.len();
        let (idx, params) = VERSION_PARAMS_L
            .iter()
            .enumerate()
            .find(|(idx, &(_, _, g1b, g1d, g2b, g2d))| {
                let version = idx + 1;
                let data_cw = g1b * g1d + g2b * g2d;
                let count_bits = if version <= 9 { 8 } else { 16 };
                4 + count_bits + len * 8 <= data_cw * 8
            })
            .ok_or_else(|| format!("QR payload too large ({len} bytes; max 367 bytes)"))?;

        let version = idx + 1;
        let size = version * 4 + 17;
        let &(_, ec_len, g1_blocks, g1_data, g2_blocks, g2_data) = params;
        let data_cw = g1_blocks * g1_data + g2_blocks * g2_data;

        // Build Byte-mode bit stream.
        let mut bits = BitBuf::new(data_cw * 8);
        bits.push(0b0100, 4);
        let count_bits = if version <= 9 { 8 } else { 16 };
        bits.push(len as u32, count_bits);
        for &b in data {
            bits.push(b as u32, 8);
        }
        let max_bits = data_cw * 8;
        let term = (max_bits - bits.bit_len()).min(4);
        bits.push(0, term);
        while !bits.bit_len().is_multiple_of(8) {
            bits.push(0, 1);
        }
        let mut codewords = bits.into_bytes();
        let mut pad_toggle = false;
        while codewords.len() < data_cw {
            codewords.push(if pad_toggle { 0x11 } else { 0xEC });
            pad_toggle = !pad_toggle;
        }

        // Split data codewords into RS blocks and compute interleaved stream.
        let (gf_exp, gf_log) = gf_tables();
        let rs_gen = rs_generator(ec_len, &gf_exp, &gf_log);
        let total_blocks = g1_blocks + g2_blocks;
        let mut data_blocks = Vec::with_capacity(total_blocks);
        let mut ec_blocks = Vec::with_capacity(total_blocks);
        let mut offset = 0usize;
        for b in 0..total_blocks {
            let blen = if b < g1_blocks { g1_data } else { g2_data };
            let block = &codewords[offset..offset + blen];
            offset += blen;
            data_blocks.push(block);
            ec_blocks.push(rs_remainder(block, &rs_gen, &gf_exp, &gf_log));
        }

        let max_data_len = g1_data.max(g2_data);
        let mut interleaved = Vec::with_capacity(data_cw + total_blocks * ec_len);
        for col in 0..max_data_len {
            for block in &data_blocks {
                if col < block.len() {
                    interleaved.push(block[col]);
                }
            }
        }
        for col in 0..ec_len {
            for ec in &ec_blocks {
                interleaved.push(ec[col]);
            }
        }

        // Place function modules and reserve format/version slots.
        let mut base = vec![false; size * size];
        let mut reserved = vec![false; size * size];
        place_finder(&mut base, &mut reserved, size, 0, 0);
        place_finder(&mut base, &mut reserved, size, size - 7, 0);
        place_finder(&mut base, &mut reserved, size, 0, size - 7);
        place_separators(&mut reserved, size);
        place_timing(&mut base, &mut reserved, size);
        place_alignments(&mut base, &mut reserved, size, ALIGN_CENTERS[version - 1]);
        // Dark module `(8, size - 8)`.
        base[(size - 8) * size + 8] = true;
        reserved[(size - 8) * size + 8] = true;
        reserve_format_areas(&mut reserved, size);
        if version >= 7 {
            place_version_info(&mut base, &mut reserved, size, version);
        }
        place_data_bits(&mut base, &reserved, size, &interleaved);

        // Evaluate all 8 masks and keep the lowest ISO/IEC 18004 penalty score.
        let mut best_score = u32::MAX;
        let mut best_modules = Vec::new();
        for mask in 0..8usize {
            let mut candidate = base.clone();
            apply_mask(&mut candidate, &reserved, size, mask);
            place_format_info(&mut candidate, size, mask);
            let score = penalty_score(&candidate, size);
            if score < best_score {
                best_score = score;
                best_modules = candidate;
            }
        }

        Ok(Self {
            version,
            size,
            modules: best_modules,
        })
    }

    /// Renders compact Unicode half-block terminal art with a 2-module quiet zone.
    ///
    /// With `invert = true` (dark terminal default), light quiet zones are rendered
    /// using `'█'` and dark QR modules using `' '` for reliable phone camera scan.
    pub fn render_unicode(&self, invert: bool) -> String {
        let quiet = 2usize;
        let total = self.size + quiet * 2;
        let lit = |qx: usize, qy: usize| -> bool {
            let dark = if qx >= quiet && qy >= quiet && qx - quiet < self.size && qy - quiet < self.size {
                self.get(qx - quiet, qy - quiet)
            } else {
                false
            };
            dark ^ invert
        };

        let mut out = String::with_capacity((total + 1) * total.div_ceil(2));
        let mut y = 0usize;
        while y < total {
            for x in 0..total {
                let top = lit(x, y);
                let bot = if y + 1 < total { lit(x, y + 1) } else { invert };
                let ch = match (top, bot) {
                    (false, false) => ' ',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (true, true) => '█',
                };
                out.push(ch);
            }
            out.push('\n');
            y += 2;
        }
        out
    }
}

struct BitBuf {
    bytes: Vec<u8>,
    bit_len: usize,
}

impl BitBuf {
    fn new(cap_bits: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(cap_bits.div_ceil(8)),
            bit_len: 0,
        }
    }

    fn bit_len(&self) -> usize {
        self.bit_len
    }

    fn push(&mut self, val: u32, count: usize) {
        for shift in (0..count).rev() {
            let bit = ((val >> shift) & 1) as u8;
            if self.bit_len.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let idx = self.bit_len / 8;
            let bit_pos = 7 - (self.bit_len % 8);
            self.bytes[idx] |= bit << bit_pos;
            self.bit_len += 1;
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

fn gf_tables() -> ([u8; 512], [u8; 256]) {
    let mut exp = [0u8; 512];
    let mut log = [0u8; 256];
    let mut x = 1usize;
    for i in 0..255usize {
        exp[i] = x as u8;
        log[x] = i as u8;
        x <<= 1;
        if x >= 256 {
            x ^= 0x11D;
        }
    }
    for i in 255..512 {
        exp[i] = exp[i - 255];
    }
    (exp, log)
}

fn gf_mul(a: u8, b: u8, exp: &[u8; 512], log: &[u8; 256]) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        exp[log[a as usize] as usize + log[b as usize] as usize]
    }
}

fn rs_generator(degree: usize, exp: &[u8; 512], log: &[u8; 256]) -> Vec<u8> {
    let mut poly = vec![1u8];
    for i in 0..degree {
        let factor = exp[i];
        let mut next = vec![0u8; poly.len() + 1];
        for (j, &coef) in poly.iter().enumerate() {
            next[j] ^= coef;
            next[j + 1] ^= gf_mul(coef, factor, exp, log);
        }
        poly = next;
    }
    poly
}

fn rs_remainder(data: &[u8], gen: &[u8], exp: &[u8; 512], log: &[u8; 256]) -> Vec<u8> {
    let ec_len = gen.len() - 1;
    let mut rem = vec![0u8; ec_len];
    for &b in data {
        let factor = b ^ rem[0];
        rem.copy_within(1.., 0);
        rem[ec_len - 1] = 0;
        for (slot, &g) in rem.iter_mut().zip(&gen[1..]) {
            *slot ^= gf_mul(g, factor, exp, log);
        }
    }
    rem
}

fn place_finder(modules: &mut [bool], reserved: &mut [bool], size: usize, x0: usize, y0: usize) {
    for dy in 0..7 {
        for dx in 0..7 {
            let dark = dx == 0
                || dx == 6
                || dy == 0
                || dy == 6
                || ((2..=4).contains(&dx) && (2..=4).contains(&dy));
            let idx = (y0 + dy) * size + (x0 + dx);
            modules[idx] = dark;
            reserved[idx] = true;
        }
    }
}

fn place_separators(reserved: &mut [bool], size: usize) {
    for i in 0..8 {
        // Top-left separator.
        reserved[7 * size + i] = true;
        reserved[i * size + 7] = true;
        // Top-right separator.
        reserved[7 * size + (size - 8 + i)] = true;
        reserved[i * size + (size - 8)] = true;
        // Bottom-left separator.
        reserved[(size - 8) * size + i] = true;
        reserved[(size - 8 + i) * size + 7] = true;
    }
}

fn place_timing(modules: &mut [bool], reserved: &mut [bool], size: usize) {
    for i in 8..(size - 8) {
        let dark = i.is_multiple_of(2);
        let h_idx = 6 * size + i;
        modules[h_idx] = dark;
        reserved[h_idx] = true;
        let v_idx = i * size + 6;
        modules[v_idx] = dark;
        reserved[v_idx] = true;
    }
}

fn place_alignments(modules: &mut [bool], reserved: &mut [bool], size: usize, centers: &[usize]) {
    for &cy in centers {
        for &cx in centers {
            let overlaps_finder = (cx <= 8 && cy <= 8)
                || (cx >= size - 8 && cy <= 8)
                || (cx <= 8 && cy >= size - 8);
            if overlaps_finder {
                continue;
            }
            for dy in -2isize..=2 {
                for dx in -2isize..=2 {
                    let x = (cx as isize + dx) as usize;
                    let y = (cy as isize + dy) as usize;
                    let dark = dx.abs() == 2 || dy.abs() == 2 || (dx == 0 && dy == 0);
                    let idx = y * size + x;
                    modules[idx] = dark;
                    reserved[idx] = true;
                }
            }
        }
    }
}

fn reserve_format_areas(reserved: &mut [bool], size: usize) {
    for i in 0..=8 {
        if i != 6 {
            reserved[8 * size + i] = true;
            reserved[i * size + 8] = true;
        }
    }
    for i in 0..8 {
        reserved[8 * size + (size - 1 - i)] = true;
        reserved[(size - 1 - i) * size + 8] = true;
    }
}

fn place_version_info(modules: &mut [bool], reserved: &mut [bool], size: usize, version: usize) {
    let mut rem = (version as u32) << 12;
    for i in (12..=17).rev() {
        if (rem >> i) & 1 != 0 {
            rem ^= 0x1F25 << (i - 12);
        }
    }
    let bits = ((version as u32) << 12) | rem;
    for i in 0..18usize {
        let dark = ((bits >> i) & 1) != 0;
        let a = i / 3;
        let b = i % 3;
        let bl = (size - 11 + b) * size + a;
        modules[bl] = dark;
        reserved[bl] = true;
        let tr = a * size + (size - 11 + b);
        modules[tr] = dark;
        reserved[tr] = true;
    }
}

fn place_data_bits(modules: &mut [bool], reserved: &[bool], size: usize, data: &[u8]) {
    let mut bit_idx = 0usize;
    let total_bits = data.len() * 8;
    let mut right = (size - 1) as isize;
    let mut upward = true;

    while right > 0 {
        if right == 6 {
            right -= 1;
        }
        for step in 0..size {
            let y = if upward { size - 1 - step } else { step };
            for dx in [0isize, -1isize] {
                let x = (right + dx) as usize;
                let idx = y * size + x;
                if reserved[idx] {
                    continue;
                }
                let bit = if bit_idx < total_bits {
                    let byte = data[bit_idx / 8];
                    ((byte >> (7 - (bit_idx % 8))) & 1) != 0
                } else {
                    false
                };
                modules[idx] = bit;
                bit_idx += 1;
            }
        }
        right -= 2;
        upward = !upward;
    }
}

fn mask_bit(mask: usize, x: usize, y: usize) -> bool {
    match mask {
        0 => (x + y).is_multiple_of(2),
        1 => y.is_multiple_of(2),
        2 => x.is_multiple_of(3),
        3 => (x + y).is_multiple_of(3),
        4 => (y / 2 + x / 3).is_multiple_of(2),
        5 => ((x * y) % 2) + ((x * y) % 3) == 0,
        6 => (((x * y) % 2) + ((x * y) % 3)).is_multiple_of(2),
        _ => (((x + y) % 2) + ((x * y) % 3)).is_multiple_of(2),
    }
}

fn apply_mask(modules: &mut [bool], reserved: &[bool], size: usize, mask: usize) {
    for y in 0..size {
        for x in 0..size {
            let idx = y * size + x;
            if !reserved[idx] && mask_bit(mask, x, y) {
                modules[idx] = !modules[idx];
            }
        }
    }
}

fn place_format_info(modules: &mut [bool], size: usize, mask: usize) {
    let ecc_l = 0b01u32;
    let data = (ecc_l << 3) | (mask as u32);
    let mut rem = data << 10;
    for i in (10..=14).rev() {
        if (rem >> i) & 1 != 0 {
            rem ^= 0x537 << (i - 10);
        }
    }
    let format = ((data << 10) | rem) ^ 0x5412;

    for bit_idx in 0..15usize {
        let dark = ((format >> bit_idx) & 1) != 0;
        let (x1, y1) = match bit_idx {
            0..=5 => (8, bit_idx),
            6 => (8, 7),
            7 => (8, 8),
            8 => (7, 8),
            9..=14 => (14 - bit_idx, 8),
            _ => unreachable!(),
        };
        modules[y1 * size + x1] = dark;

        let (x2, y2) = match bit_idx {
            0..=7 => (size - 1 - bit_idx, 8),
            8..=14 => (8, size - 15 + bit_idx),
            _ => unreachable!(),
        };
        modules[y2 * size + x2] = dark;
    }
}

fn penalty_score(modules: &[bool], size: usize) -> u32 {
    let mut score = 0u32;

    // N1: 5+ consecutive same-color modules in a row or column.
    for row_major in [true, false] {
        for a in 0..size {
            let mut run_color = false;
            let mut run_len = 0u32;
            for b in 0..size {
                let c = if row_major {
                    modules[a * size + b]
                } else {
                    modules[b * size + a]
                };
                if b == 0 || c != run_color {
                    if run_len >= 5 {
                        score += 3 + (run_len - 5);
                    }
                    run_color = c;
                    run_len = 1;
                } else {
                    run_len += 1;
                }
            }
            if run_len >= 5 {
                score += 3 + (run_len - 5);
            }
        }
    }

    // N2: 2x2 same-color blocks.
    for y in 0..(size - 1) {
        for x in 0..(size - 1) {
            let c = modules[y * size + x];
            if modules[y * size + x + 1] == c
                && modules[(y + 1) * size + x] == c
                && modules[(y + 1) * size + x + 1] == c
            {
                score += 3;
            }
        }
    }

    // N3: 1:1:3:1:1 finder-like patterns preceded or followed by 4 light modules.
    for row_major in [true, false] {
        for a in 0..size {
            let mut w = 0u16;
            for b in 0..size {
                let bit = if row_major {
                    modules[a * size + b]
                } else {
                    modules[b * size + a]
                };
                w = ((w << 1) & 0x7FF) | u16::from(bit);
                if b >= 10 && (w == 0b10111010000 || w == 0b00001011101) {
                    score += 40;
                }
            }
        }
    }

    // N4: dark module ratio deviation from 50%.
    let dark = modules.iter().filter(|&&m| m).count() as i64;
    let total = (size * size) as i64;
    let pct = (dark * 100) / total;
    let prev5 = (pct / 5) * 5;
    let next5 = prev5 + 5;
    let d1 = (prev5 - 50).abs() / 5;
    let d2 = (next5 - 50).abs() / 5;
    score += (d1.min(d2) as u32) * 10;

    score
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_finder_at(qr: &QrCode, x0: usize, y0: usize) {
        for dy in 0..7 {
            for dx in 0..7 {
                let want = dx == 0
                    || dx == 6
                    || dy == 0
                    || dy == 6
                    || ((2..=4).contains(&dx) && (2..=4).contains(&dy));
                assert_eq!(qr.get(x0 + dx, y0 + dy), want, "finder mismatch at ({}, {})", x0 + dx, y0 + dy);
            }
        }
    }

    #[test]
    fn encodes_short_v1_and_pairing_v8_to_v12_with_valid_finders_and_unicode() {
        let short = QrCode::encode(b"tachyon://pair").expect("V1 encode");
        assert_eq!(short.version(), 1);
        assert_eq!(short.size(), 21);
        assert_finder_at(&short, 0, 0);
        assert_finder_at(&short, short.size() - 7, 0);
        assert_finder_at(&short, 0, short.size() - 7);
        assert!(short.get(8, short.size() - 8), "dark module required");

        // ~175-byte pairing URI fits cleanly in Version 8.
        let tok = "a".repeat(64);
        let uri_175 = format!(
            "tachyon://pair?v=1&host=100.109.107.16&port=7501&token={tok}&alias=tsukuyomi-mac&os=macos&arch=aarch64&proto=2&ssh=tsukuyomi"
        );
        let v8 = QrCode::encode(uri_175.as_bytes()).expect("V8 encode");
        assert!(v8.version() >= 6 && v8.version() <= 9, "version={}", v8.version());
        assert_finder_at(&v8, 0, 0);
        assert_finder_at(&v8, v8.size() - 7, 0);
        assert_finder_at(&v8, 0, v8.size() - 7);
        // Timing row 6 alternates dark/light starting dark at x=8.
        for i in 8..(v8.size() - 8) {
            assert_eq!(v8.get(i, 6), i % 2 == 0, "h timing x={i}");
            assert_eq!(v8.get(6, i), i % 2 == 0, "v timing y={i}");
        }

        // >240-byte URI exercises V10+ 16-bit byte count, 2-group interleaving, and version info.
        let long_uri = format!(
            "tachyon://pair?v=1&host=my-long-workstation.tailnet.ts.net&port=7501&token={tok}&alias=amaterasu-workstation&os=windows&arch=x86_64&proto=2&ssh=razeen@amaterasu.tailnet.ts.net&color=amber&extra=0123456789abcdef0123456789"
        );
        assert!(long_uri.len() > 240);
        let v10 = QrCode::encode(long_uri.as_bytes()).expect("V10+ encode");
        assert!(v10.version() >= 10 && v10.version() <= 12, "version={}", v10.version());
        assert_finder_at(&v10, v10.size() - 7, 0);

        let art = v8.render_unicode(true);
        let lines: Vec<&str> = art.lines().collect();
        let expected_rows = (v8.size() + 4).div_ceil(2);
        assert_eq!(lines.len(), expected_rows);
        assert!(lines[0].chars().all(|c| c == '█' || c == '▀'));

        assert!(QrCode::encode(&vec![b'x'; 400]).is_err());
    }
}
