#!/usr/bin/env python3
"""
Shopping Sessions pipeline (DuckDB).

Reads  $DATA_DIR    (default /data)
Writes $OUTPUT_DIR  (default /output): user_daily/ and campaign_daily/ as Parquet
Temp   $SCRATCH_DIR (default /scratch)

CSV files are read as raw text and parsed in SQL so the dialect in rules.md is
followed exactly (DuckDB's CSV reader repairs lines the rules say to drop).
Identity resolution uses a Python union-find over dense integer ids.
"""
import json
import os
import re
import sys
import time

import duckdb

DATA = os.environ.get("DATA_DIR", "/data")
OUT = os.environ.get("OUTPUT_DIR", "/output")
SCRATCH = os.environ.get("SCRATCH_DIR", "/scratch")
NCPU = int(os.environ.get("NCPU", os.cpu_count() or 1))
MEM_GB = int(os.environ.get("MEM_GB", "8"))

T0 = time.time()


def log(msg):
    print(f"[{time.time() - T0:7.2f}s] {msg}", file=sys.stderr, flush=True)


os.makedirs(SCRATCH, exist_ok=True)
con = duckdb.connect()
con.execute(f"SET threads = {NCPU}")
con.execute(f"SET memory_limit = '{max(1, int(MEM_GB * 0.8))}GB'")
con.execute(f"SET temp_directory = '{os.path.join(SCRATCH, 'duckdb_tmp')}'")
con.execute("SET preserve_insertion_order = false")

with open(os.path.join(DATA, "manifest.json")) as f:
    manifest = json.load(f)

# ---------------------------------------------------------------------------
# Helpers: timestamp parsing, raw-line CSV parsing
# ---------------------------------------------------------------------------
TS_RE = (r"^([0-9]{4})-([0-9]{2})-([0-9]{2})T([0-9]{2}):([0-9]{2}):([0-9]{2})"
         r"(?:(Z)|([+-])([0-9]{2}):([0-9]{2}))$")
TS_FIELDS = "['y','mo','d','h','mi','s','z','sg','oh','om']"

con.execute(f"""
CREATE MACRO fld(s) AS NULLIF(
    CASE WHEN length(s) >= 2 AND starts_with(s, '"') AND ends_with(s, '"')
         THEN substr(s, 2, length(s) - 2) ELSE s END, '');
-- raw text -> epoch ms (NULL if bad). r is the struct from regexp_extract(raw, TS_RE, ...)
CREATE MACRO ts_ms(raw, r) AS
  CASE
    WHEN raw IS NULL THEN NULL
    WHEN regexp_full_match(raw, '[0-9]+') THEN try_cast(raw AS BIGINT)
    WHEN r.y = '' THEN NULL
    WHEN r.h::INT > 23 OR r.mi::INT > 59 OR r.s::INT > 59 THEN NULL
    WHEN r.z = '' AND (r.oh::INT > 23 OR r.om::INT > 59) THEN NULL
    ELSE (try_cast(r.y || '-' || r.mo || '-' || r.d AS DATE) - DATE '1970-01-01')::BIGINT * 86400000
         + r.h::BIGINT * 3600000 + r.mi::BIGINT * 60000 + r.s::BIGINT * 1000
         - CASE WHEN r.z = '' THEN (CASE WHEN r.sg = '-' THEN -1 ELSE 1 END)
                                    * (r.oh::BIGINT * 3600000 + r.om::BIGINT * 60000)
                ELSE 0 END
  END;
""")


def to_ms(iso):
    return con.execute(
        f"SELECT ts_ms('{iso}', regexp_extract('{iso}', '{TS_RE}', {TS_FIELDS}))").fetchone()[0]


W_START = to_ms(manifest["window_start"])
W_END = to_ms(manifest["window_end"])
DAY7 = 7 * 86400000
log(f"window ms {W_START}..{W_END}")


def csv_files(sub):
    d = os.path.join(DATA, sub)
    if not os.path.isdir(d):
        return []
    return sorted(os.path.join(d, n) for n in os.listdir(d) if n.endswith(".csv"))


def field_re(n):
    f = r'(?:"[^",]*"|[^",]*)'
    return "^" + f + ("," + f) * (n - 1) + "$"


def parse_lines(files, n, names):
    """Return SQL for a relation of the n parsed fields of every well-formed data line."""
    cols = ", ".join(f"fld(p[{i + 1}]) AS {names[i]}" for i in range(n))
    if not files:
        empty = ", ".join(f"NULL::VARCHAR AS {c}" for c in names)
        return f"SELECT {empty} WHERE false"
    flist = "[" + ", ".join("'" + f.replace("'", "''") + "'" for f in files) + "]"
    # read_text keeps every byte (\\r included), empty lines and a missing final newline;
    # the first line of each file is its header.
    return f"""
    SELECT {cols} FROM (
        SELECT string_split(x, ',') AS p FROM (
            SELECT unnest(string_split(
                CASE WHEN instr(content, chr(10)) = 0 THEN ''
                     ELSE substr(content, instr(content, chr(10)) + 1) END, chr(10))) AS x
            FROM read_text({flist})
        ) WHERE regexp_full_match(x, '{field_re(n)}')
    )"""


# ---------------------------------------------------------------------------
# Dimensions
# ---------------------------------------------------------------------------
con.execute(f"CREATE TEMP TABLE products AS "
            f"SELECT product_id, any_value(category) AS category FROM ({parse_lines([os.path.join(DATA, 'products.csv')], 2, ['product_id', 'category'])}) "
            f"WHERE product_id IS NOT NULL GROUP BY product_id")
con.execute(f"CREATE TEMP TABLE campaigns AS "
            f"SELECT campaign_id, any_value(channel) AS channel FROM ({parse_lines([os.path.join(DATA, 'campaigns.csv')], 2, ['campaign_id', 'channel'])}) "
            f"WHERE campaign_id IS NOT NULL GROUP BY campaign_id")
log("dimensions loaded")

# ---------------------------------------------------------------------------
# Identity resolution
# ---------------------------------------------------------------------------
con.execute(f"""
CREATE TEMP TABLE links AS
SELECT DISTINCT user_id_a AS a, user_id_b AS b
FROM ({parse_lines(csv_files('links'), 2, ['user_id_a', 'user_id_b'])})
WHERE user_id_a IS NOT NULL AND user_id_b IS NOT NULL AND user_id_a <> user_id_b
""")
con.execute("""
CREATE TEMP TABLE link_ids AS
SELECT id, (row_number() OVER ())::BIGINT - 1 AS n
FROM (SELECT a AS id FROM links UNION SELECT b FROM links)
""")
edges = con.execute("""
SELECT la.n, lb.n FROM links l JOIN link_ids la ON la.id = l.a JOIN link_ids lb ON lb.id = l.b
""").fetchall()
n_ids = con.execute("SELECT count(*) FROM link_ids").fetchone()[0]
parent = list(range(n_ids))
for a, b in edges:
    while parent[a] != a:
        parent[a] = parent[parent[a]]
        a = parent[a]
    while parent[b] != b:
        parent[b] = parent[parent[b]]
        b = parent[b]
    if a != b:
        parent[a] = b
for i in range(n_ids):  # flatten
    r = i
    while parent[r] != r:
        r = parent[r]
    while parent[i] != r:
        parent[i], i = r, parent[i]
comp_file = os.path.join(SCRATCH, "components.csv")
with open(comp_file, "w") as fh:
    fh.write("n,root\n")
    fh.write("\n".join(f"{i},{parent[i]}" for i in range(n_ids)))
    fh.write("\n")
del edges, parent
con.execute(f"""
CREATE TEMP TABLE pmap AS
SELECT li.id AS id, min(li.id) OVER (PARTITION BY c.root) AS person
FROM link_ids li JOIN read_csv('{comp_file}', header=true, columns={{'n':'BIGINT','root':'BIGINT'}}) c ON c.n = li.n
""")
os.remove(comp_file)
con.execute("DROP TABLE links; DROP TABLE link_ids")
log(f"identity resolved: {n_ids} linked ids")

# ---------------------------------------------------------------------------
# Events
# ---------------------------------------------------------------------------
con.execute(f"""
CREATE TEMP TABLE ev_dedup AS
SELECT event_id, k.user_id, k.ts_raw, k.ing, k.event_type, k.product_id, k.price_raw FROM (
SELECT event_id, arg_min(struct_pack(user_id := user_id, ts_raw := ts_raw, ing := ing, event_type := event_type,
                                     product_id := product_id, price_raw := price_raw), ing) AS k
FROM (
    SELECT event_id, user_id, ts_raw, event_type, product_id, price_raw,
           ts_ms(ing_raw, regexp_extract(ing_raw, '{TS_RE}', {TS_FIELDS})) AS ing
    FROM ({parse_lines(csv_files('events'), 7, ['event_id', 'user_id', 'ts_raw', 'ing_raw', 'event_type', 'product_id', 'price_raw'])})
    WHERE event_id IS NOT NULL
)
GROUP BY event_id)
""")
log("events deduped")

con.execute(f"""
CREATE TEMP TABLE ev AS
SELECT coalesce(m.person, e.user_id) AS person, e.ts,
       e.event_type, e.product_id,
       CASE WHEN e.event_type = 'purchase' THEN e.price END AS price
FROM (
    SELECT user_id, event_type, product_id, ts, ing, price
    FROM (
        SELECT user_id, event_type, product_id, ing,
               ts_ms(ts_raw, regexp_extract(ts_raw, '{TS_RE}', {TS_FIELDS})) AS ts,
               CASE WHEN regexp_full_match(price_raw, '[0-9]+') THEN try_cast(price_raw AS BIGINT) END AS price
        FROM ev_dedup
        WHERE user_id IS NOT NULL AND product_id IS NOT NULL
          AND event_type IN ('view', 'cart', 'purchase')
    )
    WHERE ts >= {W_START} AND ts < {W_END}
      AND (event_type <> 'purchase' OR (price >= 1 AND price <= 10000000))
      AND NOT (ing - ts > 86400000)
) e LEFT JOIN pmap m ON m.id = e.user_id
""")
con.execute("DROP TABLE ev_dedup")
log("events validated")

# ---------------------------------------------------------------------------
# Sessions
# ---------------------------------------------------------------------------
con.execute("""
CREATE TEMP TABLE ev_s AS
SELECT person, ts, product_id, price,
       sum(brk) OVER (PARTITION BY person ORDER BY ts) AS sn
FROM (SELECT person, ts, product_id, price,
             CASE WHEN ts - lag(ts) OVER (PARTITION BY person ORDER BY ts) > 1800000 THEN 1 ELSE 0 END AS brk
      FROM ev)
""")
con.execute("DROP TABLE ev")
con.execute("""
CREATE TEMP TABLE sess AS
SELECT person, sn, min(ts) AS start_ms, (epoch_ms(min(ts))::DATE) AS day, count(*) AS events,
       coalesce(sum(price), 0)::BIGINT AS revenue, count(price) AS purchases
FROM ev_s GROUP BY person, sn
""")
log("sessions built")

con.execute("""
CREATE TEMP TABLE top_cat AS
SELECT person, day, category AS top_category FROM (
    SELECT person, day, category,
           row_number() OVER (PARTITION BY person, day ORDER BY total DESC, category ASC) AS rn
    FROM (
        SELECT s.person, s.day, coalesce(p.category, 'unknown') AS category, sum(e.price) AS total
        FROM (SELECT person, sn, product_id, price FROM ev_s WHERE price IS NOT NULL) e
        JOIN sess s ON s.person = e.person AND s.sn = e.sn
        LEFT JOIN products p ON p.product_id = e.product_id
        GROUP BY s.person, s.day, coalesce(p.category, 'unknown')
    )
) WHERE rn = 1
""")
con.execute("DROP TABLE ev_s")
log("top categories built")

# ---------------------------------------------------------------------------
# Clicks
# ---------------------------------------------------------------------------
con.execute(f"""
CREATE TEMP TABLE clk AS
SELECT coalesce(m.person, c.user_id) AS person, c.campaign_id, c.click_id, c.ts
FROM (
    SELECT click_id, any_value(user_id) AS user_id, any_value(campaign_id) AS campaign_id, any_value(ts) AS ts
    FROM (
        SELECT click_id, user_id, campaign_id,
               ts_ms(ts_raw, regexp_extract(ts_raw, '{TS_RE}', {TS_FIELDS})) AS ts
        FROM ({parse_lines(csv_files('clicks'), 4, ['click_id', 'user_id', 'campaign_id', 'ts_raw'])})
        WHERE click_id IS NOT NULL
    )
    GROUP BY click_id
) c
JOIN campaigns k ON k.campaign_id = c.campaign_id
LEFT JOIN pmap m ON m.id = c.user_id
WHERE c.user_id IS NOT NULL AND c.ts >= {W_START - DAY7} AND c.ts < {W_END}
""")
log("clicks validated")

# Last touch: one click per (person, ts) (smallest click_id), ASOF join, then 7-day check.
con.execute("""
CREATE TEMP TABLE clk_ts AS
SELECT person, ts, min(click_id) AS click_id FROM clk GROUP BY person, ts
""")
con.execute(f"""
CREATE TEMP TABLE credited AS
SELECT s.person, s.start_ms, s.day, s.events, s.revenue, s.purchases,
       CASE WHEN c.ts IS NOT NULL AND s.start_ms - c.ts <= {DAY7} THEN c.click_id END AS click_id
FROM sess s
ASOF LEFT JOIN clk_ts c ON s.person = c.person AND s.start_ms >= c.ts
""")
con.execute("DROP TABLE clk_ts")
log("credited sessions built")

# Assists: merge each (person, campaign)'s click coverage windows [ts, ts + 7d] into disjoint
# intervals, then each session joins at most one interval per campaign.
con.execute(f"""
CREATE TEMP TABLE cov AS
SELECT person, campaign_id, min(ts) AS lo, max(ts) + {DAY7} AS hi FROM (
    SELECT person, campaign_id, ts,
           sum(CASE WHEN ts - prev > {DAY7} THEN 1 ELSE 0 END) OVER (PARTITION BY person, campaign_id ORDER BY ts) AS g
    FROM (SELECT person, campaign_id, ts, lag(ts) OVER (PARTITION BY person, campaign_id ORDER BY ts) AS prev FROM clk)
)
GROUP BY person, campaign_id, g
""")
con.execute("""
CREATE TEMP TABLE assists AS
SELECT v.campaign_id, s.day, count(*) AS assisted
FROM sess s
JOIN cov v ON v.person = s.person AND s.start_ms BETWEEN v.lo AND v.hi
GROUP BY v.campaign_id, s.day
""")
con.execute("CREATE TEMP TABLE clk_campaign AS SELECT click_id, any_value(campaign_id) AS campaign_id FROM clk GROUP BY click_id")
con.execute("DROP TABLE cov; DROP TABLE clk; DROP TABLE sess")
log("assists built")

# ---------------------------------------------------------------------------
# Outputs
# ---------------------------------------------------------------------------
for sub in ("user_daily", "campaign_daily"):
    os.makedirs(os.path.join(OUT, sub), exist_ok=True)

con.execute(f"""
COPY (
    SELECT s.person AS user_id, s.day, count(*)::BIGINT AS sessions, sum(s.events)::BIGINT AS events,
           sum(s.revenue)::BIGINT AS revenue_cents, any_value(t.top_category) AS top_category
    FROM credited s LEFT JOIN top_cat t ON t.person = s.person AND t.day = s.day
    GROUP BY s.person, s.day
) TO '{os.path.join(OUT, "user_daily", "part-0.parquet")}' (FORMAT parquet)
""")
con.execute(f"""
COPY (
    SELECT r.campaign_id, coalesce(k.channel, 'organic') AS channel, r.day,
           coalesce(c.sessions, 0)::BIGINT AS sessions,
           coalesce(c.purchasing_sessions, 0)::BIGINT AS purchasing_sessions,
           coalesce(c.revenue_cents, 0)::BIGINT AS revenue_cents,
           coalesce(a.assisted, 0)::BIGINT AS assisted_sessions
    FROM (
        SELECT campaign_id, day FROM (
            SELECT coalesce(c.campaign_id, 'organic') AS campaign_id, s.day
            FROM credited s LEFT JOIN clk_campaign c ON c.click_id = s.click_id
        ) UNION SELECT campaign_id, day FROM assists
    ) r
    LEFT JOIN (
        SELECT coalesce(c.campaign_id, 'organic') AS campaign_id, s.day,
               count(*) AS sessions, count(*) FILTER (WHERE s.purchases > 0) AS purchasing_sessions,
               sum(s.revenue) AS revenue_cents
        FROM credited s LEFT JOIN clk_campaign c ON c.click_id = s.click_id
        GROUP BY 1, 2
    ) c ON c.campaign_id = r.campaign_id AND c.day = r.day
    LEFT JOIN assists a ON a.campaign_id = r.campaign_id AND a.day = r.day
    LEFT JOIN campaigns k ON k.campaign_id = r.campaign_id
) TO '{os.path.join(OUT, "campaign_daily", "part-0.parquet")}' (FORMAT parquet)
""")
log("done")
