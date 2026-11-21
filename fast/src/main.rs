// Shopping Sessions: native pipeline. See rules.md for the specification.
use memchr::{memchr, memchr_iter};
use memmap2::Mmap;
use parquet::basic::Compression;
use parquet::data_type::{ByteArray, ByteArrayType, Int32Type, Int64Type};
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::parser::parse_message_type;
use rayon::prelude::*;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

type B = &'static [u8];
type Key = (u64, u64);

const DAY_MS: i64 = 86_400_000;
const WEEK_MS: i64 = 604_800_000;
const GAP_MS: i64 = 1_800_000;
const LATE_MS: i64 = 86_400_000;
const NB1: usize = 256; // event-id buckets (dedup)
const NB2: usize = 1024; // person buckets
const CHUNK: usize = 8 << 20;

// ---------- hashing ----------
#[derive(Default)]
struct IdH(u64);
impl Hasher for IdH {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, b: &[u8]) {
        for &x in b {
            self.0 = (self.0 ^ x as u64).wrapping_mul(0x9E3779B97F4A7C15);
        }
    }
    fn write_u64(&mut self, x: u64) {
        self.0 = (self.0 ^ x).wrapping_mul(0x9E3779B97F4A7C15).rotate_left(29);
    }
}
type IdMap<V> = HashMap<Key, V, BuildHasherDefault<IdH>>;

#[inline(always)]
fn mix(a: u64, b: u64) -> u64 {
    let r = (a as u128).wrapping_mul(b as u128);
    (r as u64) ^ ((r >> 64) as u64)
}

#[inline]
fn hash128(s: &[u8]) -> Key {
    const P1: u64 = 0xa076_1d64_78bd_642f;
    const P2: u64 = 0xe703_7ed1_a0b4_28db;
    const P3: u64 = 0x8ebc_6af0_9c88_c6e3;
    const P4: u64 = 0x5899_65cc_7537_4cc3;
    let len = s.len() as u64;
    let mut a = P1 ^ len;
    let mut b = P3.wrapping_add(len.wrapping_mul(P4));
    let mut i = 0;
    while i + 8 <= s.len() {
        let w = u64::from_le_bytes(s[i..i + 8].try_into().unwrap());
        a = mix(a ^ w, P2);
        b = mix(b ^ w.rotate_left(32) ^ P1, P4);
        i += 8;
    }
    if i < s.len() {
        let mut w = 0u64;
        for (k, &x) in s[i..].iter().enumerate() {
            w |= (x as u64) << (8 * k);
        }
        a = mix(a ^ w ^ P3, P2);
        b = mix(b ^ w.rotate_left(17) ^ P2, P4);
    }
    let a2 = mix(a ^ len, P2 ^ b);
    let b2 = mix(b ^ P1, P3 ^ a);
    (a2, b2)
}

// ---------- parsing helpers ----------
#[inline]
fn unq(f: B) -> Option<B> {
    if let Some(&b'"') = f.first() {
        if f.len() < 2 || f[f.len() - 1] != b'"' {
            return None;
        }
        let inner = &f[1..f.len() - 1];
        if memchr(b'"', inner).is_some() {
            return None;
        }
        Some(inner)
    } else if memchr(b'"', f).is_some() {
        None
    } else {
        Some(f)
    }
}

/// Split on commas, require exactly N fields, unquote all (None if any quote misuse).
#[inline]
fn fields<const N: usize>(line: B) -> Option<[B; N]> {
    let mut out: [B; N] = [&[]; N];
    let mut n = 0;
    let mut start = 0;
    for p in memchr_iter(b',', line) {
        if n == N - 1 {
            return None;
        }
        out[n] = unq(&line[start..p])?;
        n += 1;
        start = p + 1;
    }
    if n != N - 1 {
        return None;
    }
    out[N - 1] = unq(&line[start..])?;
    Some(out)
}

#[inline]
fn d2(b: &[u8]) -> Option<i64> {
    let (x, y) = (b[0].wrapping_sub(b'0'), b[1].wrapping_sub(b'0'));
    if x > 9 || y > 9 {
        None
    } else {
        Some((x * 10 + y) as i64)
    }
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn dim(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

/// Parse the three timestamp forms to epoch ms. None = bad.
fn parse_ts(f: &[u8]) -> Option<i64> {
    let n = f.len();
    if n == 0 {
        return None;
    }
    if f.iter().all(|c| c.is_ascii_digit()) {
        let mut v: i64 = 0;
        for &c in f {
            v = v.checked_mul(10)?.checked_add((c - b'0') as i64)?;
        }
        return Some(v);
    }
    let base = if n == 20 && f[19] == b'Z' {
        true
    } else {
        n == 25 && (f[19] == b'+' || f[19] == b'-') && f[22] == b':'
    };
    if !base || f[4] != b'-' || f[7] != b'-' || f[10] != b'T' || f[13] != b':' || f[16] != b':' {
        return None;
    }
    let y = d2(&f[0..2])? * 100 + d2(&f[2..4])?;
    let mo = d2(&f[5..7])?;
    let d = d2(&f[8..10])?;
    let h = d2(&f[11..13])?;
    let mi = d2(&f[14..16])?;
    let s = d2(&f[17..19])?;
    if !(1..=12).contains(&mo) || d < 1 || d > dim(y, mo) || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    let mut ms = (days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + s) * 1000;
    if n == 25 {
        let oh = d2(&f[20..22])?;
        let om = d2(&f[23..25])?;
        if oh > 23 || om > 59 {
            return None;
        }
        let off = (oh * 60 + om) * 60_000;
        if f[19] == b'+' {
            ms -= off;
        } else {
            ms += off;
        }
    }
    Some(ms)
}

// ---------- files ----------
fn list_csv(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map_or(false, |x| x == "csv") && p.is_file())
            .collect(),
        Err(_) => vec![],
    };
    v.sort();
    v
}

fn map_file(p: &Path) -> Option<B> {
    let f = std::fs::File::open(p).ok()?;
    if f.metadata().ok()?.len() == 0 {
        return None;
    }
    let m = unsafe { Mmap::map(&f).ok()? };
    #[cfg(unix)]
    {
        let _ = m.advise(memmap2::Advice::Sequential);
    }
    let m: &'static Mmap = Box::leak(Box::new(m));
    Some(&m[..])
}

/// Header skipped; whole-line chunks.
fn chunks_of(files: &[PathBuf]) -> Vec<B> {
    let mut out = vec![];
    for p in files {
        let Some(data) = map_file(p) else { continue };
        let Some(h) = memchr(b'\n', data) else { continue };
        let data = &data[h + 1..];
        let mut pos = 0;
        while pos < data.len() {
            let mut end = (pos + CHUNK).min(data.len());
            if end < data.len() {
                end = match memchr(b'\n', &data[end..]) {
                    Some(i) => end + i + 1,
                    None => data.len(),
                };
            }
            out.push(&data[pos..end]);
            pos = end;
        }
    }
    out
}

#[inline]
fn for_lines(chunk: B, mut f: impl FnMut(B)) {
    let mut pos = 0;
    while pos < chunk.len() {
        let rest = &chunk[pos..];
        match memchr(b'\n', rest) {
            Some(i) => {
                f(&rest[..i]);
                pos += i + 1;
            }
            None => {
                f(rest);
                break;
            }
        }
    }
}

fn read_small(path: &Path) -> Vec<B> {
    let Some(data) = map_file(path) else { return vec![] };
    let mut v = vec![];
    let mut first = true;
    for_lines(data, |l| {
        if first {
            first = false;
        } else {
            v.push(l);
        }
    });
    v
}

// ---------- records ----------
#[derive(Clone, Copy)]
struct Pre {
    e: Key,
    ingest: i64,
    ts: i64, // i64::MIN = not ok
    key: Key,
    uptr: usize,
    ulen: u32,
    pc: u32, // price | cat << 24
}

#[derive(Clone, Copy)]
struct Ev {
    key: Key,
    ts: i64,
    uptr: usize,
    ulen: u32,
    pc: u32,
}

#[derive(Clone, Copy)]
struct Cl {
    key: Key,
    ts: i64,
    cptr: usize,
    clen: u32,
    camp: u32,
}

struct Ctx {
    ws: i64,
    we: i64,
    cat_of: IdMap<u8>,
    unknown_cat: u8,
    camp_of: IdMap<u32>,
    links: IdMap<Key>,
}

#[inline]
fn resolve(ctx: &Ctx, user: B) -> Key {
    let h = hash128(user);
    match ctx.links.get(&h) {
        Some(k) => *k,
        None => h,
    }
}

fn parse_manifest(s: &str) -> (i64, i64) {
    fn get(s: &str, key: &str) -> i64 {
        let i = s.find(key).expect("manifest key");
        let r = &s[i + key.len()..];
        let q1 = r.find('"').unwrap();
        let r = &r[q1 + 1..];
        let q2 = r.find('"').unwrap();
        parse_ts(r[..q2].as_bytes()).expect("manifest ts")
    }
    (get(s, "\"window_start\""), get(s, "\"window_end\""))
}

fn parse_event_line(line: B, ctx: &Ctx) -> Option<Pre> {
    let [eid, user, ts, ing, typ, prod, price] = fields::<7>(line)?;
    if eid.is_empty() {
        return None;
    }
    let e = hash128(eid);
    let ingest = parse_ts(ing).unwrap_or(i64::MAX);
    let mut p = Pre { e, ingest, ts: i64::MIN, key: (0, 0), uptr: 0, ulen: 0, pc: 0 };
    if user.is_empty() || prod.is_empty() {
        return Some(p);
    }
    let Some(t) = parse_ts(ts) else { return Some(p) };
    if t < ctx.ws || t >= ctx.we {
        return Some(p);
    }
    if ingest != i64::MAX && ingest - t > LATE_MS {
        return Some(p);
    }
    let mut pc = 0u32;
    match typ {
        b"view" | b"cart" => {}
        b"purchase" => {
            if price.is_empty() || !price.iter().all(|c| c.is_ascii_digit()) {
                return Some(p);
            }
            let mut v: u64 = 0;
            for &c in price {
                v = (v * 10 + (c - b'0') as u64).min(10_000_001);
            }
            if v < 1 || v > 10_000_000 {
                return Some(p);
            }
            let cat = *ctx.cat_of.get(&hash128(prod)).unwrap_or(&ctx.unknown_cat);
            pc = v as u32 | ((cat as u32) << 24);
        }
        _ => return Some(p),
    }
    p.ts = t;
    p.key = resolve(ctx, user);
    p.uptr = user.as_ptr() as usize;
    p.ulen = user.len() as u32;
    p.pc = pc;
    Some(p)
}

fn slice_of(ptr: usize, len: u32) -> B {
    unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) }
}

// ---------- worker accumulator ----------
struct UdRow {
    nptr: usize,
    nlen: u32,
    day: i32,
    sessions: u32,
    events: u32,
    revenue: u64,
    top: u8, // 255 = null
}

struct Worker {
    camp: Vec<[u64; 5]>, // sessions, purchasing, revenue, assisted
    ud: Vec<UdRow>,
    cnt: Vec<u32>,
    pos: Vec<u32>,
    active: Vec<u32>,
    cat_sum: Vec<u64>,
}

fn main() {
    let t0 = Instant::now();
    let tm = |label: &str| eprintln!("[{:7.2}s] {}", t0.elapsed().as_secs_f64(), label);
    let data_dir = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into()));
    let out_dir = PathBuf::from(std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into()));
    let ncpu: usize = std::env::var("NCPU").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    rayon::ThreadPoolBuilder::new().num_threads(ncpu).build_global().unwrap();

    let manifest = std::fs::read_to_string(data_dir.join("manifest.json")).expect("manifest");
    let (ws, we) = parse_manifest(&manifest);

    // products, campaigns
    let mut cats: Vec<B> = vec![];
    let mut prod_rows: Vec<(B, B)> = vec![];
    for l in read_small(&data_dir.join("products.csv")) {
        if let Some([p, c]) = fields::<2>(l) {
            prod_rows.push((p, c));
            cats.push(c);
        }
    }
    cats.sort();
    cats.dedup();
    let unknown_cat = cats.len() as u8;
    let mut cat_of: IdMap<u8> = IdMap::default();
    for (p, c) in &prod_rows {
        cat_of.insert(hash128(p), cats.binary_search(c).unwrap() as u8);
    }
    let mut camp_names: Vec<(B, B)> = vec![];
    let mut camp_of: IdMap<u32> = IdMap::default();
    for l in read_small(&data_dir.join("campaigns.csv")) {
        if let Some([c, ch]) = fields::<2>(l) {
            let h = hash128(c);
            if !camp_of.contains_key(&h) {
                camp_of.insert(h, camp_names.len() as u32);
                camp_names.push((c, ch));
            }
        }
    }
    let ncamp = camp_names.len();
    tm("small files");

    // ---- links ----
    struct LinkRow {
        a: Key,
        b: Key,
        sa: B,
        sb: B,
    }
    let link_chunks = chunks_of(&list_csv(&data_dir.join("links")));
    let link_rows: Vec<Vec<LinkRow>> = link_chunks
        .par_iter()
        .map(|c| {
            let mut v = vec![];
            for_lines(c, |l| {
                if let Some([a, b]) = fields::<2>(l) {
                    if !a.is_empty() && !b.is_empty() {
                        v.push(LinkRow { a: hash128(a), b: hash128(b), sa: a, sb: b });
                    }
                }
            });
            v
        })
        .collect();
    let mut ids: IdMap<u32> = IdMap::default();
    let mut hs: Vec<Key> = vec![];
    let mut strs: Vec<B> = vec![];
    let mut parent: Vec<u32> = vec![];
    fn find(p: &mut Vec<u32>, mut x: u32) -> u32 {
        while p[x as usize] != x {
            let g = p[p[x as usize] as usize];
            p[x as usize] = g;
            x = g;
        }
        x
    }
    let mut intern = |h: Key, s: B, ids: &mut IdMap<u32>, hs: &mut Vec<Key>, strs: &mut Vec<B>, parent: &mut Vec<u32>| -> u32 {
        *ids.entry(h).or_insert_with(|| {
            let i = hs.len() as u32;
            hs.push(h);
            strs.push(s);
            parent.push(i);
            i
        })
    };
    for v in &link_rows {
        for r in v {
            let a = intern(r.a, r.sa, &mut ids, &mut hs, &mut strs, &mut parent);
            let b = intern(r.b, r.sb, &mut ids, &mut hs, &mut strs, &mut parent);
            let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
            if ra != rb {
                parent[ra as usize] = rb;
            }
        }
    }
    drop(ids);
    let n = hs.len();
    let mut best: Vec<u32> = vec![u32::MAX; n];
    for i in 0..n {
        let r = find(&mut parent, i as u32) as usize;
        if best[r] == u32::MAX || strs[i] < strs[best[r] as usize] {
            best[r] = i as u32;
        }
    }
    let mut links: IdMap<Key> = IdMap::with_capacity_and_hasher(n, Default::default());
    let mut names: IdMap<(usize, u32)> = IdMap::default();
    for i in 0..n {
        let r = find(&mut parent, i as u32) as usize;
        let bi = best[r] as usize;
        let key = hs[bi];
        links.insert(hs[i], key);
        if i == bi {
            names.insert(key, (strs[bi].as_ptr() as usize, strs[bi].len() as u32));
        }
    }
    drop(link_rows);
    tm("links");

    let ctx = Ctx { ws, we, cat_of, unknown_cat, camp_of, links };

    // ---- events: parse ----
    let ev_chunks = chunks_of(&list_csv(&data_dir.join("events")));
    let accs: Vec<Vec<Vec<Pre>>> = ev_chunks
        .par_iter()
        .fold(
            || (0..NB1).map(|_| Vec::new()).collect::<Vec<Vec<Pre>>>(),
            |mut acc, c| {
                for_lines(c, |l| {
                    if let Some(p) = parse_event_line(l, &ctx) {
                        let b = ((p.e.0 >> 32) as usize * NB1) >> 32;
                        acc[b].push(p);
                    }
                });
                acc
            },
        )
        .collect();
    tm("events parsed");

    // ---- dedup + repartition by person ----
    let parts: Vec<Vec<Vec<Ev>>> = (0..NB1)
        .into_par_iter()
        .map(|b| {
            let total: usize = accs.iter().map(|a| a[b].len()).sum();
            let mut v: Vec<Pre> = Vec::with_capacity(total);
            for a in &accs {
                v.extend_from_slice(&a[b]);
            }
            v.sort_unstable_by(|x, y| (x.e, x.ingest).cmp(&(y.e, y.ingest)));
            let mut out: Vec<Vec<Ev>> = (0..NB2).map(|_| Vec::new()).collect();
            let mut last: Option<Key> = None;
            for p in &v {
                if last == Some(p.e) {
                    continue;
                }
                last = Some(p.e);
                if p.ts == i64::MIN {
                    continue;
                }
                let pb = ((p.key.0 >> 32) as usize * NB2) >> 32;
                out[pb].push(Ev { key: p.key, ts: p.ts, uptr: p.uptr, ulen: p.ulen, pc: p.pc });
            }
            out
        })
        .collect();
    drop(accs);
    tm("dedup");

    // ---- clicks ----
    let cl_chunks = chunks_of(&list_csv(&data_dir.join("clicks")));
    let cl_accs: Vec<Vec<Vec<Cl>>> = cl_chunks
        .par_iter()
        .fold(
            || (0..NB2).map(|_| Vec::new()).collect::<Vec<Vec<Cl>>>(),
            |mut acc, c| {
                for_lines(c, |l| {
                    let Some([cid, user, camp, ts]) = fields::<4>(l) else { return };
                    if cid.is_empty() || user.is_empty() {
                        return;
                    }
                    let Some(t) = parse_ts(ts) else { return };
                    if t < ws - WEEK_MS || t >= we {
                        return;
                    }
                    let Some(&ci) = ctx.camp_of.get(&hash128(camp)) else { return };
                    let key = resolve(&ctx, user);
                    let pb = ((key.0 >> 32) as usize * NB2) >> 32;
                    acc[pb].push(Cl { key, ts: t, cptr: cid.as_ptr() as usize, clen: cid.len() as u32, camp: ci });
                });
                acc
            },
        )
        .collect();
    tm("clicks parsed");

    // ---- sessionize + attribute ----
    let ncat = cats.len() + 1;
    let day0 = ws.div_euclid(DAY_MS);
    let day1 = (we - 1).div_euclid(DAY_MS);
    let ndays = (day1 - day0 + 1) as usize;
    let workers: Vec<Worker> = (0..NB2)
        .into_par_iter()
        .fold(
            || Worker {
                camp: vec![[0; 5]; (ncamp + 1) * ndays],
                ud: vec![],
                cnt: vec![0; ncamp],
                pos: vec![0; ncamp],
                active: vec![],
                cat_sum: vec![0; ncat],
            },
            |mut w, p| {
                let mut evs: Vec<Ev> = Vec::new();
                for part in &parts {
                    evs.extend_from_slice(&part[p]);
                }
                if evs.is_empty() {
                    return w;
                }
                let mut cls: Vec<Cl> = Vec::new();
                for a in &cl_accs {
                    cls.extend_from_slice(&a[p]);
                }
                evs.sort_unstable_by_key(|e| (e.key, e.ts));
                cls.sort_unstable_by_key(|c| (c.key, c.ts));
                // tie-break equal (key, ts) by click_id bytes
                let mut i = 0;
                while i < cls.len() {
                    let mut j = i + 1;
                    while j < cls.len() && cls[j].key == cls[i].key && cls[j].ts == cls[i].ts {
                        j += 1;
                    }
                    if j - i > 1 {
                        cls[i..j].sort_unstable_by(|a, b| slice_of(a.cptr, a.clen).cmp(slice_of(b.cptr, b.clen)));
                    }
                    i = j;
                }
                let mut ci = 0usize;
                let mut s = 0usize;
                while s < evs.len() {
                    let key = evs[s].key;
                    let mut e = s + 1;
                    while e < evs.len() && evs[e].key == key {
                        e += 1;
                    }
                    while ci < cls.len() && cls[ci].key < key {
                        ci += 1;
                    }
                    let mut ce = ci;
                    while ce < cls.len() && cls[ce].key == key {
                        ce += 1;
                    }
                    process_person(&mut w, &evs[s..e], &cls[ci..ce], &names, ncamp, day0, ndays, ncat);
                    ci = ce;
                    s = e;
                }
                w
            },
        )
        .collect();
    drop(parts);
    drop(cl_accs);
    tm("sessions");

    // ---- output ----
    let mut camp_tot = vec![[0u64; 5]; (ncamp + 1) * ndays];
    for w in &workers {
        for (a, b) in camp_tot.iter_mut().zip(&w.camp) {
            for k in 0..5 {
                a[k] += b[k];
            }
        }
    }
    let _ = std::fs::create_dir_all(out_dir.join("user_daily"));
    let _ = std::fs::create_dir_all(out_dir.join("campaign_daily"));
    write_user_daily(&out_dir.join("user_daily"), &workers, &cats);
    write_campaign_daily(&out_dir.join("campaign_daily"), &camp_tot, &camp_names, day0, ndays, ncamp);
    tm("written");
}

#[allow(clippy::too_many_arguments)]
fn process_person(
    w: &mut Worker,
    evs: &[Ev],
    cls: &[Cl],
    names: &IdMap<(usize, u32)>,
    ncamp: usize,
    day0: i64,
    ndays: usize,
    ncat: usize,
) {
    let key = evs[0].key;
    let (nptr, nlen) = match names.get(&key) {
        Some(&(p, l)) => (p, l),
        None => (evs[0].uptr, evs[0].ulen),
    };
    let organic = ncamp;
    let (mut lo, mut hi) = (0usize, 0usize);
    // current day accumulator
    let mut cur_day: i64 = i64::MIN;
    let (mut d_sess, mut d_ev, mut d_rev, mut d_purch) = (0u32, 0u32, 0u64, false);
    // current session
    let mut s_start = 0i64;
    let mut s_cnt = 0u32;
    let mut s_rev = 0u64;
    let mut s_camp = organic;
    let mut in_sess = false;
    let mut prev_ts = 0i64;

    macro_rules! flush_day {
        () => {
            if cur_day != i64::MIN {
                let top = if d_purch {
                    let mut bi = 0usize;
                    let mut bv = 0u64;
                    for c in 0..ncat {
                        if w.cat_sum[c] > bv {
                            bv = w.cat_sum[c];
                            bi = c;
                        }
                        w.cat_sum[c] = 0;
                    }
                    bi as u8
                } else {
                    255
                };
                w.ud.push(UdRow { nptr, nlen, day: cur_day as i32, sessions: d_sess, events: d_ev, revenue: d_rev, top });
            }
        };
    }
    macro_rules! close_sess {
        () => {
            if in_sess {
                let di = (s_start.div_euclid(DAY_MS) - day0) as usize;
                let a = &mut w.camp[s_camp * ndays + di];
                a[0] += 1;
                if s_rev > 0 {
                    a[1] += 1;
                }
                a[2] += s_rev;
                d_ev += s_cnt;
                d_rev += s_rev;
            }
        };
    }

    for e in evs {
        let new_sess = !in_sess || e.ts - prev_ts > GAP_MS;
        prev_ts = e.ts;
        if new_sess {
            close_sess!();
            let s = e.ts;
            let day = s.div_euclid(DAY_MS);
            if day != cur_day {
                flush_day!();
                cur_day = day;
                d_sess = 0;
                d_ev = 0;
                d_rev = 0;
                d_purch = false;
            }
            d_sess += 1;
            // advance click window
            while hi < cls.len() && cls[hi].ts <= s {
                let c = cls[hi].camp as usize;
                if w.cnt[c] == 0 {
                    w.pos[c] = w.active.len() as u32;
                    w.active.push(c as u32);
                }
                w.cnt[c] += 1;
                hi += 1;
            }
            while lo < hi && cls[lo].ts < s - WEEK_MS {
                let c = cls[lo].camp as usize;
                w.cnt[c] -= 1;
                if w.cnt[c] == 0 {
                    let p = w.pos[c] as usize;
                    let last = *w.active.last().unwrap();
                    w.active.swap_remove(p);
                    if p < w.active.len() {
                        w.pos[last as usize] = p as u32;
                    }
                }
                lo += 1;
            }
            s_camp = organic;
            if lo < hi {
                let t = cls[hi - 1].ts;
                let mut g = hi - 1;
                while g > lo && cls[g - 1].ts == t {
                    g -= 1;
                }
                s_camp = cls[g].camp as usize;
                let di = (day - day0) as usize;
                for &c in &w.active {
                    w.camp[c as usize * ndays + di][3] += 1;
                }
            }
            s_start = s;
            s_cnt = 0;
            s_rev = 0;
            in_sess = true;
        }
        s_cnt += 1;
        let price = e.pc & 0xFF_FFFF;
        if price > 0 {
            s_rev += price as u64;
            d_purch = true;
            w.cat_sum[(e.pc >> 24) as usize] += price as u64;
        }
    }
    close_sess!();
    flush_day!();
    // reset click window state
    for c in lo..hi {
        w.cnt[cls[c].camp as usize] = 0;
    }
    w.active.clear();
}

// ---------- parquet ----------
fn props() -> Arc<WriterProperties> {
    Arc::new(
        WriterProperties::builder()
            .set_dictionary_enabled(false)
            .set_statistics_enabled(EnabledStatistics::None)
            .set_compression(Compression::UNCOMPRESSED)
            .build(),
    )
}

fn write_user_daily(dir: &Path, workers: &[Worker], cats: &[B]) {
    let schema = Arc::new(
        parse_message_type(
            "message m { required binary user_id (STRING); required int32 day (DATE); required int32 sessions; required int32 events; required int64 revenue_cents; optional binary top_category (STRING); }",
        )
        .unwrap(),
    );
    let p = props();
    let mut jobs: Vec<(usize, &[UdRow])> = vec![];
    let mut idx = 0;
    for w in workers {
        for ch in w.ud.chunks(1_000_000) {
            jobs.push((idx, ch));
            idx += 1;
        }
    }
    if jobs.is_empty() {
        jobs.push((0, &[]));
    }
    jobs.par_iter().for_each(|(i, rows)| {
        let f = std::fs::File::create(dir.join(format!("part-{:05}.parquet", i))).unwrap();
        let mut w = SerializedFileWriter::new(f, schema.clone(), p.clone()).unwrap();
        let mut rg = w.next_row_group().unwrap();
        let mut c = rg.next_column().unwrap().unwrap();
        let v: Vec<ByteArray> = rows.iter().map(|r| ByteArray::from(slice_of(r.nptr, r.nlen))).collect();
        c.typed::<ByteArrayType>().write_batch(&v, None, None).unwrap();
        c.close().unwrap();
        let mut c = rg.next_column().unwrap().unwrap();
        let v: Vec<i32> = rows.iter().map(|r| r.day).collect();
        c.typed::<Int32Type>().write_batch(&v, None, None).unwrap();
        c.close().unwrap();
        let mut c = rg.next_column().unwrap().unwrap();
        let v: Vec<i32> = rows.iter().map(|r| r.sessions as i32).collect();
        c.typed::<Int32Type>().write_batch(&v, None, None).unwrap();
        c.close().unwrap();
        let mut c = rg.next_column().unwrap().unwrap();
        let v: Vec<i32> = rows.iter().map(|r| r.events as i32).collect();
        c.typed::<Int32Type>().write_batch(&v, None, None).unwrap();
        c.close().unwrap();
        let mut c = rg.next_column().unwrap().unwrap();
        let v: Vec<i64> = rows.iter().map(|r| r.revenue as i64).collect();
        c.typed::<Int64Type>().write_batch(&v, None, None).unwrap();
        c.close().unwrap();
        let mut c = rg.next_column().unwrap().unwrap();
        let mut defs: Vec<i16> = Vec::with_capacity(rows.len());
        let mut vals: Vec<ByteArray> = vec![];
        for r in rows.iter() {
            if r.top == 255 {
                defs.push(0);
            } else {
                defs.push(1);
                let name: &[u8] = if (r.top as usize) < cats.len() { cats[r.top as usize] } else { b"unknown" };
                vals.push(ByteArray::from(name));
            }
        }
        c.typed::<ByteArrayType>().write_batch(&vals, Some(&defs), None).unwrap();
        c.close().unwrap();
        rg.close().unwrap();
        w.close().unwrap();
    });
}

fn write_campaign_daily(dir: &Path, tot: &[[u64; 5]], camps: &[(B, B)], day0: i64, ndays: usize, ncamp: usize) {
    let schema = Arc::new(
        parse_message_type(
            "message m { required binary campaign_id (STRING); required binary channel (STRING); required int32 day (DATE); required int64 sessions; required int64 purchasing_sessions; required int64 revenue_cents; required int64 assisted_sessions; }",
        )
        .unwrap(),
    );
    let mut ids: Vec<ByteArray> = vec![];
    let mut chs: Vec<ByteArray> = vec![];
    let mut days: Vec<i32> = vec![];
    let mut cols: [Vec<i64>; 4] = Default::default();
    for c in 0..=ncamp {
        for d in 0..ndays {
            let a = &tot[c * ndays + d];
            if a[0] == 0 && a[3] == 0 {
                continue;
            }
            let (id, ch): (&[u8], &[u8]) = if c == ncamp { (b"organic", b"organic") } else { camps[c] };
            ids.push(ByteArray::from(id));
            chs.push(ByteArray::from(ch));
            days.push((day0 + d as i64) as i32);
            cols[0].push(a[0] as i64);
            cols[1].push(a[1] as i64);
            cols[2].push(a[2] as i64);
            cols[3].push(a[3] as i64);
        }
    }
    let f = std::fs::File::create(dir.join("part-00000.parquet")).unwrap();
    let mut w = SerializedFileWriter::new(f, schema, props()).unwrap();
    let mut rg = w.next_row_group().unwrap();
    let mut c = rg.next_column().unwrap().unwrap();
    c.typed::<ByteArrayType>().write_batch(&ids, None, None).unwrap();
    c.close().unwrap();
    let mut c = rg.next_column().unwrap().unwrap();
    c.typed::<ByteArrayType>().write_batch(&chs, None, None).unwrap();
    c.close().unwrap();
    let mut c = rg.next_column().unwrap().unwrap();
    c.typed::<Int32Type>().write_batch(&days, None, None).unwrap();
    c.close().unwrap();
    for k in 0..4 {
        let mut c = rg.next_column().unwrap().unwrap();
        c.typed::<Int64Type>().write_batch(&cols[k], None, None).unwrap();
        c.close().unwrap();
    }
    rg.close().unwrap();
    w.close().unwrap();
}
