//! Stream processing: a vectorised fast path in front of a strict VT parser.
//!
//! # The idea
//!
//! Essentially all terminal traffic is printable ASCII. A conventional parser
//! still pays a state-machine transition and an indirect `print(char)` call for
//! every one of those bytes, and that per-byte overhead -- not escape handling,
//! not rendering -- is what dominates `cat`-a-big-file throughput.
//!
//! So we split the stream. While we know the state machine is in Ground, we
//! scan ahead with AVX2 for the first byte outside `0x20..=0x7E`, 32 bytes per
//! instruction, and hand the whole run to the grid as a bulk fill. Only the
//! remainder -- escapes, UTF-8, control bytes that change state -- reaches
//! [`vte`], which we keep for its correctness.
//!
//! # Staying honest about the state
//!
//! The fast path is only sound when the machine really is in Ground. We track
//! that ourselves:
//!
//!  * We start in Ground.
//!  * C0 bytes below `0x20` other than `ESC` execute and leave the state
//!    unchanged, so we handle them inline and stay in the fast path.
//!  * `ESC` and anything `>= 0x7F` may begin a multi-byte construct, so we drop
//!    out and feed `vte` a byte at a time.
//!  * `vte` tells us we are back in Ground by dispatching a *terminating*
//!    action: `print`, `csi_dispatch`, `esc_dispatch`, `osc_dispatch`, or
//!    `unhook`. Notably `execute` is not one of those -- a C0 byte inside a CSI
//!    is executed without leaving the sequence -- and neither are `hook`/`put`,
//!    which sit inside a DCS.
//!
//! Byte-at-a-time is slower, but it only ever applies to escape sequences,
//! which are a rounding error in the byte budget and are short besides. A
//! partial UTF-8 sequence or a truncated CSI at the end of a read simply leaves
//! us out of Ground, and the next chunk resumes correctly.

use vte::Parser;

use super::Term;

/// Drives a [`Term`] from a raw byte stream.
pub struct Processor {
    parser: Parser,
    /// True when the VT state machine is known to be in Ground.
    in_ground: bool,
}

impl Default for Processor {
    fn default() -> Self {
        Self::new()
    }
}

impl Processor {
    pub fn new() -> Processor {
        Processor {
            parser: Parser::new(),
            in_ground: true,
        }
    }

    /// Feed a chunk of PTY output.
    pub fn advance(&mut self, term: &mut Term, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if self.in_ground {
                // Bulk-consume printable ASCII.
                let n = scan_printable_ascii(bytes);
                if n > 0 {
                    term.write_ascii_run(&bytes[..n]);
                    bytes = &bytes[n..];
                    continue;
                }

                // A C0 control that does not change parser state.
                let b = bytes[0];
                if b < 0x20 && b != 0x1B {
                    term.execute_c0(b);
                    bytes = &bytes[1..];
                    continue;
                }

                // ESC, DEL, or a non-ASCII lead byte: hand over.
                self.in_ground = false;
            }

            self.parser.advance(term, &bytes[..1]);
            bytes = &bytes[1..];
            if term.take_ground_signal() {
                self.in_ground = true;
            }
        }
    }
}

// ===========================================================================
// Vectorised scanner
// ===========================================================================

/// Number of leading bytes in `0x20..=0x7E`.
///
/// Dispatch is resolved once and cached; the branch predictor makes the
/// indirect call free in the steady state.
#[inline]
pub fn scan_printable_ascii(b: &[u8]) -> usize {
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    {
        use std::sync::atomic::{AtomicU8, Ordering};
        static AVX2: AtomicU8 = AtomicU8::new(u8::MAX);

        let mut has = AVX2.load(Ordering::Relaxed);
        if has == u8::MAX {
            has = u8::from(std::is_x86_feature_detected!("avx2"));
            AVX2.store(has, Ordering::Relaxed);
        }
        if has == 1 {
            // SAFETY: guarded by the runtime feature check above.
            return unsafe { scan_avx2(b) };
        }
    }
    scan_swar(b)
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx2")]
unsafe fn scan_avx2(b: &[u8]) -> usize {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::*;

    let n = b.len();
    let mut i = 0usize;

    // Shift the accepted range down to zero so a single unsigned compare
    // decides membership: `b - 0x20 <= 0x5E`.
    let bias = _mm256_set1_epi8(0x20u8 as i8);
    let limit = _mm256_set1_epi8(0x5Eu8 as i8);

    while i + 32 <= n {
        let v = _mm256_loadu_si256(b.as_ptr().add(i) as *const __m256i);
        let y = _mm256_sub_epi8(v, bias);
        // AVX2 has no unsigned byte compare, but `min_epu8(y, L) == y` is
        // exactly `y <= L`.
        let ok = _mm256_cmpeq_epi8(_mm256_min_epu8(y, limit), y);
        let mask = _mm256_movemask_epi8(ok) as u32;
        if mask != u32::MAX {
            return i + (!mask).trailing_zeros() as usize;
        }
        i += 32;
    }

    i + scan_swar(b.get_unchecked(i..))
}

/// Portable 8-bytes-at-a-time fallback using SWAR.
fn scan_swar(b: &[u8]) -> usize {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;

    let mut i = 0usize;
    while i + 8 <= b.len() {
        let w = u64::from_le_bytes(b[i..i + 8].try_into().unwrap());

        // Any byte with the high bit set (UTF-8 lead/continuation, or C1).
        let high = w & HIGH;
        // Any byte below 0x20. Computed on the low 7 bits so an inter-byte
        // borrow can only ever produce a false positive *after* a real hit,
        // which is harmless because we report the first one.
        let lo7 = w & !HIGH;
        let lt20 = lo7.wrapping_sub(ONES.wrapping_mul(0x20)) & !lo7 & HIGH;
        // Any byte equal to 0x7F (DEL), the one remaining excluded value.
        let x = w ^ ONES.wrapping_mul(0x7F);
        let eq7f = x.wrapping_sub(ONES) & !x & HIGH;

        let bad = high | lt20 | eq7f;
        if bad != 0 {
            return i + (bad.trailing_zeros() as usize) / 8;
        }
        i += 8;
    }

    while i < b.len() && b[i] >= 0x20 && b[i] < 0x7F {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference implementation the vectorised versions must agree with.
    fn scan_ref(b: &[u8]) -> usize {
        b.iter().position(|&c| !(0x20..0x7F).contains(&c)).unwrap_or(b.len())
    }

    #[test]
    fn scanner_matches_reference_on_edge_bytes() {
        for bad in [0x00u8, 0x07, 0x08, 0x09, 0x0A, 0x0D, 0x1B, 0x1F, 0x7F, 0x80, 0xC3, 0xFF] {
            for prefix in [0usize, 1, 3, 7, 8, 15, 16, 31, 32, 33, 63, 64, 100] {
                let mut v = vec![b'x'; prefix];
                v.push(bad);
                v.extend_from_slice(b"trailing");
                assert_eq!(scan_printable_ascii(&v), prefix, "bad={bad:#04x} prefix={prefix}");
                assert_eq!(scan_swar(&v), prefix, "swar bad={bad:#04x} prefix={prefix}");
                assert_eq!(scan_ref(&v), prefix);
            }
        }
    }

    #[test]
    fn scanner_accepts_full_printable_range() {
        let all: Vec<u8> = (0x20u8..=0x7E).collect();
        assert_eq!(scan_printable_ascii(&all), all.len());
        assert_eq!(scan_swar(&all), all.len());
    }

    #[test]
    fn scanner_handles_empty_and_short() {
        assert_eq!(scan_printable_ascii(&[]), 0);
        assert_eq!(scan_printable_ascii(b"a"), 1);
        assert_eq!(scan_printable_ascii(b"\n"), 0);
    }

    #[test]
    fn scanner_fuzz_against_reference() {
        // Cheap deterministic LCG; no dev-dependency needed.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2000 {
            let len = (next() % 200) as usize;
            let buf: Vec<u8> = (0..len)
                .map(|_| {
                    let r = next();
                    // Bias heavily towards printable so runs are long.
                    if r % 8 == 0 {
                        (r >> 8) as u8
                    } else {
                        0x20 + ((r >> 8) % 0x5F) as u8
                    }
                })
                .collect();
            let want = scan_ref(&buf);
            assert_eq!(scan_printable_ascii(&buf), want, "buf={buf:?}");
            assert_eq!(scan_swar(&buf), want, "swar buf={buf:?}");
        }
    }
}
