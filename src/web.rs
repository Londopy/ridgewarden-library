//! The catalog, Wikipedia, maps, documents, alerts and about pages, the map and document
//! files themselves, and the same data as JSON.

use std::fmt::Write as _;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ridgewarden_client::ui::{Color, Page, pill};
use ridgewarden_client::util::{esc, human_duration, now_ms, short_time};
use serde_json::{Value, json};

use crate::HEARTBEAT_S;
use crate::app::App;
use crate::catalog::{self, Copy, Hold, Item, Site};
use crate::content::{Kind, content_type};
use crate::mirror::{self, Held, Pending};
use crate::{mule, nws};

/// The most one range response carries. Map readers ask for a few kilobytes at a time.
pub const MAX_SPAN: u64 = 4 << 20;
/// Bigger files are never sent whole: a multi-gigabyte download over RF would crowd out
/// everything else, and a whole response is held in memory while it is sent.
pub const WHOLE_MAX: u64 = 32 << 20;
/// Alerts should reach every site within this of a fetcher logging them.
pub const ALERT_GOAL_MS: i64 = 60_000;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(catalog_page))
        .route("/wiki", get(wiki_page))
        .route("/maps", get(maps_page))
        .route("/docs", get(docs_page))
        .route("/alerts", get(alerts_page))
        .route("/about", get(about_page))
        .route("/files/{name}", get(file))
        .route("/api/catalog", get(catalog_json))
        .route("/api/alerts", get(alerts_json))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(app)
}

struct Fail(anyhow::Error);

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        tracing::error!("request failed: {:#}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for Fail {
    fn from(e: E) -> Self {
        Fail(e.into())
    }
}

const NAV: &[(&str, &str)] = &[
    ("/", "Catalog"),
    ("/wiki", "Wikipedia"),
    ("/maps", "Maps"),
    ("/docs", "Documents"),
    ("/alerts", "Alerts"),
    ("/about", "About"),
];

async fn page(app: &App, active: &str, title: &str, body: &str, refresh: bool) -> Html<String> {
    let who = match app.whoami().await {
        Ok(w) if !w.node.is_empty() => format!("{} at {}", w.call, app.cfg.label(&w.node)),
        Ok(w) => w.call,
        Err(_) => "core unreachable".into(),
    };
    Html(
        Page {
            app: "ridgewarden-library",
            nav: NAV,
            active,
            who: &who,
            refresh,
        }
        .render(title, body),
    )
}

fn size_text(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1} GB", n as f64 / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.1} MB", n as f64 / 1e6)
    } else {
        format!("{} kB", n.div_ceil(1000))
    }
}

fn ago(ms: i64, now: i64) -> String {
    format!("{} ({} ago)", short_time(ms), human_duration(now - ms))
}

fn site_name(app: &App, call: &str, node: &str) -> String {
    if node.is_empty() {
        call.to_string()
    } else {
        format!("{} ({call})", app.cfg.label(node))
    }
}

/// How a copy was checked, in words.
fn checked_text(it: &Item, confirmed: u32) -> String {
    let mut t = match (&it.sha256, &it.hash_from) {
        (Some(_), Some(from)) => format!(
            "Every copy is checked against the publisher's SHA-256 ({from}), which reached \
             this site in {}'s signed event.",
            it.call
        ),
        (Some(_), None) => format!(
            "The publisher gives no hash for this file (it was cut or supplied at {}'s site), \
             so copies are checked against the SHA-256 that site signed.",
            it.call
        ),
        _ => "Copies are checked against the blob store's chunk hashes only.".into(),
    };
    if confirmed > 1 {
        let _ = write!(
            t,
            " {confirmed} stations fetched this exact file from the internet on their own."
        );
    }
    t
}

/// A warning when a copy is older than the operator thinks is fresh.
fn stale_banner(app: &App, title: &str, fetched: i64, now: i64) -> String {
    let age = now - fetched;
    if age <= app.cfg.stale_days as i64 * 86_400_000 {
        return String::new();
    }
    format!(
        "<p class=banner>{} was fetched {} ago. Check anything that matters against a newer \
         source when you can.</p>",
        esc(title),
        human_duration(age)
    )
}

/// One site's standing for an item, as a pill and a note.
fn standing(latest: &Item, holds: &[&Hold], copy: Option<&Copy>) -> (String, String) {
    let serving = holds.iter().find(|h| h.state == "serving");
    let busy = holds.iter().find(|h| h.state != "serving");
    let mut note = String::new();
    let p = match (serving, copy) {
        (Some(h), _) if h.root == latest.root => pill(Color::Green, "current"),
        (Some(h), _) => {
            let _ = write!(note, "serving {}", h.version);
            pill(Color::Yellow, "older version")
        }
        // only a lasting record, e.g. carried back by a data-mule car
        (None, Some(c)) if c.root == latest.root => pill(Color::Green, "current (record)"),
        (None, Some(c)) => {
            let _ = write!(note, "copied {}", c.version);
            pill(Color::Yellow, "older version (record)")
        }
        (None, None) if busy.is_some_and(|b| b.state == "skipped") => {
            pill(Color::Unknown, "not copied")
        }
        (None, None) => pill(Color::Unknown, "not held"),
    };
    if let Some(b) = busy {
        if !note.is_empty() {
            note.push_str("; ");
        }
        let d = b.detail.as_deref().unwrap_or("");
        let _ = match b.state.as_str() {
            "failed" => write!(note, "{} failed: {d}", b.version),
            "skipped" => write!(note, "{d}"),
            _ if d.is_empty() => write!(note, "copying {} from the mesh", b.version),
            _ => write!(note, "copying {}: {d}", b.version),
        };
    }
    (p, note)
}

fn pending_pill(p: &Pending) -> String {
    match p.state.as_str() {
        "failed" => pill(Color::Red, "failed"),
        "skipped" => pill(Color::Unknown, "not copied here"),
        "fetching" => pill(Color::Unknown, "copying"),
        _ => pill(Color::Unknown, "waiting"),
    }
}

fn link_for(kind: Kind) -> &'static str {
    match kind {
        Kind::Zim => "/wiki",
        Kind::Pmtiles => "/maps",
        Kind::Doc => "/docs",
    }
}

fn here_section(app: &App, b: &mut String, items: &[Item], now: i64) -> Result<(), Fail> {
    let (held, pending) = {
        let db = app.db();
        (mirror::held(&db)?, mirror::pending(&db)?)
    };
    b.push_str("<h2>At this site</h2>");
    if held.is_empty() && pending.is_empty() {
        b.push_str("<p class=mute>Nothing here yet.</p>");
        return Ok(());
    }
    b.push_str(
        "<div class=wrap><table><tr><th>Item</th><th>Version</th><th>Size</th><th>Checked</th>\
         <th>Serving since</th></tr>",
    );
    for h in &held {
        let title = items
            .iter()
            .find(|i| i.item == h.item)
            .map_or(h.item.as_str(), |i| i.title.as_str());
        let _ = write!(
            b,
            "<tr><td><a href=\"{}\">{}</a></td><td>{}</td><td>{}</td><td>{}<br>\
             <span class=\"mute small\">read back {}</span></td><td>{}</td></tr>",
            link_for(h.kind),
            esc(title),
            esc(&h.version),
            size_text(h.size),
            esc(verified_word(h)),
            esc(&ago(h.checked_ms, now)),
            esc(&ago(h.since_ms, now))
        );
    }
    for p in &pending {
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{}</td><td colspan=2>{} <span class=\"mute small\">{}</span></td>\
             <td>since {}</td></tr>",
            esc(&p.item),
            esc(&p.version),
            pending_pill(p),
            esc(p.detail.as_deref().unwrap_or("")),
            esc(&short_time(p.since_ms))
        );
    }
    b.push_str("</table></div>");
    Ok(())
}

fn trips_section(app: &App, b: &mut String, items: &[Item], now: i64) -> Result<(), Fail> {
    let trips = mule::trips(&app.db())?;
    if trips.is_empty() {
        return Ok(());
    }
    b.push_str(
        "<h2>Carried by data mule</h2><p class=\"mute small\">Versions sent to other islands \
         with ridgewarden-mule, and the last record of each trip.</p>\
         <div class=wrap><table><tr><th>Item</th><th>To</th><th>Sent</th><th>Last heard</th></tr>",
    );
    for t in &trips {
        let title = items
            .iter()
            .find(|i| i.item == t.item)
            .map_or(t.item.as_str(), |i| i.title.as_str());
        let last = match &t.last {
            Some((kind, call, ms)) => {
                let (c, w) = match kind.as_str() {
                    "receipt" => (Color::Green, "delivered"),
                    "dropoff" => (Color::Yellow, "handed over"),
                    _ => (Color::Unknown, "on the road"),
                };
                format!("{} {} {}", pill(c, w), esc(call), esc(&ago(*ms, now)))
            }
            None => pill(Color::Unknown, "waiting for a car"),
        };
        let _ = write!(
            b,
            "<tr><td>{} {}</td><td>{}</td><td>{}</td><td>{last}</td></tr>",
            esc(title),
            esc(&t.version),
            esc(&t.island),
            esc(&ago(t.sent_ms, now))
        );
    }
    b.push_str("</table></div>");
    Ok(())
}

fn sites_section(app: &App, b: &mut String, sites: &[Site], now: i64) {
    if sites.is_empty() {
        return;
    }
    b.push_str(
        "<h2>Sites</h2><div class=wrap><table><tr><th>Site</th><th>Tier</th><th>Free disk</th>\
         <th>kiwix-serve answered</th><th>Slowest alert, 7 days</th><th>As of</th></tr>",
    );
    for s in sites {
        let quiet = if now - s.ts > 3 * HEARTBEAT_S as i64 * 1000 {
            " <span class=\"pill unknown\">quiet</span>"
        } else {
            ""
        };
        let alerts = match (s.alert_delay_ms, s.alerts_seen) {
            (Some(d), Some(n)) if n > 0 => format!(
                "{} {} <span class=\"mute small\">over {n}</span>",
                pill(
                    if d <= ALERT_GOAL_MS {
                        Color::Green
                    } else {
                        Color::Red
                    },
                    if d <= ALERT_GOAL_MS {
                        "under a minute"
                    } else {
                        "slow"
                    }
                ),
                esc(&human_duration(d))
            ),
            _ => "<span class=mute>no alerts yet</span>".into(),
        };
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{alerts}</td><td>{}{quiet}</td></tr>",
            esc(&site_name(app, &s.call, &s.node)),
            esc(if s.tier.is_empty() { "-" } else { &s.tier }),
            s.free_mb
                .map(|m| size_text((m.max(0) as u64).saturating_mul(1_000_000)))
                .unwrap_or_else(|| "?".into()),
            s.kiwix_ok_ms
                .map(|t| esc(&ago(t, now)))
                .unwrap_or_else(|| "-".into()),
            esc(&ago(s.ts, now))
        );
    }
    b.push_str("</table></div>");
}

async fn catalog_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let (items, sites, holds, copies) = {
        let db = app.db();
        (
            catalog::latest(&db)?,
            catalog::sites(&db)?,
            catalog::holds(&db)?,
            catalog::copies(&db)?,
        )
    };
    let mut b = String::from(
        "<h1>Library</h1><p class=\"mute small\">Reference, maps, documents and weather alerts \
         copied to every site of the mesh, for when the internet is down. Dates are when each \
         version was fetched from the internet; \"as of\" is when a site last reported in.</p>",
    );
    here_section(&app, &mut b, &items, now)?;
    b.push_str("<h2>Across the mesh</h2>");
    if items.is_empty() {
        b.push_str(
            "<p class=mute>Nothing has been published yet. A site with internet lists what to \
             fetch as <code>[[item]]</code> entries in its library.toml.</p>",
        );
    }
    for it in &items {
        let confirmed = catalog::confirmations(&app.db(), it)?;
        let mut limits = String::new();
        if !it.tiers.is_empty() {
            let _ = write!(limits, " For {} sites only.", it.tiers.join(", "));
        }
        if !it.group.is_empty() {
            let _ = write!(
                limits,
                " One of the \"{}\" group: each site holds the biggest it can.",
                it.group
            );
        }
        let _ = write!(
            b,
            "<h3>{} <span class=\"mute small\">{}</span></h3><p>Newest version <b>{}</b>, {}, \
             fetched by {} {}.<br><span class=\"mute small\">From {}. {}{}</span></p>",
            esc(&it.title),
            esc(it.kind.word()),
            esc(&it.version),
            size_text(it.size),
            esc(&it.call),
            esc(&ago(it.ts, now)),
            esc(&it.source),
            esc(&checked_text(it, confirmed)),
            esc(&limits)
        );
        b.push_str(
            "<div class=wrap><table><tr><th>Site</th><th>Holds</th><th></th><th>As of</th></tr>",
        );
        let mut rows = 0;
        for s in &sites {
            let mine: Vec<&Hold> = holds
                .iter()
                .filter(|h| h.key == s.key && h.item == it.item)
                .collect();
            let copy = copies.iter().find(|c| c.key == s.key && c.item == it.item);
            let fetches = s.fetches.contains(&it.item);
            if mine.is_empty() && copy.is_none() && !fetches {
                continue;
            }
            let (p, mut note) = standing(it, &mine, copy);
            if fetches {
                note = if note.is_empty() {
                    "fetches and publishes it".into()
                } else {
                    format!("fetches and publishes it; {note}")
                };
            }
            let quiet = if now - s.ts > 3 * HEARTBEAT_S as i64 * 1000 {
                " <span class=\"pill unknown\">quiet</span>"
            } else {
                ""
            };
            rows += 1;
            let _ = write!(
                b,
                "<tr><td>{}</td><td>{p}</td><td class=small>{}</td><td>{}{quiet}</td></tr>",
                esc(&site_name(&app, &s.call, &s.node)),
                esc(&note),
                esc(&ago(s.ts, now))
            );
        }
        // sites known only from lasting records, such as those across a data-mule corridor
        for c in copies
            .iter()
            .filter(|c| c.item == it.item && !sites.iter().any(|s| s.key == c.key))
        {
            rows += 1;
            let (p, note) = standing(it, &[], Some(c));
            let _ = write!(
                b,
                "<tr><td>{}</td><td>{p}</td><td class=small>{}</td><td>{}</td></tr>",
                esc(&site_name(&app, &c.call, &c.node)),
                esc(&note),
                esc(&ago(c.ts, now))
            );
        }
        if rows == 0 {
            b.push_str("<tr><td colspan=4 class=mute>No site has reported on it yet.</td></tr>");
        }
        b.push_str("</table></div>");
    }
    trips_section(&app, &mut b, &items, now)?;
    sites_section(&app, &mut b, &sites, now);
    Ok(page(&app, "/", "Catalog", &b, true).await)
}

fn verified_word(h: &Held) -> &'static str {
    match h.verified.as_str() {
        "publisher" => "publisher's SHA-256",
        "fetcher" => "fetcher's SHA-256",
        _ => "chunk hashes",
    }
}

fn held_of(app: &App, kind: Kind) -> Result<Vec<(Held, Option<Item>)>, Fail> {
    let db = app.db();
    let mut out = vec![];
    for h in mirror::held(&db)?.into_iter().filter(|h| h.kind == kind) {
        let it = catalog::published(&db, &h.item, &h.root)?;
        out.push((h, it));
    }
    Ok(out)
}

fn title_of(h: &Held, it: &Option<Item>) -> String {
    it.as_ref().map_or(h.item.clone(), |i| i.title.clone())
}

fn fetched_of(h: &Held, it: &Option<Item>) -> i64 {
    it.as_ref().map_or(h.event_ts, |i| i.ts)
}

async fn wiki_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let zims = held_of(&app, Kind::Zim)?;
    let mut b = String::from("<h1>Wikipedia</h1>");
    if zims.is_empty() {
        b.push_str(
            "<p class=mute>No reference files have reached this site yet. The catalog shows \
             what is published and what is still being copied.</p>",
        );
    }
    let url = app.cfg.kiwix.url.trim();
    if !zims.is_empty() {
        if url.is_empty() {
            b.push_str(
                "<p class=banner>This site holds the files below but no kiwix-serve address is \
                 configured, so there is nothing to link to. Set <code>[kiwix] url</code> \
                 (and <code>command</code> to have this app run it).</p>",
            );
        } else {
            let _ = write!(
                b,
                "<p><a href=\"{}\"><b>Open the offline Wikipedia</b></a> \
                 <span class=\"mute small\">served by kiwix-serve</span></p>",
                esc(url)
            );
        }
    }
    if !app.cfg.kiwix.command.is_empty() {
        let _ = write!(
            b,
            "<p class=\"mute small\">kiwix-serve: {}</p>",
            esc(&app.kiwix.status())
        );
    }
    if !url.is_empty() {
        let (ok, err) = app
            .kiwix
            .probe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let line = match (ok, err) {
            (Some(t), None) => format!("answered {}", ago(t, now)),
            (Some(t), Some(e)) => {
                format!(
                    "did not answer just now ({e}); last answered {}",
                    ago(t, now)
                )
            }
            (None, Some(e)) => format!("does not answer: {e}"),
            (None, None) => "not checked yet".into(),
        };
        let _ = write!(
            b,
            "<p class=\"mute small\">Checked from this box every 5 minutes: kiwix-serve {}.</p>",
            esc(&line)
        );
    }
    for (h, it) in &zims {
        let title = title_of(h, it);
        let fetched = fetched_of(h, it);
        b.push_str(&stale_banner(&app, &title, fetched, now));
        let _ = write!(
            b,
            "<h2>{}</h2><p>Version {}, {}. Fetched from the internet {}; articles are as of then.</p>",
            esc(&title),
            esc(&h.version),
            size_text(h.size),
            esc(&ago(fetched, now))
        );
    }
    b.push_str(
        "<p class=\"mute small\">Wikipedia text is available under the Creative Commons \
         Attribution-ShareAlike License. See <a href=\"/about\">About</a>.</p>",
    );
    Ok(page(&app, "/wiki", "Wikipedia", &b, false).await)
}

fn host(headers: &HeaderMap) -> String {
    headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("this-site")
        .to_string()
}

/// The address that always serves an item's current version.
fn stable_name(h: &Held) -> String {
    format!("{}.{}", h.item, h.ext)
}

async fn maps_page(State(app): State<Arc<App>>, headers: HeaderMap) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let maps = held_of(&app, Kind::Pmtiles)?;
    let host = host(&headers);
    let mut b = String::from("<h1>Maps</h1>");
    if maps.is_empty() {
        b.push_str(
            "<p class=mute>No map files have reached this site yet. The catalog shows what is \
             published and what is still being copied.</p>",
        );
    }
    for (h, it) in &maps {
        let title = title_of(h, it);
        let fetched = fetched_of(h, it);
        b.push_str(&stale_banner(&app, &title, fetched, now));
        let name = h.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let _ = write!(
            b,
            "<h2>{}</h2><p>Map data from {}, {}. Fetched {}.</p>\
             <p><code>http://{}/files/{}</code><br><span class=\"mute small\">always the newest \
             version; this one is also at <code>/files/{}</code></span></p>",
            esc(&title),
            esc(&h.version),
            size_text(h.size),
            esc(&ago(fetched, now)),
            esc(&host),
            esc(&stable_name(h)),
            esc(name)
        );
    }
    if !maps.is_empty() {
        b.push_str(
            "<p class=small>These are PMTiles archives. Any PMTiles reader that fetches by HTTP \
             Range requests can open the address above and read only the tiles it shows: for \
             example <code>pmtiles show &lt;address&gt;</code> with the pmtiles command-line \
             tool, or a MapLibre map with the pmtiles plugin. Whole-file downloads of large maps \
             are refused here to keep the radio links free for traffic.</p>",
        );
    }
    b.push_str(
        "<p class=\"mute small\">Map data &copy; OpenStreetMap contributors, available under the \
         Open Database License. See <a href=\"/about\">About</a>.</p>",
    );
    Ok(page(&app, "/maps", "Maps", &b, false).await)
}

async fn docs_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let docs = held_of(&app, Kind::Doc)?;
    let mut b = String::from(
        "<h1>Documents</h1><p class=\"mute small\">Forms, plans and phone lists, copied to every \
         site. Anyone on the mesh can read them.</p>",
    );
    if docs.is_empty() {
        b.push_str(
            "<p class=mute>No documents have reached this site yet. A site lists them as \
             <code>kind = \"doc\"</code> items in its library.toml.</p>",
        );
    } else {
        b.push_str(
            "<div class=wrap><table><tr><th>Document</th><th>Type</th><th>Size</th>\
             <th>Version</th><th>Published</th></tr>",
        );
    }
    let mut banners = String::new();
    for (h, it) in &docs {
        let title = title_of(h, it);
        let fetched = fetched_of(h, it);
        banners.push_str(&stale_banner(&app, &title, fetched, now));
        let big = if h.size > WHOLE_MAX {
            " <span class=\"mute small\">too big to download whole here</span>"
        } else {
            ""
        };
        let _ = write!(
            b,
            "<tr><td><a href=\"/files/{}\">{}</a>{big}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            esc(&stable_name(h)),
            esc(&title),
            esc(&h.ext.to_ascii_uppercase()),
            size_text(h.size),
            esc(&h.version),
            esc(&ago(fetched, now))
        );
    }
    if !docs.is_empty() {
        b.push_str("</table></div>");
    }
    b.push_str(&banners);
    Ok(page(&app, "/docs", "Documents", &b, false).await)
}

fn alert_color(sev: Option<&str>) -> Color {
    match sev {
        Some("Extreme" | "Severe") => Color::Red,
        Some("Moderate") => Color::Yellow,
        _ => Color::Unknown,
    }
}

fn alerts_view(app: &App) -> Result<(Vec<nws::Alert>, Vec<Site>), Fail> {
    let db = app.db();
    let sites = catalog::sites(&db)?;
    let active = nws::active(&db, &sites, now_ms(), app.cfg.nws.quiet_s as i64 * 1000)?;
    Ok((active, sites))
}

async fn alerts_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let (active, sites) = alerts_view(&app)?;
    let mut b = String::from("<h1>Weather alerts</h1>");
    let as_of = nws::as_of(&sites);
    for (place, ok, who) in &as_of {
        let _ = write!(
            b,
            "<p class=\"mute small\">NWS alerts for {} as of {}, fetched by {}.</p>",
            esc(place),
            esc(&ago(*ok, now)),
            esc(who)
        );
    }
    let newest = as_of.iter().map(|x| x.1).max();
    if let Some(n) = nws::quiet_note(newest, app.cfg.nws.quiet_s as i64 * 1000) {
        let _ = write!(b, "<p class=banner>{}</p>", esc(&n));
    }
    for s in sites.iter().filter(|s| s.nws_error.is_some()) {
        let _ = write!(
            b,
            "<p class=banner>{} could not reach the NWS: {}</p>",
            esc(&site_name(&app, &s.call, &s.node)),
            esc(s.nws_error.as_deref().unwrap_or(""))
        );
    }
    if active.is_empty() {
        b.push_str("<p>No active alerts.</p>");
    }
    for a in &active {
        let status = match a.status.as_deref() {
            Some("Actual") | None => String::new(),
            Some(s) => format!(" {}", pill(Color::Unknown, s)),
        };
        let _ = write!(
            b,
            "<div class=\"tile {}\" style=\"margin:10px 0\"><div class=name>{}{status}</div>\
             <div class=why>{}</div><p class=small>{}</p><p class=small>Sent {}. Expires {}.{}</p>",
            alert_color(a.severity.as_deref()).css(),
            esc(&a.event),
            esc(a.headline.as_deref().unwrap_or("")),
            esc(a.area.as_deref().unwrap_or("")),
            esc(&a.sent_ms.map(short_time).unwrap_or_else(|| "?".into())),
            esc(&short_time(a.expires_ms)),
            a.ends_ms
                .map(|e| format!(" Ends {}.", esc(&short_time(e))))
                .unwrap_or_default()
        );
        for text in [&a.description, &a.instruction].into_iter().flatten() {
            let _ = write!(b, "<p style=\"white-space:pre-wrap\">{}</p>", esc(text));
        }
        let _ = write!(
            b,
            "<p class=\"mute small\">{} {}, {}. Relayed by {}.</p></div>",
            esc(a.sender.as_deref().unwrap_or("NWS")),
            esc(a.severity.as_deref().unwrap_or("")),
            esc(a.urgency.as_deref().unwrap_or("")),
            esc(&a.call)
        );
    }
    let (delay, count) = nws::delivery(&app.db(), now - 7 * 86_400_000)?;
    b.push_str("<h2>Delivery</h2>");
    match delay {
        Some(d) if count > 0 => {
            let _ = write!(
                b,
                "<p>Over the last 7 days, the slowest of {count} alerts reached this site {} after \
                 the fetching site logged it. The goal is under a minute.</p>",
                esc(&human_duration(d))
            );
        }
        _ => b.push_str("<p class=mute>No alerts have arrived here in the last 7 days.</p>"),
    }
    b.push_str(
        "<p class=\"mute small\">Every site's figure is on the <a href=\"/\">catalog</a>.</p>",
    );
    Ok(page(&app, "/alerts", "Alerts", &b, true).await)
}

async fn about_page(State(app): State<Arc<App>>) -> Html<String> {
    let b = "<h1>About this library</h1>\
<p>A disaster fallback for the mesh: reference, maps, documents and weather alerts fetched by a \
site that has internet and copied to the others, so they stay usable when the internet is down. \
It is not a replacement for internet access, and carries no general browsing.</p>\
<h2>Wikipedia</h2><p>Articles come from <a href=\"https://www.wikipedia.org/\">Wikipedia</a>, \
written by its volunteer contributors, packaged for offline use by \
<a href=\"https://kiwix.org/\">Kiwix</a>. The text is available under the \
<a href=\"https://creativecommons.org/licenses/by-sa/4.0/\">Creative Commons Attribution-ShareAlike \
4.0 License</a>; images carry their own licenses, shown on each file's page. The copy here may \
be weeks or months old: each article is as of the date shown on the Wikipedia page of this app.</p>\
<h2>Maps</h2><p>Map data &copy; <a href=\"https://www.openstreetmap.org/copyright\">OpenStreetMap \
contributors</a>, available under the <a href=\"https://opendatacommons.org/licenses/odbl/\">Open \
Database License</a>. Tiles built by <a href=\"https://protomaps.com/\">Protomaps</a>. Anyone \
showing these maps must credit OpenStreetMap.</p>\
<h2>Documents</h2><p>Forms, plans and phone lists come from whoever published them, named on \
the catalog page. They are readable by anyone on the mesh.</p>\
<h2>Weather alerts</h2><p>Alerts come from the <a href=\"https://www.weather.gov/\">National Weather \
Service</a> (api.weather.gov) and are passed on unchanged. They may be minutes old; the alerts \
page says when they were last fetched. In an emergency, follow official instructions.</p>\
<h2>Amateur radio rules</h2><p>Everything here crosses the mesh as plain HTTP, signed but never \
encrypted. No music or video is carried. Large files move at a capped rate so they never crowd out \
other traffic, and alerts always go first.</p>";
    page(&app, "/about", "About", b, false).await
}

/// A byte range of a file of `len` bytes, from a Range header.
#[derive(Debug, PartialEq)]
pub enum Want {
    Whole,
    /// inclusive
    Part(u64, u64),
    Unsatisfiable,
}

pub fn parse_range(h: Option<&str>, len: u64) -> Want {
    let Some(h) = h else { return Want::Whole };
    let Some(spec) = h.trim().strip_prefix("bytes=") else {
        return Want::Whole;
    };
    // map readers ask for one range at a time
    if spec.contains(',') || len == 0 {
        return Want::Unsatisfiable;
    }
    let Some((a, b)) = spec.split_once('-') else {
        return Want::Unsatisfiable;
    };
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => match n.parse::<u64>() {
            Ok(n) if n > 0 => (len.saturating_sub(n), len - 1),
            _ => return Want::Unsatisfiable,
        },
        (a, "") => match a.parse::<u64>() {
            Ok(a) => (a, len - 1),
            Err(_) => return Want::Unsatisfiable,
        },
        (a, b) => match (a.parse::<u64>(), b.parse::<u64>()) {
            (Ok(a), Ok(b)) if a <= b => (a, b.min(len - 1)),
            _ => return Want::Unsatisfiable,
        },
    };
    if start >= len {
        return Want::Unsatisfiable;
    }
    Want::Part(start, end.min(start + MAX_SPAN - 1))
}

/// File data in a response, holding its share of the memory budget until it is dropped.
struct Pinned {
    data: Vec<u8>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl AsRef<[u8]> for Pinned {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

fn read_span(path: PathBuf, start: u64, n: u64) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; n as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// The served copy a /files name means: a versioned file name, or `<item>.<ext>` for
/// whatever version is current. ZIM files are kiwix-serve's to serve.
fn served(app: &App, name: &str) -> Result<Option<Held>, Fail> {
    Ok(mirror::held(&app.db())?.into_iter().find(|h| {
        h.kind != Kind::Zim
            && (h.path.file_name().and_then(|n| n.to_str()) == Some(name) || stable_name(h) == name)
    }))
}

/// A map or document this site serves, with Range support for PMTiles readers.
async fn file(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Response, Fail> {
    let Some(h) = served(&app, &name)? else {
        return Ok((StatusCode::NOT_FOUND, "no such file here").into_response());
    };
    let len = std::fs::metadata(&h.path)?.len();
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let ctype = match h.kind {
        Kind::Doc => content_type(&h.ext),
        _ => "application/octet-stream",
    };
    let etag = h.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let common = [
        (header::ACCEPT_RANGES, "bytes".to_string()),
        (header::ETAG, format!("\"{etag}\"")),
        (header::CONTENT_TYPE, ctype.into()),
        (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".into()),
        (
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            "Content-Range, ETag".into(),
        ),
        // a text file must never be taken for a page, and a page could run script here
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
        (header::CONTENT_SECURITY_POLICY, "sandbox".into()),
    ];
    let (status, start, end) = match parse_range(range, len) {
        Want::Unsatisfiable => {
            return Ok((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{len}"))],
            )
                .into_response());
        }
        Want::Whole if len > WHOLE_MAX => {
            return Ok((
                StatusCode::FORBIDDEN,
                common,
                format!(
                    "This file is {}. Open a map with a PMTiles reader, which fetches only the \
                     parts it shows; whole downloads this big are refused to keep the radio \
                     links free.\n",
                    size_text(len)
                ),
            )
                .into_response());
        }
        Want::Whole => (StatusCode::OK, 0, len.saturating_sub(1)),
        Want::Part(a, b) => (StatusCode::PARTIAL_CONTENT, a, b),
    };
    let n = if len == 0 { 0 } else { end - start + 1 };
    // what all responses together hold in memory is bounded: the permit lives inside the
    // body and is only given back once the body is sent or the client goes away
    let want = n.div_ceil(1024).max(1) as u32;
    let permit = match tokio::time::timeout(
        std::time::Duration::from_secs(20),
        app.reads.clone().acquire_many_owned(want),
    )
    .await
    {
        Ok(p) => p?,
        Err(_) => {
            return Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "10")],
                "Busy sending other files; try again shortly.\n",
            )
                .into_response());
        }
    };
    let path = h.path.clone();
    let data = tokio::task::spawn_blocking(move || read_span(path, start, n)).await??;
    let body = axum::body::Bytes::from_owner(Pinned {
        data,
        _permit: permit,
    });
    let mut resp = (status, common, body).into_response();
    if status == StatusCode::PARTIAL_CONTENT {
        resp.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{len}")).expect("ascii header"),
        );
    }
    Ok(resp)
}

async fn catalog_json(State(app): State<Arc<App>>) -> Result<Json<Value>, Fail> {
    let db = app.db();
    let mut items = vec![];
    for i in catalog::latest(&db)? {
        let confirmed = catalog::confirmations(&db, &i)?;
        items.push(
            json!({"item": i.item, "kind": i.kind.name(), "ext": i.ext, "title": i.title,
                   "version": i.version, "size": i.size, "root": i.root, "published_by": i.call,
                   "published_ms": i.ts, "source": i.source, "sha256": i.sha256,
                   "hash_from": i.hash_from, "confirmations": confirmed, "tiers": i.tiers,
                   "group": i.group}),
        );
    }
    let holds = catalog::holds(&db)?;
    let sites: Vec<Value> = catalog::sites(&db)?
        .into_iter()
        .map(|s| {
            let h: Vec<Value> = holds
                .iter()
                .filter(|h| h.key == s.key)
                .map(|h| {
                    json!({"item": h.item, "version": h.version, "root": h.root,
                                "state": h.state, "detail": h.detail})
                })
                .collect();
            json!({"call": s.call, "node": s.node, "key": s.key, "as_of_ms": s.ts, "tier": s.tier,
                   "fetches": s.fetches, "free_mb": s.free_mb, "alert_delay_ms": s.alert_delay_ms,
                   "alerts_seen": s.alerts_seen, "kiwix_ok_ms": s.kiwix_ok_ms, "holds": h})
        })
        .collect();
    let copies: Vec<Value> = catalog::copies(&db)?
        .into_iter()
        .map(|c| {
            json!({"call": c.call, "node": c.node, "item": c.item, "version": c.version,
                        "root": c.root, "ms": c.ts})
        })
        .collect();
    let here: Vec<Value> = mirror::held(&db)?
        .into_iter()
        .map(|h| {
            json!({"item": h.item, "version": h.version, "root": h.root, "size": h.size,
                        "verified": h.verified, "since_ms": h.since_ms, "checked_ms": h.checked_ms})
        })
        .collect();
    let pending: Vec<Value> = mirror::pending(&db)?
        .into_iter()
        .map(|p| {
            json!({"item": p.item, "version": p.version, "root": p.root, "state": p.state,
                        "detail": p.detail, "since_ms": p.since_ms, "attempts": p.attempts})
        })
        .collect();
    let trips: Vec<Value> = mule::trips(&db)?
        .into_iter()
        .map(|t| {
            json!({"item": t.item, "version": t.version, "island": t.island,
                        "sent_ms": t.sent_ms, "last": t.last})
        })
        .collect();
    Ok(Json(
        json!({"generated": now_ms(), "items": items, "sites": sites, "copies": copies,
                   "here": here, "pending": pending, "mule": trips}),
    ))
}

async fn alerts_json(State(app): State<Arc<App>>) -> Result<Json<Value>, Fail> {
    let (active, sites) = alerts_view(&app)?;
    let alerts: Vec<Value> = active
        .into_iter()
        .map(|a| {
            json!({"id": a.id, "event": a.event, "headline": a.headline, "severity": a.severity,
                   "urgency": a.urgency, "area": a.area, "sender": a.sender, "status": a.status,
                   "sent_ms": a.sent_ms, "expires_ms": a.expires_ms, "ends_ms": a.ends_ms,
                   "description": a.description, "instruction": a.instruction,
                   "relayed_by": a.call})
        })
        .collect();
    let as_of: Vec<Value> = nws::as_of(&sites)
        .into_iter()
        .map(|(place, ok, who)| json!({"place": place, "as_of_ms": ok, "fetched_by": who}))
        .collect();
    let (delay, count) = nws::delivery(&app.db(), now_ms() - 7 * 86_400_000)?;
    Ok(Json(
        json!({"generated": now_ms(), "as_of": as_of, "alerts": alerts,
                   "delivery": {"slowest_ms": delay, "count": count, "goal_ms": ALERT_GOAL_MS}}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range(None, 100), Want::Whole);
        assert_eq!(parse_range(Some("bytes=0-15"), 100), Want::Part(0, 15));
        assert_eq!(parse_range(Some("bytes=90-"), 100), Want::Part(90, 99));
        assert_eq!(parse_range(Some("bytes=-10"), 100), Want::Part(90, 99));
        assert_eq!(parse_range(Some("bytes=50-500"), 100), Want::Part(50, 99));
        assert_eq!(parse_range(Some("bytes=100-"), 100), Want::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=5-1"), 100), Want::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=0-1,5-6"), 100), Want::Unsatisfiable);
        assert_eq!(parse_range(Some("items=0-1"), 100), Want::Whole);
        // a huge open-ended range is cut to one response's worth
        assert_eq!(
            parse_range(Some("bytes=0-"), 10 * MAX_SPAN),
            Want::Part(0, MAX_SPAN - 1)
        );
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(size_text(760_553), "761 kB");
        assert_eq!(size_text(13_400_000_000), "13.4 GB");
    }
}
