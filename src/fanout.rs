//! Topology-aware fan-out: a site copies a new version from the best-connected site that
//! has it, and waits while a library site between it and that copy is still fetching, so
//! the content crosses each radio hop once instead of once per site behind it.
//!
//! The mesh graph comes from the links every site's core polls (`node.poll`), each weighted
//! by its latest measured quality. The core is then asked to fetch from the best holders
//! first (`blob_want_from`); it falls back to other peers for chunks they lack, so a copy
//! never stalls on one site being down.

use std::collections::BTreeMap;

use anyhow::Result;
use rusqlite::Connection;

/// Each node's neighbours and the quality of the link to each, 0 to 100.
pub type Graph = BTreeMap<String, BTreeMap<String, f64>>;

/// What a link with no quality, SNR or anything else measured counts as: weak, so an
/// unmeasured link never wins over one known to be good.
pub const UNKNOWN: f64 = 10.0;

/// A sample's quality, 0 to 100: the worse of the two directions when the node reports
/// both (a transfer needs the link to work both ways), else SNR scaled so 30 dB or more is
/// a perfect link, else unknown.
pub fn sample_quality(quality: Option<f64>, nquality: Option<f64>, snr: Option<f64>) -> f64 {
    let q = match (quality, nquality) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    q.or_else(|| snr.map(|s| s * 100.0 / 30.0))
        .unwrap_or(UNKNOWN)
        .clamp(0.0, 100.0)
}

/// Links seen by any site's poll since `since`, each weighted by its latest sample. When
/// both ends report a link, the worse of their latest views counts.
pub fn links(conn: &Connection, since: i64) -> Result<Graph> {
    let mut st = conn.prepare(
        "SELECT node, neighbor, quality, nquality, snr FROM link_samples s
         WHERE ts >= ?1 AND ts = (SELECT max(ts) FROM link_samples t
                                  WHERE t.node = s.node AND t.neighbor = s.neighbor)",
    )?;
    let mut g = Graph::new();
    for row in st.query_map([since], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<f64>>(2)?,
            r.get::<_, Option<f64>>(3)?,
            r.get::<_, Option<f64>>(4)?,
        ))
    })? {
        let (a, b, q, nq, snr) = row?;
        add_link(&mut g, &a, &b, sample_quality(q, nq, snr));
    }
    Ok(g)
}

/// Add a link both ways; a link already known keeps the worse of the two qualities.
pub fn add_link(g: &mut Graph, a: &str, b: &str, q: f64) {
    for (x, y) in [(a, b), (b, a)] {
        let e = g
            .entry(x.to_string())
            .or_default()
            .entry(y.to_string())
            .or_insert(q);
        *e = e.min(q);
    }
}

/// For every node reachable from any of `from`: the best path's weakest link, and its hops.
///
/// A path is judged by its weakest link because a bulk transfer relayed over several hops
/// runs no faster than the slowest of them, and one bad link loses chunks for the whole
/// path; summing costs would let a short path through one terrible link beat a longer
/// path of solid ones. Among equally good paths, fewer hops win (less airtime).
pub fn best_paths(g: &Graph, from: &[String]) -> BTreeMap<String, (f64, u32)> {
    let mut best: BTreeMap<String, (f64, u32)> = from
        .iter()
        .map(|f| (f.clone(), (f64::INFINITY, 0)))
        .collect();
    let mut done: Vec<String> = vec![];
    // widest-path Dijkstra; meshes have dozens of nodes, so a linear scan is plenty
    while let Some((n, (w, h))) = best
        .iter()
        .filter(|(n, _)| !done.contains(n))
        .max_by(|a, b| better(*a.1, *b.1))
        .map(|(n, v)| (n.clone(), *v))
    {
        done.push(n.clone());
        for (m, q) in g.get(&n).into_iter().flatten() {
            let cand = (w.min(*q), h + 1);
            if !done.contains(m)
                && best
                    .get(m)
                    .is_none_or(|old| better(cand, *old) == std::cmp::Ordering::Greater)
            {
                best.insert(m.clone(), cand);
            }
        }
    }
    best
}

/// Order (weakest link, hops): a stronger weakest link is better, then fewer hops.
fn better(a: (f64, u32), b: (f64, u32)) -> std::cmp::Ordering {
    a.0.total_cmp(&b.0).then(b.1.cmp(&a.1))
}

/// The library site this one should wait for, if any: one that is still copying this
/// version and sits on this site's best path (see `best_paths`) to a copy, so the copy
/// comes down the best-connected tree. None means go ahead (a copy is a neighbour, nobody
/// is on the way, or the graph doesn't say).
pub fn wait_for(g: &Graph, me: &str, holders: &[String], copying: &[String]) -> Option<String> {
    let to_copy = best_paths(g, holders);
    let mine = *to_copy.get(me)?;
    if mine.1 <= 1 {
        return None;
    }
    let from_me = best_paths(g, &[me.to_string()]);
    copying
        .iter()
        .filter(|c| c.as_str() != me)
        .filter_map(|c| {
            let (a, b) = (from_me.get(c)?, to_copy.get(c)?);
            // going through c is as good as the best path from here, and gets closer
            let through = (a.0.min(b.0), a.1 + b.1);
            (through == mine && b.1 < mine.1).then_some((a.1, c))
        })
        .min()
        .map(|(_, c)| c.clone())
}

/// The most peers the core takes in one preference list.
pub const MAX_SOURCES: usize = 16;

/// Core URLs of the holders, best path from `me` first (see `best_paths`); holders the
/// graph can't place go last, in the order given.
pub fn nearest_first(g: &Graph, me: &str, holders: &[(String, String)]) -> Vec<String> {
    // until the core has polled its node there is no "here" to measure from
    let paths = if me.is_empty() {
        BTreeMap::new()
    } else {
        best_paths(g, &[me.to_string()])
    };
    let mut v: Vec<((f64, u32), usize, &str)> = holders
        .iter()
        .enumerate()
        .map(|(i, (node, url))| {
            let p = paths
                .get(node)
                .copied()
                .unwrap_or((f64::NEG_INFINITY, u32::MAX));
            (p, i, url.as_str())
        })
        .collect();
    v.sort_by(|a, b| better(b.0, a.0).then(a.1.cmp(&b.1)));
    let mut out: Vec<String> = vec![];
    for (_, _, url) in v {
        if !out.iter().any(|u| u == url) && out.len() < MAX_SOURCES {
            out.push(url.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fetcher - hub - relay - far, and a spur hub - side, all good links
    fn mesh() -> Graph {
        let mut g = Graph::new();
        for (a, b) in [
            ("fetcher", "hub"),
            ("hub", "relay"),
            ("relay", "far"),
            ("hub", "side"),
        ] {
            add_link(&mut g, a, b, 90.0);
        }
        g
    }

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    fn holders(h: &[&str]) -> Vec<(String, String)> {
        h.iter()
            .map(|n| (n.to_string(), format!("http://{n}:8790")))
            .collect()
    }

    #[test]
    fn copies_flow_down_the_tree() {
        let g = mesh();
        let holders = v(&["fetcher"]);
        let copying = v(&["hub", "relay", "far", "side"]);
        // the hub is next to the copy: it goes first
        assert_eq!(wait_for(&g, "hub", &holders, &copying), None);
        // everyone behind the hub waits for it
        assert_eq!(
            wait_for(&g, "far", &holders, &copying).as_deref(),
            Some("relay")
        );
        assert_eq!(
            wait_for(&g, "side", &holders, &copying).as_deref(),
            Some("hub")
        );
        // once the hub has it, the relay and the spur go; far still waits for the relay
        let holders = v(&["fetcher", "hub"]);
        let copying = v(&["relay", "far", "side"]);
        assert_eq!(wait_for(&g, "relay", &holders, &copying), None);
        assert_eq!(wait_for(&g, "side", &holders, &copying), None);
        assert_eq!(
            wait_for(&g, "far", &holders, &copying).as_deref(),
            Some("relay")
        );
    }

    #[test]
    fn a_site_waits_for_the_better_connected_site_not_the_nearer_one() {
        // far - 20 - near - 90 - src, and far - 90 - good - 90 - good2 - 90 - src
        let mut g = Graph::new();
        add_link(&mut g, "far", "near", 20.0);
        add_link(&mut g, "near", "src", 90.0);
        add_link(&mut g, "far", "good", 90.0);
        add_link(&mut g, "good", "good2", 90.0);
        add_link(&mut g, "good2", "src", 90.0);
        let src = v(&["src"]);
        // by hops it would wait for near; the strong path goes through good2
        assert_eq!(
            wait_for(&g, "far", &src, &v(&["near", "good2"])).as_deref(),
            Some("good2")
        );
        // of the sites on that path, the one nearest here is waited for
        assert_eq!(
            wait_for(&g, "far", &src, &v(&["near", "good2", "good"])).as_deref(),
            Some("good")
        );
        // a site off the best path is not waited for at all
        assert_eq!(wait_for(&g, "far", &src, &v(&["near"])), None);
        // with equal links, fewer hops decide again
        let mut h = Graph::new();
        add_link(&mut h, "far", "near", 90.0);
        add_link(&mut h, "near", "src", 90.0);
        add_link(&mut h, "far", "good", 90.0);
        add_link(&mut h, "good", "good2", 90.0);
        add_link(&mut h, "good2", "src", 90.0);
        assert_eq!(
            wait_for(&h, "far", &src, &v(&["near", "good2"])).as_deref(),
            Some("near")
        );
    }

    #[test]
    fn with_equal_links_the_nearest_holder_is_asked_first() {
        let g = mesh();
        let mut h = holders(&["fetcher", "relay", "hub"]);
        h.insert(1, ("nowhere".into(), "http://nowhere:8790".into()));
        assert_eq!(
            nearest_first(&g, "far", &h),
            [
                "http://relay:8790",
                "http://hub:8790",
                "http://fetcher:8790",
                "http://nowhere:8790"
            ]
        );
    }

    #[test]
    fn a_strong_path_beats_a_short_weak_one() {
        // here - weak(30) - a, and here - 85 - b1 - 80 - b: b is two hops but better
        let mut g = Graph::new();
        add_link(&mut g, "here", "a", 30.0);
        add_link(&mut g, "here", "b1", 85.0);
        add_link(&mut g, "b1", "b", 80.0);
        assert_eq!(
            nearest_first(&g, "here", &holders(&["a", "b"])),
            ["http://b:8790", "http://a:8790"]
        );
        // a path is as good as its weakest link: 99 then 20 loses to a single 40
        add_link(&mut g, "here", "c1", 99.0);
        add_link(&mut g, "c1", "c", 20.0);
        add_link(&mut g, "here", "d", 40.0);
        assert_eq!(
            nearest_first(&g, "here", &holders(&["c", "d"])),
            ["http://d:8790", "http://c:8790"]
        );
        // the best path is found even when a worse one reaches the node first
        add_link(&mut g, "here", "e", 15.0);
        add_link(&mut g, "b", "e", 70.0);
        assert_eq!(best_paths(&g, &["here".to_string()])["e"], (70.0, 3));
    }

    #[test]
    fn unmeasured_links_count_as_weak() {
        assert_eq!(sample_quality(None, None, None), UNKNOWN);
        assert_eq!(sample_quality(Some(90.0), Some(60.0), None), 60.0);
        assert_eq!(sample_quality(None, Some(70.0), Some(3.0)), 70.0);
        assert_eq!(sample_quality(None, None, Some(15.0)), 50.0);
        assert_eq!(sample_quality(None, None, Some(45.0)), 100.0);
        // a measured mediocre link beats an unmeasured one
        let mut g = Graph::new();
        add_link(&mut g, "here", "x", sample_quality(None, None, None));
        add_link(&mut g, "here", "y", sample_quality(Some(40.0), None, None));
        assert_eq!(
            nearest_first(&g, "here", &holders(&["x", "y"])),
            ["http://y:8790", "http://x:8790"]
        );
    }

    #[test]
    fn a_link_seen_from_both_ends_counts_its_worse_side() {
        let mut g = Graph::new();
        add_link(&mut g, "a", "b", 80.0);
        add_link(&mut g, "b", "a", 50.0);
        assert_eq!(g["a"]["b"], 50.0);
        assert_eq!(g["b"]["a"], 50.0);
    }

    #[test]
    fn the_list_has_no_repeats_and_stops_at_the_core_limit() {
        let g = mesh();
        let mut h: Vec<(String, String)> = (0..20)
            .map(|i| (format!("n{i}"), format!("http://h{i}:8790")))
            .collect();
        h.push(("hub".into(), "http://h3:8790".into()));
        let got = nearest_first(&g, "far", &h);
        assert_eq!(got.len(), MAX_SOURCES);
        assert_eq!(got[0], "http://h3:8790");
        let mut s = got.clone();
        s.sort();
        s.dedup();
        assert_eq!(s.len(), got.len());
        // no node known yet: the order given, nothing invented
        assert_eq!(nearest_first(&g, "", &h)[0], "http://h0:8790");
    }

    #[test]
    fn an_unknown_graph_never_blocks() {
        let g = mesh();
        assert_eq!(
            wait_for(&g, "elsewhere", &v(&["fetcher"]), &v(&["hub"])),
            None
        );
        assert_eq!(
            wait_for(&Graph::new(), "far", &v(&["fetcher"]), &v(&["hub"])),
            None
        );
        // a site that isn't copying this item is not waited for
        assert_eq!(wait_for(&g, "far", &v(&["fetcher"]), &v(&[])), None);
    }

    #[test]
    fn links_come_from_the_latest_samples() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        ridgewarden_client::core::init_db(&c, &[&ridgewarden_client::mesh::Mesh]).unwrap();
        let add = |ts: i64, node: &str, nb: &str, q: Option<f64>, snr: Option<f64>| {
            c.execute(
                "INSERT INTO link_samples (event_id, ts, node, neighbor, quality, snr)
                 VALUES ('e', ?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![ts, node, nb, q, snr],
            )
            .unwrap();
        };
        add(100, "a", "b", Some(20.0), None);
        add(200, "a", "b", Some(90.0), None);
        add(200, "b", "c", None, Some(24.0));
        add(200, "c", "d", None, None);
        add(10, "d", "e", Some(99.0), None);
        let g = links(&c, 50).unwrap();
        assert_eq!(g["a"]["b"], 90.0, "the latest sample, not the first");
        assert_eq!(g["b"]["c"], 80.0);
        assert_eq!(g["c"]["d"], UNKNOWN);
        assert!(!g.contains_key("e"), "too old");
    }
}
