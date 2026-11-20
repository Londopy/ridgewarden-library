//! National Weather Service alerts.
//!
//! One site per partition polls `https://api.weather.gov/alerts/active` for its area (see
//! `catalog::nws_turn`), with the User-Agent the API requires and If-None-Match, and logs
//! each alert it hasn't seen as a `library.alert` event on the main log. Alerts are small,
//! so they reach every site within a sync round, ahead of any bulk content. The fetcher's
//! heartbeat lists the ids still active, so a cancelled alert drops off everywhere.
//!
//! The GeoJSON properties are the alert's CAP fields (event, severity, urgency, sent,
//! expires and so on), so there is no second request per alert for the CAP XML.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use ridgewarden_client::mesh::s;
use ridgewarden_client::util::{now_ms, rfc3339_to_ms};
use rusqlite::{Connection, Transaction, params};
use serde_json::{Value, json};

use crate::catalog::{MAX_ACTIVE, Site, cap};

pub struct Alerts;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS alerts (
    id           TEXT PRIMARY KEY,
    ts           INTEGER NOT NULL,
    author_call  TEXT NOT NULL,
    place        TEXT NOT NULL,
    event        TEXT NOT NULL,
    headline     TEXT,
    severity     TEXT,
    urgency      TEXT,
    certainty    TEXT,
    area         TEXT,
    sender       TEXT,
    status       TEXT,
    message_type TEXT,
    sent_ms      INTEGER,
    expires_ms   INTEGER NOT NULL,
    ends_ms      INTEGER,
    description  TEXT,
    instruction  TEXT
);
CREATE INDEX IF NOT EXISTS alerts_expires ON alerts(expires_ms);
CREATE TABLE IF NOT EXISTS alert_refs (
    id  TEXT NOT NULL,
    ref TEXT NOT NULL,
    PRIMARY KEY (id, ref)
);
CREATE INDEX IF NOT EXISTS alert_refs_ref ON alert_refs(ref);
-- when this site's app first saw each alert, for the delivery-time figure; kept across
-- rebuilds, which would otherwise make every old alert look just arrived
CREATE TABLE IF NOT EXISTS alert_arrivals (
    id         TEXT PRIMARY KEY,
    logged_ms  INTEGER NOT NULL,
    arrived_ms INTEGER NOT NULL
);
"#;

/// NWS ids are URNs like "urn:oid:2.49.0.1.840.0.<hash>.001.1".
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".:-_".contains(&b))
}

/// The event payload for one feature of the NWS response, or None if it isn't usable.
pub fn from_feature(f: &Value, place: &str) -> Option<Value> {
    let p = f.get("properties")?;
    let id = s(p, "id").filter(|i| valid_id(i))?;
    let expires = s(p, "expires").filter(|t| rfc3339_to_ms(t).is_some())?;
    let refs: Vec<&str> = p
        .get("references")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| s(r, "identifier"))
        .filter(|i| valid_id(i))
        .take(50)
        .collect();
    Some(json!({
        "id": id,
        "place": place,
        "event": cap(s(p, "event"), 100).unwrap_or_else(|| "Alert".into()),
        "headline": cap(s(p, "headline"), 300),
        "severity": cap(s(p, "severity"), 20),
        "urgency": cap(s(p, "urgency"), 20),
        "certainty": cap(s(p, "certainty"), 20),
        "area": cap(s(p, "areaDesc"), 1000),
        "sender": cap(s(p, "senderName"), 100),
        "status": cap(s(p, "status"), 20),
        "message_type": cap(s(p, "messageType"), 20),
        "sent": s(p, "sent"),
        "expires": expires,
        "ends": s(p, "ends"),
        "description": cap(s(p, "description"), 4000),
        "instruction": cap(s(p, "instruction"), 2000),
        "references": refs,
    }))
}

impl Projection for Alerts {
    fn schema(&self) -> &'static str {
        SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute_batch("DELETE FROM alerts; DELETE FROM alert_refs;")?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        if e.kind != "library.alert" || e.log.is_some() {
            return Ok(());
        }
        let p = &e.payload;
        let (Some(id), Some(expires)) = (
            s(p, "id").filter(|i| valid_id(i)),
            s(p, "expires").and_then(rfc3339_to_ms),
        ) else {
            return Ok(());
        };
        let t = |k: &str| s(p, k).and_then(rfc3339_to_ms);
        // the first copy of an alert wins: a second fetcher logging it again changes nothing
        let added = tx.execute(
            "INSERT OR IGNORE INTO alerts (id, ts, author_call, place, event, headline, severity,
                 urgency, certainty, area, sender, status, message_type, sent_ms, expires_ms,
                 ends_ms, description, instruction)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                id,
                ts,
                e.author.call,
                cap(s(p, "place"), 100).unwrap_or_default(),
                cap(s(p, "event"), 100).unwrap_or_else(|| "Alert".into()),
                cap(s(p, "headline"), 300),
                cap(s(p, "severity"), 20),
                cap(s(p, "urgency"), 20),
                cap(s(p, "certainty"), 20),
                cap(s(p, "area"), 1000),
                cap(s(p, "sender"), 100),
                cap(s(p, "status"), 20),
                cap(s(p, "message_type"), 20),
                t("sent"),
                expires,
                t("ends"),
                cap(s(p, "description"), 4000),
                cap(s(p, "instruction"), 2000),
            ],
        )?;
        if added == 0 {
            return Ok(());
        }
        let now = now_ms();
        if now - ts < 6 * 3_600_000 {
            tx.execute(
                "INSERT OR IGNORE INTO alert_arrivals (id, logged_ms, arrived_ms) VALUES (?1, ?2, ?3)",
                params![id, ts, now.max(ts)],
            )?;
        }
        for r in p
            .get("references")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|r| valid_id(r))
            .take(50)
        {
            tx.execute(
                "INSERT OR IGNORE INTO alert_refs (id, ref) VALUES (?1, ?2)",
                params![id, r],
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Alert {
    pub id: String,
    pub ts: i64,
    pub call: String,
    pub place: String,
    pub event: String,
    pub headline: Option<String>,
    pub severity: Option<String>,
    pub urgency: Option<String>,
    pub area: Option<String>,
    pub sender: Option<String>,
    pub status: Option<String>,
    pub sent_ms: Option<i64>,
    pub expires_ms: i64,
    pub ends_ms: Option<i64>,
    pub description: Option<String>,
    pub instruction: Option<String>,
}

impl State {
    /// What the heartbeat reports as active.
    pub fn listed(&self) -> BTreeSet<String> {
        self.active.union(&self.before).cloned().collect()
    }

    /// Take in one poll's active set (None: unchanged since the last, a 304). Returns
    /// whether what the heartbeat reports changed.
    pub fn observe(&mut self, active: Option<BTreeSet<String>>) -> bool {
        let was = self.listed();
        let now = active.unwrap_or_else(|| self.active.clone());
        self.before = std::mem::replace(&mut self.active, now);
        self.listed() != was
    }
}

/// The slowest an alert took to reach this site's app since `since`, and how many came.
pub fn delivery(conn: &Connection, since: i64) -> Result<(Option<i64>, i64)> {
    Ok(conn.query_row(
        "SELECT max(arrived_ms - logged_ms), count(*) FROM alert_arrivals WHERE arrived_ms >= ?1",
        [since],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

/// Forget alerts that expired a month ago, so the tables don't grow for years.
pub fn prune(conn: &Connection, now: i64) -> Result<usize> {
    let cut = now - 30 * 86_400_000;
    conn.execute(
        "DELETE FROM alert_refs WHERE id IN (SELECT id FROM alerts WHERE expires_ms < ?1)",
        [cut],
    )?;
    conn.execute("DELETE FROM alert_arrivals WHERE arrived_ms < ?1", [cut])?;
    Ok(conn.execute("DELETE FROM alerts WHERE expires_ms < ?1", [cut])?)
}

/// Ids of alerts already logged that haven't expired; anything on the active list is among
/// them if it was logged at all.
pub fn known(conn: &Connection, now: i64) -> Result<BTreeSet<String>> {
    let mut st = conn.prepare("SELECT id FROM alerts WHERE expires_ms > ?1")?;
    Ok(st
        .query_map([now - 86_400_000], |r| r.get(0))?
        .collect::<Result<_, _>>()?)
}

/// The latest word from a live fetcher for each place: when it last heard from the NWS
/// and which alerts were active then.
fn fresh_lists(sites: &[Site], now: i64, quiet_ms: i64) -> Vec<(&str, i64, &[String])> {
    let mut best: Vec<(&str, i64, &[String])> = vec![];
    for s in sites.iter().filter(|s| now - s.ts <= quiet_ms) {
        let (Some(place), Some(ok)) = (s.nws_place.as_deref(), s.nws_ok_ms) else {
            continue;
        };
        match best.iter_mut().find(|b| b.0 == place) {
            Some(b) if b.1 >= ok => {}
            Some(b) => *b = (place, ok, &s.nws_active),
            None => best.push((place, ok, &s.nws_active)),
        }
    }
    best
}

/// Alerts in force: not expired, not replaced or cancelled by a later message, and still
/// on the NWS list the last time a live fetcher looked after the alert arrived.
pub fn active(conn: &Connection, sites: &[Site], now: i64, quiet_ms: i64) -> Result<Vec<Alert>> {
    let lists = fresh_lists(sites, now, quiet_ms);
    let mut st = conn.prepare(
        "SELECT id, ts, author_call, place, event, headline, severity, urgency, area, sender,
                status, sent_ms, expires_ms, ends_ms, description, instruction
         FROM alerts a
         WHERE expires_ms > ?1 AND coalesce(message_type, '') != 'Cancel'
           AND NOT EXISTS (SELECT 1 FROM alert_refs r WHERE r.ref = a.id)
         ORDER BY CASE severity WHEN 'Extreme' THEN 0 WHEN 'Severe' THEN 1 WHEN 'Moderate' THEN 2
                  WHEN 'Minor' THEN 3 ELSE 4 END, sent_ms DESC",
    )?;
    let all: Vec<Alert> = st
        .query_map([now], |r| {
            Ok(Alert {
                id: r.get(0)?,
                ts: r.get(1)?,
                call: r.get(2)?,
                place: r.get(3)?,
                event: r.get(4)?,
                headline: r.get(5)?,
                severity: r.get(6)?,
                urgency: r.get(7)?,
                area: r.get(8)?,
                sender: r.get(9)?,
                status: r.get(10)?,
                sent_ms: r.get(11)?,
                expires_ms: r.get(12)?,
                ends_ms: r.get(13)?,
                description: r.get(14)?,
                instruction: r.get(15)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(all
        .into_iter()
        .filter(|a| {
            !lists
                .iter()
                .any(|(place, ok, ids)| *place == a.place && *ok > a.ts && !ids.contains(&a.id))
        })
        .collect())
}

/// What this site knows from its own polling, reported in its heartbeat.
#[derive(Default, Clone, Debug)]
pub struct State {
    pub etag: Option<String>,
    pub ok_ms: Option<i64>,
    pub error: Option<String>,
    pub active: BTreeSet<String>,
    /// the poll before; an alert drops off only when two polls in a row leave it out
    pub before: BTreeSet<String>,
    /// polling right now (it is this site's turn)
    pub fetching: bool,
}

/// One poll's outcome: None if nothing changed since the last one (304).
pub struct Poll {
    pub etag: Option<String>,
    pub features: Option<Vec<Value>>,
}

pub async fn poll(
    http: &reqwest::Client,
    url: &str,
    query: (&str, &str),
    etag: Option<&str>,
) -> Result<Poll> {
    let mut req = http
        .get(url)
        .query(&[query])
        .header(reqwest::header::ACCEPT, "application/geo+json")
        .timeout(Duration::from_secs(30));
    if let Some(e) = etag {
        req = req.header(reqwest::header::IF_NONE_MATCH, e);
    }
    let r = req.send().await?;
    let etag = r
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if r.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(Poll {
            etag,
            features: None,
        });
    }
    if !r.status().is_success() {
        let status = r.status();
        let body: String = r
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        bail!("api.weather.gov answered {status}: {}", body.trim());
    }
    let v: Value = r
        .json()
        .await
        .context("api.weather.gov sent something that isn't JSON")?;
    let features = v
        .get("features")
        .and_then(Value::as_array)
        .context("api.weather.gov sent no features list")?;
    Ok(Poll {
        etag,
        features: Some(features.iter().take(MAX_ACTIVE).cloned().collect()),
    })
}

/// Turn a poll into the alerts to log (ones nobody has logged yet) and the active set.
pub fn digest(
    features: &[Value],
    place: &str,
    seen: impl Fn(&str) -> bool,
) -> (Vec<Value>, BTreeSet<String>) {
    let mut new = vec![];
    let mut active = BTreeSet::new();
    for f in features {
        let Some(a) = from_feature(f, place) else {
            continue;
        };
        let id = a["id"].as_str().unwrap_or_default().to_string();
        if !seen(&id) && !active.contains(&id) {
            new.push(a);
        }
        active.insert(id);
    }
    (new, active)
}

/// The "as of" for the alerts page: (place, last good poll, who polled).
pub fn as_of(sites: &[Site]) -> Vec<(String, i64, String)> {
    let mut v: Vec<(String, i64, String)> = vec![];
    for s in sites {
        let (Some(place), Some(ok)) = (&s.nws_place, s.nws_ok_ms) else {
            continue;
        };
        let who = if s.node.is_empty() {
            s.call.clone()
        } else {
            format!("{} at {}", s.call, s.node)
        };
        match v.iter_mut().find(|x| &x.0 == place) {
            Some(x) if x.1 >= ok => {}
            Some(x) => *x = (place.clone(), ok, who),
            None => v.push((place.clone(), ok, who)),
        }
    }
    v
}

pub fn quiet_note(last_ok: Option<i64>, quiet_ms: i64) -> Option<String> {
    match last_ok {
        None => Some(
            "No site has fetched NWS alerts yet. A site with internet needs [nws] enabled.".into(),
        ),
        Some(t) if now_ms() - t > quiet_ms => Some(format!(
            "No site has fetched NWS alerts for {}; what is shown may be out of date.",
            ridgewarden_client::util::human_duration(now_ms() - t)
        )),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Sites;
    use ridgewarden_client::mesh::testing::Fake;

    const HOUR: i64 = 3_600_000;
    const NOW: i64 = 1_800_000_000_000;

    fn t(ms: i64) -> String {
        ridgewarden_client::util::ms_to_rfc3339(ms)
    }

    fn feature(id: &str, expires: i64, refs: &[&str], kind: &str) -> Value {
        let refs: Vec<Value> = refs
            .iter()
            .map(|r| json!({"@id": format!("https://api.weather.gov/alerts/{r}"), "identifier": r, "sender": "w-nws.webmaster@noaa.gov", "sent": t(NOW - HOUR)}))
            .collect();
        json!({"id": format!("https://api.weather.gov/alerts/{id}"), "type": "Feature",
               "properties": {"id": id, "areaDesc": "Test Coast", "sent": t(NOW - HOUR),
                   "effective": t(NOW - HOUR), "expires": t(expires), "ends": null,
                   "status": "Actual", "messageType": kind, "severity": "Severe",
                   "certainty": "Likely", "urgency": "Expected", "event": "High Wind Warning",
                   "senderName": "NWS Test Office", "headline": "High Wind Warning for the test coast",
                   "description": "Gusts to 60 mph.", "instruction": "Secure loose objects.",
                   "references": refs}})
    }

    fn fake() -> Fake {
        Fake::new(vec![Box::new(Alerts), Box::new(Sites)])
    }

    fn log(f: &mut Fake, feat: &Value, ts: i64) {
        f.add("library.alert", from_feature(feat, "area=CA").unwrap(), ts);
    }

    #[test]
    fn expired_and_replaced_alerts_drop_off() {
        let mut f = fake();
        log(
            &mut f,
            &feature("urn:oid:1.old", NOW + HOUR, &[], "Alert"),
            NOW - HOUR,
        );
        log(
            &mut f,
            &feature("urn:oid:2.gone", NOW - 1, &[], "Alert"),
            NOW - 2 * HOUR,
        );
        log(
            &mut f,
            &feature(
                "urn:oid:3.new",
                NOW + 2 * HOUR,
                &["urn:oid:1.old"],
                "Update",
            ),
            NOW - 1000,
        );
        log(
            &mut f,
            &feature("urn:oid:4.x", NOW + HOUR, &[], "Alert"),
            NOW - HOUR,
        );
        log(
            &mut f,
            &feature("urn:oid:5.cancel", NOW + HOUR, &["urn:oid:4.x"], "Cancel"),
            NOW - 500,
        );
        let a = active(&f.conn, &[], NOW, 900_000).unwrap();
        let ids: Vec<&str> = a.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["urn:oid:3.new"]);
        assert_eq!(a[0].expires_ms, NOW + 2 * HOUR);
    }

    #[test]
    fn a_second_fetcher_logging_the_same_alert_changes_nothing() {
        let mut f = fake();
        log(
            &mut f,
            &feature("urn:oid:1.a", NOW + HOUR, &[], "Alert"),
            NOW - HOUR,
        );
        log(
            &mut f,
            &feature("urn:oid:1.a", NOW + HOUR, &[], "Alert"),
            NOW - 10,
        );
        let a = active(&f.conn, &[], NOW, 900_000).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].ts, NOW - HOUR);
    }

    #[test]
    fn dropped_from_the_nws_list_means_gone_but_only_after_it_arrived() {
        let mut f = fake();
        log(
            &mut f,
            &feature("urn:oid:1.a", NOW + HOUR, &[], "Alert"),
            NOW - 60_000,
        );
        log(
            &mut f,
            &feature("urn:oid:2.b", NOW + HOUR, &[], "Alert"),
            NOW - 1000,
        );
        f.add(
            "library.site",
            json!({"node": "tst-a", "nws": {"place": "area=CA", "ok_ms": NOW - 30_000, "active": ["urn:oid:9.other"]}}),
            NOW - 30_000,
        );
        let sites = crate::catalog::sites(&f.conn).unwrap();
        let ids: Vec<String> = active(&f.conn, &sites, NOW, 900_000)
            .unwrap()
            .into_iter()
            .map(|a| a.id)
            .collect();
        // 1.a was gone from the list polled after it arrived; 2.b arrived after that poll
        assert_eq!(ids, ["urn:oid:2.b"]);
        // a fetcher that has gone quiet doesn't get to hide anything
        let ids = active(&f.conn, &sites, NOW + 2_000_000, 900_000).unwrap();
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn one_empty_reply_does_not_clear_the_list() {
        let mut st = State::default();
        let a: BTreeSet<String> = ["urn:oid:1.a".to_string()].into();
        assert!(st.observe(Some(a.clone())));
        // unchanged (304) twice: still listed
        assert!(!st.observe(None));
        assert!(!st.observe(None));
        assert_eq!(st.listed(), a);
        // the NWS answers once with nothing
        assert!(
            !st.observe(Some(BTreeSet::new())),
            "one empty poll changes nothing"
        );
        assert_eq!(st.listed(), a);
        assert!(st.observe(Some(BTreeSet::new())), "gone after two");
        assert!(st.listed().is_empty());
    }

    #[test]
    fn delivery_times_and_pruning() {
        let mut f = fake();
        let now = now_ms();
        log(
            &mut f,
            &feature("urn:oid:1.a", now - 40 * 86_400_000, &[], "Alert"),
            now - 45_000,
        );
        let (slowest, n) = delivery(&f.conn, now - 3_600_000).unwrap();
        assert_eq!(n, 1);
        assert!(slowest.unwrap() >= 45_000);
        // an old alert replayed by a rebuild is not counted as just arrived
        log(
            &mut f,
            &feature("urn:oid:2.b", now + HOUR, &[], "Alert"),
            now - 30 * 86_400_000,
        );
        assert_eq!(delivery(&f.conn, now - 3_600_000).unwrap().1, 1);
        assert_eq!(prune(&f.conn, now).unwrap(), 1);
        assert_eq!(known(&f.conn, now).unwrap().len(), 1);
    }

    #[test]
    fn digest_skips_known_and_bad_features() {
        let feats = vec![
            feature("urn:oid:1.a", NOW + HOUR, &[], "Alert"),
            feature("urn:oid:2.b", NOW + HOUR, &[], "Alert"),
            json!({"properties": {"id": "<script>", "expires": t(NOW)}}),
            json!({"properties": {"id": "urn:oid:3.c"}}),
        ];
        let (new, active) = digest(&feats, "area=CA", |id| id == "urn:oid:1.a");
        assert_eq!(new.len(), 1);
        assert_eq!(new[0]["id"], "urn:oid:2.b");
        assert_eq!(new[0]["references"], json!([]));
        assert_eq!(active.len(), 2);
    }

    #[tokio::test]
    async fn polls_with_etag_and_understands_304() {
        use axum::http::{HeaderMap, StatusCode};
        use axum::response::IntoResponse;
        let app = axum::Router::new().route(
            "/alerts/active",
            axum::routing::get(
                |h: HeaderMap, q: axum::extract::Query<Vec<(String, String)>>| async move {
                    assert_eq!(q.0, vec![("area".to_string(), "CA".to_string())]);
                    if h.get("if-none-match").is_some_and(|v| v == "W/\"one\"") {
                        return StatusCode::NOT_MODIFIED.into_response();
                    }
                    let body = json!({"type": "FeatureCollection",
                    "features": [feature("urn:oid:1.a", NOW + HOUR, &[], "Alert")]});
                    ([("etag", "W/\"one\"")], axum::Json(body)).into_response()
                },
            ),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/alerts/active", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let http = reqwest::Client::new();
        let p = poll(&http, &url, ("area", "CA"), None).await.unwrap();
        assert_eq!(p.etag.as_deref(), Some("W/\"one\""));
        assert_eq!(p.features.unwrap().len(), 1);
        let p = poll(&http, &url, ("area", "CA"), Some("W/\"one\""))
            .await
            .unwrap();
        assert!(p.features.is_none());
    }
}
