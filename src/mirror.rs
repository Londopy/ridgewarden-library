//! Copying published content to this site, and swapping it in.
//!
//! For each item's newest version this site should hold: check it is confirmed, the
//! manifest's size matches the event's, it fits the budget and the disk, and no library
//! site on the best path to a copy is still fetching it; then ask the core to want the
//! blob, wait until the core says it is complete, have the core export it, check it (the
//! right kind of file, and the SHA-256 from the signed `library.item` event), and only
//! then make it the copy that is served. Until that moment the old copy keeps serving.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ridgewarden_client::util::now_ms;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use crate::app::App;
use crate::catalog::{self, Item};
use crate::config::Config;
use crate::content::{Kind, check_file, file_name};
use crate::{disk, fanout};

/// The copy this site serves for an item.
#[derive(Clone, Debug, PartialEq)]
pub struct Held {
    pub item: String,
    pub kind: Kind,
    pub ext: String,
    pub version: String,
    pub root: String,
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
    /// "publisher" (matched the publisher's hash), "fetcher" (the fetcher's own hash; the
    /// publisher gives none) or "blob" (only the blob store's chunk hashes)
    pub verified: String,
    /// when the `library.item` event this copy belongs to was written
    pub event_ts: i64,
    pub since_ms: i64,
    /// when the file was last read back and found whole
    pub checked_ms: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    pub item: String,
    pub version: String,
    pub root: String,
    /// "waiting" (see detail), "fetching", "failed" or "skipped"
    pub state: String,
    pub detail: Option<String>,
    pub since_ms: i64,
    pub size: u64,
    pub attempts: i64,
    /// don't try again before this
    pub next_ms: i64,
}

const HELD_COLS: &str =
    "item, kind, ext, version, root, path, size, sha256, verified, event_ts, since_ms, checked_ms";

fn held_row(r: &rusqlite::Row) -> rusqlite::Result<Held> {
    let kind: String = r.get(1)?;
    let path: String = r.get(5)?;
    Ok(Held {
        item: r.get(0)?,
        kind: Kind::parse(&kind).unwrap_or(Kind::Doc),
        ext: r.get(2)?,
        version: r.get(3)?,
        root: r.get(4)?,
        path: PathBuf::from(path),
        size: r.get::<_, i64>(6)? as u64,
        sha256: r.get(7)?,
        verified: r.get(8)?,
        event_ts: r.get(9)?,
        since_ms: r.get(10)?,
        checked_ms: r.get(11)?,
    })
}

pub fn held(conn: &Connection) -> Result<Vec<Held>> {
    let mut st = conn.prepare(&format!("SELECT {HELD_COLS} FROM held ORDER BY kind, item"))?;
    Ok(st.query_map([], held_row)?.collect::<Result<_, _>>()?)
}

pub fn held_one(conn: &Connection, item: &str) -> Result<Option<Held>> {
    Ok(conn
        .query_row(
            &format!("SELECT {HELD_COLS} FROM held WHERE item = ?1"),
            [item],
            held_row,
        )
        .optional()?)
}

pub fn pending(conn: &Connection) -> Result<Vec<Pending>> {
    let mut st = conn.prepare(
        "SELECT item, version, root, state, detail, since_ms, size, attempts, next_ms
         FROM pending ORDER BY item",
    )?;
    Ok(st
        .query_map([], |r| {
            Ok(Pending {
                item: r.get(0)?,
                version: r.get(1)?,
                root: r.get(2)?,
                state: r.get(3)?,
                detail: r.get(4)?,
                since_ms: r.get(5)?,
                size: r.get::<_, i64>(6)? as u64,
                attempts: r.get(7)?,
                next_ms: r.get(8)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

/// Record where copying `it` stands. A new root starts the clock and the tries over.
fn set_pending(conn: &Connection, it: &Item, state: &str, detail: Option<&str>) -> Result<()> {
    conn.execute(
        "INSERT INTO pending (item, version, root, state, detail, since_ms, size) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(item) DO UPDATE SET state = excluded.state, detail = excluded.detail,
             since_ms = CASE WHEN pending.root = excluded.root THEN pending.since_ms ELSE excluded.since_ms END,
             attempts = CASE WHEN pending.root = excluded.root THEN pending.attempts ELSE 0 END,
             sources = CASE WHEN pending.root = excluded.root THEN pending.sources ELSE NULL END,
             next_ms = CASE WHEN pending.root = excluded.root THEN pending.next_ms ELSE 0 END,
             version = excluded.version, root = excluded.root, size = excluded.size",
        params![it.item, it.version, it.root, state, detail, now_ms(), it.size as i64],
    )?;
    Ok(())
}

/// Record a failed step, keeping the state it was in, and when to try again.
fn note_error(conn: &Connection, it: &Item, detail: &str, retry_ms: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO pending (item, version, root, state, detail, since_ms, size, next_ms)
         VALUES (?1, ?2, ?3, 'waiting', ?4, ?5, ?6, ?7)
         ON CONFLICT(item) DO UPDATE SET detail = excluded.detail, next_ms = excluded.next_ms
         WHERE pending.root = excluded.root",
        params![
            it.item,
            it.version,
            it.root,
            detail,
            now_ms(),
            it.size as i64,
            retry_ms
        ],
    )?;
    Ok(())
}

/// After `attempts` failed exports: wait 5 minutes, doubling up to 6 hours.
pub fn backoff_ms(attempts: i64) -> i64 {
    ((5 * 60_000i64) << attempts.clamp(0, 10)).min(6 * 3_600_000)
}

/// Put a replaced or released copy aside. It is deleted once nothing may still use it.
fn retire(conn: &Connection, h: &Held) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO retired (path, kind, since_ms) VALUES (?1, ?2, ?3)",
        params![h.path.to_string_lossy(), h.kind.name(), now_ms()],
    )?;
    Ok(())
}

/// Make `h` the copy this site serves. The copy it replaces is retired, and its blob
/// let go so the core can free the space. A mirror says so in a lasting
/// `library.copied` event, which reaches sites that never see its heartbeats.
pub async fn install(app: &App, h: &Held, announce: bool) -> Result<()> {
    let old = {
        let db = app.db();
        let old = held_one(&db, &h.item)?;
        db.execute(
            &format!(
                "INSERT OR REPLACE INTO held ({HELD_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
            ),
            params![
                h.item,
                h.kind.name(),
                h.ext,
                h.version,
                h.root,
                h.path.to_string_lossy(),
                h.size as i64,
                h.sha256,
                h.verified,
                h.event_ts,
                h.since_ms,
                h.checked_ms
            ],
        )?;
        db.execute("DELETE FROM pending WHERE item = ?1", [&h.item])?;
        db.execute(
            "DELETE FROM retired WHERE path = ?1",
            [h.path.to_string_lossy()],
        )?;
        if let Some(o) = old.as_ref().filter(|o| o.path != h.path) {
            retire(&db, o)?;
        }
        old
    };
    tracing::info!(
        "{} {} is now served from {}",
        h.item,
        h.version,
        h.path.display()
    );
    app.changed.notify_one();
    if h.kind == Kind::Zim {
        app.zims_changed.notify_one();
    }
    if let Some(o) = old.filter(|o| o.root != h.root) {
        unwant(app, &o.root).await;
    }
    if announce {
        let node = app.whoami().await.map(|w| w.node).unwrap_or_default();
        let ev = json!({"item": h.item, "version": h.version, "root": h.root, "node": node});
        if let Err(e) = app.core.append("library.copied", ev).await {
            tracing::warn!("logging the copy of {} {}: {e:#}", h.item, h.version);
        }
    }
    Ok(())
}

/// Tell the core this site no longer needs a blob, unless something it serves uses it.
async fn unwant(app: &App, root: &str) {
    let used = held(&app.db()).is_ok_and(|h| h.iter().any(|h| h.root == root));
    if used {
        return;
    }
    match app.core.blob_unwant(root).await {
        Ok(freed) => {
            tracing::info!("released blob {} ({} MB)", &root[..12], freed / 1_000_000);
            let _ = app
                .db()
                .execute("DELETE FROM wanted WHERE root = ?1", [root]);
        }
        Err(e) => tracing::warn!("releasing blob {}: {e:#}", &root[..12]),
    }
}

/// Let go of every blob this site asked for and no longer needs: superseded by a newer
/// version, refused, failed its check, or replaced.
async fn release_unneeded(app: &App) -> Result<()> {
    let roots: Vec<String> = app
        .db()
        .prepare(
            "SELECT root FROM wanted w WHERE NOT EXISTS (SELECT 1 FROM held h WHERE h.root = w.root)
             AND NOT EXISTS (SELECT 1 FROM pending p WHERE p.root = w.root
                             AND p.state IN ('fetching', 'waiting'))",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for r in roots {
        unwant(app, &r).await;
    }
    Ok(())
}

/// Stop serving an item this site no longer should (another of its group took over).
async fn release(app: &App, h: &Held) -> Result<()> {
    {
        let db = app.db();
        db.execute("DELETE FROM held WHERE item = ?1", [&h.item])?;
        retire(&db, h)?;
    }
    tracing::info!("{} {} is no longer served here", h.item, h.version);
    app.changed.notify_one();
    if h.kind == Kind::Zim {
        app.zims_changed.notify_one();
    }
    unwant(app, &h.root).await;
    Ok(())
}

/// Delete retired copies nothing can still be using: kiwix-serve keeps its last working
/// set of ZIM files until a new set has run for a minute.
pub fn sweep(app: &App) -> Result<()> {
    let kiwix = !app.cfg.kiwix.command.is_empty();
    let in_use = if kiwix { app.kiwix.in_use() } else { vec![] };
    // after a restart kiwix-serve has not proven anything yet; keep old ZIMs until it has
    let zims: Vec<PathBuf> = held(&app.db())?
        .into_iter()
        .filter(|h| h.kind == Kind::Zim)
        .map(|h| h.path)
        .collect();
    let settled = !kiwix || app.kiwix.settled(&zims);
    let rows: Vec<(String, String)> = app
        .db()
        .prepare("SELECT path, kind FROM retired")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (p, kind) in rows {
        let path = PathBuf::from(&p);
        if in_use.contains(&path) || (kind == "zim" && !settled) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::info!("removed the old copy {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!("removing the old copy {}: {e}", path.display());
                continue;
            }
        }
        app.db()
            .execute("DELETE FROM retired WHERE path = ?1", [&p])?;
    }
    Ok(())
}

/// What to do about one published item, given what this site has.
#[derive(Debug, PartialEq)]
pub enum Next {
    Nothing,
    /// this site doesn't copy it, and why
    Skip(String),
    /// another item of its group took over here; stop serving this one
    Release,
    Want,
    /// the core is fetching; see whether it's done
    Check,
}

/// The item of `it`'s group this site should hold: the biggest one it doesn't refuse.
pub fn group_choice<'a>(cfg: &Config, it: &Item, items: &'a [Item]) -> Option<&'a Item> {
    if it.group.is_empty() {
        return None;
    }
    items
        .iter()
        .filter(|o| o.group == it.group && cfg.refuses(&o.item, o.size, &o.tiers).is_none())
        .max_by_key(|o| (o.size, o.item.clone()))
}

pub fn next(
    cfg: &Config,
    it: &Item,
    items: &[Item],
    held: &[Held],
    p: Option<&Pending>,
    now: i64,
) -> Next {
    let mine = held.iter().find(|h| h.item == it.item);
    if let Some(why) = cfg.refuses(&it.item, it.size, &it.tiers) {
        // a copy made before the config changed keeps serving
        return if mine.is_some() {
            Next::Nothing
        } else {
            Next::Skip(why)
        };
    }
    if let Some(other) = group_choice(cfg, it, items).filter(|o| o.item != it.item) {
        let taken_over = held.iter().any(|h| h.item == other.item);
        return match mine {
            Some(_) if taken_over => Next::Release,
            Some(_) => Next::Nothing,
            None => Next::Skip(format!("this site holds {} instead", other.title)),
        };
    }
    if let Some(h) = mine
        && (h.root == it.root || it.ts <= h.event_ts)
    {
        return Next::Nothing;
    }
    match p.filter(|p| p.root == it.root) {
        Some(p) if p.state == "failed" => Next::Nothing,
        Some(p) if p.next_ms > now => Next::Nothing,
        Some(p) if p.state == "fetching" => Next::Check,
        _ => Next::Want,
    }
}

/// Bytes this site serves or is copying, leaving out one item (being replaced).
pub fn committed(held: &[Held], pend: &[Pending], except: &str) -> u64 {
    held.iter()
        .filter(|h| h.item != except)
        .map(|h| h.size)
        .chain(
            pend.iter()
                .filter(|p| p.item != except && p.state == "fetching")
                .map(|p| p.size),
        )
        .sum()
}

/// One pass over every published item.
pub async fn round(app: &App) -> Result<()> {
    let items = catalog::latest(&app.db())?;
    let now = now_ms();
    for it in &items {
        // read again for each item, so one just started counts against the next
        let (held, pend) = {
            let db = app.db();
            (held(&db)?, pending(&db)?)
        };
        let p = pend.iter().find(|p| p.item == it.item);
        let r = match next(&app.cfg, it, &items, &held, p, now) {
            Next::Nothing => Ok(()),
            Next::Skip(why) => {
                if p.is_none_or(|p| p.root != it.root || p.state != "skipped") {
                    set_pending(&app.db(), it, "skipped", Some(&why))?;
                    app.changed.notify_one();
                }
                Ok(())
            }
            Next::Release => match held.iter().find(|h| h.item == it.item) {
                Some(h) => release(app, h).await,
                None => Ok(()),
            },
            Next::Want => want(app, it, &held, &pend).await,
            Next::Check => check(app, it).await,
        };
        if let Err(e) = r {
            tracing::warn!("mirroring {} {}: {e:#}", it.item, it.version);
            note_error(&app.db(), it, &format!("{e:#}"), now + 60_000)?;
        }
    }
    release_unneeded(app).await?;
    sweep(app)
}

/// Why this site shouldn't start copying `it` yet, if it shouldn't.
async fn not_yet(app: &App, it: &Item, held: &[Held], pend: &[Pending]) -> Result<Option<String>> {
    let cfg = &app.cfg.mirror;
    let n = catalog::confirmations(&app.db(), it)?;
    if n < cfg.confirmations {
        return Ok(Some(format!(
            "{n} of the {} fetchers mirror.confirmations asks for have published this file",
            cfg.confirmations
        )));
    }
    let others = committed(&[], pend, &it.item);
    let total = committed(held, pend, &it.item) + it.size;
    if total > cfg.total_mb * 1_000_000 {
        return Ok(Some(format!(
            "it would take this site past mirror.total_mb = {}",
            cfg.total_mb
        )));
    }
    let core = app.cfg.core.imports_dir.parent().map(Path::to_path_buf);
    let content = app.cfg.content_dir();
    let (core_free, app_free) = tokio::task::spawn_blocking(move || {
        (core.as_deref().and_then(disk::free), disk::free(&content))
    })
    .await?;
    if let Err(why) = disk::room(
        core_free,
        app_free,
        it.size,
        others,
        cfg.keep_free_mb * 1_000_000,
    ) {
        return Ok(Some(why));
    }
    if cfg.fanout
        && now_ms() - it.ts < cfg.fanout_wait_min as i64 * 60_000
        && let Some(site) = wait_for(app, it).await?
    {
        return Ok(Some(format!(
            "waiting for {} to copy it first; it is on the best path to a copy",
            app.cfg.label(&site)
        )));
    }
    Ok(None)
}

async fn wait_for(app: &App, it: &Item) -> Result<Option<String>> {
    let me = app.whoami().await?.node;
    if me.is_empty() {
        return Ok(None);
    }
    let db = app.db();
    let adj = fanout::links(&db, now_ms() - 3_600_000)?;
    let holders = catalog::holder_nodes(&db, &it.item, &it.root)?;
    // a site that stopped reporting is not worth waiting for
    let sites: Vec<_> = catalog::sites(&db)?
        .into_iter()
        .filter(|s| now_ms() - s.ts <= 3 * crate::HEARTBEAT_S as i64 * 1000)
        .collect();
    let copying: Vec<String> = catalog::holds(&db)?
        .into_iter()
        .filter(|h| h.item == it.item && h.root == it.root && h.state == "fetching")
        .filter_map(|h| {
            sites
                .iter()
                .find(|s| s.key == h.key)
                .map(|s| s.node.clone())
        })
        .filter(|n| !n.is_empty())
        .collect();
    Ok(fanout::wait_for(&adj, &me, &holders, &copying))
}

async fn want(app: &App, it: &Item, held: &[Held], pend: &[Pending]) -> Result<()> {
    if let Some(why) = not_yet(app, it, held, pend).await? {
        set_pending(&app.db(), it, "waiting", Some(&why))?;
        return Ok(());
    }
    // the manifest travels in the log just ahead of the item event; it may lag a round
    let waiting = match app.core.blob_status(&it.root).await? {
        None => Some("waiting for the blob's manifest to arrive"),
        Some(s) if !s.trusted => Some("only an untrusted station's manifest has arrived so far"),
        Some(_) => None,
    };
    if let Some(why) = waiting {
        set_pending(&app.db(), it, "waiting", Some(why))?;
        return Ok(());
    }
    let from = sources(app, it).await.unwrap_or_else(|e| {
        tracing::warn!(
            "{}: no nearest-first list ({e:#}); any peer will do",
            it.item
        );
        vec![]
    });
    ask(app, it, &from).await?;
    app.db().execute(
        "INSERT OR IGNORE INTO wanted (root, ms) VALUES (?1, ?2)",
        params![it.root, now_ms()],
    )?;
    // the core now fetches by the trusted manifest; its size must be the event's
    if let Some(s) = app.core.blob_status(&it.root).await?
        && s.size != it.size
    {
        let why = format!(
            "the blob is {} bytes but the library event says {}; not copying it",
            s.size, it.size
        );
        set_pending(&app.db(), it, "failed", Some(&why))?;
        app.changed.notify_one();
        return Ok(());
    }
    tracing::info!(
        "fetching {} {} from the mesh, best-connected copies first: {}",
        it.item,
        it.version,
        if from.is_empty() {
            "none known".to_string()
        } else {
            from.join(", ")
        }
    );
    set_pending(&app.db(), it, "fetching", None)?;
    save_sources(app, it, &from)?;
    app.changed.notify_one();
    Ok(())
}

/// The cores to fetch a version from first: those of sites that hold it, best path first
/// (see `fanout::best_paths`); the fetcher is usually last. Only cores
/// this site's core already syncs with are named, so a heartbeat can reorder the peers the
/// operator set up but never point the core somewhere new.
async fn sources(app: &App, it: &Item) -> Result<Vec<String>> {
    let me = app.whoami().await?;
    let peers = core_peers(app).await?;
    let (adj, holders) = {
        let db = app.db();
        (
            fanout::links(&db, now_ms() - 3_600_000)?,
            catalog::holders(&db, &it.item, &it.root)?,
        )
    };
    let quiet = 3 * crate::HEARTBEAT_S as i64 * 1000;
    let placed = place(holders, &me.key, &peers, now_ms(), quiet);
    Ok(fanout::nearest_first(&adj, &me.node, &placed))
}

/// Holders as (node, core URL) for the core's preference list: never this station, never
/// a site that stopped reporting, and only addresses the core already has as peers.
pub fn place(
    holders: Vec<catalog::Holder>,
    me: &str,
    peers: &[(String, Option<String>)],
    now: i64,
    quiet_ms: i64,
) -> Vec<(String, String)> {
    holders
        .into_iter()
        .filter(|h| h.key != me && h.ts.is_none_or(|t| now - t <= quiet_ms))
        .filter_map(|h| {
            let said = h
                .core_url
                .as_deref()
                .and_then(catalog::peer_url)
                .filter(|u| peers.iter().any(|p| &p.0 == u));
            let url = said.or_else(|| by_call(peers, &h.call))?;
            Some((h.node, url))
        })
        .collect()
}

/// The peers this site's core syncs with, from its `/v1/peers`: (url, call).
async fn core_peers(app: &App) -> Result<Vec<(String, Option<String>)>> {
    let v: serde_json::Value = app
        .local
        .get(format!("{}/v1/peers", app.core.base()))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(v["peers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let url = catalog::peer_url(p["url"].as_str()?)?;
            Some((url, p["call"].as_str().map(str::to_string)))
        })
        .collect())
}

/// The core URL of the one peer with this callsign; None if there are none or several.
pub fn by_call(peers: &[(String, Option<String>)], call: &str) -> Option<String> {
    let mut m = peers.iter().filter(|p| p.1.as_deref() == Some(call));
    match (m.next(), m.next()) {
        (Some(p), None) => Some(p.0.clone()),
        _ => None,
    }
}

/// Have the core fetch this blob, from these peers first if any are known. A list the core
/// won't take must not stop the copy: then it fetches from any peer.
async fn ask(app: &App, it: &Item, from: &[String]) -> Result<()> {
    if !from.is_empty() {
        match app.core.blob_want_from(&it.root, from).await {
            Ok(()) => return Ok(()),
            Err(e) => tracing::warn!(
                "{}: the core refused the peer list ({e:#}); asking without it",
                it.item
            ),
        }
    }
    app.core.blob_want(&it.root).await
}

fn save_sources(app: &App, it: &Item, from: &[String]) -> Result<()> {
    app.db().execute(
        "UPDATE pending SET sources = ?2 WHERE item = ?1",
        params![it.item, serde_json::to_string(from)?],
    )?;
    Ok(())
}

async fn check(app: &App, it: &Item) -> Result<()> {
    match app.core.blob_status(&it.root).await? {
        Some(s) if s.complete => finish(app, it).await,
        // the core forgot (a reset, or someone let it go): ask again
        Some(s) if !s.wanted => {
            let from = sources(app, it).await.unwrap_or_default();
            ask(app, it, &from).await?;
            save_sources(app, it, &from)
        }
        // a site nearer than the ones asked so far may have finished its copy meanwhile;
        // if the list can't be worked out now, the core keeps the one it has
        Some(_) => {
            let Ok(from) = sources(app, it).await else {
                return Ok(());
            };
            let before: Option<Option<String>> = app
                .db()
                .query_row(
                    "SELECT sources FROM pending WHERE item = ?1",
                    [&it.item],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(before) = before else { return Ok(()) };
            if before.as_deref() != Some(serde_json::to_string(&from)?.as_str()) {
                tracing::info!("fetching {} {} from {:?} first", it.item, it.version, from);
                ask(app, it, &from).await?;
                save_sources(app, it, &from)?;
            }
            Ok(())
        }
        None => set_pending(
            &app.db(),
            it,
            "waiting",
            Some("the core no longer knows this blob"),
        ),
    }
}

/// Export, check and swap in a complete blob. A failed export is tried again later,
/// further apart each time; a copy that fails its check is never served.
async fn finish(app: &App, it: &Item) -> Result<()> {
    let name = file_name(&it.item, &it.version, &it.root, &it.ext);
    let exported = match app.core.blob_export(&it.root, &name).await {
        Ok(p) => PathBuf::from(p),
        Err(e) => return export_failed(app, it, &e),
    };
    let (path, kind, ext, sha) = (exported.clone(), it.kind, it.ext.clone(), it.sha256.clone());
    let checked =
        tokio::task::spawn_blocking(move || check_file(&path, kind, &ext, sha.as_deref())).await?;
    let sha256 = match checked {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_file(&exported);
            tracing::warn!("{} {} failed its check: {e:#}", it.item, it.version);
            set_pending(&app.db(), it, "failed", Some(&format!("{e:#}")))?;
            app.changed.notify_one();
            return Ok(());
        }
    };
    let dest = app.cfg.content_dir().join(&name);
    let (from, to) = (exported.clone(), dest.clone());
    if let Err(e) = tokio::task::spawn_blocking(move || move_file(&from, &to)).await? {
        let _ = std::fs::remove_file(&exported);
        return export_failed(app, it, &e);
    }
    let verified = match (&it.sha256, &it.hash_from) {
        (Some(_), Some(_)) => "publisher",
        (Some(_), None) => "fetcher",
        _ => "blob",
    };
    let now = now_ms();
    install(
        app,
        &Held {
            item: it.item.clone(),
            kind: it.kind,
            ext: it.ext.clone(),
            version: it.version.clone(),
            root: it.root.clone(),
            path: dest,
            size: it.size,
            sha256,
            verified: verified.into(),
            event_ts: it.ts,
            since_ms: now,
            checked_ms: now,
        },
        true,
    )
    .await
}

fn export_failed(app: &App, it: &Item, e: &anyhow::Error) -> Result<()> {
    let db = app.db();
    let attempts: i64 = db
        .query_row(
            "SELECT attempts FROM pending WHERE item = ?1",
            [&it.item],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0)
        + 1;
    // a full disk or a core restart clears up on its own; keep trying, further apart
    let retry = now_ms() + backoff_ms(attempts - 1);
    tracing::warn!(
        "exporting {} {} (try {attempts}): {e:#}",
        it.item,
        it.version
    );
    set_pending(&db, it, "fetching", Some(&format!("export failed: {e:#}")))?;
    db.execute(
        "UPDATE pending SET attempts = ?2, next_ms = ?3 WHERE item = ?1",
        params![it.item, attempts, retry],
    )?;
    Ok(())
}

/// Read every served file back once every scrub_days. One that no longer matches its
/// SHA-256 (a failing SSD) stops being served and is exported again from the core's blob
/// store, which checks and refetches any damaged chunks.
pub async fn scrub(app: &App) -> Result<()> {
    let every = app.cfg.scrub_days as i64 * 86_400_000;
    let due: Vec<Held> = held(&app.db())?
        .into_iter()
        .filter(|h| now_ms() - h.checked_ms >= every)
        .collect();
    for h in due {
        let path = h.path.clone();
        let got = tokio::task::spawn_blocking(move || crate::sha256::file_hex(&path)).await?;
        let why = match got {
            Ok(s) if s == h.sha256 => {
                app.db().execute(
                    "UPDATE held SET checked_ms = ?2 WHERE item = ?1",
                    params![h.item, now_ms()],
                )?;
                continue;
            }
            Ok(s) => format!("its SHA-256 is now {s}, was {}", h.sha256),
            Err(e) => format!("{e:#}"),
        };
        tracing::warn!("{} is damaged ({why}); copying it again", h.path.display());
        {
            let db = app.db();
            // the copy may have been replaced while it was being read
            let n = db.execute(
                "DELETE FROM held WHERE item = ?1 AND path = ?2",
                params![h.item, h.path.to_string_lossy()],
            )?;
            if n == 0 {
                continue;
            }
            db.execute(
                "INSERT OR REPLACE INTO pending (item, version, root, state, detail, since_ms, size)
                 VALUES (?1, ?2, ?3, 'fetching', ?4, ?5, ?6)",
                params![
                    h.item,
                    h.version,
                    h.root,
                    format!("the served copy was damaged on disk: {why}"),
                    now_ms(),
                    h.size as i64
                ],
            )?;
        }
        let _ = std::fs::remove_file(&h.path);
        app.changed.notify_one();
        if h.kind == Kind::Zim {
            app.zims_changed.notify_one();
        }
    }
    Ok(())
}

/// Rename, or copy and delete when the two folders are on different file systems.
pub fn move_file(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_file(from, to)?;
    std::fs::remove_file(from).with_context(|| format!("removing {}", from.display()))
}

/// Copy through a temporary name, so a half-copied file is never taken for a whole one.
pub fn copy_file(from: &Path, to: &Path) -> Result<()> {
    let tmp = to.with_extension("part");
    std::fs::copy(from, &tmp)
        .with_context(|| format!("copying {} to {}", from.display(), tmp.display()))?;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&tmp)?
        .sync_all()?;
    std::fs::rename(&tmp, to)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EXAMPLE;

    fn item(id: &str, root: char, ts: i64, size: u64) -> Item {
        Item {
            event_id: format!("e{ts}"),
            ts,
            call: "N0TEST".into(),
            item: id.into(),
            kind: Kind::Zim,
            ext: "zim".into(),
            title: id.into(),
            version: "2026-10".into(),
            size,
            root: root.to_string().repeat(64),
            source: String::new(),
            sha256: None,
            hash_from: None,
            tiers: vec![],
            group: String::new(),
            credit: Default::default(),
        }
    }

    fn held(id: &str, root: char, event_ts: i64) -> Held {
        Held {
            item: id.into(),
            kind: Kind::Zim,
            ext: "zim".into(),
            version: "2026-08".into(),
            root: root.to_string().repeat(64),
            path: PathBuf::from(format!("/x/{id}")),
            size: 1,
            sha256: String::new(),
            verified: "publisher".into(),
            event_ts,
            since_ms: 0,
            checked_ms: 0,
        }
    }

    fn pend(root: char, state: &str) -> Pending {
        Pending {
            item: "wiki".into(),
            version: "2026-10".into(),
            root: root.to_string().repeat(64),
            state: state.into(),
            detail: None,
            since_ms: 0,
            size: 1000,
            attempts: 0,
            next_ms: 0,
        }
    }

    fn cfg() -> Config {
        toml::from_str(EXAMPLE).unwrap()
    }

    #[test]
    fn decides_what_to_do() {
        let cfg = cfg();
        let new = item("wiki", 'b', 2000, 1000);
        let one = std::slice::from_ref(&new);
        let n = |h: &[Held], p: Option<&Pending>| next(&cfg, &new, one, h, p, 5000);
        assert_eq!(n(&[], None), Next::Want);
        assert_eq!(n(&[held("wiki", 'b', 2000)], None), Next::Nothing);
        // holding something newer (a fetcher that just published): don't go back
        assert_eq!(n(&[held("wiki", 'a', 3000)], None), Next::Nothing);
        let h = [held("wiki", 'a', 1000)];
        assert_eq!(n(&h, None), Next::Want);
        assert_eq!(n(&h, Some(&pend('b', "waiting"))), Next::Want);
        assert_eq!(n(&h, Some(&pend('b', "fetching"))), Next::Check);
        // backing off after a failed export
        let mut later = pend('b', "fetching");
        later.next_ms = 6000;
        assert_eq!(n(&h, Some(&later)), Next::Nothing);
        // a version that failed its check is not tried again; a newer one is
        assert_eq!(n(&h, Some(&pend('b', "failed"))), Next::Nothing);
        assert_eq!(n(&h, Some(&pend('c', "failed"))), Next::Want);
        let big = item("wiki", 'b', 2000, 50_000_000_000);
        assert!(matches!(
            next(&cfg, &big, std::slice::from_ref(&big), &[], None, 0),
            Next::Skip(_)
        ));
    }

    #[test]
    fn hubs_hold_nopic_and_small_sites_mini() {
        let mut mini = item("wiki-mini", 'a', 1000, 13_000_000_000);
        mini.group = "wiki".into();
        let mut nopic = item("wiki-nopic", 'b', 1000, 49_000_000_000);
        nopic.group = "wiki".into();
        nopic.tiers = vec!["hub".into()];
        let items = vec![mini.clone(), nopic.clone()];
        let mut small = cfg();
        small.mirror.max_mb = 60_000;
        // the default total_mb (40000) can't hold nopic: mini is the choice, not a deadlock
        let mut tight = small.clone();
        tight.mirror.tier = "hub".into();
        assert_eq!(next(&tight, &mini, &items, &[], None, 0), Next::Want);
        small.mirror.total_mb = 80_000;
        assert_eq!(next(&small, &mini, &items, &[], None, 0), Next::Want);
        assert!(matches!(
            next(&small, &nopic, &items, &[], None, 0),
            Next::Skip(_)
        ));
        let mut hub = small.clone();
        hub.mirror.tier = "hub".into();
        assert_eq!(next(&hub, &nopic, &items, &[], None, 0), Next::Want);
        match next(&hub, &mini, &items, &[], None, 0) {
            Next::Skip(why) => assert!(why.contains("wiki-nopic"), "{why}"),
            other => panic!("{other:?}"),
        }
        // a hub that held mini keeps serving it until nopic has arrived, then lets it go
        let had = [held("wiki-mini", 'a', 1000)];
        assert_eq!(next(&hub, &mini, &items, &had, None, 0), Next::Nothing);
        let both = [held("wiki-mini", 'a', 1000), held("wiki-nopic", 'b', 1000)];
        assert_eq!(next(&hub, &mini, &items, &both, None, 0), Next::Release);
    }

    #[test]
    fn budget_counts_what_is_held_and_coming() {
        let h = [held("a", 'a', 0), held("b", 'b', 0)];
        let mut p = pend('c', "fetching");
        p.item = "c".into();
        let mut w = pend('d', "waiting");
        w.item = "d".into();
        assert_eq!(committed(&h, &[p.clone(), w], "a"), 1 + 1000);
        assert_eq!(committed(&h, &[p], "zzz"), 2 + 1000);
    }

    #[test]
    fn only_live_holders_at_known_peers_are_named() {
        let h = |key: &str, call: &str, url: Option<&str>, ts: Option<i64>| catalog::Holder {
            key: key.into(),
            call: call.into(),
            node: format!("tst-{key}"),
            core_url: url.map(str::to_string),
            ts,
        };
        let peers = vec![
            ("http://a:8790".to_string(), None),
            (
                "http://10.0.0.2:8790".to_string(),
                Some("N0BBB".to_string()),
            ),
        ];
        let got = place(
            vec![
                h("a", "N0AAA", Some("http://a:8790/"), Some(990)),
                h("b", "N0BBB", None, None),
                h("stale", "N0AAA", Some("http://a:8790"), Some(0)),
                h("me", "N0ME", Some("http://a:8790"), Some(1000)),
                h("odd", "N0ODD", Some("http://not-a-peer:8790"), Some(1000)),
            ],
            "me",
            &peers,
            1000,
            100,
        );
        assert_eq!(
            got,
            [
                ("tst-a".to_string(), "http://a:8790".to_string()),
                ("tst-b".to_string(), "http://10.0.0.2:8790".to_string())
            ]
        );
    }

    #[test]
    fn export_retries_back_off() {
        assert_eq!(backoff_ms(0), 300_000);
        assert_eq!(backoff_ms(1), 600_000);
        assert_eq!(backoff_ms(20), 6 * 3_600_000);
    }

    #[test]
    fn copies_never_leave_half_files() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.bin");
        std::fs::write(&a, b"hello").unwrap();
        let b = d.path().join("b.bin");
        move_file(&a, &b).unwrap();
        assert!(!a.exists());
        assert_eq!(std::fs::read(&b).unwrap(), b"hello");
        let c = d.path().join("c.bin");
        copy_file(&b, &c).unwrap();
        assert!(b.exists() && !d.path().join("c.part").exists());
    }
}
