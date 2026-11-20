//! New versions carried to other islands by data-mule cars (ridgewarden-mule).
//!
//! A fetcher with `[mule] to` set logs a `mule.msg` for each new version, addressed to each
//! of those islands, with the version's blob as its file. The mule app plans it as bulk
//! cargo; cars record `mule.pickup` and `mule.dropoff`, and the far island signs a
//! `mule.receipt`. This view follows those records for the messages this app wrote, so the
//! catalog can say where each update is on the road.

use anyhow::Result;
use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use ridgewarden_client::mesh::s;
use ridgewarden_client::util::valid_node;
use rusqlite::{Connection, Transaction, params};
use serde_json::{Value, json};

use crate::catalog::{Item, cap, is_hash};
use crate::content::{file_name, valid_item, valid_version};

/// Marks the mule messages this app wrote, so it follows only those.
pub const APP: &str = "ridgewarden-library";

pub struct Mule;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS mule_msgs (
    id      TEXT PRIMARY KEY,
    item    TEXT NOT NULL,
    version TEXT NOT NULL,
    island  TEXT NOT NULL,
    ts      INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS mule_hops (
    msg    TEXT NOT NULL,
    kind   TEXT NOT NULL,
    call   TEXT NOT NULL,
    island TEXT,
    ts     INTEGER NOT NULL,
    PRIMARY KEY (msg, kind, call)
);
"#;

/// The `mule.msg` that sends `it` (just published here, with this call) to `island`.
pub fn message(it: &Item, call: &str, from: &str, island: &str) -> Value {
    json!({
        "app": APP,
        "item": it.item,
        "version": it.version,
        "from_island": from,
        "to": island,
        "from": call,
        "class": "bulk",
        "text": format!(
            "Offline library update: {} version {} ({} MB). The library app at each site takes it from here.",
            it.title, it.version, it.size.div_ceil(1_000_000)
        ),
        "file": {
            "root": it.root,
            "name": file_name(&it.item, &it.version, &it.root, &it.ext),
            "size": it.size,
        },
    })
}

impl Projection for Mule {
    fn schema(&self) -> &'static str {
        SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute_batch("DELETE FROM mule_msgs; DELETE FROM mule_hops;")?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        if e.log.is_some() {
            return Ok(());
        }
        let p = &e.payload;
        match e.kind.as_str() {
            "mule.msg" if s(p, "app") == Some(APP) => {
                let (Some(item), Some(version), Some(island)) = (
                    s(p, "item").filter(|i| valid_item(i)),
                    s(p, "version").filter(|v| valid_version(v)),
                    s(p, "to").filter(|i| valid_node(i)),
                ) else {
                    return Ok(());
                };
                tx.execute(
                    "INSERT OR IGNORE INTO mule_msgs (id, item, version, island, ts) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![e.id, item, version, island, ts],
                )?;
            }
            "mule.pickup" | "mule.dropoff" | "mule.receipt" => {
                let Some(msg) = s(p, "msg").filter(|m| is_hash(m)) else {
                    return Ok(());
                };
                tx.execute(
                    "INSERT OR IGNORE INTO mule_hops (msg, kind, call, island, ts)
                     SELECT ?1, ?2, ?3, ?4, ?5 WHERE EXISTS (SELECT 1 FROM mule_msgs WHERE id = ?1)",
                    params![
                        msg,
                        &e.kind[5..],
                        e.author.call,
                        cap(s(p, "island"), 63),
                        ts
                    ],
                )?;
            }
            _ => {}
        }
        Ok(())
    }
}

/// Where one version sent to one island is now.
#[derive(Clone, Debug, PartialEq)]
pub struct Trip {
    pub item: String,
    pub version: String,
    pub island: String,
    pub sent_ms: i64,
    /// the last thing that happened: ("pickup" | "dropoff" | "receipt", who, when)
    pub last: Option<(String, String, i64)>,
}

pub fn trips(conn: &Connection) -> Result<Vec<Trip>> {
    let mut st = conn.prepare(
        "SELECT m.item, m.version, m.island, m.ts,
                (SELECT kind || ' ' || call || ' ' || ts FROM mule_hops h WHERE h.msg = m.id
                 ORDER BY CASE kind WHEN 'receipt' THEN 3 WHEN 'dropoff' THEN 2 ELSE 1 END DESC, ts DESC
                 LIMIT 1)
         FROM mule_msgs m ORDER BY m.ts DESC LIMIT 100",
    )?;
    let rows = st
        .query_map([], |r| {
            let last: Option<String> = r.get(4)?;
            Ok(Trip {
                item: r.get(0)?,
                version: r.get(1)?,
                island: r.get(2)?,
                sent_ms: r.get(3)?,
                last: last.and_then(|l| {
                    let mut w = l.split(' ');
                    Some((w.next()?.into(), w.next()?.into(), w.next()?.parse().ok()?))
                }),
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::Kind;
    use ridgewarden_client::mesh::testing::Fake;

    fn item() -> Item {
        Item {
            event_id: String::new(),
            ts: 0,
            call: "N0AAA".into(),
            item: "wiki".into(),
            kind: Kind::Zim,
            ext: "zim".into(),
            title: "Wikipedia".into(),
            version: "2026-08".into(),
            size: 2_500_000,
            root: "a".repeat(64),
            source: String::new(),
            sha256: None,
            hash_from: None,
            tiers: vec![],
            group: String::new(),
        }
    }

    #[test]
    fn follows_its_own_messages_to_the_receipt() {
        let mut f = Fake::new(vec![Box::new(Mule)]);
        let msg = f.add(
            "mule.msg",
            message(&item(), "N0AAA", "north", "south"),
            1000,
        );
        // someone else's mule traffic is not ours to show
        let other = f.add(
            "mule.msg",
            json!({"from_island": "north", "to": "south", "from": "N0AAA", "class": "urgent", "text": "hi"}),
            1100,
        );
        f.add(
            "mule.pickup",
            json!({"msg": other.id, "operator": "N0DRV"}),
            1200,
        );
        f.add(
            "mule.pickup",
            json!({"msg": msg.id, "operator": "N0DRV"}),
            2000,
        );
        let t = trips(&f.conn).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].last.as_ref().unwrap().0, "pickup");
        f.add(
            "mule.receipt",
            json!({"msg": msg.id, "island": "south"}),
            9000,
        );
        f.add(
            "mule.dropoff",
            json!({"msg": msg.id, "operator": "N0DRV", "island": "south"}),
            8000,
        );
        let t = trips(&f.conn).unwrap();
        assert_eq!(
            t[0].last.as_ref().unwrap(),
            &("receipt".into(), "N0TEST".into(), 9000)
        );
        let m = message(&item(), "N0AAA", "north", "south");
        assert_eq!(m["file"]["name"], "wiki_2026-08_aaaaaaaa.zim");
        assert_eq!(m["class"], "bulk");
    }
}
