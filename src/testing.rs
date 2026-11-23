//! A stand-in for the core's local API, and tests of the flows that go through it:
//! mirroring and swapping, publishing, and serving files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::app::App;
use crate::config::Config;

#[derive(Default)]
pub struct Core {
    /// root -> (content, complete, wanted)
    pub blobs: HashMap<String, (Vec<u8>, bool, bool)>,
    pub appended: Vec<(String, Value)>,
    pub unwanted: Vec<String>,
    /// each want: (root, the peers it asked for first)
    pub wants: Vec<(String, Vec<String>)>,
    /// what /v1/peers answers: (url, call)
    pub peers: Vec<(String, String)>,
    pub export_fails: bool,
    /// a manifest that claims this size instead of the real one
    pub lie_size: Option<u64>,
    /// only a station the core doesn't trust has published a manifest
    pub untrusted: bool,
    pub exports: PathBuf,
    pub imports: PathBuf,
}

type S = State<Arc<Mutex<Core>>>;

fn root_of(data: &[u8]) -> String {
    crate::sha256::hex_of(data)
}

async fn whoami() -> Json<Value> {
    Json(json!({"call": "N0TEST", "key": "ab".repeat(32), "node": "tst-b"}))
}

async fn status(State(c): S, Query(q): Query<HashMap<String, String>>) -> Response {
    let c = c.lock().unwrap();
    let root = q.get("root").cloned().unwrap_or_default();
    match c.blobs.get(&root) {
        Some((d, complete, wanted)) => Json(json!({"root": root, "name": "x",
            "size": c.lie_size.unwrap_or(d.len() as u64), "complete": complete, "wanted": wanted,
            "priority": 8, "author": "ab".repeat(32), "trusted": !c.untrusted}))
        .into_response(),
        None => (StatusCode::NOT_FOUND, "no such blob").into_response(),
    }
}

async fn want(State(c): S, Json(b): Json<Value>) -> Response {
    let mut c = c.lock().unwrap();
    let root = b["root"].as_str().unwrap_or("").to_string();
    let peers: Vec<String> = b["peers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str().map(str::to_string))
        .collect();
    // the real core refuses the whole request over one address it won't take
    if peers.len() > crate::fanout::MAX_SOURCES
        || peers.iter().any(|p| crate::catalog::peer_url(p).is_none())
    {
        return (StatusCode::BAD_REQUEST, "not a peer address").into_response();
    }
    c.wants.push((root.clone(), peers));
    match c.blobs.get_mut(&root) {
        Some(e) => {
            e.2 = true;
            Json(json!({"wanted": b["root"]})).into_response()
        }
        None => (StatusCode::NOT_FOUND, "no manifest for that blob yet").into_response(),
    }
}

async fn peers(State(c): S) -> Json<Value> {
    let c = c.lock().unwrap();
    let list: Vec<Value> = c
        .peers
        .iter()
        .map(|(u, call)| json!({"url": u, "call": call, "last_ok": null, "last_err": null}))
        .collect();
    Json(json!({"peers": list}))
}

async fn unwant(State(c): S, Json(b): Json<Value>) -> Json<Value> {
    let mut c = c.lock().unwrap();
    let root = b["root"].as_str().unwrap_or("").to_string();
    c.unwanted.push(root);
    Json(json!({"freed": 0}))
}

async fn export(State(c): S, Json(b): Json<Value>) -> Response {
    let c = c.lock().unwrap();
    if c.export_fails {
        return (StatusCode::CONFLICT, "chunk damaged on disk").into_response();
    }
    let Some((d, true, _)) = c.blobs.get(b["root"].as_str().unwrap_or("")) else {
        return (StatusCode::CONFLICT, "blob is not complete").into_response();
    };
    let path = c.exports.join(b["name"].as_str().unwrap_or("x"));
    std::fs::write(&path, d).unwrap();
    Json(json!({"exported": path})).into_response()
}

async fn add(State(c): S, Json(b): Json<Value>) -> Json<Value> {
    let mut c = c.lock().unwrap();
    let d = std::fs::read(c.imports.join(b["file"].as_str().unwrap())).unwrap();
    let root = root_of(&d);
    c.blobs.insert(root.clone(), (d, true, true));
    Json(json!({"root": root}))
}

async fn append(State(c): S, Json(b): Json<Value>) -> Json<Value> {
    let kind = b["kind"].as_str().unwrap_or("").to_string();
    let payload = b["payload"].clone();
    let mut c = c.lock().unwrap();
    c.appended.push((kind.clone(), payload.clone()));
    let n = c.appended.len();
    Json(
        json!({"id": format!("{n:064x}"), "author": {"call": "N0TEST", "key": "ab".repeat(32)},
                "seq": n, "prev": null, "lamport": n, "wall_time": "2026-11-20T00:00:00Z",
                "mono_time": 0, "kind": kind, "payload": payload, "sig": "00"}),
    )
}

/// A fake core on a local port, and an app talking to it with its data in `dir`.
pub async fn app(dir: &Path, extra: &str) -> (Arc<App>, Arc<Mutex<Core>>) {
    let core_dir = dir.join("core");
    let exports = core_dir.join("exports");
    let imports = core_dir.join("imports");
    std::fs::create_dir_all(&exports).unwrap();
    std::fs::create_dir_all(&imports).unwrap();
    std::fs::write(core_dir.join("local.token"), "t").unwrap();
    let state = Arc::new(Mutex::new(Core {
        exports,
        imports: imports.clone(),
        ..Default::default()
    }));
    let r = Router::new()
        .route("/v1/whoami", get(whoami))
        .route("/v1/blobs", get(status))
        .route("/local/v1/blob/want", post(want))
        .route("/v1/peers", get(peers))
        .route("/local/v1/blob/unwant", post(unwant))
        .route("/local/v1/blob/export", post(export))
        .route("/local/v1/blob/add", post(add))
        .route("/local/v1/append", post(append))
        .with_state(state.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    let p = |p: &Path| p.to_string_lossy().replace('\\', "/");
    let text = format!(
        "data_dir = '{}'\n{extra}\n[core]\nurl = '{url}'\ntoken_file = '{}'\nimports_dir = '{}'\n\
         [mirror]\nkeep_free_mb = 0\nfanout = false\n",
        p(&dir.join("lib")),
        p(&core_dir.join("local.token")),
        p(&imports)
    );
    let cfg: Config = toml::from_str(&text).unwrap();
    cfg.validate().unwrap();
    (Arc::new(App::new(cfg).unwrap()), state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Items;
    use crate::content::Kind;
    use crate::mirror::{self, Held};
    use ridgewarden_client::core::Projection;
    use ridgewarden_client::mesh::testing::Fake;
    use ridgewarden_client::util::now_ms;

    fn zim(seed: u8) -> Vec<u8> {
        let mut d = vec![0x5a, 0x49, 0x4d, 0x04];
        d.extend((0..5000u32).map(|i| (i as u8).wrapping_mul(seed)));
        d
    }

    /// Log a `library.item` for `data` into the app's view, as a fetcher would.
    fn publish(app: &App, data: &[u8], version: &str, sha: Option<String>, ts: i64) -> String {
        let root = root_of(data);
        let e = Fake::new(vec![]).add(
            "library.item",
            json!({"item": "wiki", "kind": "zim", "title": "Wikipedia", "version": version,
                   "size": data.len(), "root": root, "source": "test",
                   "sha256": sha.unwrap_or_else(|| crate::sha256::hex_of(data)),
                   "hash_from": "https://example.org/x.sha256"}),
            ts,
        );
        let mut db = app.db();
        let tx = db.transaction().unwrap();
        Items.apply(&tx, &e, ts).unwrap();
        tx.commit().unwrap();
        root
    }

    async fn serve_old(app: &App) -> Held {
        let old = zim(3);
        let path = app.cfg.content_dir().join("wiki_old.zim");
        std::fs::write(&path, &old).unwrap();
        let h = Held {
            item: "wiki".into(),
            kind: Kind::Zim,
            ext: "zim".into(),
            version: "2026-08".into(),
            root: root_of(&old),
            path,
            size: old.len() as u64,
            sha256: crate::sha256::hex_of(&old),
            verified: "publisher".into(),
            event_ts: 1000,
            since_ms: 1000,
            checked_ms: now_ms(),
        };
        mirror::install(app, &h, false).await.unwrap();
        h
    }

    #[tokio::test]
    async fn a_new_version_is_swapped_in_only_after_it_checks_out() {
        let d = tempfile::tempdir().unwrap();
        let (app, core) = app(d.path(), "").await;
        let old = serve_old(&app).await;
        let new = zim(7);
        let root = publish(&app, &new, "2026-10", None, now_ms());
        core.lock()
            .unwrap()
            .blobs
            .insert(root.clone(), (new.clone(), false, false));
        mirror::round(&app).await.unwrap();
        assert!(core.lock().unwrap().blobs[&root].2, "the blob was wanted");
        // still fetching: the old copy keeps serving
        mirror::round(&app).await.unwrap();
        assert_eq!(
            mirror::held_one(&app.db(), "wiki").unwrap().unwrap().root,
            old.root
        );
        core.lock().unwrap().blobs.get_mut(&root).unwrap().1 = true;
        mirror::round(&app).await.unwrap();
        let h = mirror::held_one(&app.db(), "wiki").unwrap().unwrap();
        assert_eq!(
            (h.version.as_str(), h.root.as_str()),
            ("2026-10", root.as_str())
        );
        assert_eq!(std::fs::read(&h.path).unwrap(), new);
        assert!(!old.path.exists(), "the replaced copy is removed");
        let c = core.lock().unwrap();
        assert_eq!(
            c.unwanted,
            std::slice::from_ref(&old.root),
            "the old blob is let go"
        );
        assert!(
            c.appended
                .iter()
                .any(|(k, p)| k == "library.copied" && p["root"] == root)
        );
    }

    #[tokio::test]
    async fn a_file_damaged_on_disk_is_found_and_copied_again() {
        let d = tempfile::tempdir().unwrap();
        let (app, core) = app(d.path(), "").await;
        let data = zim(4);
        let root = publish(&app, &data, "2026-10", None, now_ms());
        core.lock()
            .unwrap()
            .blobs
            .insert(root.clone(), (data.clone(), true, false));
        mirror::round(&app).await.unwrap();
        mirror::round(&app).await.unwrap();
        let h = mirror::held_one(&app.db(), "wiki").unwrap().unwrap();
        // a month later one byte has gone bad
        app.db()
            .execute("UPDATE held SET checked_ms = 0", [])
            .unwrap();
        let mut bad = data.clone();
        bad[100] ^= 1;
        std::fs::write(&h.path, &bad).unwrap();
        mirror::scrub(&app).await.unwrap();
        assert!(
            mirror::held_one(&app.db(), "wiki").unwrap().is_none(),
            "no longer served"
        );
        mirror::round(&app).await.unwrap();
        let again = mirror::held_one(&app.db(), "wiki").unwrap().unwrap();
        assert_eq!(std::fs::read(&again.path).unwrap(), data);
        // a whole file is left alone
        mirror::scrub(&app).await.unwrap();
        assert!(mirror::held_one(&app.db(), "wiki").unwrap().is_some());
    }

    /// Run an event from another station through the app's views.
    fn from_station(app: &App, key: char, call: &str, kind: &str, payload: Value, ts: i64) {
        let mut e = Fake::new(vec![]).add(kind, payload, ts);
        // 'Z' stands for this app's own core, whose key the fake whoami gives
        e.author.key = if key == 'Z' {
            "ab".repeat(32)
        } else {
            key.to_string().repeat(64)
        };
        e.author.call = call.into();
        e.id = format!("{key}{kind}{ts}");
        let mut db = app.db();
        let tx = db.transaction().unwrap();
        for p in crate::app::PROJECTIONS {
            p.apply(&tx, &e, ts).unwrap();
        }
        tx.commit().unwrap();
    }

    #[tokio::test]
    async fn the_nearest_site_that_has_it_is_asked_first() {
        let d = tempfile::tempdir().unwrap();
        let (app, core) = app(d.path(), "").await;
        let now = now_ms();
        // tst-b (this site) - tst-near - tst-mid - tst-far (the fetcher)
        for (a, b) in [
            ("tst-b", "tst-near"),
            ("tst-near", "tst-mid"),
            ("tst-mid", "tst-far"),
        ] {
            app.db()
                .execute(
                    "INSERT INTO link_samples (event_id, ts, node, neighbor) VALUES ('x', ?1, ?2, ?3)",
                    rusqlite::params![now, a, b],
                )
                .unwrap();
        }
        let data = zim(8);
        let root = root_of(&data);
        let item = json!({"item": "wiki", "kind": "zim", "title": "Wikipedia", "version": "2026-10",
                          "size": data.len(), "root": root, "source": "test",
                          "sha256": crate::sha256::hex_of(&data)});
        from_station(&app, 'f', "N0FAR", "library.item", item, now - 5000);
        from_station(
            &app,
            'f',
            "N0FAR",
            "library.site",
            json!({"node": "tst-far", "core_url": "http://far.local.mesh:8790", "holds": []}),
            now - 4000,
        );
        // a station claiming an address this core doesn't sync with is not named
        from_station(
            &app,
            'x',
            "N0ODD",
            "library.site",
            json!({"node": "tst-near", "core_url": "http://elsewhere.example:8790",
                   "holds": [{"item": "wiki", "version": "2026-10", "root": root, "state": "serving"}]}),
            now - 3500,
        );
        // this station's own heartbeat says it holds it too (a stale view); never ask itself
        from_station(
            &app,
            'Z',
            "N0TEST",
            "library.site",
            json!({"node": "tst-b", "core_url": "http://b.local.mesh:8790",
                   "holds": [{"item": "wiki", "version": "2026-10", "root": root, "state": "serving"}]}),
            now - 3500,
        );
        core.lock().unwrap().peers = vec![
            ("http://mid.local.mesh:8790".into(), "N0MID".into()),
            ("http://far.local.mesh:8790".into(), "N0FAR".into()),
            ("http://b.local.mesh:8790".into(), "N0TEST".into()),
        ];
        // the middle site has it and says where its core is; the near one is known to this
        // core only as a sync peer with its callsign
        from_station(
            &app,
            'm',
            "N0MID",
            "library.site",
            json!({"node": "tst-mid", "core_url": "http://mid.local.mesh:8790",
                   "holds": [{"item": "wiki", "version": "2026-10", "root": root, "state": "serving"}]}),
            now - 3000,
        );
        core.lock()
            .unwrap()
            .blobs
            .insert(root.clone(), (data, false, false));
        mirror::round(&app).await.unwrap();
        assert_eq!(
            core.lock().unwrap().wants.last().unwrap().1,
            ["http://mid.local.mesh:8790", "http://far.local.mesh:8790"]
        );
        // the near site finishes its copy: the next round asks it first instead
        from_station(
            &app,
            'n',
            "N0NEAR",
            "library.copied",
            json!({"item": "wiki", "version": "2026-10", "root": root, "node": "tst-near"}),
            now - 1000,
        );
        core.lock()
            .unwrap()
            .peers
            .push(("http://10.0.0.7:8790".into(), "N0NEAR".into()));
        mirror::round(&app).await.unwrap();
        assert_eq!(
            core.lock().unwrap().wants.last().unwrap().1,
            [
                "http://10.0.0.7:8790",
                "http://mid.local.mesh:8790",
                "http://far.local.mesh:8790"
            ]
        );
        let n = core.lock().unwrap().wants.len();
        mirror::round(&app).await.unwrap();
        assert_eq!(
            core.lock().unwrap().wants.len(),
            n,
            "nothing new to say, so no new want"
        );
    }

    #[test]
    fn peers_are_matched_by_callsign_only_when_unambiguous() {
        let peers = vec![
            ("http://a:8790".to_string(), Some("N0AAA".to_string())),
            ("http://b:8790".to_string(), Some("N0BBB".to_string())),
            ("http://c:8790".to_string(), Some("N0BBB".to_string())),
        ];
        assert_eq!(
            mirror::by_call(&peers, "N0AAA").as_deref(),
            Some("http://a:8790")
        );
        assert_eq!(mirror::by_call(&peers, "N0BBB"), None);
        assert_eq!(mirror::by_call(&peers, "N0ZZZ"), None);
    }

    #[tokio::test]
    async fn a_copy_that_fails_its_check_is_never_served() {
        let d = tempfile::tempdir().unwrap();
        let (app, core) = app(d.path(), "").await;
        let old = serve_old(&app).await;
        let new = zim(9);
        let root = publish(&app, &new, "2026-10", Some("00".repeat(32)), now_ms());
        core.lock()
            .unwrap()
            .blobs
            .insert(root.clone(), (new, true, false));
        mirror::round(&app).await.unwrap();
        mirror::round(&app).await.unwrap();
        let h = mirror::held_one(&app.db(), "wiki").unwrap().unwrap();
        assert_eq!(h.root, old.root);
        assert!(old.path.exists());
        let p = mirror::pending(&app.db()).unwrap();
        assert_eq!(p[0].state, "failed");
        assert!(p[0].detail.as_deref().unwrap().contains("publisher says"));
        assert_eq!(std::fs::read_dir(app.cfg.content_dir()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn a_manifest_whose_size_disagrees_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let (app, core) = app(d.path(), "").await;
        let new = zim(5);
        let root = publish(&app, &new, "2026-10", None, now_ms());
        {
            let mut c = core.lock().unwrap();
            c.blobs.insert(root.clone(), (new, false, false));
            c.lie_size = Some(50_000_000_000);
        }
        mirror::round(&app).await.unwrap();
        let p = mirror::pending(&app.db()).unwrap();
        assert_eq!(p[0].state, "failed", "{:?}", p[0].detail);
        assert_eq!(core.lock().unwrap().unwanted, [root], "let go again");
    }

    #[tokio::test]
    async fn failed_exports_back_off() {
        let d = tempfile::tempdir().unwrap();
        let (app, core) = app(d.path(), "").await;
        let new = zim(5);
        let root = publish(&app, &new, "2026-10", None, now_ms());
        {
            let mut c = core.lock().unwrap();
            c.blobs.insert(root, (new, true, false));
            c.export_fails = true;
        }
        mirror::round(&app).await.unwrap();
        mirror::round(&app).await.unwrap();
        let p = mirror::pending(&app.db()).unwrap().remove(0);
        assert_eq!((p.state.as_str(), p.attempts), ("fetching", 1));
        assert!(p.next_ms > now_ms() + 60_000);
        // the next round leaves it alone instead of exporting again
        mirror::round(&app).await.unwrap();
        assert_eq!(mirror::pending(&app.db()).unwrap()[0].attempts, 1);
        // when it is due again it tries again, waits longer, and never gives up
        for n in 2..=12 {
            app.db()
                .execute("UPDATE pending SET next_ms = 0", [])
                .unwrap();
            mirror::round(&app).await.unwrap();
            let p = mirror::pending(&app.db()).unwrap().remove(0);
            assert_eq!((p.state.as_str(), p.attempts), ("fetching", n));
        }
        let p = mirror::pending(&app.db()).unwrap().remove(0);
        assert!(p.next_ms - now_ms() > 5 * 3_600_000, "backed off to hours");
        core.lock().unwrap().export_fails = false;
        app.db()
            .execute("UPDATE pending SET next_ms = 0", [])
            .unwrap();
        mirror::round(&app).await.unwrap();
        assert!(mirror::held_one(&app.db(), "wiki").unwrap().is_some());
    }

    #[tokio::test]
    async fn a_superseded_version_still_on_its_way_is_let_go() {
        let d = tempfile::tempdir().unwrap();
        let (app, core) = app(d.path(), "").await;
        let (v1, v2) = (zim(5), zim(6));
        let r1 = publish(&app, &v1, "2026-09", None, now_ms() - 1000);
        core.lock()
            .unwrap()
            .blobs
            .insert(r1.clone(), (v1, false, false));
        mirror::round(&app).await.unwrap();
        assert!(core.lock().unwrap().blobs[&r1].2);
        let r2 = publish(&app, &v2, "2026-10", None, now_ms());
        core.lock()
            .unwrap()
            .blobs
            .insert(r2.clone(), (v2, false, false));
        mirror::round(&app).await.unwrap();
        let c = core.lock().unwrap();
        assert!(c.blobs[&r2].2, "the new version is wanted");
        assert_eq!(c.unwanted, [r1], "the old one is released");
    }

    #[tokio::test]
    async fn two_copies_that_only_fit_one_at_a_time_dont_start_together() {
        let d = tempfile::tempdir().unwrap();
        let (app, core) = app(d.path(), "").await;
        let mut app_cfg = app.cfg.clone();
        app_cfg.mirror.total_mb = 1;
        let app = std::sync::Arc::new(App::new(app_cfg).unwrap());
        let (a, b) = (
            vec![0x5a, 0x49, 0x4d, 0x04]
                .into_iter()
                .chain(std::iter::repeat_n(1, 600_000))
                .collect::<Vec<u8>>(),
            vec![0x5a, 0x49, 0x4d, 0x04]
                .into_iter()
                .chain(std::iter::repeat_n(2, 600_000))
                .collect::<Vec<u8>>(),
        );
        let ra = publish(&app, &a, "2026-10", None, now_ms());
        let e = Fake::new(vec![]).add(
            "library.item",
            json!({"item": "other", "kind": "zim", "title": "Other", "version": "2026-10",
                   "size": b.len(), "root": root_of(&b), "source": "test",
                   "sha256": crate::sha256::hex_of(&b)}),
            now_ms(),
        );
        {
            let mut db = app.db();
            let tx = db.transaction().unwrap();
            Items.apply(&tx, &e, now_ms()).unwrap();
            tx.commit().unwrap();
        }
        {
            let mut c = core.lock().unwrap();
            c.blobs.insert(ra, (a, false, false));
            c.blobs.insert(root_of(&b), (b, false, false));
        }
        mirror::round(&app).await.unwrap();
        let states: Vec<String> = mirror::pending(&app.db())
            .unwrap()
            .into_iter()
            .map(|p| p.state)
            .collect();
        assert_eq!(
            states.iter().filter(|s| *s == "fetching").count(),
            1,
            "{states:?}"
        );
        assert!(states.contains(&"waiting".to_string()));
    }

    #[tokio::test]
    async fn a_manifest_from_an_untrusted_station_is_not_fetched() {
        let d = tempfile::tempdir().unwrap();
        let (app, core) = app(d.path(), "").await;
        let new = zim(5);
        let root = publish(&app, &new, "2026-10", None, now_ms());
        {
            let mut c = core.lock().unwrap();
            c.blobs.insert(root.clone(), (new, false, false));
            c.untrusted = true;
        }
        mirror::round(&app).await.unwrap();
        assert!(!core.lock().unwrap().blobs[&root].2);
        let p = mirror::pending(&app.db()).unwrap().remove(0);
        assert!(p.detail.unwrap().contains("untrusted"));
    }

    #[tokio::test]
    async fn an_operator_file_is_published_once_it_has_settled_and_sent_by_mule() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("phones.csv");
        std::fs::write(&f, "name,phone\nShelter desk,555-0100\n").unwrap();
        let extra = format!(
            "[[item]]\nid = 'phones'\nkind = 'doc'\ntitle = 'Shelter phones'\nfile = '{}'\nmax_mb = 1\n\
             license = 'For agency use'\nattribution = 'County office'\n\
             [mule]\nisland = 'north'\nto = ['south']\n",
            f.to_string_lossy().replace('\\', "/")
        );
        let (app, core) = app(d.path(), &extra).await;
        crate::fetch::round(&app).await.unwrap();
        assert!(
            core.lock().unwrap().appended.is_empty(),
            "not on first sight"
        );
        // two minutes later, unchanged
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(300);
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(past)
            .unwrap();
        crate::fetch::round(&app).await.unwrap();
        crate::fetch::round(&app).await.unwrap();
        let c = core.lock().unwrap();
        let kinds: Vec<&str> = c.appended.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(kinds, ["library.item", "mule.msg"]);
        assert_eq!(c.appended[0].1["ext"], "csv");
        assert_eq!(c.appended[0].1["license"], "For agency use");
        assert_eq!(c.appended[0].1["attribution"], "County office");
        assert_eq!(c.appended[1].1["to"], "south");
        let h = mirror::held_one(&app.db(), "phones").unwrap().unwrap();
        assert_eq!(h.kind, Kind::Doc);
        assert!(f.exists(), "the operator's file is copied, not taken");
    }

    #[tokio::test]
    async fn files_are_served_by_range_and_by_their_stable_name() {
        let d = tempfile::tempdir().unwrap();
        let (app, _core) = app(d.path(), "").await;
        let mut data = b"PMTiles\x03".to_vec();
        data.extend(std::iter::repeat_n(7u8, 100_000));
        let path = app
            .cfg
            .content_dir()
            .join("map_2026-11-18_abcdabcd.pmtiles");
        std::fs::write(&path, &data).unwrap();
        let big = app.cfg.content_dir().join("plan_2026-11-18_abcdabcd.pdf");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(crate::web::WHOLE_MAX + 1)
            .unwrap();
        for (item, kind, ext, p) in [
            ("map", Kind::Pmtiles, "pmtiles", &path),
            ("plan", Kind::Doc, "pdf", &big),
        ] {
            let h = Held {
                item: item.into(),
                kind,
                ext: ext.into(),
                version: "2026-11-18".into(),
                root: "abcdabcd".repeat(8),
                path: p.clone(),
                size: std::fs::metadata(p).unwrap().len(),
                sha256: String::new(),
                verified: "fetcher".into(),
                event_ts: 1,
                since_ms: 1,
                checked_ms: now_ms(),
            };
            mirror::install(&app, &h, false).await.unwrap();
        }
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let router = crate::web::router(app.clone());
        tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let r = http
            .get(format!("{base}/files/map.pmtiles"))
            .header("range", "bytes=0-7")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 206);
        assert_eq!(r.headers()["content-range"], "bytes 0-7/100008");
        assert_eq!(r.bytes().await.unwrap().as_ref(), b"PMTiles\x03");
        let r = http
            .get(format!("{base}/files/map_2026-11-18_abcdabcd.pmtiles"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.bytes().await.unwrap().len(), data.len());
        let r = http
            .get(format!("{base}/files/plan.pdf"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "too big to send whole");
        let r = http
            .get(format!("{base}/files/plan.pdf"))
            .header("range", "bytes=0-99")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 206);
        assert_eq!(r.headers()["content-type"], "application/pdf");
        assert_eq!(r.headers()["x-content-type-options"], "nosniff");
        for missing in ["nothing.pmtiles", "..%2Flibrary.db", "wiki.zim"] {
            let r = http
                .get(format!("{base}/files/{missing}"))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 404, "{missing}");
        }
        for page in [
            "/",
            "/wiki",
            "/maps",
            "/docs",
            "/alerts",
            "/about",
            "/api/catalog",
            "/api/alerts",
        ] {
            let r = http.get(format!("{base}{page}")).send().await.unwrap();
            assert_eq!(r.status(), 200, "{page}");
        }
    }

    async fn serve(app: Arc<App>) -> (String, reqwest::Client) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let router = crate::web::router(app);
        tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
        (base, reqwest::Client::builder().no_proxy().build().unwrap())
    }

    async fn text(http: &reqwest::Client, url: String) -> String {
        http.get(url).send().await.unwrap().text().await.unwrap()
    }

    /// Put `data` in place as the version this site serves of `item`.
    async fn serve_file(app: &App, item: &str, kind: Kind, ext: &str, data: &[u8]) -> Held {
        let path = app
            .cfg
            .content_dir()
            .join(format!("{item}_2026-11-18.{ext}"));
        std::fs::write(&path, data).unwrap();
        let h = Held {
            item: item.into(),
            kind,
            ext: ext.into(),
            version: "2026-11-18".into(),
            root: root_of(data),
            path,
            size: data.len() as u64,
            sha256: crate::sha256::hex_of(data),
            verified: "fetcher".into(),
            event_ts: 1000,
            since_ms: 1000,
            checked_ms: now_ms(),
        };
        mirror::install(app, &h, false).await.unwrap();
        h
    }

    /// Apply a `library.item` from this site's own core, as its `seq`th event.
    fn mine(app: &App, payload: Value, seq: u64, ts: i64) {
        let mut e = Fake::new(vec![]).add("library.item", payload, ts);
        e.author.key = "ab".repeat(32);
        e.seq = seq;
        e.id = format!("mine{seq}");
        let mut db = app.db();
        let tx = db.transaction().unwrap();
        Items.apply(&tx, &e, ts).unwrap();
        tx.commit().unwrap();
    }

    #[tokio::test]
    async fn every_page_credits_each_item_as_its_fetcher_states() {
        let d = tempfile::tempdir().unwrap();
        let (app, _core) = app(d.path(), "").await;
        let wiki = serve_old(&app).await;
        let mut tiles = b"PMTiles\x03".to_vec();
        tiles.extend([1u8; 100]);
        let map = serve_file(&app, "map", Kind::Pmtiles, "pmtiles", &tiles).await;
        let doc = serve_file(&app, "phones", Kind::Doc, "csv", b"name,phone\n").await;
        for (seq, p) in [
            json!({"item": "wiki", "kind": "zim", "title": "Wikipedia", "version": "2026-08",
                   "size": wiki.size, "root": wiki.root, "source": "test",
                   "license": "CC BY-SA 4.0",
                   "license_url": "https://example.org/by-sa?a=\"b\"",
                   "attribution": "Wikipedia <b>contributors</b>."}),
            json!({"item": "map", "kind": "pmtiles", "title": "Region", "version": "2026-11-18",
                   "size": map.size, "root": map.root, "source": "test", "license": "ODbL.",
                   "license_url": "javascript:alert(1)", "attribution": "OSM \"contributors\""}),
            json!({"item": "phones", "kind": "doc", "ext": "csv", "title": "Phones",
                   "version": "2026-11-18", "size": doc.size, "root": doc.root, "source": "test",
                   "license": ""}),
            json!({"item": "old", "kind": "doc", "ext": "csv", "title": "Old list",
                   "version": "2026-11-18", "size": 1, "root": "e".repeat(64), "source": "test"}),
        ]
        .into_iter()
        .enumerate()
        {
            mine(&app, p, seq as u64 + 1, 1000);
        }
        let (base, http) = serve(app.clone()).await;
        let wiki_credit = "License: <a href=\"https://example.org/by-sa?a=&quot;b&quot;\">CC \
                           BY-SA 4.0</a>. Credit: Wikipedia &lt;b&gt;contributors&lt;/b&gt;.";
        let map_credit = "License: ODbL. Credit: OSM &quot;contributors&quot;.";
        let doc_credit = "License not stated by the station that published it.";
        let pages = [
            ("/wiki", vec![wiki_credit]),
            ("/maps", vec![map_credit]),
            (
                "/viewer?map=map",
                vec![map_credit, "Region version 2026-11-18."],
            ),
            ("/docs", vec![doc_credit]),
            ("/", vec![wiki_credit, map_credit, doc_credit]),
            (
                "/about",
                vec![
                    wiki_credit,
                    map_credit,
                    doc_credit,
                    "License not recorded (published before 0.2.0).",
                    "97.115",
                ],
            ),
        ];
        for (path, want) in pages {
            let body = text(&http, format!("{base}{path}")).await;
            for w in want {
                assert!(body.contains(w), "{path} lacks {w}:\n{body}");
            }
            assert!(!body.contains("javascript:"), "{path}");
        }
        let api: Value =
            serde_json::from_str(&text(&http, format!("{base}/api/catalog")).await).unwrap();
        let map = api["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["item"] == "map")
            .unwrap();
        assert_eq!(map["license"], "ODbL.");
        assert!(map["license_url"].is_null());
    }

    #[tokio::test]
    async fn a_changed_license_is_restated_for_the_version_already_published() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("phones.csv");
        std::fs::write(&f, "name,phone\n").unwrap();
        let extra = format!(
            "[[item]]\nid = 'phones'\nkind = 'doc'\ntitle = 'Shelter phones'\nfile = '{}'\n\
             max_mb = 1\nlicense = 'CC BY 4.0'\nattribution = 'County office'\n\
             [mule]\nisland = 'north'\nto = ['south']\n",
            f.to_string_lossy().replace('\\', "/")
        );
        let (app, core) = app(d.path(), &extra).await;
        // published by 0.1.0, whose events carried no license
        let h = serve_file(&app, "phones", Kind::Doc, "csv", b"name,phone\n").await;
        let old = json!({"item": "phones", "kind": "doc", "ext": "csv",
                         "title": "Shelter phones", "version": "2026-11-18", "size": h.size,
                         "root": h.root, "source": "a file supplied by the site's operator"});
        mine(&app, old, 1, 1000);
        crate::fetch::round(&app).await.unwrap();
        let sent = core.lock().unwrap().appended.clone();
        assert_eq!(sent.len(), 1, "one restatement, no mule message: {sent:?}");
        let (kind, p) = &sent[0];
        assert_eq!(kind, "library.item");
        assert_eq!(p["root"], h.root.as_str());
        assert_eq!(p["version"], "2026-11-18");
        assert_eq!(p["license"], "CC BY 4.0");
        assert_eq!(p["attribution"], "County office");
        mine(&app, p.clone(), 2, 5000);
        let l = crate::catalog::latest(&app.db()).unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].credit.license, "CC BY 4.0");
        assert_eq!(l[0].ts, 1000, "still fetched when it was");
        crate::fetch::round(&app).await.unwrap();
        assert_eq!(core.lock().unwrap().appended.len(), 1, "said once");
        // an older restatement arriving late doesn't undo a newer one
        let mut stale = p.clone();
        stale["license"] = json!("something older");
        mine(&app, stale, 1, 900);
        assert_eq!(
            crate::catalog::latest(&app.db()).unwrap()[0].credit.license,
            "CC BY 4.0"
        );
    }

    #[tokio::test]
    async fn a_database_from_0_1_0_opens_and_its_versions_have_no_recorded_license() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("lib")).unwrap();
        {
            let c = rusqlite::Connection::open(d.path().join("lib/library.db")).unwrap();
            c.execute_batch(
                "CREATE TABLE items (event_id TEXT PRIMARY KEY, ts INTEGER NOT NULL,
                     author_key TEXT NOT NULL, author_call TEXT NOT NULL, item TEXT NOT NULL,
                     kind TEXT NOT NULL, ext TEXT NOT NULL, title TEXT NOT NULL,
                     version TEXT NOT NULL, size INTEGER NOT NULL, root TEXT NOT NULL,
                     source TEXT NOT NULL, sha256 TEXT, hash_from TEXT, tiers TEXT NOT NULL,
                     grp TEXT NOT NULL);
                 INSERT INTO items VALUES ('e1', 1000, 'k', 'N0OLD', 'wiki', 'zim', 'zim',
                     'Wikipedia', '2026-08', 10, 'aaaa', 'test', NULL, NULL, '[]', '');
                 CREATE TABLE sources (item TEXT PRIMARY KEY, ident TEXT,
                     checked_ms INTEGER NOT NULL DEFAULT 0, error TEXT, outbox TEXT,
                     candidate TEXT, fails INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO sources (item, ident) VALUES ('wiki', 'x');",
            )
            .unwrap();
        }
        let (app, _core) = app(d.path(), "").await;
        let l = crate::catalog::latest(&app.db()).unwrap();
        assert_eq!(l[0].credit, crate::catalog::Credit::default());
        let (base, http) = serve(app.clone()).await;
        let about = text(&http, format!("{base}/about")).await;
        assert!(about.contains("License not recorded (published before 0.2.0)."));
    }

    #[tokio::test]
    async fn the_map_viewer_and_everything_it_loads_come_from_here() {
        use crate::viewer::{ASSETS, PREFIX};
        let prefix = PREFIX.as_str();
        let d = tempfile::tempdir().unwrap();
        let (app, _core) = app(d.path(), "").await;
        let (base, http) = serve(app.clone()).await;
        // no map yet: the page says so and loads no script
        let body = http
            .get(format!("{base}/viewer"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            body.contains("No map files") && !body.contains("<script"),
            "{body}"
        );
        let path = app
            .cfg
            .content_dir()
            .join("region_2026-11-18_abcdabcd.pmtiles");
        std::fs::write(&path, b"PMTiles\x03rest").unwrap();
        let h = Held {
            item: "region".into(),
            kind: Kind::Pmtiles,
            ext: "pmtiles".into(),
            version: "2026-11-18".into(),
            root: "abcdabcd".repeat(8),
            path,
            size: 12,
            sha256: String::new(),
            verified: "fetcher".into(),
            event_ts: 1,
            since_ms: 1,
            checked_ms: now_ms(),
        };
        mirror::install(&app, &h, false).await.unwrap();
        let body = http
            .get(format!("{base}/viewer"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            body.contains("data-file=\"/files/region.pmtiles\""),
            "{body}"
        );
        assert!(body.contains(&format!("data-assets=\"{prefix}\"")));
        assert!(body.contains("OpenStreetMap contributors") && body.contains("Protomaps"));
        assert!(
            body.contains("<noscript>"),
            "a page without JavaScript still says where the map is"
        );
        assert!(
            !body
                .replace("pmtiles://", "")
                .replace("glyphs://", "")
                .contains("://")
                && !body.contains("\"//"),
            "nothing is fetched from elsewhere"
        );
        for wired in [
            format!("href=\"{prefix}/maplibre-gl.css\""),
            format!("src=\"{prefix}/pmtiles.js\""),
            format!("src=\"{prefix}/basemaps.js\""),
            "<script type=module>".to_string(),
        ] {
            assert!(body.contains(&wired), "{wired}");
        }
        let r = http
            .get(format!("{base}/viewer?map=nope"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
        let maps = http
            .get(format!("{base}/maps"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(maps.contains("/viewer?map=region"));
        for a in ASSETS {
            let url = format!("{base}{prefix}/{}", a.path.replace(' ', "%20"));
            let r = http.get(&url).send().await.unwrap();
            assert_eq!(r.status(), 200, "{url}");
            assert_eq!(r.headers()["content-type"], a.ctype, "{url}");
            assert!(
                r.headers()["cache-control"]
                    .to_str()
                    .unwrap()
                    .contains("immutable")
            );
            assert_eq!(r.headers()["x-content-type-options"], "nosniff");
            let got = r.bytes().await.unwrap();
            assert_eq!(
                crate::sha256::hex_of(&got),
                crate::sha256::hex_of(a.body),
                "{url}"
            );
        }
        // MapLibre asks for its worker with a version query, and for whole font stacks
        let r = http
            .get(format!("{base}{prefix}/maplibre-gl-worker.mjs?v=6.19.0"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let r = http
            .get(format!(
                "{base}{prefix}/fonts/Noto%20Sans%20Regular,Noto%20Sans%20Devanagari%20Regular%20v1/0-255.pbf"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        for missing in [
            format!("{prefix}/fonts/Noto%20Sans%20Regular/2304-2559.pbf"),
            "/viewer/000000000000/maplibre-gl.mjs".to_string(),
            format!("{prefix}/..%2F..%2FCargo.toml"),
        ] {
            let r = http.get(format!("{base}{missing}")).send().await.unwrap();
            assert_eq!(r.status(), 404, "{missing}");
        }
        // a glyph range not held is not asked for again on every visit
        let r = http
            .get(format!(
                "{base}{prefix}/fonts/Noto%20Sans%20Regular/2304-2559.pbf"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(r.headers()["cache-control"], "public, max-age=86400");
        let r = http
            .get(format!("{base}/about/third-party"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.headers()["content-type"], "text/plain; charset=utf-8");
        assert!(r.text().await.unwrap().contains("BSD-3-Clause"));
    }
}
