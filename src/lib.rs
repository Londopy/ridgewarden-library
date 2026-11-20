//! ridgewarden-library: Wikipedia, regional maps and NWS alerts at every site of an AREDN
//! mesh, refreshed whenever any site has internet.
//!
//! A site with internet (a fetcher) downloads what its operator configured, checks it
//! against the publisher's own hash, and publishes it as a blob through its ridgewarden
//! core with a signed `library.item` event. Every other site's copy of this app sees the
//! event, has its core fetch the blob over the mesh, checks it again, and only then swaps
//! it in for the old copy. NWS alerts travel as small events on the main log.

pub mod app;
pub mod catalog;
pub mod config;
pub mod content;
pub mod disk;
pub mod fanout;
pub mod fetch;
pub mod kiwix;
pub mod mirror;
pub mod mule;
pub mod nws;
pub mod sha256;
#[cfg(test)]
mod testing;
pub mod web;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use ridgewarden_client::util::now_ms;
use serde_json::{Value, json};

use crate::app::App;

/// Seconds between heartbeats when nothing changes.
pub const HEARTBEAT_S: u64 = 300;

/// `base` seconds, give or take `pct` percent.
pub fn jittered(base: u64, pct: u64) -> Duration {
    let spread = base * 1000 * pct / 100;
    let r = getrandom::u64().unwrap_or(0) % (2 * spread + 1);
    Duration::from_millis(base * 1000 - spread + r)
}

const WEEK_MS: i64 = 7 * 86_400_000;

/// This site's heartbeat: what it serves, what it is still copying, its free disk, how
/// fast alerts reach it, and its NWS state.
pub fn heartbeat_payload(app: &App, node: &str, free: Option<u64>) -> Result<Value> {
    let (held, pending, (delay, count)) = {
        let db = app.db();
        (
            mirror::held(&db)?,
            mirror::pending(&db)?,
            nws::delivery(&db, now_ms() - WEEK_MS)?,
        )
    };
    let mut holds: Vec<Value> = held
        .iter()
        .map(|h| json!({"item": h.item, "version": h.version, "root": h.root, "state": "serving"}))
        .collect();
    holds.extend(pending.iter().map(|p| {
        let state = match p.state.as_str() {
            "failed" | "skipped" => p.state.as_str(),
            _ => "fetching",
        };
        json!({"item": p.item, "version": p.version, "root": p.root, "state": state, "detail": p.detail})
    }));
    let nws = if app.cfg.nws.enabled {
        let st = app.nws_state();
        json!({"place": app.cfg.nws.place(), "ok_ms": st.ok_ms, "error": st.error,
               "active": st.listed()})
    } else {
        Value::Null
    };
    let fetches: Vec<&str> = app.cfg.items.iter().map(|i| i.id.as_str()).collect();
    let kiwix_ok = app.kiwix.probe.lock().unwrap_or_else(|e| e.into_inner()).0;
    Ok(json!({
        "node": node,
        "tier": app.cfg.mirror.tier,
        "fetches": fetches,
        "holds": holds,
        "free_mb": free.map(|b| b / 1_000_000),
        "alerts": {"delay_ms": delay, "count": count},
        "kiwix_ok_ms": kiwix_ok,
        "core_url": app.cfg.core.public_url,
        "nws": nws,
    }))
}

async fn heartbeat(app: &App) -> Result<()> {
    let who = app.whoami().await?;
    let content = app.cfg.content_dir();
    let free = tokio::task::spawn_blocking(move || disk::free(&content).map(|f| f.free)).await?;
    let p = heartbeat_payload(app, &who.node, free)?;
    app.core.append_telemetry("library.site", p).await?;
    Ok(())
}

/// Poll the NWS if it is this site's turn. New alerts are logged before the ETag is kept,
/// so an alert whose event failed to log is fetched again next time.
async fn nws_round(app: &App) -> Result<()> {
    let me = app.whoami().await?.key;
    let place = app.cfg.nws.place();
    let now = now_ms();
    let turn = catalog::nws_turn(
        &catalog::sites(&app.db())?,
        &me,
        &place,
        now,
        app.cfg.nws.quiet_s as i64 * 1000,
    );
    if std::mem::replace(&mut app.nws_state().fetching, turn) != turn {
        tracing::info!(
            "{} fetching NWS alerts for {place}",
            if turn { "now" } else { "no longer" }
        );
        app.changed.notify_one();
    }
    if !turn {
        return Ok(());
    }
    let etag = app.nws_state().etag.clone();
    let query = app.cfg.nws.query().unwrap_or(("area", ""));
    let polled = match nws::poll(&app.nws_http, &app.cfg.nws.api_url, query, etag.as_deref()).await
    {
        Ok(p) => p,
        Err(e) => return Err(nws_failed(app, e)),
    };
    let mut active = None;
    if let Some(features) = &polled.features {
        let known = nws::known(&app.db(), now)?;
        let (new, now_active) = nws::digest(features, &place, |id| known.contains(id));
        for a in new {
            if let Err(e) = app.core.append("library.alert", a.clone()).await {
                return Err(nws_failed(app, e.context("logging an alert")));
            }
            tracing::info!("NWS alert logged: {}", a["headline"].as_str().unwrap_or(""));
        }
        active = Some(now_active);
    }
    let mut st = app.nws_state();
    if st.observe(active) {
        app.changed.notify_one();
    }
    st.etag = polled.etag;
    if st.ok_ms.is_none() || st.error.is_some() {
        app.changed.notify_one();
    }
    st.ok_ms = Some(now);
    st.error = None;
    Ok(())
}

/// Note a failed poll, and tell the mesh soon: until it knows, other sites take this one
/// for a working fetcher and stay quiet.
fn nws_failed(app: &App, e: anyhow::Error) -> anyhow::Error {
    let msg: String = format!("{e:#}").chars().take(200).collect();
    let mut st = app.nws_state();
    if st.error.is_none() {
        app.changed.notify_one();
    }
    st.error = Some(msg);
    e
}

fn zims(app: &App) -> Vec<std::path::PathBuf> {
    match mirror::held(&app.db()) {
        Ok(h) => h
            .into_iter()
            .filter(|h| h.kind == content::Kind::Zim)
            .map(|h| h.path)
            .collect(),
        Err(e) => {
            tracing::warn!("reading held files: {e:#}");
            vec![]
        }
    }
}

/// Follow the core, fetch, mirror, send heartbeats, poll the NWS and keep kiwix-serve up.
pub fn spawn_loops(app: Arc<App>) {
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = ridgewarden_client::follow(&a.core, &a.db, app::PROJECTIONS).await {
                tracing::warn!("following the core: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
    let a = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        loop {
            if let Err(e) = heartbeat(&a).await {
                tracing::warn!("heartbeat: {e:#}");
            }
            tokio::select! {
                _ = a.changed.notified() => {}
                _ = tokio::time::sleep(jittered(HEARTBEAT_S, 10)) => {}
            }
            // let a burst of changes go out as one heartbeat
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
    let a = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(15)).await;
        loop {
            if let Err(e) = mirror::round(&a).await {
                tracing::warn!("mirror round: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
    if !app.cfg.items.is_empty() {
        let a = app.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            loop {
                if let Err(e) = fetch::round(&a).await {
                    tracing::warn!("fetch round: {e:#}");
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
    }
    if app.cfg.nws.enabled {
        let a = app.clone();
        tokio::spawn(async move {
            // let the feed land first, so we know who else is fetching
            tokio::time::sleep(Duration::from_secs(20)).await;
            loop {
                if let Err(e) = nws_round(&a).await {
                    tracing::warn!("NWS alerts: {e:#}");
                }
                tokio::time::sleep(jittered(a.cfg.nws.poll_s, 10)).await;
            }
        });
    }
    let a = app.clone();
    tokio::spawn(async move {
        // the first scrub waits, so a restart doesn't reread every file at once
        tokio::time::sleep(Duration::from_secs(600)).await;
        loop {
            if let Err(e) = mirror::scrub(&a).await {
                tracing::warn!("checking served files: {e:#}");
            }
            if let Err(e) = nws::prune(&a.db(), now_ms()) {
                tracing::warn!("pruning old alerts: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
    if !app.cfg.kiwix.url.trim().is_empty() {
        let a = app.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            loop {
                let r = kiwix::probe(&a.local, a.cfg.kiwix.url.trim()).await;
                a.kiwix.note_probe(r);
                tokio::time::sleep(Duration::from_secs(300)).await;
            }
        });
    }
    if !app.cfg.kiwix.command.is_empty() {
        tokio::spawn(async move {
            let log = app.cfg.data_dir.join("kiwix-serve.log");
            loop {
                app.kiwix.check(&app.cfg.kiwix, zims(&app), &log);
                tokio::select! {
                    _ = app.zims_changed.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                }
            }
        });
    }
}
