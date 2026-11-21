//! Fast structural split of CSV lines.
//!
//! For every line start we compute bitmasks of `,` `"` and `\n` over a 128-byte window
//! (AVX2 chosen at runtime on x86_64 with an SSE2 baseline, NEON on aarch64, scalar
//! elsewhere; AVX-512 is not used because it cannot be verified on our test machines). A line that ends inside the window and has the common shape (exactly N-1
//! commas, every quote wrapping a whole field) is split straight from the masks. Anything
//! else (no newline in the window, wrong comma count, any other quote placement, the last
//! 127 bytes of a chunk) goes through the exact scalar path `slow`, so the result is always
//! identical to `slow` (see the `fuzz_split` test).

pub type B = &'static [u8];

const W: usize = 128;

#[derive(Clone, Copy)]
struct Masks {
    comma: u128,
    quote: u128,
    nl: u128,
}

/// Split one line with the common shape from its masks. `None` = use the slow path.
#[inline(always)]
fn split_window<const N: usize>(w: B, m: &Masks, len: usize) -> Option<[B; N]> {
    let lm = (1u128 << len) - 1; // len <= 127
    let mut c = m.comma & lm;
    if c.count_ones() as usize != N - 1 {
        return None;
    }
    let q = m.quote & lm;
    let mut out: [B; N] = [&[]; N];
    let mut s = 0usize;
    for (i, o) in out.iter_mut().enumerate() {
        let e = if i + 1 < N {
            let e = c.trailing_zeros() as usize;
            c &= c - 1;
            e
        } else {
            len
        };
        if q == 0 {
            *o = &w[s..e];
        } else {
            let fq = q & (((1u128 << e) - 1) & !((1u128 << s) - 1));
            if fq == 0 {
                *o = &w[s..e];
            } else if e - s >= 2 && fq == ((1u128 << s) | (1u128 << (e - 1))) {
                *o = &w[s + 1..e - 1];
            } else {
                return None;
            }
        }
        s = e + 1;
    }
    Some(out)
}

/// Shared line loop, stamped out per instruction set so the mask code inlines into it.
macro_rules! line_loop {
    ($chunk:ident, $f:ident, $slow:ident, $masks:path) => {{
        let n = $chunk.len();
        let mut pos = 0usize;
        while pos + W <= n {
            let w: B = &$chunk[pos..];
            // SAFETY: pos + W <= n, so W bytes are readable.
            let m = unsafe { $masks(w.as_ptr()) };
            if m.nl != 0 {
                let len = m.nl.trailing_zeros() as usize;
                let line = &w[..len];
                match split_window::<N>(w, &m, len) {
                    Some(fl) => $f(line, Some(fl)),
                    None => $f(line, $slow(line)),
                }
                pos += len + 1;
            } else {
                let len = memchr::memchr(b'\n', w).unwrap_or(w.len());
                let line = &w[..len];
                $f(line, $slow(line));
                pos += len + 1;
            }
        }
        // tail (< W bytes): exact scalar path
        while pos < n {
            let w: B = &$chunk[pos..];
            let len = memchr::memchr(b'\n', w).unwrap_or(w.len());
            let line = &w[..len];
            $f(line, $slow(line));
            pos += len + 1;
        }
    }};
}

#[inline(always)]
fn masks_scalar(p: *const u8) -> Masks {
    let mut m = Masks { comma: 0, quote: 0, nl: 0 };
    for i in 0..W {
        let b = unsafe { *p.add(i) };
        m.comma |= ((b == b',') as u128) << i;
        m.quote |= ((b == b'"') as u128) << i;
        m.nl |= ((b == b'\n') as u128) << i;
    }
    m
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use super::*;
    use std::arch::x86_64::*;

    #[inline]
    #[target_feature(enable = "sse2")]
    pub unsafe fn masks_sse2(p: *const u8) -> Masks {
        let (c, q, l) = (_mm_set1_epi8(b',' as i8), _mm_set1_epi8(b'"' as i8), _mm_set1_epi8(b'\n' as i8));
        let mut m = Masks { comma: 0, quote: 0, nl: 0 };
        for k in 0..8 {
            let v = _mm_loadu_si128(p.add(16 * k) as *const __m128i);
            m.comma |= ((_mm_movemask_epi8(_mm_cmpeq_epi8(v, c)) as u16) as u128) << (16 * k);
            m.quote |= ((_mm_movemask_epi8(_mm_cmpeq_epi8(v, q)) as u16) as u128) << (16 * k);
            m.nl |= ((_mm_movemask_epi8(_mm_cmpeq_epi8(v, l)) as u16) as u128) << (16 * k);
        }
        m
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    pub unsafe fn masks_avx2(p: *const u8) -> Masks {
        let (c, q, l) = (_mm256_set1_epi8(b',' as i8), _mm256_set1_epi8(b'"' as i8), _mm256_set1_epi8(b'\n' as i8));
        let mut m = Masks { comma: 0, quote: 0, nl: 0 };
        for k in 0..4 {
            let v = _mm256_loadu_si256(p.add(32 * k) as *const __m256i);
            m.comma |= ((_mm256_movemask_epi8(_mm256_cmpeq_epi8(v, c)) as u32) as u128) << (32 * k);
            m.quote |= ((_mm256_movemask_epi8(_mm256_cmpeq_epi8(v, q)) as u32) as u128) << (32 * k);
            m.nl |= ((_mm256_movemask_epi8(_mm256_cmpeq_epi8(v, l)) as u32) as u128) << (32 * k);
        }
        m
    }

    #[target_feature(enable = "avx2")]
    pub unsafe fn lines_avx2<const N: usize>(chunk: B, mut f: impl FnMut(B, Option<[B; N]>), slow: fn(B) -> Option<[B; N]>) {
        line_loop!(chunk, f, slow, masks_avx2)
    }
    #[target_feature(enable = "sse2")]
    pub unsafe fn lines_sse2<const N: usize>(chunk: B, mut f: impl FnMut(B, Option<[B; N]>), slow: fn(B) -> Option<[B; N]>) {
        line_loop!(chunk, f, slow, masks_sse2)
    }
}

#[cfg(target_arch = "aarch64")]
mod arm {
    use super::*;
    use std::arch::aarch64::*;

    /// 16 compare results (0xFF/0x00 bytes) -> 16-bit mask.
    #[inline(always)]
    unsafe fn mask16(v: uint8x16_t) -> u16 {
        const BITS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
        let b = vandq_u8(v, vld1q_u8(BITS.as_ptr()));
        let lo = vaddv_u8(vget_low_u8(b)) as u16;
        let hi = vaddv_u8(vget_high_u8(b)) as u16;
        lo | (hi << 8)
    }

    #[inline(always)]
    pub unsafe fn masks_neon(p: *const u8) -> Masks {
        let (c, q, l) = (vdupq_n_u8(b','), vdupq_n_u8(b'"'), vdupq_n_u8(b'\n'));
        let mut m = Masks { comma: 0, quote: 0, nl: 0 };
        for k in 0..8 {
            let v = vld1q_u8(p.add(16 * k));
            m.comma |= (mask16(vceqq_u8(v, c)) as u128) << (16 * k);
            m.quote |= (mask16(vceqq_u8(v, q)) as u128) << (16 * k);
            m.nl |= (mask16(vceqq_u8(v, l)) as u128) << (16 * k);
        }
        m
    }

    pub fn lines_neon<const N: usize>(chunk: B, mut f: impl FnMut(B, Option<[B; N]>), slow: fn(B) -> Option<[B; N]>) {
        line_loop!(chunk, f, slow, masks_neon)
    }
}

#[allow(dead_code)]
pub fn lines_scalar<const N: usize>(chunk: B, mut f: impl FnMut(B, Option<[B; N]>), slow: fn(B) -> Option<[B; N]>) {
    #[inline(always)]
    unsafe fn m(p: *const u8) -> Masks {
        masks_scalar(p)
    }
    line_loop!(chunk, f, slow, m)
}

/// Calls `f(line, fields)` for every `\n`-separated line of `chunk` (same lines as a plain
/// memchr split; a final line without `\n` included, no empty line after a trailing `\n`).
/// `fields` always equals `slow(line)`.
#[inline]
pub fn for_lines_split<const N: usize>(chunk: B, f: impl FnMut(B, Option<[B; N]>), slow: fn(B) -> Option<[B; N]>) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            return unsafe { x86::lines_avx2(chunk, f, slow) };
        }
        return unsafe { x86::lines_sse2(chunk, f, slow) };
    }
    #[cfg(target_arch = "aarch64")]
    {
        return arm::lines_neon(chunk, f, slow);
    }
    #[allow(unreachable_code)]
    lines_scalar(chunk, f, slow)
}

/// All implementations available on this CPU, for the equivalence fuzz test.
#[cfg(test)]
pub fn all_impls<const N: usize>() -> Vec<(&'static str, fn(B, &mut dyn FnMut(B, Option<[B; N]>), fn(B) -> Option<[B; N]>))> {
    let mut v: Vec<(&'static str, fn(B, &mut dyn FnMut(B, Option<[B; N]>), fn(B) -> Option<[B; N]>))> = vec![];
    v.push(("scalar", |c, f, s| lines_scalar(c, f, s)));
    #[cfg(target_arch = "x86_64")]
    {
        v.push(("sse2", |c, f, s| unsafe { x86::lines_sse2(c, f, s) }));
        if is_x86_feature_detected!("avx2") {
            v.push(("avx2", |c, f, s| unsafe { x86::lines_avx2(c, f, s) }));
        }
    }
    #[cfg(target_arch = "aarch64")]
    v.push(("neon", |c, f, s| arm::lines_neon(c, f, s)));
    v
}
