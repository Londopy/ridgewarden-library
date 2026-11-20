//! What exists and who holds it, built from three kinds of event:
//!
//! `library.item` (main log): a fetcher published a version of an item as a blob, with
//! where it came from and the publisher's own hash.
//! `library.copied` (main log): a site finished copying a version. Small and lasting, so
//! it travels with data-mule cars, which carry no telemetry.
//! `library.site` (telemetry): each site's heartbeat, with the versions it holds and
//! serves, what it is still copying, its free disk, alert delivery times and NWS state.

use anyhow::Result;
use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use ridgewarden_client::mesh::s;
use ridgewarden_client::util::{norm_node, valid_node};
use rusqlite::{Connection, Transaction, params};
use serde_json::Value;

use crate::content::{DOC_EXTS, Kind, valid_item, valid_version};

pub struct Items;
pub struct Sites;

/// Most items one heartbeat may list; a real library has a handful.
pub const MAX_HOLDS: usize = 64;
/// Most alert ids one heartbeat may list as active.
pub const MAX_ACTIVE: usize = 500;

const ITEMS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS items (
    event_id    TEXT PRIMARY KEY,
    ts          INTEGER NOT NULL,
    author_key  TEXT NOT NULL,
    author_call TEXT NOT NULL,
    item        TEXT NOT NULL,
    kind        TEXT NOT NULL,
    ext         TEXT NOT NULL,
    title       TEXT NOT NULL,
    version     TEXT NOT NULL,
    size        INTEGER NOT NULL,
    root        TEXT NOT NULL,
    source      TEXT NOT NULL,
    sha256      TEXT,
    hash_from   TEXT,
    tiers       TEXT NOT NULL,
    grp         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS items_item_ts ON items(item, ts);
CREATE INDEX IF NOT EXISTS items_item_root ON items(item, root);
CREATE TABLE IF NOT EXISTS copies (
    author_key TEXT NOT NULL,
    call       TEXT NOT NULL,
    node       TEXT NOT NULL,
    item       TEXT NOT NULL,
    version    TEXT NOT NULL,
    root       TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    PRIMARY KEY (author_key, item)
);
"#;

const SITES_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sites (
    author_key     TEXT PRIMARY KEY,
    call           TEXT NOT NULL,
    node           TEXT NOT NULL,
    ts             INTEGER NOT NULL,
    tier           TEXT NOT NULL,
    fetches        TEXT NOT NULL,
    free_mb        INTEGER,
    alert_delay_ms INTEGER,
    alerts_seen    INTEGER,
    kiwix_ok_ms    INTEGER,
    core_url       TEXT,
    nws_place      TEXT,
    nws_ok_ms      INTEGER,
    nws_error      TEXT,
    nws_active     TEXT
);
CREATE TABLE IF NOT EXISTS site_holds (
    author_key TEXT NOT NULL,
    item       TEXT NOT NULL,
    version    TEXT NOT NULL,
    root       TEXT NOT NULL,
    state      TEXT NOT NULL,
    detail     TEXT,
    PRIMARY KEY (author_key, item, state)
);
"#;

/// A core's base URL in the form the core accepts as a peer (the same checks as its
/// `blobs::peer_url`): plain http, a host and maybe a port, nothing else.
pub fn peer_url(u: &str) -> Option<String> {
    let u = u.trim();
    if u.len() >= 200 {
        return None;
    }
    let p = reqwest::Url::parse(u).ok()?;
    let plain = p.scheme() == "http"
        && p.username().is_empty()
        && p.password().is_none()
        && matches!(p.path(), "" | "/")
        && p.query().is_none()
        && p.fragment().is_none();
    let host = p.host_str().filter(|h| plain && !h.is_empty())?;
    Some(match p.port() {
        Some(port) => format!("http://{host}:{port}"),
        None => format!("http://{host}"),
    })
}

pub fn is_hash(h: &str) -> bool {
    h.len() == 64
        && h.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// At most `n` characters, so one station can't fill every page with one string.
pub fn cap(v: Option<&str>, n: usize) -> Option<String> {
    v.map(|x| x.chars().take(n).collect())
}

/// A published version of an item.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub event_id: String,
    pub ts: i64,
    pub call: String,
    pub item: String,
    pub kind: Kind,
    /// the file extension it is stored and served with
    pub ext: String,
    pub title: String,
    pub version: String,
    pub size: u64,
    pub root: String,
    pub source: String,
    pub sha256: Option<String>,
    /// where the fetcher got the publisher's hash; None when there is none
    pub hash_from: Option<String>,
    /// site tiers that copy it; empty means every site
    pub tiers: Vec<String>,
    /// items in a group are alternatives; a site holds one
    pub group: String,
}

fn strings(v: Option<&Value>, max: usize, len: usize) -> Vec<String> {
    v.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(max)
        .map(|x| x.chars().take(len).collect())
        .collect()
}

/// The item a `library.item` payload describes, if every field checks out. The event's
/// id, time and author are filled in by the caller.
pub fn parse_item(p: &Value) -> Option<Item> {
    let kind = Kind::parse(s(p, "kind")?)?;
    let ext = match kind {
        Kind::Doc => s(p, "ext").filter(|e| DOC_EXTS.contains(e))?.to_string(),
        k => k.name().to_string(),
    };
    let group = s(p, "group").unwrap_or("");
    Some(Item {
        event_id: String::new(),
        ts: 0,
        call: String::new(),
        item: s(p, "item").filter(|i| valid_item(i))?.into(),
        kind,
        ext,
        title: cap(s(p, "title"), 120).filter(|t| !t.trim().is_empty())?,
        version: s(p, "version").filter(|v| valid_version(v))?.into(),
        size: p.get("size").and_then(Value::as_u64).filter(|n| *n > 0)?,
        root: s(p, "root").filter(|r| is_hash(r))?.into(),
        source: cap(s(p, "source"), 300).unwrap_or_default(),
        sha256: s(p, "sha256").filter(|h| is_hash(h)).map(str::to_string),
        hash_from: None,
        tiers: strings(p.get("tiers"), 8, 40)
            .into_iter()
            .filter(|t| valid_item(t))
            .collect(),
        group: if valid_item(group) {
            group.into()
        } else {
            String::new()
        },
    })
    .map(|mut it| {
        it.hash_from = it.sha256.as_ref().and(cap(s(p, "hash_from"), 300));
        it
    })
}

/// A copy a site reported in a `library.copied` event.
fn apply_copied(tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
    let p = &e.payload;
    let (Some(item), Some(version), Some(root)) = (
        s(p, "item").filter(|i| valid_item(i)),
        s(p, "version").filter(|v| valid_version(v)),
        s(p, "root").filter(|r| is_hash(r)),
    ) else {
        return Ok(());
    };
    let node = norm_node(s(p, "node").unwrap_or(""));
    tx.execute(
        "INSERT INTO copies (author_key, call, node, item, version, root, ts)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(author_key, item) DO UPDATE SET call = excluded.call, node = excluded.node,
             version = excluded.version, root = excluded.root, ts = excluded.ts
         WHERE excluded.ts > copies.ts",
        params![
            e.author.key,
            e.author.call,
            if valid_node(&node) {
                node
            } else {
                String::new()
            },
            item,
            version,
            root,
            ts
        ],
    )?;
    Ok(())
}

impl Projection for Items {
    fn schema(&self) -> &'static str {
        ITEMS_SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute_batch("DELETE FROM items; DELETE FROM copies;")?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        if e.log.is_some() {
            return Ok(());
        }
        if e.kind == "library.copied" {
            return apply_copied(tx, e, ts);
        }
        if e.kind != "library.item" {
            return Ok(());
        }
        let Some(it) = parse_item(&e.payload) else {
            return Ok(());
        };
        tx.execute(
            "INSERT OR IGNORE INTO items (event_id, ts, author_key, author_call, item, kind, ext,
                 title, version, size, root, source, sha256, hash_from, tiers, grp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                e.id,
                ts,
                e.author.key,
                e.author.call,
                it.item,
                it.kind.name(),
                it.ext,
                it.title,
                it.version,
                it.size as i64,
                it.root,
                it.source,
                it.sha256,
                it.hash_from,
                serde_json::to_string(&it.tiers)?,
                it.group,
            ],
        )?;
        Ok(())
    }
}

const ITEM_COLS: &str = "event_id, ts, author_call, item, kind, ext, title, version, size, root, \
                         source, sha256, hash_from, tiers, grp";

fn item_row(r: &rusqlite::Row) -> rusqlite::Result<Item> {
    let kind: String = r.get(4)?;
    let tiers: String = r.get(13)?;
    Ok(Item {
        event_id: r.get(0)?,
        ts: r.get(1)?,
        call: r.get(2)?,
        item: r.get(3)?,
        kind: Kind::parse(&kind).unwrap_or(Kind::Doc),
        ext: r.get(5)?,
        title: r.get(6)?,
        version: r.get(7)?,
        size: r.get::<_, i64>(8)? as u64,
        root: r.get(9)?,
        source: r.get(10)?,
        sha256: r.get(11)?,
        hash_from: r.get(12)?,
        tiers: serde_json::from_str(&tiers).unwrap_or_default(),
        group: r.get(14)?,
    })
}

/// The newest published version of every item.
pub fn latest(conn: &Connection) -> Result<Vec<Item>> {
    let mut st = conn.prepare(&format!(
        "SELECT {ITEM_COLS} FROM items i
         WHERE NOT EXISTS (SELECT 1 FROM items j WHERE j.item = i.item
                           AND (j.ts > i.ts OR (j.ts = i.ts AND j.event_id > i.event_id)))
         ORDER BY kind, title"
    ))?;
    let rows = st.query_map([], item_row)?.collect::<Result<_, _>>()?;
    Ok(rows)
}

/// The event that published this version of an item. Two items can share a root (the
/// same file published twice), so both are needed.
pub fn published(conn: &Connection, item: &str, root: &str) -> Result<Option<Item>> {
    let mut st = conn.prepare(&format!(
        "SELECT {ITEM_COLS} FROM items WHERE item = ?1 AND root = ?2 ORDER BY ts DESC LIMIT 1"
    ))?;
    Ok(st.query_map([item, root], item_row)?.next().transpose()?)
}

/// How many different trusted stations published this exact file (same blob, same SHA-256)
/// for this item. Each fetched it from the internet on its own.
pub fn confirmations(conn: &Connection, it: &Item) -> Result<u32> {
    Ok(conn.query_row(
        "SELECT count(DISTINCT author_key) FROM items
         WHERE item = ?1 AND root = ?2 AND coalesce(sha256, '') = coalesce(?3, '')",
        params![it.item, it.root, it.sha256],
        |r| r.get(0),
    )?)
}

/// A site's latest heartbeat.
#[derive(Clone, Debug, Default)]
pub struct Site {
    pub key: String,
    pub call: String,
    pub node: String,
    pub ts: i64,
    pub tier: String,
    pub fetches: Vec<String>,
    pub free_mb: Option<i64>,
    /// the slowest an NWS alert reached this site in the last week, and how many arrived
    pub alert_delay_ms: Option<i64>,
    pub alerts_seen: Option<i64>,
    /// when kiwix-serve last answered at that site
    pub kiwix_ok_ms: Option<i64>,
    /// how other cores reach that site's core
    pub core_url: Option<String>,
    pub nws_place: Option<String>,
    pub nws_ok_ms: Option<i64>,
    pub nws_error: Option<String>,
    pub nws_active: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hold {
    pub key: String,
    pub item: String,
    pub version: String,
    pub root: String,
    /// "serving", "fetching", "failed" or "skipped"
    pub state: String,
    pub detail: Option<String>,
}

/// A copy a site reported in a lasting `library.copied` event.
#[derive(Clone, Debug, PartialEq)]
pub struct Copy {
    pub key: String,
    pub call: String,
    pub node: String,
    pub item: String,
    pub version: String,
    pub root: String,
    pub ts: i64,
}

fn int(v: Option<&Value>, k: &str) -> Option<i64> {
    v.and_then(|v| v.get(k)).and_then(Value::as_i64)
}

impl Projection for Sites {
    fn schema(&self) -> &'static str {
        SITES_SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute_batch("DELETE FROM sites; DELETE FROM site_holds;")?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        if e.kind != "library.site" {
            return Ok(());
        }
        let p = &e.payload;
        let node = norm_node(s(p, "node").unwrap_or(""));
        let node = if valid_node(&node) {
            node
        } else {
            String::new()
        };
        // heartbeats can arrive out of order; only a newer one replaces what we know
        let newer: bool = tx.query_row(
            "SELECT NOT EXISTS (SELECT 1 FROM sites WHERE author_key = ?1 AND ts > ?2)",
            params![e.author.key, ts],
            |r| r.get(0),
        )?;
        if !newer {
            return Ok(());
        }
        let fetches: Vec<String> = strings(p.get("fetches"), MAX_HOLDS, 40)
            .into_iter()
            .filter(|i| valid_item(i))
            .collect();
        let tier = s(p, "tier").filter(|t| valid_item(t)).unwrap_or("");
        let alerts = p.get("alerts").filter(|a| a.is_object());
        let nws = p.get("nws").filter(|n| n.is_object());
        let place = nws.and_then(|n| cap(s(n, "place"), 100));
        let active = strings(nws.and_then(|n| n.get("active")), MAX_ACTIVE, 200);
        tx.execute(
            "INSERT OR REPLACE INTO sites (author_key, call, node, ts, tier, fetches, free_mb,
                 alert_delay_ms, alerts_seen, kiwix_ok_ms, core_url, nws_place, nws_ok_ms,
                 nws_error, nws_active)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                e.author.key,
                e.author.call,
                node,
                ts,
                tier,
                serde_json::to_string(&fetches)?,
                p.get("free_mb").and_then(Value::as_i64),
                int(alerts, "delay_ms"),
                int(alerts, "count"),
                p.get("kiwix_ok_ms").and_then(Value::as_i64),
                s(p, "core_url").and_then(peer_url),
                place,
                // a fast clock must not keep a dead fetcher looking alive
                int(nws, "ok_ms").map(|ok| ok.min(ts)),
                nws.and_then(|n| cap(s(n, "error"), 200)),
                place
                    .as_ref()
                    .map(|_| serde_json::to_string(&active))
                    .transpose()?,
            ],
        )?;
        tx.execute(
            "DELETE FROM site_holds WHERE author_key = ?1",
            params![e.author.key],
        )?;
        for h in p
            .get("holds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(MAX_HOLDS)
        {
            let (Some(item), Some(version), Some(root), Some(state)) = (
                s(h, "item").filter(|i| valid_item(i)),
                s(h, "version").filter(|v| valid_version(v)),
                s(h, "root").filter(|r| is_hash(r)),
                s(h, "state")
                    .filter(|x| matches!(*x, "serving" | "fetching" | "failed" | "skipped")),
            ) else {
                continue;
            };
            tx.execute(
                "INSERT OR REPLACE INTO site_holds (author_key, item, version, root, state, detail)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    e.author.key,
                    item,
                    version,
                    root,
                    state,
                    cap(s(h, "detail"), 200)
                ],
            )?;
        }
        Ok(())
    }
}

pub fn sites(conn: &Connection) -> Result<Vec<Site>> {
    let mut st = conn.prepare(
        "SELECT author_key, call, node, ts, tier, fetches, free_mb, alert_delay_ms, alerts_seen,
                kiwix_ok_ms, nws_place, nws_ok_ms, nws_error, nws_active, core_url
         FROM sites ORDER BY node, call",
    )?;
    let rows = st
        .query_map([], |r| {
            let fetches: String = r.get(5)?;
            let active: Option<String> = r.get(13)?;
            Ok(Site {
                key: r.get(0)?,
                call: r.get(1)?,
                node: r.get(2)?,
                ts: r.get(3)?,
                tier: r.get(4)?,
                fetches: serde_json::from_str(&fetches).unwrap_or_default(),
                free_mb: r.get(6)?,
                alert_delay_ms: r.get(7)?,
                alerts_seen: r.get(8)?,
                kiwix_ok_ms: r.get(9)?,
                core_url: r.get(14)?,
                nws_place: r.get(10)?,
                nws_ok_ms: r.get(11)?,
                nws_error: r.get(12)?,
                nws_active: active
                    .and_then(|a| serde_json::from_str(&a).ok())
                    .unwrap_or_default(),
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

pub fn holds(conn: &Connection) -> Result<Vec<Hold>> {
    let mut st = conn.prepare(
        "SELECT author_key, item, version, root, state, detail FROM site_holds ORDER BY item",
    )?;
    let rows = st
        .query_map([], |r| {
            Ok(Hold {
                key: r.get(0)?,
                item: r.get(1)?,
                version: r.get(2)?,
                root: r.get(3)?,
                state: r.get(4)?,
                detail: r.get(5)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

pub fn copies(conn: &Connection) -> Result<Vec<Copy>> {
    let mut st = conn.prepare(
        "SELECT author_key, call, node, item, version, root, ts FROM copies ORDER BY item",
    )?;
    let rows = st
        .query_map([], |r| {
            Ok(Copy {
                key: r.get(0)?,
                call: r.get(1)?,
                node: r.get(2)?,
                item: r.get(3)?,
                version: r.get(4)?,
                root: r.get(5)?,
                ts: r.get(6)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

/// A station known to hold a version.
#[derive(Clone, Debug, PartialEq)]
pub struct Holder {
    pub key: String,
    pub call: String,
    pub node: String,
    /// its core's address, if its heartbeat said
    pub core_url: Option<String>,
    /// its last heartbeat; None if it is known only from lasting records
    pub ts: Option<i64>,
}

/// Every station known to hold this exact version: serving it by its heartbeat, having
/// reported a copy, or having published it.
pub fn holders(conn: &Connection, item: &str, root: &str) -> Result<Vec<Holder>> {
    let mut st = conn.prepare(
        "SELECT author_key, call, node, core_url, ts FROM sites WHERE author_key IN (
             SELECT author_key FROM site_holds WHERE item = ?1 AND root = ?2 AND state = 'serving'
             UNION SELECT author_key FROM items WHERE item = ?1 AND root = ?2
             UNION SELECT author_key FROM copies WHERE item = ?1 AND root = ?2)
         UNION
         SELECT author_key, call, node, NULL, NULL FROM copies
         WHERE item = ?1 AND root = ?2 AND author_key NOT IN (SELECT author_key FROM sites)
         UNION
         SELECT author_key, author_call, '', NULL, NULL FROM items
         WHERE item = ?1 AND root = ?2 AND author_key NOT IN (SELECT author_key FROM sites)
           AND author_key NOT IN (SELECT author_key FROM copies WHERE item = ?1 AND root = ?2)",
    )?;
    let rows = st
        .query_map([item, root], |r| {
            Ok(Holder {
                key: r.get(0)?,
                call: r.get(1)?,
                node: r.get(2)?,
                core_url: r.get(3)?,
                ts: r.get(4)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

/// Nodes of the sites known to hold this exact version: serving it by their heartbeat, or
/// having reported a copy.
pub fn holder_nodes(conn: &Connection, item: &str, root: &str) -> Result<Vec<String>> {
    let mut st = conn.prepare(
        "SELECT s.node FROM site_holds h JOIN sites s ON s.author_key = h.author_key
         WHERE h.item = ?1 AND h.root = ?2 AND h.state = 'serving' AND s.node != ''
         UNION
         SELECT node FROM copies WHERE item = ?1 AND root = ?2 AND node != ''
         UNION
         SELECT s.node FROM items i JOIN sites s ON s.author_key = i.author_key
         WHERE i.item = ?1 AND i.root = ?2 AND s.node != ''",
    )?;
    let rows = st
        .query_map([item, root], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

/// Can this site fetch alerts for `place`? It must have sent a heartbeat within `quiet_ms`,
/// and either had a good poll within `quiet_ms` or not have tried yet (a standby). A
/// fetcher that lost its internet keeps sending heartbeats, but its polls stop working, so
/// it stops counting.
pub fn nws_live(s: &Site, place: &str, now: i64, quiet_ms: i64) -> bool {
    let polling = match s.nws_ok_ms {
        Some(ok) => now - ok <= quiet_ms,
        None => s.nws_error.is_none(),
    };
    s.nws_place.as_deref() == Some(place) && now - s.ts <= quiet_ms && polling
}

/// Should this site fetch NWS alerts for `place` right now? Among live fetchers for the same
/// place, the lowest key does it. When it goes quiet (down, cut off by a split, or its
/// internet gone), the next lowest takes over; each side of a split has its own fetcher.
/// Our own heartbeat may not be back from the core yet, so we always count ourselves.
pub fn nws_turn(sites: &[Site], me: &str, place: &str, now: i64, quiet_ms: i64) -> bool {
    !sites
        .iter()
        .any(|s| s.key.as_str() < me && nws_live(s, place, now, quiet_ms))
}
#[cfg(test)]
mod tests {
    use super::*;
    use ridgewarden_client::mesh::testing::Fake;
    use serde_json::json;

    fn fake() -> Fake {
        Fake::new(vec![Box::new(Items), Box::new(Sites)])
    }

    fn item(version: &str, root: &str) -> Value {
        json!({"item": "wiki", "kind": "zim", "title": "Wikipedia", "version": version,
               "size": 760553, "root": root, "source": "https://example.org/w.zim",
               "sha256": "9a".repeat(32), "hash_from": "https://example.org/w.zim.sha256"})
    }

    #[test]
    fn newest_version_wins() {
        let mut f = fake();
        f.add("library.item", item("2026-08", &"a".repeat(64)), 1000);
        f.add("library.item", item("2026-10", &"b".repeat(64)), 3000);
        f.add("library.item", item("2026-09", &"c".repeat(64)), 2000);
        let l = latest(&f.conn).unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].version, "2026-10");
        assert_eq!(
            l[0].hash_from.as_deref(),
            Some("https://example.org/w.zim.sha256")
        );
        assert_eq!(
            published(&f.conn, "wiki", &"c".repeat(64))
                .unwrap()
                .unwrap()
                .version,
            "2026-09"
        );
    }

    #[test]
    fn malformed_items_are_ignored() {
        let mut f = fake();
        let mut bad = item("2026-08", &"a".repeat(64));
        bad["item"] = json!("../../etc");
        f.add("library.item", bad, 1);
        let mut bad = item("2026-08", "not-a-hash");
        bad["item"] = json!("x");
        f.add("library.item", bad, 1);
        let mut bad = item("../x", &"a".repeat(64));
        bad["item"] = json!("y");
        f.add("library.item", bad, 1);
        assert!(latest(&f.conn).unwrap().is_empty());
        // a document must say which allowed type it is
        let mut doc = item("2026-08", &"a".repeat(64));
        doc["kind"] = json!("doc");
        doc["ext"] = json!("html");
        f.add("library.item", doc.clone(), 1);
        assert!(latest(&f.conn).unwrap().is_empty());
        doc["ext"] = json!("pdf");
        f.add("library.item", doc, 1);
        assert_eq!(latest(&f.conn).unwrap()[0].ext, "pdf");
    }

    /// Run an event signed by another station (any key will do for the projection).
    fn from(f: &mut Fake, key: char, kind: &str, payload: Value, ts: i64) {
        let mut e = Fake::new(vec![]).add(kind, payload, ts);
        e.author.key = key.to_string().repeat(64);
        e.id = format!("{key}{ts}");
        let tx = f.conn.transaction().unwrap();
        Items.apply(&tx, &e, ts).unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn independent_fetchers_confirm_a_file() {
        let mut f = fake();
        let root = "a".repeat(64);
        f.add("library.item", item("2026-08", &root), 1000);
        let it = latest(&f.conn).unwrap().remove(0);
        assert_eq!(confirmations(&f.conn, &it).unwrap(), 1);
        // the same station saying it twice is still one
        f.add("library.item", item("2026-08", &root), 1500);
        assert_eq!(confirmations(&f.conn, &it).unwrap(), 1);
        // another station claiming a different hash for the same blob doesn't count
        let mut lie = item("2026-08", &root);
        lie["sha256"] = json!("00".repeat(32));
        from(&mut f, 'b', "library.item", lie, 2000);
        assert_eq!(confirmations(&f.conn, &it).unwrap(), 1);
        from(&mut f, 'c', "library.item", item("2026-08", &root), 2100);
        assert_eq!(confirmations(&f.conn, &it).unwrap(), 2);
    }

    #[test]
    fn peer_addresses_are_plain_core_urls() {
        assert_eq!(
            peer_url("http://TST-Paso-Pi.local.mesh:8790/").as_deref(),
            Some("http://tst-paso-pi.local.mesh:8790")
        );
        assert_eq!(
            peer_url("http://10.1.2.3").as_deref(),
            Some("http://10.1.2.3")
        );
        for bad in [
            "https://x:8790",
            "http://x:8790/path",
            "http://user@x:8790",
            "http://x:99999",
            "http://<x>",
            "ftp://x",
            "http://x:+8790",
            "http://999.1.1.1",
            "http://1.2.3.4.5",
        ] {
            assert_eq!(peer_url(bad), None, "{bad}");
        }
    }

    #[test]
    fn copies_and_holders() {
        let mut f = fake();
        let r = "a".repeat(64);
        f.add("library.item", item("2026-08", &r), 1000);
        f.add(
            "library.copied",
            json!({"item": "wiki", "version": "2026-08", "root": r, "node": "TST-Paso"}),
            2000,
        );
        f.add(
            "library.copied",
            json!({"item": "wiki", "version": "2026-01", "root": "b".repeat(64), "node": "tst-paso"}),
            1500,
        );
        let c = copies(&f.conn).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(
            c[0].version, "2026-08",
            "an older record arriving late changes nothing"
        );
        assert_eq!(holder_nodes(&f.conn, "wiki", &r).unwrap(), ["tst-paso"]);
    }

    fn beat(node: &str, holds: Value, nws: Value) -> Value {
        json!({"node": node, "fetches": ["wiki"], "holds": holds, "nws": nws})
    }

    #[test]
    fn heartbeats_replace_holdings_and_ignore_stale_ones() {
        let mut f = fake();
        let r = "a".repeat(64);
        f.add(
            "library.site",
            beat(
                "TST-Cuesta",
                json!([{"item": "wiki", "version": "2026-08", "root": r, "state": "serving"}]),
                Value::Null,
            ),
            2000,
        );
        // an older heartbeat arriving late changes nothing
        f.add(
            "library.site",
            beat(
                "tst-cuesta",
                json!([{"item": "wiki", "version": "2026-01", "root": r, "state": "failed"}]),
                Value::Null,
            ),
            1000,
        );
        let h = holds(&f.conn).unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(
            (h[0].version.as_str(), h[0].state.as_str()),
            ("2026-08", "serving")
        );
        let s = sites(&f.conn).unwrap();
        assert_eq!(s[0].node, "tst-cuesta");
        assert_eq!(s[0].ts, 2000);
        f.add(
            "library.site",
            beat("tst-cuesta", json!([]), Value::Null),
            3000,
        );
        assert!(holds(&f.conn).unwrap().is_empty());
    }

    fn site(key: &str, place: Option<&str>, ts: i64) -> Site {
        Site {
            key: key.into(),
            call: "N0TEST".into(),
            ts,
            nws_place: place.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn lowest_live_fetcher_takes_alerts() {
        let q = 900_000;
        let now = 10_000_000;
        let sites = vec![
            site("aa", Some("area=CA"), now - 60_000),
            site("cc", Some("area=CA"), now - 60_000),
            site("00", None, now),
            site("01", Some("zone=CAZ034"), now),
        ];
        assert!(nws_turn(&sites, "aa", "area=CA", now, q));
        assert!(!nws_turn(&sites, "bb", "area=CA", now, q));
        // aa goes quiet: bb takes over without anyone deciding anything
        let mut quiet = sites.clone();
        quiet[0].ts = now - q - 1;
        assert!(nws_turn(&quiet, "bb", "area=CA", now, q));
        assert!(!nws_turn(&quiet, "dd", "area=CA", now, q));
    }

    #[test]
    fn a_fetcher_that_lost_its_internet_hands_over() {
        let q = 900_000;
        let now = 10_000_000;
        let mut aa = site("aa", Some("area=CA"), now - 60_000);
        aa.nws_ok_ms = Some(now - 100_000);
        assert!(!nws_turn(&[aa.clone()], "bb", "area=CA", now, q));
        // still heartbeating, but its polls have failed for longer than quiet_s
        aa.nws_ok_ms = Some(now - q - 1);
        aa.nws_error = Some("could not connect".into());
        assert!(nws_turn(&[aa.clone()], "bb", "area=CA", now, q));
        // one failed poll after a recent good one is not enough
        aa.nws_ok_ms = Some(now - 60_000);
        assert!(!nws_turn(&[aa.clone()], "bb", "area=CA", now, q));
        // never polled and already failing: not live either
        aa.nws_ok_ms = None;
        assert!(nws_turn(&[aa], "bb", "area=CA", now, q));
    }
}
