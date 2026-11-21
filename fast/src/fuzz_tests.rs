//! Equivalence fuzz for the fast line splitter (`scan.rs`) against the exact scalar path.
//! Not part of the shipped binary: run with `cargo test --release` (FUZZ_LINES=N to scale).
use super::*;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, pct: usize) -> bool {
        self.below(100) < pct
    }
    fn pick<'a>(&mut self, xs: &'a [&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

const ALNUM: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789ABCXYZ";
const NOISE: &[u8] = b",\"\n\r :-+TZtz0123456789aeu.\t\x00\xff";

fn id(r: &mut Rng, p: char) -> String {
    let mut s = String::new();
    if !r.chance(3) {
        s.push(p);
    }
    let n = match r.below(20) {
        0 => 0,
        1 => 40 + r.below(120), // long: crosses the 128-byte window
        _ => 4 + r.below(10),
    };
    for _ in 0..n {
        s.push(ALNUM[r.below(ALNUM.len())] as char);
    }
    if r.chance(2) {
        s.insert(0, ' ');
    }
    s
}

fn ts(r: &mut Rng) -> String {
    let y = 2020 + r.below(10);
    let (mo, d, h, mi, se) = (1 + r.below(13), 1 + r.below(31), r.below(25), r.below(61), r.below(61));
    match r.below(10) {
        0..=4 => format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{se:02}Z"),
        5 | 6 => format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{se:02}{}{:02}:{:02}", if r.chance(50) { '+' } else { '-' }, r.below(25), r.below(61)),
        7 | 8 => format!("{:0w$}", 1_700_000_000_000u64 + r.next() % 400_000_000_000, w = 13 + r.below(4)),
        _ => r.pick(&["", "2026-10-03 23:58:10Z", "2026-10-03T23:58:10.5Z", "1.79e12", "-5", "2026-10-03t23:58:10z", "+0200"]).to_string(),
    }
}

fn field_values(r: &mut Rng, arity: usize) -> Vec<String> {
    match arity {
        7 => vec![
            id(r, 'e'),
            id(r, 'u'),
            ts(r),
            ts(r),
            r.pick(&["view", "cart", "purchase", "purchase", "Purchase", "cart ", ""]).to_string(),
            id(r, 'p'),
            r.pick(&["", "115", "0250", "0", "-5", "1.5", "10000001", "10000000", "99999999999999999999999"]).to_string(),
        ],
        4 => vec![id(r, 'k'), id(r, 'u'), format!("c_{}", r.below(3000)), ts(r)],
        _ => (0..arity).map(|_| id(r, 'u')).collect(),
    }
}

fn line(r: &mut Rng, arity: usize) -> Vec<u8> {
    if r.chance(1) {
        return vec![];
    }
    let mut f = field_values(r, arity);
    if r.chance(3) {
        if r.chance(50) {
            f.pop();
        } else {
            f.push("x".into());
        }
    }
    let mut s: Vec<u8> = vec![];
    for (i, v) in f.iter().enumerate() {
        if i > 0 {
            s.push(b',');
        }
        let q = r.chance(25);
        if q {
            s.push(b'"');
        }
        s.extend_from_slice(v.as_bytes());
        if q && !r.chance(2) {
            s.push(b'"');
        }
    }
    // byte mutations
    let nm = match r.below(10) {
        0..=5 => 0,
        6..=8 => 1,
        _ => 1 + r.below(4),
    };
    for _ in 0..nm {
        let b = NOISE[r.below(NOISE.len())];
        if b == b'\n' && !r.chance(20) {
            continue;
        }
        let at = r.below(s.len() + 1);
        match r.below(3) {
            0 => s.insert(at, b),
            1 if at < s.len() => s[at] = b,
            _ if at < s.len() => {
                s.remove(at);
            }
            _ => {}
        }
    }
    s
}

fn reference<const N: usize>(chunk: B) -> Vec<(B, Option<[B; N]>)> {
    let mut v = vec![];
    for_lines(chunk, |l| v.push((l, fields::<N>(l))));
    v
}

fn check<const N: usize>(seed: u64, total_lines: usize) {
    let mut r = Rng(seed);
    let impls = scan::all_impls::<N>();
    let mut done = 0usize;
    let mut fast_hits = 0usize;
    while done < total_lines {
        let k = 1 + r.below(400);
        let mut buf: Vec<u8> = vec![];
        for i in 0..k {
            buf.extend_from_slice(&line(&mut r, N));
            if i + 1 < k || r.chance(50) {
                buf.push(b'\n');
            }
        }
        done += k;
        let chunk: B = Box::leak(buf.into_boxed_slice());
        let want = reference::<N>(chunk);
        fast_hits += want.iter().filter(|(_, f)| f.is_some()).count();
        for (name, run) in &impls {
            let mut got: Vec<(B, Option<[B; N]>)> = vec![];
            run(chunk, &mut |l, f| got.push((l, f)), fields::<N>);
            assert_eq!(got.len(), want.len(), "{name}: line count differs");
            for (g, w) in got.iter().zip(want.iter()) {
                assert_eq!(g.0, w.0, "{name}: line bytes differ");
                assert_eq!(g.1, w.1, "{name}: fields differ for line {:?}", String::from_utf8_lossy(w.0));
                // the fields must be sub-slices of the line at the same positions
                if let (Some(a), Some(b)) = (&g.1, &w.1) {
                    for (x, y) in a.iter().zip(b.iter()) {
                        assert_eq!(x.as_ptr(), y.as_ptr(), "{name}: field position differs");
                    }
                }
            }
        }
        // the dispatched entry point too
        let mut got: Vec<(B, Option<[B; N]>)> = vec![];
        scan::for_lines_split::<N>(chunk, |l, f| got.push((l, f)), fields::<N>);
        assert!(got == want, "dispatch differs");
    }
    eprintln!("arity {N}: {done} lines, {fast_hits} well-formed, impls {:?}", impls.iter().map(|x| x.0).collect::<Vec<_>>());
}

fn n_lines() -> usize {
    std::env::var("FUZZ_LINES").ok().and_then(|s| s.parse().ok()).unwrap_or(2_000_000)
}

#[test]
fn fuzz_split_events() {
    check::<7>(0x9E37_79B9_7F4A_7C15, n_lines());
}

#[test]
fn fuzz_split_clicks() {
    check::<4>(0xD1B5_4A32_D192_ED03, n_lines() / 2);
}

#[test]
fn fuzz_split_links() {
    check::<2>(0x8CB9_2BA7_2F3D_8DD7, n_lines() / 4);
}

/// Full event parse through the fast splitter equals the exact path on mutated lines.
#[test]
fn fuzz_event_records() {
    let mut r = Rng(42);
    let ctx = test_ctx();
    let mut n = 0usize;
    let mut ok = 0usize;
    while n < n_lines() / 2 {
        let mut buf: Vec<u8> = vec![];
        for _ in 0..200 {
            buf.extend_from_slice(&line(&mut r, 7));
            buf.push(b'\n');
        }
        n += 200;
        let chunk: B = Box::leak(buf.into_boxed_slice());
        let mut want = vec![];
        for_lines(chunk, |l| want.push(parse_event_line(l, &ctx)));
        let mut got = vec![];
        scan::for_lines_split::<7>(chunk, |_, f| got.push(f.and_then(|f| parse_event_fields(f, &ctx))), fields::<7>);
        ok += want.iter().filter(|p| p.map_or(false, |p| p.ts != i64::MIN)).count();
        assert!(got == want, "event records differ");
    }
    eprintln!("event records: {n} lines, {ok} valid");
}

fn test_ctx() -> PCtx {
    let mut cat_of: IdMap<u8> = IdMap::default();
    for i in 0..5000 {
        cat_of.insert(hash128(format!("p{i}").as_bytes()), (i % 7) as u8);
    }
    let ws = parse_ts(b"2024-01-01T00:00:00Z").unwrap();
    let we = parse_ts(b"2028-01-01T00:00:00Z").unwrap();
    PCtx { ws, we, cat_of, unknown_cat: 7, camp_of: IdMap::default() }
}
