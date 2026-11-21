#!/usr/bin/env python3
"""Slow, literal reference implementation of rules.md (an oracle, not a fast pipeline).

Everything is bytes (IDs compare byte-wise). No csv module, no datetime parsing.
Usage: reference.py [DATA_DIR OUTPUT_DIR]   (else env DATA_DIR / OUTPUT_DIR)
"""
import bisect
import datetime
import json
import os
import sys

DAY_MS = 86_400_000
WEEK_MS = 7 * DAY_MS
SESSION_GAP_MS = 1_800_000
LATE_MS = DAY_MS
ORGANIC = b"organic"


# ---------------------------------------------------------------- CSV dialect
def read_rows(path, ncols):
    """Yield the list of ncols byte-fields of every well-formed data line of a CSV file.
    Only b'\\n' ends a line; the first line is the header; an empty line is malformed."""
    with open(path, "rb") as f:
        data = f.read()
    if not data:
        return
    lines = data.split(b"\n")
    if lines[-1] == b"":  # file ends with the terminator of its last line
        lines.pop()
    for line in lines[1:]:  # skip header (only the first line, whatever else looks like it)
        fields = line.split(b",")  # no value holds a comma, so every comma separates
        if len(fields) != ncols:  # wrong field count (also covers empty line: 1 field)
            continue
        out = []
        for fld in fields:
            if b'"' in fld:
                # a quote is only legal as the first and last byte, around the whole field
                if len(fld) >= 2 and fld[:1] == b'"' and fld[-1:] == b'"' and b'"' not in fld[1:-1]:
                    fld = fld[1:-1]
                else:
                    out = None
                    break
            out.append(fld)  # empty field (b"") means null
        if out is not None:
            yield out


def csv_files(folder):
    names = sorted(n for n in os.listdir(folder) if n.endswith(".csv"))
    return [os.path.join(folder, n) for n in names if os.path.isfile(os.path.join(folder, n))]


# ----------------------------------------------------------------- timestamps
def is_leap(y):
    return y % 4 == 0 and (y % 100 != 0 or y % 400 == 0)


def days_in_month(y, m):
    if m == 2:
        return 29 if is_leap(y) else 28
    return 30 if m in (4, 6, 9, 11) else 31


def days_from_civil(y, m, d):
    """Days since 1970-01-01 of a proleptic Gregorian date."""
    y -= m <= 2
    era = y // 400
    yoe = y - era * 400
    doy = (153 * (m + (-3 if m > 2 else 9)) + 2) // 5 + d - 1
    doe = yoe * 365 + yoe // 4 - yoe // 100 + doy
    return era * 146097 + doe - 719468


def num(b):
    """Integer of an all-ASCII-digit byte string, else None (bytes.isdigit is ASCII-only)."""
    return int(b) if b.isdigit() else None


def parse_ts(b):
    """Epoch ms for F1 / F2 / F3, or None if bad. (Window checks are done by the caller.)"""
    if not b:
        return None
    if b.isdigit():  # F3: digits only; any length. Too long for 64-bit is just "outside window"
        s = b.lstrip(b"0")
        if len(s) > 25:
            return 10 ** 25
        return int(s) if s else 0
    if len(b) == 20 and b[19:20] == b"Z":  # F1
        off_ms = 0
    elif len(b) == 25 and b[19:20] in (b"+", b"-") and b[22:23] == b":":  # F2
        hh, mm = num(b[20:22]), num(b[23:25])
        if hh is None or mm is None or hh > 23 or mm > 59:
            return None
        off_ms = (hh * 60 + mm) * 60_000
        if b[19:20] == b"-":
            off_ms = -off_ms
    else:
        return None
    if not (b[4:5] == b"-" and b[7:8] == b"-" and b[10:11] == b"T" and b[13:14] == b":" and b[16:17] == b":"):
        return None
    y, mo, d = num(b[0:4]), num(b[5:7]), num(b[8:10])
    h, mi, s = num(b[11:13]), num(b[14:16]), num(b[17:19])
    if None in (y, mo, d, h, mi, s):
        return None
    if not (1 <= mo <= 12 and 1 <= d <= days_in_month(y, mo) and h <= 23 and mi <= 59 and s <= 59):
        return None
    local_ms = ((days_from_civil(y, mo, d) * 24 + h) * 60 + mi) * 60_000 + s * 1000
    return local_ms - off_ms  # local = UTC + offset, so UTC = local - offset


# ------------------------------------------------------------------- pipeline
def main():
    if len(sys.argv) >= 3:
        data_dir, out_dir = sys.argv[-2], sys.argv[-1]
    else:
        data_dir, out_dir = os.environ.get("DATA_DIR"), os.environ.get("OUTPUT_DIR")
    if not data_dir or not out_dir or not os.path.isdir(data_dir):
        sys.exit("need DATA_DIR (existing) and OUTPUT_DIR via env or last two args")

    with open(os.path.join(data_dir, "manifest.json"), "rb") as f:
        manifest = json.load(f)
    win_start = parse_ts(manifest["window_start"].encode())
    win_end = parse_ts(manifest["window_end"].encode())
    if win_start is None or win_end is None:
        sys.exit("bad manifest window")

    # products / campaigns (clean files, same dialect)
    category_of = {}
    for pid, cat in read_rows(os.path.join(data_dir, "products.csv"), 2):
        category_of[pid] = cat
    channel_of = {}
    for cid, ch in read_rows(os.path.join(data_dir, "campaigns.csv"), 2):
        channel_of[cid] = ch

    # ---- Linked IDs: union-find; the root of each set is always its byte-wise smallest ID
    parent = {}

    def find(x):
        root = x
        while parent.get(root, root) != root:
            root = parent[root]
        while parent.get(x, x) != root:  # path compression
            parent[x], x = root, parent[x]
        return root

    for path in csv_files(os.path.join(data_dir, "links")):
        for a, b in read_rows(path, 2):
            if not a or not b:  # a line with an empty side is dropped
                continue
            ra, rb = find(a), find(b)
            if ra != rb:
                if ra < rb:
                    parent[rb] = ra
                else:
                    parent[ra] = rb

    # ---- Events 1-2: parse, drop empty event_id, dedup keeping smallest ingest_ts
    best = {}  # event_id -> (ingest_ms, fields)
    for path in csv_files(os.path.join(data_dir, "events")):
        for fields in read_rows(path, 7):
            eid = fields[0]
            if not eid:
                continue
            ingest = parse_ts(fields[3])
            if ingest is None:  # spec says ingest_ts is always valid; if not, treat line as unusable
                continue
            cur = best.get(eid)
            if cur is None or ingest < cur[0]:
                best[eid] = (ingest, fields)

    # ---- Events 3-4: validate the kept copy, drop late
    per_person = {}  # person -> list of (ts, price_cents, category_or_None)
    for ingest, (eid, user, ts_raw, _ing, etype, pid, price_raw) in best.values():
        if not user:
            continue
        ts = parse_ts(ts_raw)
        if ts is None or not (win_start <= ts < win_end):
            continue
        if etype not in (b"view", b"cart", b"purchase"):
            continue
        if not pid:
            continue
        price, cat = 0, None
        if etype == b"purchase":
            if not price_raw.isdigit():  # ASCII digits only
                continue
            stripped = price_raw.lstrip(b"0")
            if len(stripped) > 8:  # certainly over 10,000,000 (also avoids huge int())
                continue
            price = int(stripped) if stripped else 0
            if not (1 <= price <= 10_000_000):
                continue
            cat = category_of.get(pid, b"unknown")  # Events 7: missing product -> unknown
        if ingest - ts > LATE_MS:  # exactly 24h kept; ingest before ts kept
            continue
        per_person.setdefault(find(user), []).append((ts, price, cat))
    best = None

    # ---- Clicks 1-3: parse, dedup on click_id (any copy), validate
    clicks_by_person = {}  # person -> sorted list of (ts, click_id, campaign_id)
    seen_clicks = set()
    for path in csv_files(os.path.join(data_dir, "clicks")):
        for click_id, user, camp, ts_raw in read_rows(path, 4):
            if not click_id or click_id in seen_clicks:
                continue
            seen_clicks.add(click_id)
            if not user:
                continue
            ts = parse_ts(ts_raw)
            if ts is None or not (win_start - WEEK_MS <= ts < win_end):
                continue
            if camp not in channel_of:
                continue
            clicks_by_person.setdefault(find(user), []).append((ts, click_id, camp))
    seen_clicks = None
    for lst in clicks_by_person.values():
        lst.sort()  # by ts, then click_id byte-wise

    # ---- Sessions, attribution, aggregation
    user_daily = {}      # (person, day) -> [sessions, events, revenue, {category: sum}]
    campaign_daily = {}  # (campaign, day) -> [sessions, purchasing, revenue, assisted]

    for person, evs in per_person.items():
        evs.sort(key=lambda e: e[0])
        # Events 5: split where gap between consecutive timestamps > 30 min (equal ts => gap 0)
        sessions = []
        for e in evs:
            if sessions and e[0] - sessions[-1][-1][0] <= SESSION_GAP_MS:
                sessions[-1].append(e)
            else:
                sessions.append([e])
        clicks = clicks_by_person.get(person, [])
        click_ts = [c[0] for c in clicks]
        for sess in sessions:
            start = sess[0][0]
            day = start // DAY_MS  # Events 6: UTC date of the start
            n_events = len(sess)
            revenue = sum(e[1] for e in sess)
            cat_sums = {}
            for _ts, price, cat in sess:
                if cat is not None:
                    cat_sums[cat] = cat_sums.get(cat, 0) + price

            ud = user_daily.setdefault((person, day), [0, 0, 0, {}])
            ud[0] += 1
            ud[1] += n_events
            ud[2] += revenue
            for cat, v in cat_sums.items():
                ud[3][cat] = ud[3].get(cat, 0) + v

            # Clicks 4/7: candidates have start-7d <= click.ts <= start
            lo = bisect.bisect_left(click_ts, start - WEEK_MS)
            hi = bisect.bisect_right(click_ts, start)
            cands = clicks[lo:hi]
            if cands:
                # latest ts, tie -> smallest click_id
                top_ts = max(c[0] for c in cands)
                credited = min(c[1:] for c in cands if c[0] == top_ts)[1]
                assisting = {c[2] for c in cands}
            else:
                credited, assisting = ORGANIC, set()  # Clicks 5
            row = campaign_daily.setdefault((credited, day), [0, 0, 0, 0])
            row[0] += 1
            row[1] += 1 if any(e[2] is not None for e in sess) else 0
            row[2] += revenue
            for camp in assisting:  # each assisting campaign once per session; organic never
                campaign_daily.setdefault((camp, day), [0, 0, 0, 0])[3] += 1

    # ---- Output (polars only here)
    import polars as pl

    def to_date(day_num):
        return datetime.date(1970, 1, 1) + datetime.timedelta(days=day_num)

    def s(b):
        return b.decode("utf-8", errors="replace")

    ud_rows = []
    for (person, day), (n_sess, n_ev, rev, cats) in user_daily.items():
        top = None
        if cats:
            top = s(min(cats, key=lambda c: (-cats[c], c)))  # highest sum, tie byte-wise smallest
        ud_rows.append((s(person), to_date(day), n_sess, n_ev, rev, top))
    cd_rows = []
    for (camp, day), (n_sess, n_pur, rev, n_assist) in campaign_daily.items():
        channel = ORGANIC if camp == ORGANIC else channel_of[camp]
        cd_rows.append((s(camp), s(channel), to_date(day), n_sess, n_pur, rev, n_assist))

    ud_schema = {"user_id": pl.String, "day": pl.Date, "sessions": pl.Int64, "events": pl.Int64,
                 "revenue_cents": pl.Int64, "top_category": pl.String}
    cd_schema = {"campaign_id": pl.String, "channel": pl.String, "day": pl.Date, "sessions": pl.Int64,
                 "purchasing_sessions": pl.Int64, "revenue_cents": pl.Int64, "assisted_sessions": pl.Int64}
    for name, schema, rows in (("user_daily", ud_schema, ud_rows), ("campaign_daily", cd_schema, cd_rows)):
        folder = os.path.join(out_dir, name)
        os.makedirs(folder, exist_ok=True)
        df = pl.DataFrame(rows, schema=schema, orient="row")
        df.write_parquet(os.path.join(folder, "part-0.parquet"))


if __name__ == "__main__":
    main()
